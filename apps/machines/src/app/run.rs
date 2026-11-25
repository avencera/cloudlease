//! The run object: one per run, the owner of every machine of the run
//!
//! Every lease ending goes through [`end_lease`], so each record says who ended its lease and
//! why. Every handler that changes state re-arms the alarm last, from storage, for the
//! earliest of: a lease deadline, a cleanup retry, the moment the run's spend would reach its
//! cap, a stale pending create, and a permit that still has to be returned.

use std::{cell::Cell, collections::BTreeMap, num::NonZeroU64, time::Duration};

use cloudlease_core::{
    alarm::{Backoff, Wake},
    capacity::{AbsenceProof, AcquireRequest, Permit, ReleaseRequest},
    holding::{Holding, HoldingError},
    ids::{Generation, ResourceId},
    lease::{Lease, LeaseError},
    time::UnixMillis,
};
use cloudlease_machines::{
    alert::{Alert, Priority},
    boot::{Boot, LeaseFile, VolumeMount, check_fits, render_user_data},
    config::{
        AwsClass, DigitalOceanClass, LatitudeClass, MachineClass, MachinesConfig, PhoenixNapClass,
        ProviderClass,
    },
    ids::{AppName, ClassName, RunId},
    money::MicroUsd,
    provider::{
        LatPlacement, LatitudeSpec, MachineKind, MachineSpec, PnapPlacement, PoolChoice,
        choose_pools,
    },
    run::{
        Admission, Admitted, LeaseEnd, MachineEntry, MachineRecord, RunEnd, RunRecord, admit,
        confirm, end_lease, lease_view, presence_target, presence_wake, run_spend, run_view,
    },
    time::Timestamp,
    token::LeaseToken,
    wire::{
        ApiError, Caller, CreateMachineRequest, CreateRunRequest, ErrorCode, LeaseView,
        ReleaseLeaseRequest, ReleaseReason, ReleasedBy, RenewView, RunAction, RunCall, RunView,
    },
};
use cloudlease_provider::{
    aws::{
        AwsInstanceSpec, IdempotencyKey, InstanceName, LaunchPools, RegionalSpec, SpotPool,
        UserData,
    },
    digitalocean::{BootScript, DoSpec, DropletSpec, ResourceName, VolumeSpec},
    error::ProviderErrorCode,
    latitude::{self, UserDataSpec},
    phoenixnap::{self, ServerSpec},
    provider::{CloudProvider, CreateSpec, Presence},
};
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
        settings::{
            MachineProviders, answer, aws, config, error_response, internal, latitude, phoenixnap,
            provider,
        },
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
            return owner::answer_sweep::<MachineKind>(&holdings, &mut request, runtime::now())
                .await;
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
            .list::<MachineKind>()?
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
    holdings: BTreeMap<Generation, Holding<MachineKind>>,
}

struct Machine<'a> {
    record: &'a MachineRecord,
    admitted: &'a Admitted,
    holding: &'a Holding<MachineKind>,
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
        require_provider(env, &request.class, class)?;
        let spend_cap = run_spend_cap(&config, request.spend_cap_cents)?;

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
            provider: class.provider.name(),
            billing: class.billing,
            spend_cap,
            heartbeat: class.heartbeat,
            requested_spend_cap_cents: request.spend_cap_cents,
            ended: None,
            create_failure_alerted: false,
        };
        stores.runs.put_run(&run).map_err(internal)?;

        answer(201, &run_view(&run, &[], now))
    }

    async fn release_run(&self, stores: &Stores, caller: &Caller, now: UnixMillis) -> Answer {
        let loaded = client_run(stores, caller)?;
        stores
            .runs
            .update_run(|run| {
                run.ended.get_or_insert(RunEnd::Requested { at: now });
            })
            .map_err(internal)?;
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
        require_provider(&self.env, &loaded.run.class, class)?;
        check_fits(&boot, &config.api_base, class.provider.volume_mount())
            .and_then(|script| class.provider.check_user_data(&script))
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
        let Some(current) = stores.runs.record(generation).map_err(internal)? else {
            // the pending record expired while the gate answered; its generation is never
            // handed out again, so only the permit needs undoing
            return_permit(&gate, &permit).await;
            return Err(ApiError::new(
                ErrorCode::Busy,
                "the create waited too long on the capacity gate; retry it",
            ));
        };
        if current.admitted().is_some() {
            return answer(200, &loaded.lease_view(generation)?);
        }

        let started = confirm(&loaded.run, &loaded.entries(), now).and_then(|()| {
            let lease_id = current
                .machine_id(&loaded.run.run_id)
                .lease_id()
                .map_err(internal)?;
            let policy = loaded
                .run
                .heartbeat
                .lease_policy(class.max_lifetime)
                .map_err(internal)?;
            Lease::start(lease_id, &policy, now, Some(loaded.run.deadline_at)).map_err(internal)
        });
        let lease = match started {
            Ok(lease) => lease,
            Err(error) => {
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
            out_of_stock: false,
        };
        stores
            .put_admitted(&current, admitted.clone())
            .map_err(internal)?;
        let holding = Holding::<MachineKind>::leased(owner, generation, lease);
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
        if let Err(failure) = provisioned {
            self.create_failed(stores, &loaded.run, &current, failure)
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
    ) -> Result<(), CreateFailure> {
        let provider = provider(&self.env)?;
        let lease_id = record.machine_id(&run.run_id).to_string();
        let lease = LeaseFile {
            lease_id: &lease_id,
            token: token.expose(),
            api_base: &config.api_base,
            hard_deadline_at: Timestamp(admitted.hard_deadline_at),
            heartbeat_ttl_secs: run.heartbeat.ttl().as_secs(),
            interruption_notice: class.provider.name().interruption_notice(),
        };
        let provisioning = Provisioning {
            stores,
            provider: &provider,
            run,
            record,
            lease: &lease,
            boot,
            created: Cell::new(false),
        };

        let provisioned = match &class.provider {
            ProviderClass::DigitalOcean(droplet) => provisioning.droplet(droplet).await,
            ProviderClass::PhoenixNap(server) => provisioning.server(server).await,
            ProviderClass::Latitude(server) => provisioning.latitude(server).await,
            ProviderClass::Aws(instance) => provisioning.aws(instance).await,
        };
        provisioned.map_err(|error| CreateFailure {
            out_of_stock: !provisioning.created.get() && is_out_of_stock(&error),
            error,
        })
    }

    /// Ends a lease whose create failed, so its cleanup finds anything that was created
    async fn create_failed(
        &self,
        stores: &Stores,
        run: &RunRecord,
        record: &MachineRecord,
        failure: CreateFailure,
    ) -> Result<(), ApiError> {
        let CreateFailure {
            error,
            out_of_stock,
        } = failure;
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
            let end = if out_of_stock {
                LeaseEnd::OutOfStock
            } else {
                LeaseEnd::Revoke {
                    reason: ReleaseReason::CreateFailed,
                    by: ReleasedBy::Cloudlease,
                }
            };
            end_machine(stores, &machine, end, now).await?;
        }
        // nothing was created, so the permit goes back now instead of after the tag search's
        // settle window, where repeated refusals during a stock outage would fill the hourly
        // cap; if the gate call fails, the alarm retries it
        if out_of_stock {
            self.return_permits(stores).await.map_err(internal)?;
        }

        let mut first_failure = false;
        stores
            .runs
            .update_run(|run| {
                first_failure = !run.create_failure_alerted;
                run.create_failure_alerted = true;
            })
            .map_err(internal)?;
        if !first_failure {
            worker::console_error!("create of {} failed: {error}", record.hostname);
            return Ok(());
        }
        notify(
            &self.env,
            &Alert {
                title: format!("cloudlease: machine create failed in run {}", run.name),
                message: format!(
                    "Run {} ({}, class {}): creating {} failed: {error}. Later failures in \
                     this run are logged, not alerted.",
                    run.run_id, run.name, run.class, record.hostname
                ),
                priority: Priority::Default,
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
        let policy = loaded.run.heartbeat.renew_policy().map_err(internal)?;

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
                        idle_hold_until: machine
                            .admitted
                            .idle_hold_until(&loaded.run, now)
                            .map(Timestamp),
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
        let processed = owner::run_alarm::<MachineKind, _>(
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
        // last, so slow presence reads never hold up a delete or permit already due; a lease
        // ended here is cleaned up on the next pass, which its releasing holding asks for now
        let now = runtime::now().max(now);
        self.end_interrupted(stores, &provider, now).await?;
        self.rearm(stores, now).await
    }

    /// Ends the lease of every watched machine the provider no longer has, such as an AWS
    /// Spot instance EC2 took back
    ///
    /// The ended lease is cleaned up like any other: its delete finds the instance gone,
    /// its holding is released and its permit returned. A failed read is logged and left to
    /// the next check, and the heartbeat still ends a lease whose machine stopped renewing.
    async fn end_interrupted(
        &self,
        stores: &Stores,
        provider: &MachineProviders,
        now: UnixMillis,
    ) -> CloudleaseResult<()> {
        let Some(loaded) = stores.load()? else {
            return Ok(());
        };
        let targets: Vec<_> = loaded
            .machines()
            .filter_map(|machine| {
                let (kind, id) =
                    presence_target(&loaded.run, machine.admitted, machine.holding, now)?;
                Some((machine.record.generation, kind, id.clone()))
            })
            .collect();

        for (generation, kind, id) in targets {
            match provider.presence(kind, &id).await {
                Ok(Presence::Present) => {}
                Ok(Presence::Absent) => self.end_absent(stores, generation, &id, now).await?,
                Err(error) => {
                    worker::console_warn!("presence check of {id} will retry: {error}");
                }
            }
        }

        Ok(())
    }

    /// Ends the lease of `generation` as interrupted if it still holds the absent `id`
    ///
    /// The run object serves other requests while a presence read is in flight, so the lease
    /// is read again here: a release or expiry that landed meanwhile keeps its own reason.
    async fn end_absent(
        &self,
        stores: &Stores,
        generation: Generation,
        id: &ResourceId,
        now: UnixMillis,
    ) -> CloudleaseResult<()> {
        let Some(loaded) = stores.load()? else {
            return Ok(());
        };
        let Some(machine) = loaded.machine(generation) else {
            return Ok(());
        };
        // the time after the read, so a lease whose deadline passed meanwhile is left to
        // expire with its own reason
        let now = runtime::now().max(now);
        let current = presence_target(&loaded.run, machine.admitted, machine.holding, now);
        if current.is_none_or(|(_, current)| current != id) {
            return Ok(());
        }

        let end = LeaseEnd::Revoke {
            reason: ReleaseReason::Interrupted,
            by: ReleasedBy::Cloudlease,
        };
        end_machine(stores, &machine, end, now)
            .await
            .map_err(api_failure)?;
        worker::console_warn!(
            "machine {} of run {} is gone from {}; its lease ended as interrupted",
            machine.record.hostname,
            loaded.run.run_id,
            loaded.run.provider
        );

        Ok(())
    }

    async fn hit_spend_cap(
        &self,
        stores: &Stores,
        loaded: &Loaded,
        now: UnixMillis,
    ) -> CloudleaseResult<()> {
        stores.runs.update_run(|run| {
            run.ended.get_or_insert(RunEnd::SpendCap { at: now });
        })?;

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

        let run = &loaded.run;
        let spend = run_spend(run, &loaded.entries(), now);
        notify(
            &self.env,
            &Alert {
                title: format!("cloudlease: run {} reached its spend cap", run.name),
                message: format!(
                    "Run {} ({}, class {}) has spent {} of its {} cap. Its {ended} live \
                     machines are being deleted and it takes no new ones.",
                    run.run_id, run.name, run.class, spend.accrued, run.spend_cap
                ),
                priority: Priority::High,
            },
        )
        .await;

        Ok(())
    }

    /// Returns the gate permit of every machine whose resources are confirmed gone or whose
    /// create was refused for stock
    async fn return_permits(&self, stores: &Stores) -> CloudleaseResult<()> {
        let Some(loaded) = stores.load()? else {
            return Ok(());
        };
        let owner = loaded.run.run_id.owner()?;
        let due: Vec<_> = loaded
            .machines()
            .filter(|machine| machine.admitted.permit_due(machine.holding))
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
        wakes.extend(presence_wake(&loaded.run, &loaded.entries(), now).map(Wake::At));
        let unreturned = loaded
            .machines()
            .any(|machine| machine.admitted.permit_due(machine.holding));
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

/// Returns the run's class as configured now, if it still costs and bills what the run was
/// opened at, with the same provider
///
/// Both caps count every machine of a run at the price and billing stored when the run was
/// created, so a class repriced since then would be undercounted; such a run takes no new
/// machines.
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
    let repriced = class.price_per_hour != run.price_per_hour
        || class.gate_units != run.gate_units
        || class.billing != run.billing;
    // the run's boot grace came from the class's provider, so it would not fit another one
    let moved = class.provider.name() != run.provider;
    if repriced || moved {
        return Err(ApiError::new(
            ErrorCode::RunEnded,
            format!(
                "class {} was repriced or moved to another provider since the run started; \
                 start a new run",
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

/// The run's spend cap: the client's, when it asked for one within the config's, else the
/// config's
fn run_spend_cap(
    config: &MachinesConfig,
    requested: Option<NonZeroU64>,
) -> Result<MicroUsd, ApiError> {
    let Some(cents) = requested else {
        return Ok(config.run_spend_cap);
    };

    MicroUsd::from_cents(cents.get())
        .filter(|cap| *cap <= config.run_spend_cap)
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "spend_cap_cents may be at most {}",
                    config.run_spend_cap.whole_cents()
                ),
            )
        })
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

/// One admitted machine's resources being created, each saved as requested before its
/// provider call
struct Provisioning<'a> {
    stores: &'a Stores,
    provider: &'a MachineProviders,
    run: &'a RunRecord,
    record: &'a MachineRecord,
    lease: &'a LeaseFile<'a>,
    boot: &'a Boot,
    // whether any billed resource of the machine was created, after which a failure may have
    // something billed behind it
    created: Cell<bool>,
}

/// A failed machine create
struct CreateFailure {
    error: CloudleaseError,
    /// Whether the provider refused the machine's first billed resource for lack of stock,
    /// so nothing billed was created
    out_of_stock: bool,
}

impl From<CloudleaseError> for CreateFailure {
    fn from(error: CloudleaseError) -> Self {
        Self {
            error,
            out_of_stock: false,
        }
    }
}

fn is_out_of_stock(error: &CloudleaseError) -> bool {
    // phoenixNAP, Latitude.sh and EC2 answer no stock before they create anything, and the
    // adapters report it only once every location, site, or instance type and zone of the
    // class refused
    matches!(error, CloudleaseError::Provider(error) if error.code == ProviderErrorCode::NoCapacity)
}

impl Provisioning<'_> {
    async fn create(&self, spec: &MachineSpec) -> CloudleaseResult<ResourceId> {
        let (id, _) = owner::create(
            &self.stores.storage,
            &self.stores.holdings,
            self.provider,
            self.record.generation,
            spec,
            runtime::now(),
        )
        .await?;
        // a Latitude.sh user data record costs nothing, so a server refused for stock after
        // it still leaves the machine unbilled
        if spec.kind().is_billed() {
            self.created.set(true);
        }

        Ok(id)
    }

    fn user_data(&self, volume: Option<VolumeMount<'_>>) -> CloudleaseResult<BootScript> {
        render_user_data(self.lease, volume, self.boot)
            .map_err(|error| CloudleaseError::Config(error.to_string()))
    }

    /// Creates the class's volume, if it has one, then the droplet that mounts it
    async fn droplet(&self, class: &DigitalOceanClass) -> CloudleaseResult<()> {
        let volume_name = volume_name(&self.run.run_id, self.record.generation)?;
        let mut volume_ids = Vec::new();
        let mut mount = None;
        if let Some(volume) = &class.volume {
            let spec = MachineSpec::DigitalOcean(DoSpec::Volume(VolumeSpec {
                name: volume_name.clone(),
                region: class.region.clone(),
                size_gib: volume.size_gib,
            }));
            volume_ids.push(self.create(&spec).await?);
            mount = Some(VolumeMount {
                name: &volume_name,
                mount: &volume.mount,
            });
        }

        let spec = MachineSpec::DigitalOcean(DoSpec::Droplet(DropletSpec {
            name: ResourceName::parse(self.record.hostname.as_str())?,
            region: class.region.clone(),
            size: class.size.clone(),
            image: class.image.clone(),
            boot: self.user_data(mount)?,
            ssh_key_ids: class.ssh_key_ids.clone(),
            volume_ids,
            vpc_uuid: class.vpc_uuid.clone(),
        }));
        self.create(&spec).await?;

        Ok(())
    }

    /// Creates the server in the first of the class's locations that has stock
    async fn server(&self, class: &PhoenixNapClass) -> CloudleaseResult<()> {
        let user_data = self.user_data(None)?;
        let first = class
            .locations
            .as_slice()
            .first()
            .ok_or_else(|| CloudleaseError::Config("a class has no locations".to_owned()))?;
        let spec = MachineSpec::PhoenixNap(PnapPlacement {
            server: ServerSpec {
                hostname: phoenixnap::Hostname::parse(self.record.hostname.as_str())?,
                location: first.clone(),
                server_type: class.server_type.clone(),
                os: class.os.clone(),
                boot: phoenixnap::BootScript::parse(user_data.as_str())?,
                ssh_key_ids: class.ssh_key_ids.clone(),
            },
            locations: class.locations.clone(),
        });
        self.create(&spec).await?;

        Ok(())
    }

    /// Creates the user data record that names the machine's owner, then the server that
    /// boots from it in the first of the class's sites that has stock
    async fn latitude(&self, class: &LatitudeClass) -> CloudleaseResult<()> {
        let user_data = self.user_data(None)?;
        let record = MachineSpec::Latitude(LatitudeSpec::UserData(UserDataSpec {
            boot: latitude::BootScript::shell(user_data.as_str())?,
        }));
        let record = self.create(&record).await?;

        let first = class
            .sites
            .as_slice()
            .first()
            .ok_or_else(|| CloudleaseError::Config("a class has no sites".to_owned()))?;
        let spec = MachineSpec::Latitude(LatitudeSpec::Server(LatPlacement {
            server: latitude::ServerSpec {
                plan: class.plan.clone(),
                site: first.clone(),
                operating_system: class.operating_system.clone(),
                user_data: record,
                ssh_key_ids: class.ssh_key_ids.clone(),
            },
            sites: class.sites.clone(),
        }));
        self.create(&spec).await?;

        Ok(())
    }

    /// Launches the Spot instance in whichever of the class's pools EC2 picks, leaving out
    /// the pools that just took one of the run's machines back
    async fn aws(&self, class: &AwsClass) -> CloudleaseResult<()> {
        let user_data = self.user_data(None)?;
        let pools = self.aws_pools(class).await?;
        let spec = MachineSpec::Aws(RegionalSpec {
            region: class.region.clone(),
            instance: AwsInstanceSpec {
                name: InstanceName::parse(self.record.hostname.as_str())?,
                pools,
                launch_template: class.launch_template.clone(),
                image_id: class.image_id.clone(),
                root_device_name: class.root_device_name.clone(),
                root_volume_gib: class.root_volume_gib,
                user_data: UserData::parse(user_data.as_str())?,
                idempotency_key: IdempotencyKey::parse(self.lease.lease_id)?,
                max_price: Some(class.spot_max_price.clone()),
            },
        });
        self.create(&spec).await?;

        Ok(())
    }

    /// Returns the class's pools less those that interrupted the run in the last hour
    ///
    /// The history is only advice: if EC2 cannot be read, every pool is offered and the
    /// launch's own call reports any real outage.
    async fn aws_pools(&self, class: &AwsClass) -> CloudleaseResult<LaunchPools> {
        let owner = self.run.run_id.owner()?;
        let interruptions = match self
            .provider
            .spot_interruptions(&class.region, &owner)
            .await
        {
            Ok(interruptions) => interruptions,
            Err(error) => {
                worker::console_warn!(
                    "cloudlease machines: run {} offers every Spot pool, its interruptions \
                     are unread: {error}",
                    self.run.run_id
                );
                return Ok(class.pools.clone());
            }
        };

        match choose_pools(class.pools.clone(), &interruptions, runtime::now()) {
            PoolChoice::All(pools) => Ok(pools),
            PoolChoice::Without { pools, left_out } => {
                worker::console_log!(
                    "cloudlease machines: run {} leaves out Spot pools that interrupted it in \
                     the last hour: {}",
                    self.run.run_id,
                    describe_pools(&left_out)
                );
                Ok(pools)
            }
            PoolChoice::AllInterrupted(pools) => {
                worker::console_log!(
                    "cloudlease machines: run {} offers every Spot pool, though each interrupted \
                     it in the last hour: {}",
                    self.run.run_id,
                    describe_pools(pools.as_slice())
                );
                Ok(pools)
            }
        }
    }
}

fn describe_pools(pools: &[SpotPool]) -> String {
    pools
        .iter()
        .map(|pool| {
            format!(
                "{} in {}",
                pool.instance_type.as_str(),
                pool.availability_zone.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Refuses a class whose provider this Worker has no credentials for, before anything is
/// stored or created
fn require_provider(env: &Env, name: &ClassName, class: &MachineClass) -> Result<(), ApiError> {
    let configured = match &class.provider {
        ProviderClass::DigitalOcean(_) => true,
        ProviderClass::PhoenixNap(_) => phoenixnap(env).map_err(internal)?.is_some(),
        ProviderClass::Latitude(_) => latitude(env).map_err(internal)?.is_some(),
        // a region the Worker does not list would escape the sweep
        ProviderClass::Aws(instance) => aws(env)
            .map_err(internal)?
            .is_some_and(|aws| aws.has_region(&instance.region)),
    };
    if configured {
        return Ok(());
    }

    let settings = match &class.provider {
        ProviderClass::DigitalOcean(_) => "DIGITALOCEAN_TOKEN".to_owned(),
        ProviderClass::PhoenixNap(_) => "PNAP_CLIENT_ID and PNAP_CLIENT_SECRET".to_owned(),
        ProviderClass::Latitude(_) => "LATITUDE_API_KEY and LATITUDE_PROJECT".to_owned(),
        ProviderClass::Aws(instance) => format!(
            "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, with {} in AWS_REGIONS",
            instance.region
        ),
    };
    Err(ApiError::new(
        ErrorCode::UnknownClass,
        format!(
            "class {name} uses {}, which this Worker has no credentials for ({settings})",
            class.provider.name()
        ),
    ))
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
        priority: Priority::High,
    }
}
