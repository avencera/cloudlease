//! The scheduled tag sweep
//!
//! Run it from the app's `scheduled` handler; the app's cron trigger sets the cadence (the
//! batch app plans every 5 minutes). One sweep:
//!
//! 1. lists every resource with the app's managed tag; if that fails, nothing is deleted
//! 2. reads the capacity gate's suspect permits, when the app has a gate
//! 3. asks each owner object which of its resources it claims and which generations it is
//!    already releasing; an owner object that never existed claims nothing, and an owner that
//!    fails to answer keeps its resources until the next sweep
//! 4. deletes resources past their ceiling whatever their owner answers, and every other
//!    resource no live lease claims; each delete reports whether the owner was already
//!    releasing it, so an app can alert on orphans only
//! 5. releases suspect permits once their resources are proven absent

use std::collections::{BTreeMap, BTreeSet};

use cloudlease_core::{
    capacity::{AbsenceProof, Permit, PermitKey, PermitStatus, ReleaseRequest},
    ids::{OwnerId, PermitId},
    sweep::{OwnerAnswer, OwnerReply, Unrecognized, doom, plan, releasable_permits},
    time::UnixMillis,
};
use cloudlease_provider::{
    http::Runtime,
    provider::CloudProvider,
    sweep::{ConfirmPolicy, DeleteOutcome, DeleteReport, delete_doomed},
};
use serde::Serialize;
use worker::{Env, Method};

use crate::{
    capacity::CapacityClient, error::CloudleaseResult, http::call_json, owner::SWEEP_PATH,
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
    /// Owners asked
    pub owners_asked: usize,
    /// Owners that did not answer
    pub owners_failed: Vec<OwnerFailure>,
    /// Every resource the sweep tried to delete
    pub deletes: Vec<DeleteReport<K>>,
    /// Suspect permits released after their resources were proven absent
    pub released_permits: Vec<PermitId>,
    /// Capacity gate failures, which never block deletion
    pub capacity_errors: Vec<String>,
}

impl<K> SweepReport<K> {
    /// Reports whether every doomed resource was confirmed gone and every owner answered
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.owners_failed.is_empty()
            && self.capacity_errors.is_empty()
            && self
                .deletes
                .iter()
                .all(|report| report.outcome == DeleteOutcome::Gone)
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
    let namespace = env.durable_object(config.owner_binding)?;
    let mut replies = BTreeMap::new();
    let mut owners_failed = Vec::new();
    for (owner, query) in &plan.owners {
        let answer: CloudleaseResult<OwnerAnswer> = match namespace.get_by_name(owner.as_str()) {
            Ok(stub) => {
                call_json(
                    &stub,
                    owner.as_str(),
                    Method::Post,
                    &format!("https://owner{SWEEP_PATH}"),
                    Some(query),
                )
                .await
            }
            Err(error) => Err(error.into()),
        };

        let reply = match answer {
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

    let doomed = doom(&plan, &replies);
    let deletes = delete_doomed(provider, runtime, doomed, config.confirm).await;

    let gone: BTreeSet<_> = deletes
        .iter()
        .filter(|report| report.outcome == DeleteOutcome::Gone)
        .map(|report| report.resource.id.clone())
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
        owners_asked: plan.owners.len(),
        owners_failed,
        deletes,
        released_permits,
        capacity_errors,
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
