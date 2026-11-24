//! The run object: one per run, the owner of every machine of the run
//!
//! Every lease ending goes through [`end_lease`], so each record says who ended its lease and
//! why. Every handler that changes state re-arms the alarm last, from storage, for the
//! earliest of: a lease deadline, a cleanup retry, the moment the run's spend would reach its
//! cap, a stale pending create, and a permit that still has to be returned.

use std::{collections::BTreeMap, time::Duration};

use cloudlease_core::{
    alarm::{Backoff, Wake},
    capacity::{AbsenceProof, AcquireRequest, Permit, ReleaseRequest},
    holding::{Holding, HoldingError},
    ids::Generation,
    lease::{Lease, LeaseError},
    time::UnixMillis,
};
use cloudlease_machines::{
    alert::{Alert, Priority},
    boot::{Boot, LeaseFile, VolumeMount, check_fits, render_user_data},
    config::{HEARTBEAT_TTL, MachineClass, MachinesConfig, renew_policy},
    ids::{AppName, RunId},
    run::{
        Admission, Admitted, LeaseEnd, MachineEntry, MachineRecord, RunEnd, RunRecord, admit,
        confirm, end_lease, lease_view, run_spend, run_view,
    },
    time::Timestamp,
    token::LeaseToken,
    wire::{
        ApiError, Caller, CreateMachineRequest, CreateRunRequest, ErrorCode, LeaseView,
        ReleaseLeaseRequest, ReleaseReason, ReleasedBy, RenewView, RunAction, RunCall, RunView,
    },
};
use cloudlease_provider::digitalocean::{DoKind, DoSpec, DropletSpec, ResourceName, VolumeSpec};
use cloudlease_worker::{
    alarm::{rearm, rearm_after_failure},
    capacity::{Acquired, CapacityClient},
    error::{CloudleaseError, CloudleaseResult},
    http::read_json,
    owner::{self, SWEEP_PATH},
    runtime,
    store::HoldingStore,
};
use worker::{DurableObject, Env, Method, Request, Response, State, Storage, durable_object};

use crate::{
    GATE, GATE_CONFIG, GATE_OBJECT, PENDING_EXPIRY, PENDING_RESUME, PERMIT_MARGIN, PERMIT_RETRY,
    app::{
        alerts::notify,
        api::CALL_PATH,
        settings::{answer, config, error_response, internal, provider},
    },
};

type Answer = Result<Response, ApiError>;

/// One run's records, holdings and alarm
#[durable_object(alarm)]
pub struct Run {
    state: State,
    env: Env,
}

impl DurableObject for Run {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut request: Request) -> worker::Result<Response> {
        let path = request.path();
        if request.method() == Method::Post && path == SWEEP_PATH {
            let holdings = HoldingStore::open(self.state.storage().sql())?;
            return owner::answer_sweep::<DoKind>(&holdings, &mut request, runtime::now()).await;
        }
        if request.method() != Method::Post || path != CALL_PATH {
            return error_response(&ApiError::new(ErrorCode::NotFound, "no such route"));
        }

        let call: RunCall = match read_json(&mut request).await {
            Ok(call) => call,
            Err(error) => return error.into_response(),
        };
        match self.handle(call).await {
            Ok(response) => Ok(response),
            Err(error) => error_response(&error),
        }
    }

    async fn alarm(&self) -> worker::Result<Response> {
        let stores = Stores::open(&self.state)?;
        let now = runtime::now();
        if let Err(error) = self.alarm_pass(&stores, now).await {
            // a failing pass must leave an alarm, or nothing enforces the run's deadlines
            rearm_after_failure(&stores.storage, now).await?;
            return Err(error.into());
        }

        Response::empty()
    }
}

struct Stores {
    storage: Storage,
    runs: super::store::RunStore,
    holdings: HoldingStore,
}

impl Stores {
    fn open(state: &State) -> CloudleaseResult<Self> {
        let storage = state.storage();

        Ok(Self {
            runs: super::store::RunStore::open(storage.sql())?,
            holdings: HoldingStore::open(storage.sql())?,
            storage,
        })
    }

    fn load(&self) -> CloudleaseResult<Option<Loaded>> {
        let Some(run) = self.runs.run()? else {
            return Ok(None);
        };
        let holdings = self
            .holdings
            .list::<DoKind>()?
            .into_iter()
            .map(|holding| (holding.generation(), holding))
            .collect();

        Ok(Some(Loaded {
            run,
            records: self.runs.records()?,
            holdings,
        }))
    }

    fn put_admitted(&self, record: &MachineRecord, admitted: Admitted) -> CloudleaseResult<()> {
        self.runs.put_record(&MachineRecord {
            admission: Admission::Admitted(admitted),
            ..record.clone()
        })
    }
}

struct Loaded {
    run: RunRecord,
    records: Vec<MachineRecord>,
    holdings: BTreeMap<Generation, Holding<DoKind>>,
}

struct Machine<'a> {
    record: &'a MachineRecord,
    admitted: &'a Admitted,
    holding: &'a Holding<DoKind>,
}

impl Loaded {
    fn entries(&self) -> Vec<MachineEntry<'_>> {
        self.records
            .iter()
            .map(|record| MachineEntry {
                record,
                holding: self.holdings.get(&record.generation),
            })
            .collect()
    }

    fn view(&self, now: UnixMillis) -> RunView {
        run_view(&self.run, &self.entries(), now)
    }

    fn machines(&self) -> impl Iterator<Item = Machine<'_>> {
        self.records.iter().filter_map(|record| {
            Some(Machine {
                record,
                admitted: record.admitted()?,
                holding: self.holdings.get(&record.generation)?,
            })
        })
    }

    fn machine(&self, generation: Generation) -> Option<Machine<'_>> {
        self.machines()
            .find(|machine| machine.record.generation == generation)
    }

    fn lease_view(&self, generation: Generation) -> Result<LeaseView, ApiError> {
        let machine = self
            .machine(generation)
            .ok_or_else(|| internal(format!("machine {generation} has no lease")))?;

        Ok(lease_view(
            &self.run,
            machine.record,
            machine.admitted,
            machine.holding,
        ))
    }
}

fn not_found(caller: &Caller, code: ErrorCode, message: &str) -> ApiError {
    // a bearer that is no client token proves nothing until it matches a lease
    match caller {
        Caller::Lease(_) => unauthorized(),
        Caller::Client(_) => ApiError::new(code, message),
    }
}

fn unauthorized() -> ApiError {
    ApiError::new(ErrorCode::Unauthorized, "a valid bearer token is required")
}

fn lease_ended() -> ApiError {
    ApiError::new(ErrorCode::LeaseEnded, "the lease has ended")
}

fn client_app(caller: &Caller) -> Result<&AppName, ApiError> {
    match caller {
        Caller::Client(app) => Ok(app),
        Caller::Lease(_) => Err(unauthorized()),
    }
}

impl Run {
    async fn handle(&self, call: RunCall) -> Answer {
        let stores = Stores::open(&self.state).map_err(internal)?;
        let now = runtime::now();
        let RunCall {
            caller,
            run,
            action,
        } = call;

        match action {
            RunAction::Create(request) => {
                Self::create_run(&stores, &self.env, &caller, &run, request, now)
            }
            RunAction::Read => {
                let loaded = client_run(&stores, &caller)?;
                answer(200, &loaded.view(now))
            }
            RunAction::Release(_) => self.release_run(&stores, &caller, now).await,
            RunAction::CreateMachine(request) => {
                self.create_machine(&stores, &caller, request, now).await
            }
            RunAction::ReadLease(generation) => {
                self.read_lease(&stores, &caller, generation, now).await
            }
            RunAction::ReleaseLease(generation, request) => {
                self.release_lease(&stores, &caller, generation, request, now)
                    .await
            }
            RunAction::RenewLease(generation) => {
                self.renew_lease(&stores, &caller, generation, now).await
            }
        }
    }

    fn create_run(
        stores: &Stores,
        env: &Env,
        caller: &Caller,
        run_id: &RunId,
        request: CreateRunRequest,
        now: UnixMillis,
    ) -> Answer {
        let app = client_app(caller)?;
        if *run_id != RunId::derive(app, request.request_id) {
            return Err(ApiError::new(
                ErrorCode::InvalidRequest,
                "the run does not match the request",
            ));
        }
        if let Some(loaded) = stores.load().map_err(internal)? {
            if loaded.run.app == *app && loaded.run.matches(&request) {
                return answer(200, &loaded.view(now));
            }
            return Err(ApiError::new(
                ErrorCode::RequestConflict,
                "the request_id was already used with a different body",
            ));
        }

        let config = config(env)?;
        let class = config.class_for(app, &request.class).map_err(|_| {
            ApiError::new(
                ErrorCode::UnknownClass,
                format!("{app} may not use class {}", request.class),
            )
        })?;
        check_run_request(&config, &request, now)?;

        let run = RunRecord {
            run_id: run_id.clone(),
            app: app.clone(),
            request_id: request.request_id,
            name: request.name,
            class: request.class,
            max_machines: request.max_machines,
            deadline_at: request.deadline_at.0,
            created_at: now,
            price_per_hour: class.price_per_hour,
            gate_units: class.gate_units,
            spend_cap: config.run_spend_cap,
            ended: None,
            create_failure_alerted: false,
        };
        stores.runs.put_run(&run).map_err(internal)?;

        answer(201, &run_view(&run, &[], now))
    }

    async fn release_run(&self, stores: &Stores, caller: &Caller, now: UnixMillis) -> Answer {
        let loaded = client_run(stores, caller)?;
        if loaded.run.ended.is_none() {
            let run = RunRecord {
                ended: Some(RunEnd::Requested { at: now }),
                ..loaded.run.clone()
            };
            stores.runs.put_run(&run).map_err(internal)?;
        }
        let end = LeaseEnd::Revoke {
            reason: ReleaseReason::RunReleased,
            by: ReleasedBy::Client,
        };
        for machine in loaded.machines() {
            end_machine(stores, &machine, end, now).await?;
        }

        self.rearm(stores, now).await.map_err(internal)?;
        let loaded = client_run(stores, caller)?;
        answer(202, &loaded.view(now))
    }

    async fn create_machine(
        &self,
        stores: &Stores,
        caller: &Caller,
        request: CreateMachineRequest,
        now: UnixMillis,
    ) -> Answer {
        let loaded = client_run(stores, caller)?;
        let boot = Boot::new(request.boot.clone())
            .map_err(|error| ApiError::new(ErrorCode::InvalidBoot, error.to_string()))?;

        let existing = stores
            .runs
            .by_request(request.request_id)
            .map_err(internal)?;
        if let Some(record) = &existing {
            if !record.matches(&request) {
                return Err(ApiError::new(
                    ErrorCode::RequestConflict,
                    "the request_id was already used with a different body",
                ));
            }
            match record.admission {
                Admission::Admitted(_) => {
                    return answer(200, &loaded.lease_view(record.generation)?);
                }
                Admission::Pending { since } if now < since.saturating_add(PENDING_RESUME) => {
                    return Err(ApiError::new(
                        ErrorCode::Busy,
                        "the same request is still being handled",
                    ));
                }
                // the first attempt stalled or failed on the gate; this retry takes over
                Admission::Pending { .. } => {}
            }
        }

        let config = config(&self.env)?;
        let class = run_class(&config, &loaded.run)?;
        let mount = class.volume.as_ref().map(|volume| &volume.mount);
        check_fits(&boot, &config.api_base, mount)
            .map_err(|error| ApiError::new(ErrorCode::InvalidBoot, error.to_string()))?;

        let record = match existing {
            Some(record) => record,
            None => {
                admit(&loaded.run, &loaded.entries(), now)?;
                let generation = stores.runs.next_generation().map_err(internal)?;
                let record = MachineRecord::pending(generation, &request, now);
                stores.runs.put_record(&record).map_err(internal)?;
                // the record reserves spend and a slot until it expires, so its expiry must be
                // armed before the gate call, which this object may not outlive
                self.rearm(stores, now).await.map_err(internal)?;
                record
            }
        };

        let create = Create {
            config: &config,
            class,
            boot: &boot,
        };
        let answered = self.admit_and_provision(stores, record, &create, now).await;
        // a pending record counts toward max_machines, so one left by a failure must still
        // expire on the alarm
        if answered.is_err()
            && let Err(error) = self.rearm(stores, runtime::now().max(now)).await
        {
            worker::console_error!("re-arming after a failed create: {error}");
        }

        answered
    }

    /// Takes a gate permit for a pending record, starts its lease and creates its resources
    async fn admit_and_provision(
        &self,
        stores: &Stores,
        record: MachineRecord,
        create: &Create<'_>,
        now: UnixMillis,
    ) -> Answer {
        let Create {
            config,
            class,
            boot,
        } = *create;
        let generation = record.generation;
        let loaded = stores
            .load()
            .map_err(internal)?
            .ok_or_else(|| internal("the run vanished"))?;
        let owner = loaded.run.run_id.owner().map_err(internal)?;
        let token = mint_token()?;

        let gate = CapacityClient::connect(&self.env, GATE, GATE_OBJECT).map_err(internal)?;
        let acquire = AcquireRequest {
            owner_id: owner.clone(),
            generation,
            ttl_ms: permit_ttl(&loaded.run, class, now),
            units: loaded.run.gate_units,
        };
        // on a gate error the record stays pending, and a retry after PENDING_RESUME resumes
        let permit = match gate.acquire(&acquire).await.map_err(internal)? {
            Acquired::Granted(permit) => permit,
            Acquired::Exhausted => {
                stores.runs.delete_record(generation).map_err(internal)?;
                notify(&self.env, &hourly_cap_alert(&loaded.run)).await;
                return Err(ApiError::new(
                    ErrorCode::SpendCapReached,
                    "the hourly spend cap is reached: machines already running cost as much \
                     per hour as cloudlease allows",
                ));
            }
        };

        // the run and the record may have changed while the gate answered
        let now = runtime::now().max(now);
        let loaded = stores
            .load()
            .map_err(internal)?
            .ok_or_else(|| internal("the run vanished"))?;
        let current = stores
            .runs
            .record(generation)
            .map_err(internal)?
            .ok_or_else(|| internal("the pending machine vanished"))?;
        if current.admitted().is_some() {
            return answer(200, &loaded.lease_view(generation)?);
        }

        let started = confirm(&loaded.run, &loaded.entries(), now).and_then(|()| {
            let lease_id = current
                .machine_id(&loaded.run.run_id)
                .lease_id()
                .map_err(internal)?;
            let policy = class.lease_policy().map_err(internal)?;
            Lease::start(lease_id, &policy, now, Some(loaded.run.deadline_at)).map_err(internal)
        });
        let lease = match started {
            Ok(lease) => lease,
            Err(error) => {
                // the permit goes back before the record goes, so a later create cannot reuse
                // this generation and be handed the permit this return then deletes
                return_permit(&gate, &permit).await;
                stores.runs.delete_record(generation).map_err(internal)?;
                return Err(error);
            }
        };

        let admitted = Admitted {
            token_hash: token.hash(),
            created_at: now,
            hard_deadline_at: lease.hard_deadline(),
            permit: permit.permit_id,
            permit_returned: false,
            release: None,
        };
        stores
            .put_admitted(&current, admitted.clone())
            .map_err(internal)?;
        let holding = Holding::<DoKind>::leased(owner, generation, lease);
        owner::save(&stores.storage, &stores.holdings, &holding, now)
            .await
            .map_err(internal)?;
        // owner::save and owner::create arm the alarm from holdings alone, so the spend wake
        // is missing while the provider calls run; the rearm after them restores it, and if
        // this object dies first, the next renew from any machine of the run does

        let provisioned = self
            .create_resources(
                stores,
                &loaded.run,
                class,
                config,
                &current,
                &admitted,
                &token,
                boot,
            )
            .await;
        if let Err(error) = provisioned {
            self.create_failed(stores, &loaded.run, &current, error)
                .await?;
        }

        self.rearm(stores, now).await.map_err(internal)?;
        let loaded = stores
            .load()
            .map_err(internal)?
            .ok_or_else(|| internal("the run vanished"))?;
        answer(201, &loaded.lease_view(generation)?)
    }

    #[allow(clippy::too_many_arguments)]
    // every argument is a distinct input of the create; bundling them would only rename them
    async fn create_resources(
        &self,
        stores: &Stores,
        run: &RunRecord,
        class: &MachineClass,
        config: &MachinesConfig,
        record: &MachineRecord,
        admitted: &Admitted,
        token: &LeaseToken,
        boot: &Boot,
    ) -> CloudleaseResult<()> {
        let provider = provider(&self.env)?;
        let generation = record.generation;

        let mut volume_ids = Vec::new();
        let volume_name = volume_name(&run.run_id, generation)?;
        let mut mount = None;
        if let Some(volume) = &class.volume {
            let spec = DoSpec::Volume(VolumeSpec {
                name: volume_name.clone(),
                region: class.region.clone(),
                size_gib: volume.size_gib,
            });
            let (id, _) = owner::create(
                &stores.storage,
                &stores.holdings,
                &provider,
                generation,
                &spec,
                runtime::now(),
            )
            .await?;
            volume_ids.push(id);
            mount = Some(VolumeMount {
                name: &volume_name,
                mount: &volume.mount,
            });
        }

        let lease_id = record.machine_id(&run.run_id).to_string();
        let user_data = render_user_data(
            &LeaseFile {
                lease_id: &lease_id,
                token: token.expose(),
                api_base: &config.api_base,
                hard_deadline_at: Timestamp(admitted.hard_deadline_at),
                heartbeat_ttl_secs: HEARTBEAT_TTL.as_secs(),
            },
            mount,
            boot,
        )
        .map_err(|error| CloudleaseError::Config(error.to_string()))?;
        let spec = DoSpec::Droplet(DropletSpec {
            name: ResourceName::parse(record.hostname.as_str())?,
            region: class.region.clone(),
            size: class.size.clone(),
            image: class.image.clone(),
            boot: user_data,
            ssh_key_ids: class.ssh_key_ids.clone(),
            volume_ids,
            vpc_uuid: class.vpc_uuid.clone(),
        });
        owner::create(
            &stores.storage,
            &stores.holdings,
            &provider,
            generation,
            &spec,
            runtime::now(),
        )
        .await?;

        Ok(())
    }

    /// Ends a lease whose create failed, so its cleanup finds anything that was created
    async fn create_failed(
        &self,
        stores: &Stores,
        run: &RunRecord,
        record: &MachineRecord,
        error: CloudleaseError,
    ) -> Result<(), ApiError> {
        // a lease that ended while the create was in flight is already being cleaned up
        if matches!(error, CloudleaseError::Holding(_)) {
            worker::console_warn!(
                "machine {} ended during its create: {error}",
                record.hostname
            );
            return Ok(());
        }

        let now = runtime::now();
        let loaded = stores
            .load()
            .map_err(internal)?
            .ok_or_else(|| internal("the run vanished"))?;
        if let Some(machine) = loaded.machine(record.generation) {
            let end = LeaseEnd::Revoke {
                reason: ReleaseReason::CreateFailed,
                by: ReleasedBy::Cloudlease,
            };
            end_machine(stores, &machine, end, now).await?;
        }

        if loaded.run.create_failure_alerted {
            worker::console_error!("create of {} failed: {error}", record.hostname);
            return Ok(());
        }
        let alerted = RunRecord {
            create_failure_alerted: true,
            ..loaded.run.clone()
        };
        stores.runs.put_run(&alerted).map_err(internal)?;
        notify(
            &self.env,
            &Alert {
                title: format!("cloudlease: machine create failed in run {}", run.name),
                message: format!(
                    "Run {} ({}, class {}): creating {} failed: {error}. Later failures in \
                     this run are logged, not alerted.",
                    run.run_id, run.name, run.class, record.hostname
                ),
                priority: Priority::High,
            },
        )
        .await;

        Ok(())
    }

    async fn release_lease(
        &self,
        stores: &Stores,
        caller: &Caller,
        generation: Generation,
        request: ReleaseLeaseRequest,
        now: UnixMillis,
    ) -> Answer {
        let loaded = loaded_for(stores, caller, ErrorCode::LeaseNotFound)?;
        let (machine, by) = lease_access(&loaded, caller, generation)?;
        let lease_id = machine
            .record
            .machine_id(&loaded.run.run_id)
            .lease_id()
            .map_err(internal)?;
        let release = LeaseEnd::Release {
            lease_id: &lease_id,
            reason: request.reason.into(),
            by,
        };

        let status = match end_machine(stores, &machine, release, now).await {
            Ok(true) => 202,
            Ok(false) => 200,
            // it lapsed before an alarm stored that; store it now, and the lease is unchanged
            // from the caller's point of view: already ending
            Err(error) if error.code == ErrorCode::LeaseEnded => {
                end_machine(stores, &machine, LeaseEnd::Expire, now).await?;
                200
            }
            Err(error) => return Err(error),
        };

        self.rearm(stores, now).await.map_err(internal)?;
        let loaded = loaded_for(stores, caller, ErrorCode::LeaseNotFound)?;
        answer(status, &loaded.lease_view(generation)?)
    }

    async fn read_lease(
        &self,
        stores: &Stores,
        caller: &Caller,
        generation: Generation,
        now: UnixMillis,
    ) -> Answer {
        let loaded = loaded_for(stores, caller, ErrorCode::LeaseNotFound)?;
        let (machine, by) = lease_access(&loaded, caller, generation)?;
        // a lease past a deadline has ended even when its alarm has not run yet
        if end_machine(stores, &machine, LeaseEnd::Expire, now).await? {
            self.rearm(stores, now).await.map_err(internal)?;
        }

        let loaded = loaded_for(stores, caller, ErrorCode::LeaseNotFound)?;
        let (machine, _) = lease_access(&loaded, caller, generation)?;
        // a lease token stops working once its lease ends
        if by == ReleasedBy::Machine && machine.holding.lease().is_none() {
            return Err(lease_ended());
        }

        answer(200, &loaded.lease_view(generation)?)
    }

    async fn renew_lease(
        &self,
        stores: &Stores,
        caller: &Caller,
        generation: Generation,
        now: UnixMillis,
    ) -> Answer {
        let loaded = loaded_for(stores, caller, ErrorCode::LeaseNotFound)?;
        let (machine, by) = lease_access(&loaded, caller, generation)?;
        if by == ReleasedBy::Client {
            return Err(ApiError::new(
                ErrorCode::Forbidden,
                "only the lease's own token renews it",
            ));
        }
        let lease_id = machine
            .record
            .machine_id(&loaded.run.run_id)
            .lease_id()
            .map_err(internal)?;
        let policy = renew_policy().map_err(internal)?;

        match machine.holding.renew(&lease_id, now, &policy) {
            Ok(renewed) => {
                owner::save(&stores.storage, &stores.holdings, &renewed, now)
                    .await
                    .map_err(internal)?;
                self.rearm(stores, now).await.map_err(internal)?;
                let heartbeat = renewed
                    .lease()
                    .map_or(machine.admitted.hard_deadline_at, Lease::heartbeat_due_at);
                answer(
                    200,
                    &RenewView {
                        heartbeat_deadline_at: Timestamp(heartbeat),
                        hard_deadline_at: Timestamp(machine.admitted.hard_deadline_at),
                    },
                )
            }
            Err(HoldingError::Lease(LeaseError::Expired(_))) => {
                end_machine(stores, &machine, LeaseEnd::Expire, now).await?;
                self.rearm(stores, now).await.map_err(internal)?;
                Err(lease_ended())
            }
            Err(HoldingError::InvalidTransition { .. }) => Err(lease_ended()),
            Err(error) => Err(internal(error)),
        }
    }

    async fn alarm_pass(&self, stores: &Stores, now: UnixMillis) -> CloudleaseResult<()> {
        let Some(loaded) = stores.load()? else {
            return Ok(());
        };

        for machine in loaded.machines() {
            end_machine(stores, &machine, LeaseEnd::Expire, now)
                .await
                .map_err(api_failure)?;
        }
        for record in &loaded.records {
            let stale = matches!(
                record.admission,
                Admission::Pending { since } if now >= since.saturating_add(PENDING_EXPIRY)
            );
            if stale {
                stores.runs.delete_record(record.generation)?;
            }
        }

        let loaded = stores
            .load()?
            .ok_or_else(|| CloudleaseError::Storage("the run vanished".into()))?;
        let spend = run_spend(&loaded.run, &loaded.entries(), now);
        if loaded.run.ended.is_none() && spend.has_reached(loaded.run.spend_cap) {
            self.hit_spend_cap(stores, &loaded, now).await?;
        }

        let provider = provider(&self.env)?;
        let processed = owner::run_alarm::<DoKind, _>(
            &stores.storage,
            &stores.holdings,
            &provider,
            now,
            &Backoff::default(),
        )
        .await?;
        for error in processed.errors {
            worker::console_warn!("cleanup will retry: {error}");
        }

        self.return_permits(stores).await?;
        self.rearm(stores, now).await
    }

    async fn hit_spend_cap(
        &self,
        stores: &Stores,
        loaded: &Loaded,
        now: UnixMillis,
    ) -> CloudleaseResult<()> {
        let run = RunRecord {
            ended: Some(RunEnd::SpendCap { at: now }),
            ..loaded.run.clone()
        };
        stores.runs.put_run(&run)?;

        let end = LeaseEnd::Revoke {
            reason: ReleaseReason::SpendCap,
            by: ReleasedBy::Cloudlease,
        };
        let mut ended = 0;
        for machine in loaded.machines() {
            if end_machine(stores, &machine, end, now)
                .await
                .map_err(api_failure)?
            {
                ended += 1;
            }
        }

        let spend = run_spend(&loaded.run, &loaded.entries(), now);
        notify(
            &self.env,
            &Alert {
                title: format!("cloudlease: run {} reached its spend cap", run.name),
                message: format!(
                    "Run {} ({}, class {}) has spent {} of its {} cap. Its {ended} live \
                     machines are being deleted and it takes no new ones.",
                    run.run_id, run.name, run.class, spend.accrued, run.spend_cap
                ),
                priority: Priority::Urgent,
            },
        )
        .await;

        Ok(())
    }

    /// Returns the gate permit of every machine whose resources are confirmed gone
    async fn return_permits(&self, stores: &Stores) -> CloudleaseResult<()> {
        let Some(loaded) = stores.load()? else {
            return Ok(());
        };
        let owner = loaded.run.run_id.owner()?;
        let due: Vec<_> = loaded
            .machines()
            .filter(|machine| machine.holding.is_released() && !machine.admitted.permit_returned)
            .map(|machine| (machine.record.clone(), machine.admitted.clone()))
            .collect();
        if due.is_empty() {
            return Ok(());
        }

        let gate = CapacityClient::connect(&self.env, GATE, GATE_OBJECT)?;
        for (record, admitted) in due {
            let request = ReleaseRequest {
                permit_id: admitted.permit.clone(),
                owner_id: owner.clone(),
                generation: record.generation,
                owner_absent: AbsenceProof,
            };
            if let Err(error) = gate.release(&request).await {
                worker::console_warn!("permit return will retry: {error}");
                continue;
            }

            stores.put_admitted(
                &record,
                Admitted {
                    permit_returned: true,
                    ..admitted
                },
            )?;
        }

        Ok(())
    }

    /// Sets the alarm for the earliest moment the run object must act
    async fn rearm(&self, stores: &Stores, now: UnixMillis) -> CloudleaseResult<()> {
        let Some(loaded) = stores.load()? else {
            return Ok(());
        };

        let mut wakes: Vec<Wake> = loaded.holdings.values().filter_map(Holding::wake).collect();
        if loaded.run.ended.is_none() {
            let spend = run_spend(&loaded.run, &loaded.entries(), now);
            wakes.extend(
                spend
                    .reaches_cap_at(loaded.run.spend_cap, now)
                    .map(Wake::At),
            );
        }
        for record in &loaded.records {
            if let Admission::Pending { since } = record.admission {
                wakes.push(Wake::At(since.saturating_add(PENDING_EXPIRY)));
            }
        }
        let unreturned = loaded
            .machines()
            .any(|machine| machine.holding.is_released() && !machine.admitted.permit_returned);
        if unreturned {
            wakes.push(Wake::At(now.saturating_add(PERMIT_RETRY)));
        }

        rearm(&self.state.storage(), wakes, now).await
    }
}

/// What a machine create reads from the configuration
#[derive(Clone, Copy)]
struct Create<'a> {
    config: &'a MachinesConfig,
    class: &'a MachineClass,
    boot: &'a Boot,
}

/// Returns the run's class as configured now, if it still costs what the run was opened at
///
/// Both caps count every machine of a run at the price stored when the run was created, so a
/// class repriced since then would be undercounted; such a run takes no new machines.
fn run_class<'a>(
    config: &'a MachinesConfig,
    run: &RunRecord,
) -> Result<&'a MachineClass, ApiError> {
    let class = config.class_for(&run.app, &run.class).map_err(|_| {
        ApiError::new(
            ErrorCode::RunEnded,
            format!(
                "class {} is no longer configured; start a new run",
                run.class
            ),
        )
    })?;
    if class.price_per_hour != run.price_per_hour || class.gate_units != run.gate_units {
        return Err(ApiError::new(
            ErrorCode::RunEnded,
            format!(
                "class {} was repriced since the run started; start a new run",
                run.class
            ),
        ));
    }

    Ok(class)
}

fn client_run(stores: &Stores, caller: &Caller) -> Result<Loaded, ApiError> {
    let app = client_app(caller)?;
    let loaded = stores.load().map_err(internal)?;

    // another app's run is reported as missing, so its existence leaks nothing
    loaded
        .filter(|loaded| loaded.run.app == *app)
        .ok_or_else(|| ApiError::new(ErrorCode::RunNotFound, "no such run"))
}

fn loaded_for(stores: &Stores, caller: &Caller, missing: ErrorCode) -> Result<Loaded, ApiError> {
    stores
        .load()
        .map_err(internal)?
        .ok_or_else(|| not_found(caller, missing, "no such lease"))
}

fn lease_access<'a>(
    loaded: &'a Loaded,
    caller: &Caller,
    generation: Generation,
) -> Result<(Machine<'a>, ReleasedBy), ApiError> {
    let machine = loaded
        .machine(generation)
        .ok_or_else(|| not_found(caller, ErrorCode::LeaseNotFound, "no such lease"))?;

    let by = match caller {
        Caller::Client(app) if *app == loaded.run.app => ReleasedBy::Client,
        Caller::Client(_) => {
            return Err(ApiError::new(ErrorCode::LeaseNotFound, "no such lease"));
        }
        Caller::Lease(hash) if hash.matches(&machine.admitted.token_hash) => ReleasedBy::Machine,
        Caller::Lease(_) => return Err(unauthorized()),
    };

    Ok((machine, by))
}

/// Ends one machine's lease and stores the record and holding, or returns `false` when
/// there was nothing to end
async fn end_machine(
    stores: &Stores,
    machine: &Machine<'_>,
    end: LeaseEnd<'_>,
    now: UnixMillis,
) -> Result<bool, ApiError> {
    let ended = match end_lease(machine.holding, machine.admitted, end, now) {
        Ok(ended) => ended,
        Err(HoldingError::Lease(LeaseError::Expired(_))) => return Err(lease_ended()),
        Err(error) => return Err(internal(error)),
    };
    let Some((holding, admitted)) = ended else {
        return Ok(false);
    };

    stores
        .put_admitted(machine.record, admitted)
        .map_err(internal)?;
    owner::save(&stores.storage, &stores.holdings, &holding, now)
        .await
        .map_err(internal)?;

    Ok(true)
}

fn api_failure(error: ApiError) -> CloudleaseError {
    CloudleaseError::Storage(error.message)
}

fn check_run_request(
    config: &MachinesConfig,
    request: &CreateRunRequest,
    now: UnixMillis,
) -> Result<(), ApiError> {
    if request.max_machines == 0 || request.max_machines > config.max_machines_per_run {
        return Err(ApiError::new(
            ErrorCode::InvalidRequest,
            format!(
                "max_machines must be from 1 through {}",
                config.max_machines_per_run
            ),
        ));
    }
    if request.deadline_at.0 <= now {
        return Err(ApiError::new(
            ErrorCode::InvalidRequest,
            "deadline_at has passed",
        ));
    }
    if request.deadline_at.0 > now.saturating_add(config.max_run_lifetime) {
        return Err(ApiError::new(
            ErrorCode::DeadlineTooFar,
            format!(
                "deadline_at may be at most {} hours away",
                config.max_run_lifetime.as_secs() / 3600
            ),
        ));
    }

    Ok(())
}

/// The permit lives as long as the lease can, plus a margin for deletion; after that the gate
/// marks it suspect and only proof of absence returns it
fn permit_ttl(run: &RunRecord, class: &MachineClass, now: UnixMillis) -> u64 {
    let hard_deadline = run.deadline_at.min(now.saturating_add(class.max_lifetime));
    let lifetime = Duration::from_millis(hard_deadline.as_millis().saturating_sub(now.as_millis()));
    let ttl = lifetime
        .saturating_add(PERMIT_MARGIN)
        .min(GATE_CONFIG.max_ttl);

    u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX)
}

fn mint_token() -> Result<LeaseToken, ApiError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| internal(format!("random bytes: {error}")))?;

    Ok(LeaseToken::from_random(bytes))
}

fn volume_name(run: &RunId, generation: Generation) -> CloudleaseResult<ResourceName> {
    // volume names are unique per region, so they carry the run and generation, not the
    // client's hostname
    Ok(ResourceName::parse(format!(
        "{}-{generation}",
        run.as_str().replace('_', "-")
    ))?)
}

async fn return_permit(gate: &CapacityClient, permit: &Permit) {
    // nothing was created under this permit; if the return fails, it turns suspect at its
    // expiry and the sweep returns it, since no holding claims its generation
    let request = ReleaseRequest {
        permit_id: permit.permit_id.clone(),
        owner_id: permit.owner_id.clone(),
        generation: permit.generation,
        owner_absent: AbsenceProof,
    };
    if let Err(error) = gate.release(&request).await {
        worker::console_warn!("unused permit not returned: {error}");
    }
}

fn hourly_cap_alert(run: &RunRecord) -> Alert {
    Alert {
        title: "cloudlease: hourly spend cap reached".to_owned(),
        message: format!(
            "Run {} ({}, class {}) was refused a machine: machines not yet confirmed gone \
             already cost the hourly cap (HOURLY_SPEND_CAP_CENTS).",
            run.run_id, run.name, run.class
        ),
        priority: Priority::Urgent,
    }
}
