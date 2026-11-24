//! The scheduled tag sweep
//!
//! Run it from the app's `scheduled` handler; the app's cron trigger sets the cadence (the
//! batch app plans every 5 minutes). One sweep:
//!
//! 1. lists every resource with the app's managed tag; if that fails, nothing is deleted
//! 2. reads the capacity gate's suspect permits, when the app has a gate
//! 3. deletes resources past their ceiling, before asking any owner, so a hard deadline never
//!    waits on one
//! 4. asks each owner object which of its resources it claims and which generations it has
//!    already ended; an owner object that never existed claims nothing, and an owner that
//!    fails or does not answer within [`SweepConfig::owner_timeout`] keeps its resources below
//!    their ceiling until the next sweep
//! 5. deletes every other resource no live lease claims; each delete reports whether the owner
//!    had already ended its lease, so an app can alert on orphans only
//! 6. releases suspect permits once their resources are proven absent

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use cloudlease_core::{
    capacity::{AbsenceProof, Permit, PermitKey, PermitStatus, ReleaseRequest},
    ids::{OwnerId, PermitId},
    sweep::{
        OwnerAnswer, OwnerQuery, OwnerReply, Unlisted, Unrecognized, doom_past_ceiling,
        doom_unclaimed, owner_ended, plan, releasable_permits,
    },
    tags::ResourceKind,
    time::UnixMillis,
};
use cloudlease_provider::{
    http::Runtime,
    provider::CloudProvider,
    sweep::{ConfirmPolicy, DeleteOutcome, DeleteReport, delete_doomed},
};
use serde::Serialize;
use worker::{Env, Method, ObjectNamespace};

use crate::{
    capacity::CapacityClient,
    error::{CloudleaseError, CloudleaseResult},
    http::call_json,
    owner::SWEEP_PATH,
    runtime::within,
};

/// Where the sweep finds an app's objects
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SweepConfig<'a> {
    /// The namespace binding of the owner objects, addressed by owner ID
    pub owner_binding: &'a str,
    /// The capacity gate binding and object name, when the app has one
    pub capacity: Option<(&'a str, &'a str)>,
    /// How long to wait for deleted resources to disappear
    pub confirm: ConfirmPolicy,
    /// How long to wait for one owner's answer before counting it as failed this sweep
    pub owner_timeout: Duration,
}

/// An owner that could not answer
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerFailure {
    /// The owner
    pub owner: OwnerId,
    /// What went wrong
    pub error: String,
}

/// What one sweep saw and did
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepReport<K> {
    /// Resources with an exact managed tag set
    pub managed: usize,
    /// Resources with the managed tag in another shape, left alone
    pub unrecognized: Vec<Unrecognized>,
    /// Providers that failed to list, so their resources were neither deleted nor proven gone
    pub unlisted: Vec<Unlisted>,
    /// Owners asked
    pub owners_asked: usize,
    /// Owners that did not answer
    pub owners_failed: Vec<OwnerFailure>,
    /// Every resource the sweep tried to delete, past their ceiling first
    pub deletes: Vec<SweepDelete<K>>,
    /// Suspect permits released after their resources were proven absent
    pub released_permits: Vec<PermitId>,
    /// Capacity gate failures, which never block deletion
    pub capacity_errors: Vec<String>,
}

/// One resource the sweep tried to delete
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepDelete<K> {
    /// What happened to it
    #[serde(flatten)]
    pub report: DeleteReport<K>,
    /// Whether its owner answered that it had already ended its lease; false marks an orphan,
    /// a resource that outlived a lease its owner did not end, or an owner that did not answer
    pub owner_ended: bool,
}

impl<K> SweepReport<K> {
    /// Reports whether every provider listed, every doomed resource was confirmed gone, and
    /// every owner answered
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.unlisted.is_empty()
            && self.owners_failed.is_empty()
            && self.capacity_errors.is_empty()
            && self
                .deletes
                .iter()
                .all(|delete| delete.report.outcome == DeleteOutcome::Gone)
    }
}

/// Runs one sweep
pub async fn run_sweep<P, R>(
    env: &Env,
    provider: &P,
    runtime: &R,
    config: SweepConfig<'_>,
    now: UnixMillis,
) -> CloudleaseResult<SweepReport<P::Kind>>
where
    P: CloudProvider,
    R: Runtime,
{
    let inventory = provider.inventory().await?;
    let mut capacity_errors = Vec::new();

    let (gate, permits) = match config.capacity {
        Some((binding, name)) => match read_suspect_permits(env, binding, name, now).await {
            Ok((gate, permits)) => (Some(gate), permits),
            Err(error) => {
                capacity_errors.push(error.to_string());
                (None, Vec::new())
            }
        },
        None => (None, Vec::new()),
    };
    let suspect_keys: Vec<PermitKey> = permits.iter().map(Permit::key).collect();

    let plan = plan(&inventory, &suspect_keys, now);
    let mut reports =
        delete_doomed(provider, runtime, doom_past_ceiling(&plan), config.confirm).await;

    let namespace = env.durable_object(config.owner_binding)?;
    let mut replies = BTreeMap::new();
    let mut owners_failed = Vec::new();
    for (owner, query) in &plan.owners {
        let reply = match ask_owner(&namespace, owner, query, config.owner_timeout).await {
            Ok(answer) => OwnerReply::Answered(answer),
            Err(error) => {
                owners_failed.push(OwnerFailure {
                    owner: owner.clone(),
                    error: error.to_string(),
                });
                OwnerReply::Failed
            }
        };
        replies.insert(owner.clone(), reply);
    }

    let unclaimed = doom_unclaimed(&plan, &replies);
    reports.extend(delete_doomed(provider, runtime, unclaimed, config.confirm).await);
    let deletes: Vec<_> = reports
        .into_iter()
        .map(|report| SweepDelete {
            owner_ended: owner_ended(&replies, &report.resource),
            report,
        })
        .collect();

    let gone: BTreeSet<_> = deletes
        .iter()
        .filter(|delete| delete.report.outcome == DeleteOutcome::Gone)
        .map(|delete| delete.report.resource.id.clone())
        .collect();
    let survivors: BTreeSet<PermitKey> = inventory
        .managed
        .iter()
        .filter(|resource| !gone.contains(&resource.id))
        .map(|resource| PermitKey {
            owner_id: resource.tags.owner.clone(),
            generation: resource.tags.generation,
        })
        .collect();

    let mut released_permits = Vec::new();
    if let Some(gate) = gate {
        let releasable =
            releasable_permits(inventory.is_complete(), &permits, &survivors, &replies);
        for permit in releasable {
            let request = ReleaseRequest {
                permit_id: permit.permit_id.clone(),
                owner_id: permit.owner_id.clone(),
                generation: permit.generation,
                owner_absent: AbsenceProof,
            };
            match gate.release(&request).await {
                Ok(()) => released_permits.push(permit.permit_id),
                Err(error) => capacity_errors.push(error.to_string()),
            }
        }
    }

    Ok(SweepReport {
        managed: inventory.managed.len(),
        unrecognized: inventory.unrecognized,
        unlisted: inventory.unlisted,
        owners_asked: plan.owners.len(),
        owners_failed,
        deletes,
        released_permits,
        capacity_errors,
    })
}

async fn ask_owner<K: ResourceKind>(
    namespace: &ObjectNamespace,
    owner: &OwnerId,
    query: &OwnerQuery<K>,
    timeout: Duration,
) -> CloudleaseResult<OwnerAnswer> {
    let stub = namespace.get_by_name(owner.as_str())?;
    let url = format!("https://owner{SWEEP_PATH}");
    let call = call_json(&stub, owner.as_str(), Method::Post, &url, Some(query));

    within(timeout, call).await.unwrap_or_else(|| {
        Err(CloudleaseError::Timeout {
            object: owner.as_str().to_owned(),
            limit: timeout,
        })
    })
}

async fn read_suspect_permits(
    env: &Env,
    binding: &str,
    name: &str,
    now: UnixMillis,
) -> CloudleaseResult<(CapacityClient, Vec<Permit>)> {
    let gate = CapacityClient::connect(env, binding, name)?;
    let snapshot = gate.snapshot().await?;
    // the gate's alarm may not have run yet, so judge expiry by the clock
    let suspect = snapshot
        .permits
        .iter()
        .map(|permit| permit.at(now))
        .filter(|permit| permit.status == PermitStatus::Suspect)
        .collect();

    Ok((gate, suspect))
}
