//! Run and machine records, admission, release reasons and the wire views
//!
//! A run object stores one [`RunRecord`] and a [`MachineRecord`] per create request. A machine
//! goes through admission before any resource exists:
//!
//! ```text
//! Pending --permit granted--> Admitted (holding leased, then resources created)
//!         \--refused--------> deleted  (no lease, no resources; a retry decides afresh)
//! ```
//!
//! A pending record holds its generation and counts against `max_machines` while the run
//! object waits on the capacity gate, so two creates can never share a generation.
//!
//! An admitted machine's lifetime is the core [`Holding`]; this module adds what the contract
//! needs on top: who ended a lease and why, its last deadlines, its capacity permit, and the
//! spend of the run.

use std::num::NonZeroU64;

use cloudlease_core::{
    capacity::PermitUnits,
    holding::{Holding, HoldingError, HoldingState, ReleaseReason as CoreReason, Slot},
    ids::{Generation, LeaseId, PermitId},
    lease::Lease,
    time::UnixMillis,
};
use serde::{Deserialize, Serialize};

use crate::{
    config::Heartbeat,
    ids::{AppName, ClassName, Hostname, MachineId, RequestId, RunId, RunName},
    money::{BilledLife, Billing, MicroUsd, Spend},
    provider::{MachineKind, ProviderName},
    time::Timestamp,
    token::TokenHash,
    wire::{
        ApiError, CreateMachineRequest, CreateRunRequest, ErrorCode, LeaseState, LeaseView,
        ReleaseReason, ReleaseView, ReleasedBy, RunState, RunView,
    },
};

/// Why a run stopped taking machines before its deadline
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum RunEnd {
    /// The client released it
    Requested {
        /// When
        at: UnixMillis,
    },
    /// Its machines spent the run's cap
    SpendCap {
        /// When
        at: UnixMillis,
    },
}

/// One run, fixed at create except for its end
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunRecord {
    /// The run
    pub run_id: RunId,
    /// The app that created it, the only one that may see it
    pub app: AppName,
    /// The create's idempotency key
    pub request_id: RequestId,
    /// Its display name
    pub name: RunName,
    /// Its machine class
    pub class: ClassName,
    /// The most live leases at once
    pub max_machines: u16,
    /// No lease outlives this
    pub deadline_at: UnixMillis,
    /// When it was created
    pub created_at: UnixMillis,
    /// The class's hourly price when the run was created
    pub price_per_hour: MicroUsd,
    /// The class's weight in the hourly spend gate when the run was created
    pub gate_units: PermitUnits,
    /// The class's provider when the run was created
    pub provider: ProviderName,
    /// How the class's provider billed when the run was created
    pub billing: Billing,
    /// What the run may spend, fixed at create
    pub spend_cap: MicroUsd,
    /// The heartbeat window and boot grace of the run's leases, from the config and the
    /// class at create
    pub heartbeat: Heartbeat,
    /// The cap the client asked for, if any, which a retried create must repeat
    #[serde(default)]
    pub requested_spend_cap_cents: Option<NonZeroU64>,
    /// Why it stopped taking machines early, if it did
    pub ended: Option<RunEnd>,
    /// Whether a failed provider create was already alerted on, so a client that keeps
    /// asking does not flood the topic
    #[serde(default)]
    pub create_failure_alerted: bool,
}

impl RunRecord {
    /// Reports whether a retried create names this run with the same body
    #[must_use]
    pub fn matches(&self, request: &CreateRunRequest) -> bool {
        self.request_id == request.request_id
            && self.name == request.name
            && self.class == request.class
            && self.max_machines == request.max_machines
            && self.deadline_at == request.deadline_at.0
            && self.requested_spend_cap_cents == request.spend_cap_cents
    }

    /// Reports whether the run takes new machines at `now`
    #[must_use]
    pub fn is_open(&self, now: UnixMillis) -> bool {
        self.ended.is_none() && now < self.deadline_at
    }

    /// Reports whether the run was stopped by its spend cap
    #[must_use]
    pub const fn hit_spend_cap(&self) -> bool {
        matches!(self.ended, Some(RunEnd::SpendCap { .. }))
    }
}

/// How a lease ended, with the deadline it had then
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Release {
    /// Why
    pub reason: ReleaseReason,
    /// Who
    pub by: ReleasedBy,
    /// When
    pub at: UnixMillis,
    /// The heartbeat deadline the lease had when it ended, which the view keeps showing
    pub heartbeat_deadline_at: UnixMillis,
}

/// What an admitted machine holds beyond its core holding
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Admitted {
    /// The hash of the lease token written into the machine's boot data
    pub token_hash: TokenHash,
    /// When the lease started, before any provider call
    pub created_at: UnixMillis,
    /// When the lease ends whatever happens
    pub hard_deadline_at: UnixMillis,
    /// The hourly spend gate permit the machine holds until its resources are gone
    pub permit: PermitId,
    /// Whether the permit was returned to the gate
    pub permit_returned: bool,
    /// How the lease ended, once it has
    pub release: Option<Release>,
}

/// Where a create request is in admission
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "admission")]
pub enum Admission {
    /// The request holds its generation while the run object asks the gate for a permit
    Pending {
        /// When the request was first handled
        since: UnixMillis,
    },
    /// The machine has a lease and a holding
    Admitted(Admitted),
}

/// One create request of a run
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MachineRecord {
    /// The generation it was given, unique within the run
    pub generation: Generation,
    /// The create's idempotency key
    pub request_id: RequestId,
    /// The machine's hostname
    pub hostname: Hostname,
    /// The SHA-256 of the request body, to tell a retry from a conflicting reuse of its key
    pub request_digest: TokenHash,
    /// Where it is in admission
    pub admission: Admission,
}

impl MachineRecord {
    /// Starts admission for a new request
    #[must_use]
    pub fn pending(
        generation: Generation,
        request: &CreateMachineRequest,
        now: UnixMillis,
    ) -> Self {
        Self {
            generation,
            request_id: request.request_id,
            hostname: request.hostname.clone(),
            request_digest: request_digest(request),
            admission: Admission::Pending { since: now },
        }
    }

    /// Reports whether a retried request has the same body
    #[must_use]
    pub fn matches(&self, request: &CreateMachineRequest) -> bool {
        self.request_id == request.request_id
            && self.request_digest.matches(&request_digest(request))
    }

    /// Returns the admitted part, if the machine was admitted
    #[must_use]
    pub const fn admitted(&self) -> Option<&Admitted> {
        match &self.admission {
            Admission::Admitted(admitted) => Some(admitted),
            _ => None,
        }
    }

    /// Returns the machine's lease ID in `run`
    #[must_use]
    pub fn machine_id(&self, run: &RunId) -> MachineId {
        MachineId {
            run: run.clone(),
            generation: self.generation,
        }
    }
}

fn request_digest(request: &CreateMachineRequest) -> TokenHash {
    // serializing the parsed request gives one spelling per body, whatever the client's
    // whitespace or key order
    TokenHash::of(&serde_json::to_string(request).unwrap_or_default())
}

/// One create request with its holding, when it has one
#[derive(Clone, Copy, Debug)]
pub struct MachineEntry<'a> {
    /// The request
    pub record: &'a MachineRecord,
    /// Its holding, present once admitted
    pub holding: Option<&'a Holding<MachineKind>>,
}

impl MachineEntry<'_> {
    /// Reports whether the machine may hold resources or still become a lease, so it counts
    /// against `max_machines`
    #[must_use]
    pub fn is_live(&self) -> bool {
        match &self.record.admission {
            Admission::Pending { .. } => true,
            Admission::Admitted(_) => !self.holding.is_some_and(Holding::is_released),
        }
    }

    fn billed_life(&self) -> Option<BilledLife> {
        let admitted = self.record.admitted()?;
        let gone_at = match self.holding.map(Holding::state) {
            Some(HoldingState::Released { at, .. }) => Some(*at),
            _ => None,
        };

        Some(BilledLife {
            from: admitted.created_at,
            gone_at,
        })
    }
}

/// Adds up a run's spend from its machines
#[must_use]
pub fn run_spend(run: &RunRecord, machines: &[MachineEntry<'_>], now: UnixMillis) -> Spend {
    Spend::of(
        run.billing,
        run.price_per_hour,
        machines.iter().filter_map(MachineEntry::billed_life),
        now,
    )
}

/// Returns what the run has committed to spend: its machines' accrued spend, plus the minimum
/// charge of each create still pending, which may become a machine at any moment
fn committed(run: &RunRecord, machines: &[MachineEntry<'_>], now: UnixMillis) -> MicroUsd {
    let minimum = run.billing.minimum(run.price_per_hour);
    let pending = machines
        .iter()
        .filter(|machine| matches!(machine.record.admission, Admission::Pending { .. }));

    pending.fold(run_spend(run, machines, now).accrued, |total, _| {
        total.saturating_add(minimum)
    })
}

fn spend_cap_reached(run: &RunRecord, committed: MicroUsd) -> ApiError {
    ApiError::new(
        ErrorCode::SpendCapReached,
        format!(
            "the run has spent or committed {committed} of its {} cap",
            run.spend_cap
        ),
    )
}

fn run_ended() -> ApiError {
    ApiError::new(ErrorCode::RunEnded, "the run takes no new machines")
}

/// Decides whether the run takes one more create request at `now`
///
/// The run must be open, below `max_machines`, and able to pay one more machine's minimum on
/// top of what it has committed without passing its spend cap. The hourly cap is the gate's
/// to decide.
pub fn admit(
    run: &RunRecord,
    machines: &[MachineEntry<'_>],
    now: UnixMillis,
) -> Result<(), ApiError> {
    if !run.is_open(now) {
        return Err(run_ended());
    }

    let live = machines.iter().filter(|machine| machine.is_live()).count();
    if live >= usize::from(run.max_machines) {
        return Err(ApiError::new(
            ErrorCode::RunFull,
            format!("the run already has {live} live machines"),
        ));
    }

    let after =
        committed(run, machines, now).saturating_add(run.billing.minimum(run.price_per_hour));
    if after > run.spend_cap {
        return Err(spend_cap_reached(run, after));
    }

    Ok(())
}

/// Decides whether a pending create may start its lease at `now`, after the gate answered
///
/// The run may have ended or spent more meanwhile, and a resumed request was never checked
/// against the spend it now joins. `machines` includes the pending request itself.
pub fn confirm(
    run: &RunRecord,
    machines: &[MachineEntry<'_>],
    now: UnixMillis,
) -> Result<(), ApiError> {
    if !run.is_open(now) {
        return Err(run_ended());
    }

    let committed = committed(run, machines, now);
    if committed > run.spend_cap {
        return Err(spend_cap_reached(run, committed));
    }

    Ok(())
}

/// Reports where the run is in its life
#[must_use]
pub fn run_state(run: &RunRecord, machines: &[MachineEntry<'_>], now: UnixMillis) -> RunState {
    if run.is_open(now) {
        return RunState::Open;
    }
    if machines.iter().any(MachineEntry::is_live) {
        return RunState::Releasing;
    }

    RunState::Released
}

/// How a lease is being ended
#[derive(Clone, Copy, Debug)]
pub enum LeaseEnd<'a> {
    /// The client or the machine released it while live
    Release {
        /// The lease being released
        lease_id: &'a LeaseId,
        /// Why
        reason: ReleaseReason,
        /// Who
        by: ReleasedBy,
    },
    /// cloudlease or the client ended it for a run-wide reason, live or not
    Revoke {
        /// Why
        reason: ReleaseReason,
        /// Who
        by: ReleasedBy,
    },
    /// It passed its heartbeat or hard deadline
    Expire,
}

/// Ends a leased holding and records why, or returns `None` when there is nothing to end:
/// the lease already ended, or for [`LeaseEnd::Expire`], it has not expired
///
/// Every lease ending goes through here, so the record always says who ended it and why,
/// with the heartbeat deadline it had.
pub fn end_lease(
    holding: &Holding<MachineKind>,
    admitted: &Admitted,
    end: LeaseEnd<'_>,
    now: UnixMillis,
) -> Result<Option<(Holding<MachineKind>, Admitted)>, HoldingError> {
    let Some(lease) = holding.lease() else {
        return Ok(None);
    };

    let (ended, reason, by) = match end {
        LeaseEnd::Release {
            lease_id,
            reason,
            by,
        } => (holding.release(lease_id, now)?, reason, by),
        LeaseEnd::Revoke { reason, by } => (holding.revoke(now), reason, by),
        LeaseEnd::Expire => {
            let Some(ended) = holding.expire(now) else {
                return Ok(None);
            };
            let reason = match ended.state() {
                HoldingState::Releasing {
                    reason: CoreReason::HardDeadline,
                    ..
                }
                | HoldingState::Released {
                    reason: CoreReason::HardDeadline,
                    ..
                } => ReleaseReason::HardDeadline,
                _ => ReleaseReason::HeartbeatLapsed,
            };
            (ended, reason, ReleasedBy::Cloudlease)
        }
    };

    let release = Release {
        reason,
        by,
        at: now,
        heartbeat_deadline_at: lease.heartbeat_due_at(),
    };

    Ok(Some((
        ended,
        Admitted {
            release: Some(release),
            ..admitted.clone()
        },
    )))
}

/// Builds a lease's view from its record and holding
#[must_use]
pub fn lease_view(
    run: &RunRecord,
    record: &MachineRecord,
    admitted: &Admitted,
    holding: &Holding<MachineKind>,
) -> LeaseView {
    let (state, heartbeat_deadline_at, gone_at) = match holding.state() {
        HoldingState::Leased { lease, slots } => (
            leased_state(
                lease,
                slots
                    .iter()
                    .find(|(kind, _)| kind.is_machine())
                    .map(|(_, slot)| slot),
            ),
            lease.heartbeat_due_at(),
            None,
        ),
        HoldingState::Releasing { .. } => (
            LeaseState::Releasing,
            last_heartbeat_deadline(admitted),
            None,
        ),
        HoldingState::Released { at, .. } => (
            LeaseState::Released,
            last_heartbeat_deadline(admitted),
            Some(Timestamp(*at)),
        ),
    };

    LeaseView {
        lease_id: record.machine_id(&run.run_id),
        run_id: run.run_id.clone(),
        request_id: record.request_id,
        hostname: record.hostname.clone(),
        state,
        created_at: Timestamp(admitted.created_at),
        hard_deadline_at: Timestamp(admitted.hard_deadline_at),
        heartbeat_deadline_at: Timestamp(heartbeat_deadline_at),
        release: admitted
            .release
            .map(|release| ReleaseView {
                reason: release.reason,
                by: release.by,
                at: Timestamp(release.at),
            })
            .or_else(|| fallback_release(holding)),
        gone_at,
    }
}

fn leased_state(lease: &Lease, machine: Option<&Slot>) -> LeaseState {
    // a machine whose create response was lost still boots and renews, so a renew proves it
    // exists as well as a recorded ID does
    let created = matches!(machine, Some(Slot::Created { .. }));
    if created || lease.last_sequence() > 0 {
        return LeaseState::Running;
    }

    LeaseState::Provisioning
}

fn last_heartbeat_deadline(admitted: &Admitted) -> UnixMillis {
    admitted
        .release
        .map_or(admitted.hard_deadline_at, |release| {
            release.heartbeat_deadline_at
        })
}

/// The release of a lease that ended without passing through [`end_lease`], which only a
/// cleanup pass racing a request can cause; it names cloudlease and the core's reason
fn fallback_release(holding: &Holding<MachineKind>) -> Option<ReleaseView> {
    let (reason, at) = match holding.state() {
        HoldingState::Leased { .. } => return None,
        HoldingState::Releasing { reason, since, .. } => (*reason, *since),
        HoldingState::Released { reason, at } => (*reason, *at),
    };
    let reason = match reason {
        CoreReason::HeartbeatMissed => ReleaseReason::HeartbeatLapsed,
        CoreReason::HardDeadline => ReleaseReason::HardDeadline,
        CoreReason::Released => ReleaseReason::Requested,
        CoreReason::Revoked => ReleaseReason::RunReleased,
    };

    Some(ReleaseView {
        reason,
        by: ReleasedBy::Cloudlease,
        at: Timestamp(at),
    })
}

/// Builds a run's view with every admitted machine
#[must_use]
pub fn run_view(run: &RunRecord, machines: &[MachineEntry<'_>], now: UnixMillis) -> RunView {
    let leases = machines
        .iter()
        .filter_map(|machine| {
            let admitted = machine.record.admitted()?;
            let holding = machine.holding?;
            Some(lease_view(run, machine.record, admitted, holding))
        })
        .collect();

    RunView {
        run_id: run.run_id.clone(),
        name: run.name.clone(),
        class: run.class.clone(),
        max_machines: run.max_machines,
        deadline_at: Timestamp(run.deadline_at),
        state: run_state(run, machines, now),
        created_at: Timestamp(run.created_at),
        machines: leases,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        Admission, Admitted, LeaseEnd, MachineEntry, MachineRecord, RunEnd, RunRecord, admit,
        confirm, end_lease, lease_view, run_spend, run_state,
    };
    use crate::{
        boot::BootPayload,
        config::Heartbeat,
        ids::{AppName, ClassName, Hostname, MachineId, RequestId, RunId, RunName},
        money::{Billing, MicroUsd},
        provider::{MachineKind, ProviderName},
        token::TokenHash,
        wire::{CreateMachineRequest, ErrorCode, LeaseState, ReleaseReason, ReleasedBy, RunState},
    };
    use cloudlease_core::{
        capacity::PermitUnits,
        holding::Holding,
        ids::{Generation, PermitId, ResourceId},
        lease::{Lease, LeasePolicy},
        time::UnixMillis,
    };

    const MINUTE: u64 = 60_000;

    fn at(millis: u64) -> UnixMillis {
        UnixMillis::new(millis).unwrap()
    }

    fn run() -> RunRecord {
        let app = AppName::parse("homebased").unwrap();
        let request_id = RequestId::parse("0193a2b4-1c2d-7e3f-8a9b-0c1d2e3f4a5b").unwrap();
        RunRecord {
            run_id: RunId::derive(&app, request_id),
            app,
            request_id,
            name: RunName::parse("afv").unwrap(),
            class: ClassName::parse("do-c16").unwrap(),
            max_machines: 2,
            deadline_at: at(600 * MINUTE),
            created_at: at(0),
            // a cent a second, so a minute costs $0.60
            price_per_hour: MicroUsd::parse_dollars("36").unwrap(),
            gate_units: PermitUnits::new(3_600).unwrap(),
            provider: ProviderName::DigitalOcean,
            billing: Billing::DIGITALOCEAN,
            spend_cap: MicroUsd::parse_dollars("2").unwrap(),
            heartbeat: Heartbeat::default(),
            requested_spend_cap_cents: None,
            ended: None,
            create_failure_alerted: false,
        }
    }

    fn request(slot: &str) -> CreateMachineRequest {
        CreateMachineRequest {
            request_id: RequestId::parse(slot).unwrap(),
            hostname: Hostname::parse("afv-1").unwrap(),
            boot: BootPayload {
                files: Vec::new(),
                run: vec![vec!["true".to_owned()]],
            },
        }
    }

    fn policy() -> LeasePolicy {
        LeasePolicy::new(Duration::from_secs(600), Duration::from_secs(12 * 3600))
            .unwrap()
            .with_boot_grace(Duration::from_secs(900))
            .unwrap()
    }

    fn admitted_machine(
        generation: u64,
        started: UnixMillis,
    ) -> (MachineRecord, Admitted, Holding<MachineKind>) {
        let run = run();
        let record = MachineRecord::pending(
            Generation::new(generation).unwrap(),
            &request("0193a2b4-1c2d-7e3f-8a9b-000000000001"),
            started,
        );
        let id = record.machine_id(&run.run_id).lease_id().unwrap();
        let lease = Lease::start(id, &policy(), started, Some(run.deadline_at)).unwrap();
        let admitted = Admitted {
            token_hash: TokenHash::of("cll_token"),
            created_at: started,
            hard_deadline_at: lease.hard_deadline(),
            permit: PermitId::parse("cap_1").unwrap(),
            permit_returned: false,
            release: None,
        };
        let holding = Holding::leased(run.run_id.owner().unwrap(), record.generation, lease);
        let record = MachineRecord {
            admission: Admission::Admitted(admitted.clone()),
            ..record
        };
        (record, admitted, holding)
    }

    #[test]
    fn a_retry_matches_only_the_same_body() {
        let first = request("0193a2b4-1c2d-7e3f-8a9b-000000000001");
        let record = MachineRecord::pending(Generation::FIRST, &first, at(0));

        assert!(record.matches(&first));
        let mut changed = first.clone();
        changed.boot.run.push(vec!["reboot".to_owned()]);
        assert!(!record.matches(&changed));
    }

    fn entries<'a>(
        list: &[(&'a MachineRecord, Option<&'a Holding<MachineKind>>)],
    ) -> Vec<MachineEntry<'a>> {
        list.iter()
            .map(|(record, holding)| MachineEntry {
                record,
                holding: *holding,
            })
            .collect()
    }

    #[test]
    fn admission_counts_pending_and_unreleased_machines_and_the_run_spend() {
        let run = run();
        let pending = MachineRecord::pending(
            Generation::new(9).unwrap(),
            &request("0193a2b4-1c2d-7e3f-8a9b-000000000009"),
            at(0),
        );
        let (record, _, holding) = admitted_machine(1, at(0));

        assert_eq!(admit(&run, &[], at(0)), Ok(()));
        let full = entries(&[(&pending, None), (&record, Some(&holding))]);
        assert_eq!(
            admit(&run, &full, at(0)).unwrap_err().code,
            ErrorCode::RunFull
        );

        // after 90 s the admitted machine has cost $0.90; one more minute fits under $2
        let one = entries(&[(&record, Some(&holding))]);
        assert_eq!(admit(&run, &one, at(90_000)), Ok(()));
        // after 150 s it has cost $1.50, and another machine's $0.60 minimum would pass $2
        assert_eq!(
            admit(&run, &one, at(150_000)).unwrap_err().code,
            ErrorCode::SpendCapReached
        );

        let ended = RunRecord {
            ended: Some(RunEnd::Requested { at: at(1) }),
            ..run.clone()
        };
        assert_eq!(
            admit(&ended, &[], at(2)).unwrap_err().code,
            ErrorCode::RunEnded
        );
        assert_eq!(
            admit(&run, &[], run.deadline_at).unwrap_err().code,
            ErrorCode::RunEnded
        );
        assert_eq!(run_state(&ended, &one, at(2)), RunState::Releasing);
        assert_eq!(run_state(&ended, &[], at(2)), RunState::Released);
        // the 91st second has started, and DigitalOcean bills it
        assert_eq!(run_spend(&run, &one, at(90_000)).accrued.micros(), 910_000);
    }

    #[test]
    fn pending_creates_reserve_their_minimum_against_the_spend_cap() {
        let run = RunRecord {
            max_machines: 8,
            ..run()
        };
        let pending = |generation: u64, slot: &str| {
            MachineRecord::pending(Generation::new(generation).unwrap(), &request(slot), at(0))
        };
        let first = pending(1, "0193a2b4-1c2d-7e3f-8a9b-000000000001");
        let second = pending(2, "0193a2b4-1c2d-7e3f-8a9b-000000000002");
        let third = pending(3, "0193a2b4-1c2d-7e3f-8a9b-000000000003");

        // three pending minimums of $0.60 fit under $2, a fourth does not
        let three = entries(&[(&first, None), (&second, None), (&third, None)]);
        assert_eq!(
            admit(&run, &three, at(0)).unwrap_err().code,
            ErrorCode::SpendCapReached
        );
        assert_eq!(confirm(&run, &three, at(0)), Ok(()));

        // a pending create that waited on the gate while an admitted machine ran is refused
        // once their spend together passes the cap: $1.50 run plus $0.60 pending
        let (record, _, holding) = admitted_machine(4, at(0));
        let late = entries(&[(&first, None), (&record, Some(&holding))]);
        assert_eq!(confirm(&run, &late, at(60_000)), Ok(()));
        assert_eq!(
            confirm(&run, &late, at(150_000)).unwrap_err().code,
            ErrorCode::SpendCapReached
        );

        let ended = RunRecord {
            ended: Some(RunEnd::Requested { at: at(1) }),
            ..run
        };
        assert_eq!(
            confirm(&ended, &three, at(2)).unwrap_err().code,
            ErrorCode::RunEnded
        );
    }

    #[test]
    fn every_lease_ending_records_who_ended_it_and_why() {
        let run = run();
        let (record, admitted, holding) = admitted_machine(1, at(0));
        let lease_id = record.machine_id(&run.run_id).lease_id().unwrap();

        // the machine renews once, then stops: the heartbeat lapses ten minutes later
        let renewed = holding.renew(&lease_id, at(MINUTE), &policy()).unwrap();
        assert_eq!(
            end_lease(&renewed, &admitted, LeaseEnd::Expire, at(2 * MINUTE)),
            Ok(None)
        );
        let (lapsed, lapsed_record) =
            end_lease(&renewed, &admitted, LeaseEnd::Expire, at(11 * MINUTE))
                .unwrap()
                .unwrap();
        let release = lapsed_record.release.unwrap();
        assert_eq!(
            (release.reason, release.by, release.heartbeat_deadline_at),
            (
                ReleaseReason::HeartbeatLapsed,
                ReleasedBy::Cloudlease,
                at(11 * MINUTE)
            )
        );
        // a lease that already ended is not ended twice
        assert_eq!(
            end_lease(
                &lapsed,
                &lapsed_record,
                LeaseEnd::Revoke {
                    reason: ReleaseReason::SpendCap,
                    by: ReleasedBy::Cloudlease
                },
                at(12 * MINUTE)
            ),
            Ok(None)
        );

        let (released, released_record) = end_lease(
            &holding,
            &admitted,
            LeaseEnd::Release {
                lease_id: &lease_id,
                reason: ReleaseReason::Idle,
                by: ReleasedBy::Machine,
            },
            at(MINUTE),
        )
        .unwrap()
        .unwrap();
        // a lease with no resources yet is released at once
        let view = lease_view(&run, &record, &released_record, &released);
        assert_eq!(view.state, LeaseState::Released);
        assert_eq!(view.release.unwrap().reason, ReleaseReason::Idle);
        assert_eq!(view.gone_at.unwrap().0, at(MINUTE));
        assert_eq!(view.heartbeat_deadline_at.0, at(15 * MINUTE));

        let past_run_deadline = end_lease(&holding, &admitted, LeaseEnd::Expire, run.deadline_at)
            .unwrap()
            .unwrap();
        assert_eq!(
            past_run_deadline.1.release.unwrap().reason,
            ReleaseReason::HardDeadline
        );
    }

    #[test]
    fn a_lease_runs_once_its_machine_exists_or_renews() {
        let run = run();
        let (record, admitted, holding) = admitted_machine(1, at(0));
        let view = |holding: &Holding<MachineKind>| lease_view(&run, &record, &admitted, holding);
        assert_eq!(view(&holding).state, LeaseState::Provisioning);
        assert_eq!(view(&holding).heartbeat_deadline_at.0, at(15 * MINUTE));
        assert_eq!(
            view(&holding).lease_id,
            MachineId {
                run: run.run_id.clone(),
                generation: Generation::FIRST
            }
        );

        let requested = holding.request(MachineKind::Droplet, at(1)).unwrap();
        assert_eq!(view(&requested).state, LeaseState::Provisioning);
        let created = requested
            .record_created(
                MachineKind::Droplet,
                ResourceId::parse("101").unwrap(),
                at(2),
            )
            .unwrap();
        assert_eq!(view(&created).state, LeaseState::Running);

        let lease_id = record.machine_id(&run.run_id).lease_id().unwrap();
        let renewed = requested.renew(&lease_id, at(MINUTE), &policy()).unwrap();
        assert_eq!(view(&renewed).state, LeaseState::Running);
        assert_eq!(view(&renewed).heartbeat_deadline_at.0, at(11 * MINUTE));

        let releasing = created.revoke(at(3));
        assert_eq!(view(&releasing).state, LeaseState::Releasing);
        // ended outside end_lease, the view still names a reason
        assert_eq!(view(&releasing).release.unwrap().by, ReleasedBy::Cloudlease);
    }
}
