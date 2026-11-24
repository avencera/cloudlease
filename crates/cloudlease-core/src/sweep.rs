//! Which tagged resources a sweep deletes and which permits it releases
//!
//! The sweep is deny by default. It lists every resource with the app's managed tag and asks
//! each owner which of its resources it claims. A resource is kept only when its owner
//! answers and claims it; an owner that does not exist claims nothing, so its resources are
//! deleted. A resource past its [`Ceiling`](crate::tags::Ceiling) is deleted whatever its
//! owner answers.
//!
//! Owners also say which of the queried generations they are already releasing. A doomed
//! resource of such a generation is the owner's own cleanup racing the sweep; any other
//! doomed resource is an orphan or outlived a lease its owner failed to end, which an app
//! may want to alert on.
//!
//! ```text
//! inventory --plan--> SweepPlan --owners answer--> doom --> delete and confirm
//!                                                      \--> releasable permits
//! ```

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    capacity::{Permit, PermitKey, PermitStatus},
    holding::{Holding, HoldingState},
    ids::{Generation, OwnerId, ResourceId},
    tags::{ManagedTags, ResourceKind, UnrecognizedReason},
    time::UnixMillis,
};

/// A provider resource with an exact managed tag set
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Observed<K> {
    /// The provider ID
    pub id: ResourceId,
    /// The parsed tags
    pub tags: ManagedTags<K>,
}

/// A provider resource that carries the managed tag but not an exact managed set
///
/// The sweep never deletes these; it reports them for an operator.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Unrecognized {
    /// The provider's name for the resource type, such as `droplet`
    pub provider_kind: String,
    /// The provider ID
    pub id: String,
    /// The provider name
    pub name: String,
    /// Why the tags are not an exact managed set
    pub reason: UnrecognizedReason,
    /// The raw tags
    pub tags: Vec<String>,
}

/// Every resource with the app's managed tag
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Inventory<K> {
    /// Resources with exact managed tags
    pub managed: Vec<Observed<K>>,
    /// Resources with the managed tag but another shape
    pub unrecognized: Vec<Unrecognized>,
}

impl<K> Inventory<K> {
    /// Reports whether every managed-tagged resource was recognized
    ///
    /// Absence can be proven from an inventory only when it is complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unrecognized.is_empty()
    }
}

/// The question the sweep asks one owner
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    rename_all = "camelCase",
    bound(
        serialize = "K: ResourceKind",
        deserialize = "K: ResourceKind + DeserializeOwned"
    )
)]
pub struct OwnerQuery<K> {
    /// The owner's resources in the inventory
    pub resources: Vec<Observed<K>>,
    /// Generations the owner reports on: those of its resources, including resources past
    /// their ceiling, and those of its suspect permits
    pub generations: BTreeSet<Generation>,
}

/// One owner's answer to an [`OwnerQuery`]
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerAnswer {
    /// Resources a live lease of this owner keeps
    pub claimed: BTreeSet<ResourceId>,
    /// Queried generations that are not yet confirmed gone
    pub busy: BTreeSet<Generation>,
    /// Queried generations whose lease ended and whose resources the owner is deleting
    #[serde(default)]
    pub releasing: BTreeSet<Generation>,
}

/// Answers a sweep query from an owner's holdings
///
/// A generation is busy while it is leased or still releasing, since its owner may yet
/// create or delete resources; only a released or unknown generation can have its permit
/// released by the sweep. The releasing generations are reported apart, so the sweep can tell
/// the owner's own cleanup from an orphan.
#[must_use]
pub fn answer<'a, K: ResourceKind>(
    holdings: impl IntoIterator<Item = &'a Holding<K>>,
    query: &OwnerQuery<K>,
    now: UnixMillis,
) -> OwnerAnswer {
    let holdings: Vec<_> = holdings.into_iter().collect();
    let claimed = query
        .resources
        .iter()
        .filter(|resource| {
            holdings
                .iter()
                .any(|holding| holding.claims(&resource.id, &resource.tags, now))
        })
        .map(|resource| resource.id.clone())
        .collect();
    let reported = |keep: fn(&Holding<K>) -> bool| -> BTreeSet<Generation> {
        query
            .generations
            .iter()
            .copied()
            .filter(|generation| {
                holdings
                    .iter()
                    .any(|holding| holding.generation() == *generation && keep(holding))
            })
            .collect()
    };

    OwnerAnswer {
        claimed,
        busy: reported(|holding| !holding.is_released()),
        releasing: reported(|holding| matches!(holding.state(), HoldingState::Releasing { .. })),
    }
}

/// What the sweep must ask and delete before it hears from owners
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SweepPlan<K> {
    /// Resources past their ceiling, deleted without asking their owner
    pub past_ceiling: Vec<Observed<K>>,
    /// One query per owner with resources or suspect permits
    pub owners: BTreeMap<OwnerId, OwnerQuery<K>>,
}

/// Plans a sweep from the inventory and the suspect permits
#[must_use]
pub fn plan<K: ResourceKind>(
    inventory: &Inventory<K>,
    suspect_permits: &[PermitKey],
    now: UnixMillis,
) -> SweepPlan<K> {
    let mut past_ceiling = Vec::new();
    let mut owners: BTreeMap<OwnerId, OwnerQuery<K>> = BTreeMap::new();

    for resource in &inventory.managed {
        // the owner is asked about a resource past its ceiling too, but only to learn whether
        // it was already deleting it; its answer cannot keep the resource
        let query = owners
            .entry(resource.tags.owner.clone())
            .or_insert_with(empty_query);
        query.generations.insert(resource.tags.generation);
        if resource.tags.ceiling.is_reached_at(now) {
            past_ceiling.push(resource.clone());
            continue;
        }

        query.resources.push(resource.clone());
    }

    for permit in suspect_permits {
        owners
            .entry(permit.owner_id.clone())
            .or_insert_with(empty_query)
            .generations
            .insert(permit.generation);
    }

    SweepPlan {
        past_ceiling,
        owners,
    }
}

const fn empty_query<K>() -> OwnerQuery<K> {
    OwnerQuery {
        resources: Vec::new(),
        generations: BTreeSet::new(),
    }
}

/// An owner's reply, or the lack of one
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OwnerReply {
    /// The owner answered
    Answered(OwnerAnswer),
    /// The owner could not be reached or failed; its resources are kept this round
    Failed,
}

/// Why the sweep deletes a resource
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DoomReason {
    /// The resource is past its ceiling
    PastCeiling,
    /// No live lease of its owner claims it, including when the owner does not exist
    Unclaimed,
}

/// A resource the sweep deletes
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Doomed<K> {
    /// The resource
    pub resource: Observed<K>,
    /// Why it is deleted
    pub reason: DoomReason,
    /// Whether its owner reported the generation as releasing, so its own cleanup was already
    /// deleting the resource; when false the resource is an orphan or outlived a lease its
    /// owner did not end
    pub owner_releasing: bool,
}

/// Selects the resources to delete from the plan and the owners' replies
///
/// An owner missing from `replies` counts as [`OwnerReply::Failed`], so a resource is never
/// deleted because a reply was lost.
#[must_use]
pub fn doom<K: ResourceKind>(
    plan: &SweepPlan<K>,
    replies: &BTreeMap<OwnerId, OwnerReply>,
) -> Vec<Doomed<K>> {
    let answer_of = |owner: &OwnerId| match replies.get(owner) {
        Some(OwnerReply::Answered(answer)) => Some(answer),
        Some(OwnerReply::Failed) | None => None,
    };
    let owner_releasing = move |resource: &Observed<K>| {
        answer_of(&resource.tags.owner)
            .is_some_and(|answer| answer.releasing.contains(&resource.tags.generation))
    };

    let past_ceiling = plan.past_ceiling.iter().map(move |resource| Doomed {
        resource: resource.clone(),
        reason: DoomReason::PastCeiling,
        owner_releasing: owner_releasing(resource),
    });
    let unclaimed = plan.owners.iter().flat_map(move |(owner, query)| {
        let answer = answer_of(owner);

        query
            .resources
            .iter()
            .filter(move |resource| {
                answer.is_some_and(|answer| !answer.claimed.contains(&resource.id))
            })
            .map(move |resource| Doomed {
                resource: resource.clone(),
                reason: DoomReason::Unclaimed,
                owner_releasing: owner_releasing(resource),
            })
    });

    past_ceiling.chain(unclaimed).collect()
}

/// Selects the suspect permits a sweep may release
///
/// A permit is released only when the inventory was complete, no resource of its owner and
/// generation survived the sweep, and its owner answered without marking the generation
/// busy.
#[must_use]
pub fn releasable_permits(
    inventory_complete: bool,
    permits: &[Permit],
    survivors: &BTreeSet<PermitKey>,
    replies: &BTreeMap<OwnerId, OwnerReply>,
) -> Vec<Permit> {
    if !inventory_complete {
        return Vec::new();
    }

    permits
        .iter()
        .filter(|permit| permit.status == PermitStatus::Suspect)
        .filter(|permit| !survivors.contains(&permit.key()))
        .filter(|permit| match replies.get(&permit.owner_id) {
            Some(OwnerReply::Answered(answer)) => !answer.busy.contains(&permit.generation),
            Some(OwnerReply::Failed) | None => false,
        })
        .cloned()
        .collect()
}
