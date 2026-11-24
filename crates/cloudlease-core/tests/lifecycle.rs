//! A holding from lease to confirmed deletion, and the sweep decisions built on it

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use cloudlease_core::{
    alarm::{Backoff, Wake, next_alarm},
    capacity::{Permit, PermitKey, PermitStatus, PermitUnits},
    deletes::{DeleteTarget, DeletesError},
    holding::{Holding, HoldingError, HoldingState, ReleaseReason, Retry, TAG_SEARCH_SETTLE},
    ids::{Generation, LeaseId, OwnerId, PermitId, ResourceId},
    lease::{Expiry, Lease, LeaseError, LeasePolicy},
    sweep::{
        DoomReason, Inventory, Observed, OwnerAnswer, OwnerQuery, OwnerReply, Unrecognized, answer,
        doom, plan, releasable_permits,
    },
    tags::{Ceiling, CeilingTag, OwnerTags, ResourceKind, UnrecognizedReason},
    time::UnixMillis,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
enum Kind {
    Machine,
    Disk,
}

impl ResourceKind for Kind {
    const ALL: &'static [Self] = &[Self::Machine, Self::Disk];

    fn tag_name(self) -> &'static str {
        match self {
            Self::Machine => "machine",
            Self::Disk => "disk",
        }
    }

    fn delete_rank(self) -> u8 {
        match self {
            Self::Machine => 0,
            Self::Disk => 1,
        }
    }
}

const MINUTE: Duration = Duration::from_secs(60);

fn at(millis: u64) -> UnixMillis {
    UnixMillis::new(millis).unwrap()
}

fn start() -> UnixMillis {
    at(1_000_000)
}

fn policy() -> LeasePolicy {
    LeasePolicy::new(2 * MINUTE, 60 * MINUTE).unwrap()
}

fn owner() -> OwnerId {
    OwnerId::parse("job_1").unwrap()
}

fn lease_id() -> LeaseId {
    LeaseId::parse("lease_1").unwrap()
}

fn id(value: &str) -> ResourceId {
    ResourceId::parse(value).unwrap()
}

fn generation(value: u64) -> Generation {
    Generation::new(value).unwrap()
}

fn leased() -> Holding<Kind> {
    let lease = Lease::start(lease_id(), &policy(), start(), None).unwrap();

    Holding::leased(owner(), generation(1), lease)
}

/// A leased holding whose machine is created and whose disk create response was lost
fn half_created() -> Holding<Kind> {
    leased()
        .request(Kind::Machine, start())
        .unwrap()
        .record_created(Kind::Machine, id("101"), start())
        .unwrap()
        .request(Kind::Disk, start())
        .unwrap()
}

fn observed(owner: &str, generation_value: u64, kind: Kind, value: &str) -> Observed<Kind> {
    Observed {
        id: id(value),
        tags: OwnerTags {
            owner: OwnerId::parse(owner).unwrap(),
            generation: generation(generation_value),
            ceiling: None,
        }
        .with_kind(kind),
    }
}

#[test]
fn an_expired_lease_releases_machines_before_disks_and_finds_lost_creates_by_tag() {
    let holding = half_created();
    assert!(holding.expire(at(1_119_999)).is_none());

    let releasing = holding.expire(at(1_120_000)).unwrap();
    let HoldingState::Releasing {
        reason, deletes, ..
    } = releasing.state()
    else {
        panic!("an expired lease must start releasing");
    };
    assert_eq!(*reason, ReleaseReason::HeartbeatMissed);
    assert_eq!(
        deletes.iter().cloned().collect::<Vec<_>>(),
        vec![
            DeleteTarget::Known {
                kind: Kind::Machine,
                id: id("101"),
            },
            DeleteTarget::Tagged { kind: Kind::Disk },
        ]
    );

    // the disk waits until the machine is confirmed gone
    assert_eq!(
        releasing.ready_deletes(),
        vec![DeleteTarget::Known {
            kind: Kind::Machine,
            id: id("101"),
        }]
    );
    assert_eq!(
        releasing.confirm_gone(&DeleteTarget::Tagged { kind: Kind::Disk }, at(1_120_001)),
        Err(HoldingError::Deletes(DeletesError::RankNotReady))
    );

    let machine_gone = releasing
        .confirm_gone(
            &DeleteTarget::Known {
                kind: Kind::Machine,
                id: id("101"),
            },
            at(1_121_000),
        )
        .unwrap();
    assert_eq!(
        machine_gone.ready_deletes(),
        vec![DeleteTarget::Tagged { kind: Kind::Disk }]
    );

    // an empty search soon after the lease ended proves nothing, since the disk's create
    // may still be in flight; once the settle time has passed it proves absence
    let settled = 1_120_000 + u64::try_from(TAG_SEARCH_SETTLE.as_millis()).unwrap();
    let nothing: [ResourceId; 0] = [];
    assert_eq!(
        machine_gone
            .resolve_tagged(Kind::Disk, nothing.clone(), at(settled - 1))
            .unwrap(),
        None
    );
    let settled_empty = machine_gone
        .resolve_tagged(Kind::Disk, nothing, at(settled))
        .unwrap()
        .unwrap();
    assert!(settled_empty.is_released());

    // a tag search that finds the lost disk turns it into a known target at once
    let found = machine_gone
        .resolve_tagged(Kind::Disk, [id("vol-1")], at(1_122_000))
        .unwrap()
        .unwrap();
    assert_eq!(
        found.ready_deletes(),
        vec![DeleteTarget::Known {
            kind: Kind::Disk,
            id: id("vol-1"),
        }]
    );

    let released = found
        .confirm_gone(
            &DeleteTarget::Known {
                kind: Kind::Disk,
                id: id("vol-1"),
            },
            at(1_123_000),
        )
        .unwrap();
    assert_eq!(
        released.state(),
        &HoldingState::Released {
            reason: ReleaseReason::HeartbeatMissed,
            at: at(1_123_000),
        }
    );
    assert_eq!(released.wake(), None);
}

#[test]
fn a_lease_with_no_resources_releases_at_once() {
    let released = leased().release(&lease_id(), at(1_010_000)).unwrap();

    assert!(released.is_released());
}

#[test]
fn a_stale_client_cannot_release_heartbeat_or_add_resources() {
    let holding = half_created();
    let stale = LeaseId::parse("lease_stale").unwrap();

    assert_eq!(
        holding.release(&stale, at(1_005_000)),
        Err(HoldingError::Lease(LeaseError::WrongLease))
    );
    assert_eq!(
        holding.heartbeat(&stale, 1, at(1_005_000), &policy()),
        Err(HoldingError::Lease(LeaseError::WrongLease))
    );
    assert_eq!(
        holding.release(&lease_id(), at(1_120_000)),
        Err(HoldingError::Lease(LeaseError::Expired(
            Expiry::HeartbeatMissed
        )))
    );
    assert_eq!(
        leased().request(Kind::Disk, at(1_120_000)),
        Err(HoldingError::Lease(LeaseError::Expired(
            Expiry::HeartbeatMissed
        )))
    );
    assert_eq!(
        holding.request(Kind::Machine, start()),
        Err(HoldingError::SlotTaken)
    );
    assert_eq!(
        holding.record_created(Kind::Machine, id("999"), start()),
        Err(HoldingError::SlotMismatch)
    );
    assert_eq!(
        holding.record_created(Kind::Machine, id("101"), start()),
        Ok(holding.clone())
    );
    // a create that returns after the lease lapsed is not recorded, even before the
    // expiry is stored, since the sweep has stopped counting the resource as claimed
    assert_eq!(
        leased()
            .request(Kind::Machine, start())
            .unwrap()
            .record_created(Kind::Machine, id("101"), at(1_120_000)),
        Err(HoldingError::Lease(LeaseError::Expired(
            Expiry::HeartbeatMissed
        )))
    );

    let releasing = holding.expire(at(1_120_000)).unwrap();
    assert!(matches!(
        releasing.heartbeat(&lease_id(), 1, at(1_120_000), &policy()),
        Err(HoldingError::InvalidTransition { .. })
    ));
}

#[test]
fn cleanup_retries_back_off_and_progress_resets_them() {
    let releasing = half_created().expire(at(1_120_000)).unwrap();
    assert_eq!(releasing.wake(), Some(Wake::Now));

    let backoff = Backoff::default();
    let first = releasing.retry_later(at(1_120_000), &backoff).unwrap();
    let second = first.retry_later(at(1_121_000), &backoff).unwrap();
    let HoldingState::Releasing { retry, .. } = second.state() else {
        panic!("a retried holding stays releasing");
    };
    assert_eq!(
        *retry,
        Some(Retry {
            attempt: 2,
            at: at(1_123_000),
        })
    );
    assert_eq!(second.wake(), Some(Wake::At(at(1_123_000))));

    let progressed = second
        .confirm_gone(
            &DeleteTarget::Known {
                kind: Kind::Machine,
                id: id("101"),
            },
            at(1_123_000),
        )
        .unwrap();
    assert_eq!(progressed.wake(), Some(Wake::Now));
    assert!(matches!(
        leased().retry_later(at(1_000_001), &backoff),
        Err(HoldingError::InvalidTransition { .. })
    ));
}

#[test]
fn an_owner_alarm_follows_the_earliest_holding() {
    let early = leased();
    let late = Holding::leased(
        owner(),
        generation(2),
        Lease::start(lease_id(), &policy(), at(1_060_000), None).unwrap(),
    );
    let wakes = [&late, &early].into_iter().filter_map(Holding::wake);

    assert_eq!(next_alarm(wakes, start()), Some(at(1_120_000)));
}

#[test]
fn holdings_round_trip_through_storage() {
    for holding in [
        half_created(),
        half_created().expire(at(1_120_000)).unwrap(),
        leased().release(&lease_id(), at(1_000_500)).unwrap(),
    ] {
        let json = serde_json::to_string(&holding).unwrap();

        assert_eq!(
            serde_json::from_str::<Holding<Kind>>(&json).unwrap(),
            holding
        );
    }

    let mut empty_deletes =
        serde_json::to_value(half_created().expire(at(1_120_000)).unwrap()).unwrap();
    empty_deletes["state"]["deletes"] = serde_json::json!([]);
    assert!(serde_json::from_value::<Holding<Kind>>(empty_deletes).is_err());
}

#[test]
fn only_a_live_lease_of_the_same_generation_claims_a_resource() {
    let holding = half_created();
    let now = at(1_010_000);
    let machine = observed("job_1", 1, Kind::Machine, "101");
    let duplicate_machine = observed("job_1", 1, Kind::Machine, "102");
    let pending_disk = observed("job_1", 1, Kind::Disk, "vol-1");
    let older_generation = observed("job_1", 2, Kind::Machine, "103");
    let other_owner = observed("job_2", 1, Kind::Machine, "101");

    assert!(holding.claims(&machine.id, &machine.tags, now));
    assert!(!holding.claims(&duplicate_machine.id, &duplicate_machine.tags, now));
    assert!(holding.claims(&pending_disk.id, &pending_disk.tags, now));
    assert!(!holding.claims(&older_generation.id, &older_generation.tags, now));
    assert!(!holding.claims(&other_owner.id, &other_owner.tags, now));

    // an expired lease claims nothing even before its owner processes the expiry
    assert!(!holding.claims(&machine.id, &machine.tags, at(1_120_000)));
    let releasing = holding.expire(at(1_120_000)).unwrap();
    assert!(!releasing.claims(&machine.id, &machine.tags, at(1_120_000)));
}

#[test]
fn the_sweep_deletes_orphans_expired_and_past_ceiling_resources_but_keeps_live_ones() {
    let now = at(1_010_000);
    let live = half_created();
    let mut past_ceiling = observed("job_1", 1, Kind::Machine, "101");
    past_ceiling.tags.ceiling = CeilingTag::At(Ceiling::at_or_before(at(1_000_000)));
    // a malformed ceiling is never read as reached or as no deadline: the owner decides
    let mut malformed = observed("ghost", 1, Kind::Machine, "202");
    malformed.tags.ceiling = CeilingTag::Malformed;
    let inventory = Inventory {
        managed: vec![
            observed("job_1", 1, Kind::Disk, "vol-1"),
            past_ceiling.clone(),
            observed("ghost", 1, Kind::Machine, "201"),
            malformed,
            observed("ghost", 1, Kind::Disk, "vol-2"),
            observed("down", 1, Kind::Machine, "301"),
        ],
        unrecognized: Vec::new(),
    };

    let plan = plan(&inventory, &[], now);
    assert_eq!(plan.past_ceiling, vec![past_ceiling.clone()]);
    assert_eq!(
        plan.owners.keys().map(OwnerId::as_str).collect::<Vec<_>>(),
        vec!["down", "ghost", "job_1"]
    );

    // the ghost owner object never existed, so it has no holdings and claims nothing
    let replies = BTreeMap::from([
        (
            owner(),
            OwnerReply::Answered(answer([&live], &plan.owners[&owner()], now)),
        ),
        (
            OwnerId::parse("ghost").unwrap(),
            OwnerReply::Answered(answer(
                std::iter::empty::<&Holding<Kind>>(),
                &plan.owners[&OwnerId::parse("ghost").unwrap()],
                now,
            )),
        ),
        (OwnerId::parse("down").unwrap(), OwnerReply::Failed),
    ]);
    let doomed = doom(&plan, &replies);

    assert_eq!(
        doomed
            .iter()
            .map(|doomed| (doomed.resource.id.as_str(), doomed.reason))
            .collect::<Vec<_>>(),
        vec![
            ("101", DoomReason::PastCeiling),
            ("201", DoomReason::Unclaimed),
            ("202", DoomReason::Unclaimed),
            ("vol-2", DoomReason::Unclaimed),
        ]
    );

    // once the lease expires, the same owner stops claiming its disk
    let expired_at = at(1_120_000);
    let expired_answer = answer([&live], &plan.owners[&owner()], expired_at);
    assert_eq!(expired_answer.claimed, BTreeSet::new());
}

#[test]
fn only_resources_their_owner_is_not_already_releasing_are_marked_as_orphans() {
    let now = at(1_120_000);
    // generation 1 expired and is releasing; generation 2 is live
    let releasing = half_created().expire(now).unwrap();
    let live_lease = Lease::start(lease_id(), &policy(), at(1_110_000), None).unwrap();
    let live = Holding::<Kind>::leased(owner(), generation(2), live_lease);
    let mut outlived = observed("job_1", 2, Kind::Machine, "111");
    outlived.tags.ceiling = CeilingTag::At(Ceiling::at_or_before(at(1_000_000)));
    let mut ghost_past_ceiling = observed("ghost", 2, Kind::Machine, "202");
    ghost_past_ceiling.tags.ceiling = CeilingTag::At(Ceiling::at_or_before(at(1_000_000)));
    let inventory = Inventory {
        managed: vec![
            observed("job_1", 1, Kind::Machine, "101"),
            outlived.clone(),
            observed("job_1", 3, Kind::Machine, "131"),
            observed("ghost", 1, Kind::Machine, "201"),
            ghost_past_ceiling,
        ],
        unrecognized: Vec::new(),
    };

    let plan = plan(&inventory, &[], now);
    // the owner is asked about the generation past its ceiling, but not to keep it
    let query = &plan.owners[&owner()];
    assert_eq!(
        query.generations,
        BTreeSet::from([generation(1), generation(2), generation(3)])
    );
    assert!(!query.resources.contains(&outlived));

    let answer_job = answer([&releasing, &live], query, now);
    assert_eq!(answer_job.releasing, BTreeSet::from([generation(1)]));
    assert_eq!(
        answer_job.busy,
        BTreeSet::from([generation(1), generation(2)])
    );
    let replies = BTreeMap::from([
        (owner(), OwnerReply::Answered(answer_job)),
        (OwnerId::parse("ghost").unwrap(), OwnerReply::Failed),
    ]);

    let marks: Vec<_> = doom(&plan, &replies)
        .iter()
        .map(|doomed| {
            (
                doomed.resource.id.as_str().to_owned(),
                doomed.reason,
                doomed.owner_releasing,
            )
        })
        .collect();
    assert_eq!(
        marks,
        vec![
            // a live lease its owner failed to end at the ceiling is not the owner's cleanup
            ("111".to_owned(), DoomReason::PastCeiling, false),
            // the failed owner keeps 201 this round, but its past-ceiling resource still goes
            ("202".to_owned(), DoomReason::PastCeiling, false),
            ("101".to_owned(), DoomReason::Unclaimed, true),
            ("131".to_owned(), DoomReason::Unclaimed, false),
        ]
    );

    // an answer from an owner that predates the field reads as releasing nothing
    let old: OwnerAnswer = serde_json::from_value(serde_json::json!({
        "claimed": ["101"],
        "busy": [1],
    }))
    .unwrap();
    assert!(old.releasing.is_empty());
}

#[test]
fn suspect_permits_are_released_only_with_a_complete_inventory_and_an_idle_owner() {
    let suspect = |owner: &str, generation_value: u64| Permit {
        permit_id: PermitId::parse(format!("cap_{owner}")).unwrap(),
        owner_id: OwnerId::parse(owner).unwrap(),
        generation: generation(generation_value),
        units: PermitUnits::ONE,
        expires_at: at(1),
        status: PermitStatus::Suspect,
    };
    let active = Permit {
        status: PermitStatus::Active,
        ..suspect("active", 1)
    };
    let permits = vec![
        suspect("gone", 1),
        suspect("survivor", 1),
        suspect("busy", 1),
        suspect("down", 1),
        active,
    ];
    let survivors = BTreeSet::from([PermitKey {
        owner_id: OwnerId::parse("survivor").unwrap(),
        generation: generation(1),
    }]);
    let idle = OwnerReply::Answered(OwnerAnswer::default());
    let replies = BTreeMap::from([
        (OwnerId::parse("gone").unwrap(), idle.clone()),
        (OwnerId::parse("survivor").unwrap(), idle.clone()),
        (
            OwnerId::parse("busy").unwrap(),
            OwnerReply::Answered(OwnerAnswer {
                claimed: BTreeSet::new(),
                busy: BTreeSet::from([generation(1)]),
                releasing: BTreeSet::new(),
            }),
        ),
        (OwnerId::parse("down").unwrap(), OwnerReply::Failed),
        (OwnerId::parse("active").unwrap(), idle),
    ]);

    let released = releasable_permits(true, &permits, &survivors, &replies);
    assert_eq!(
        released
            .iter()
            .map(|permit| permit.owner_id.as_str())
            .collect::<Vec<_>>(),
        vec!["gone"]
    );
    assert!(releasable_permits(false, &permits, &survivors, &replies).is_empty());

    // a releasing generation is busy, a released one is not
    let releasing = half_created().expire(at(1_120_000)).unwrap();
    let query = OwnerQuery {
        resources: Vec::new(),
        generations: BTreeSet::from([generation(1)]),
    };
    assert_eq!(
        answer([&releasing], &query, at(1_120_000)).busy,
        BTreeSet::from([generation(1)])
    );
    let released_holding = leased().release(&lease_id(), at(1_000_500)).unwrap();
    assert!(
        answer([&released_holding], &query, at(1_120_000))
            .busy
            .is_empty()
    );
}

#[test]
fn unrecognized_resources_make_the_inventory_incomplete_and_are_never_doomed() {
    let inventory = Inventory::<Kind> {
        managed: Vec::new(),
        unrecognized: vec![Unrecognized {
            provider_kind: "droplet".to_owned(),
            id: "401".to_owned(),
            name: "hand-made".to_owned(),
            reason: UnrecognizedReason::ExtraTag,
            tags: vec!["cl-managed".to_owned(), "team-x".to_owned()],
        }],
    };
    let plan = plan(&inventory, &[], start());

    assert!(!inventory.is_complete());
    assert!(plan.owners.is_empty());
    assert!(doom(&plan, &BTreeMap::new()).is_empty());
}

#[test]
fn an_owner_can_revoke_a_lease_once() {
    let revoked = half_created().revoke(at(1_005_000));
    let HoldingState::Releasing { reason, .. } = revoked.state() else {
        panic!("a revoked lease must start releasing");
    };

    assert_eq!(*reason, ReleaseReason::Revoked);
    assert_eq!(revoked.revoke(at(1_006_000)), revoked);
}
