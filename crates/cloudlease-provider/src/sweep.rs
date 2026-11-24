//! Deletes the resources a sweep doomed and confirms them gone

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use cloudlease_core::{
    capacity::PermitKey,
    ids::{Generation, OwnerId},
    sweep::{DoomReason, Doomed, Observed},
    tags::ResourceKind,
};
use serde::Serialize;

use crate::{
    http::Runtime,
    provider::{CloudProvider, Presence},
};

/// How long a sweep waits for a deleted resource to disappear
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfirmPolicy {
    /// Reads after the delete request, at least one
    pub reads: u8,
    /// The pause between reads
    pub pause: Duration,
}

impl Default for ConfirmPolicy {
    /// Up to 10 reads, 3 seconds apart
    fn default() -> Self {
        Self {
            reads: 10,
            pause: Duration::from_secs(3),
        }
    }
}

/// What happened to one doomed resource
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "outcome",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DeleteOutcome {
    /// Deleted and confirmed absent
    Gone,
    /// Deleted, but the provider still reported it after every confirm read
    StillPresent,
    /// Not attempted because a lower-rank resource of the same generation is not gone yet
    Deferred,
    /// The delete or confirm request failed
    Failed {
        /// The provider message
        message: String,
    },
}

/// One doomed resource and what happened to it
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteReport<K> {
    /// The resource
    pub resource: Observed<K>,
    /// Why it was doomed
    pub reason: DoomReason,
    /// What happened
    pub outcome: DeleteOutcome,
}

impl<K> DeleteReport<K> {
    /// Returns the permit key of a resource that may still exist
    #[must_use]
    pub fn survivor(&self) -> Option<PermitKey> {
        (self.outcome != DeleteOutcome::Gone).then(|| PermitKey {
            owner_id: self.resource.tags.owner.clone(),
            generation: self.resource.tags.generation,
        })
    }
}

/// Deletes doomed resources one generation at a time, lowest delete rank first
///
/// Within a generation, a rank starts only after every resource of the ranks below it is
/// confirmed gone, so a volume is not deleted while its droplet may still hold it. Resources
/// left over are reported and picked up by the next sweep.
pub async fn delete_doomed<P, R>(
    provider: &P,
    runtime: &R,
    doomed: Vec<Doomed<P::Kind>>,
    confirm: ConfirmPolicy,
) -> Vec<DeleteReport<P::Kind>>
where
    P: CloudProvider,
    R: Runtime,
{
    let mut groups: BTreeMap<(OwnerId, Generation), Vec<Doomed<P::Kind>>> = BTreeMap::new();
    for doomed in doomed {
        let key = (
            doomed.resource.tags.owner.clone(),
            doomed.resource.tags.generation,
        );
        groups.entry(key).or_default().push(doomed);
    }

    let mut reports = Vec::new();
    for mut group in groups.into_values() {
        group.sort_by_key(|doomed| doomed.resource.tags.kind.delete_rank());
        reports.extend(delete_group(provider, runtime, group, confirm).await);
    }

    reports
}

async fn delete_group<P, R>(
    provider: &P,
    runtime: &R,
    group: Vec<Doomed<P::Kind>>,
    confirm: ConfirmPolicy,
) -> Vec<DeleteReport<P::Kind>>
where
    P: CloudProvider,
    R: Runtime,
{
    let ranks: BTreeSet<u8> = group
        .iter()
        .map(|doomed| doomed.resource.tags.kind.delete_rank())
        .collect();
    let mut reports = Vec::new();
    let mut blocked = false;

    for rank in ranks {
        let members = group
            .iter()
            .filter(|doomed| doomed.resource.tags.kind.delete_rank() == rank);
        for doomed in members {
            let outcome = if blocked {
                DeleteOutcome::Deferred
            } else {
                delete_one(provider, runtime, &doomed.resource, confirm).await
            };

            reports.push(DeleteReport {
                resource: doomed.resource.clone(),
                reason: doomed.reason,
                outcome,
            });
        }

        blocked = blocked
            || reports
                .iter()
                .any(|report| report.outcome != DeleteOutcome::Gone);
    }

    reports
}

async fn delete_one<P, R>(
    provider: &P,
    runtime: &R,
    resource: &Observed<P::Kind>,
    confirm: ConfirmPolicy,
) -> DeleteOutcome
where
    P: CloudProvider,
    R: Runtime,
{
    let kind = resource.tags.kind;
    if let Err(error) = provider.delete(kind, &resource.id).await {
        return DeleteOutcome::Failed {
            message: error.to_string(),
        };
    }

    for read in 0..confirm.reads.max(1) {
        if read > 0 {
            runtime.sleep(confirm.pause).await;
        }

        match provider.presence(kind, &resource.id).await {
            Ok(Presence::Absent) => return DeleteOutcome::Gone,
            Ok(Presence::Present) => {}
            Err(error) => {
                return DeleteOutcome::Failed {
                    message: error.to_string(),
                };
            }
        }
    }

    DeleteOutcome::StillPresent
}
