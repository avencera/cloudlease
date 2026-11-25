//! The owner side of a lease: processing due holdings and answering the sweep
//!
//! An owner Durable Object keeps its holdings in a [`HoldingStore`], saves every change with
//! [`save`], creates resources with [`create`], runs [`run_alarm`] from its `alarm` handler
//! and serves [`answer_sweep`] at [`SWEEP_PATH`].

use cloudlease_core::{
    alarm::{Backoff, Wake},
    holding::Holding,
    ids::{Generation, ResourceId},
    sweep::{OwnerQuery, answer},
    tags::ResourceKind,
    time::UnixMillis,
};
use cloudlease_provider::{
    error::ProviderError,
    provider::{CloudProvider, CreateSpec},
    reaper::reap,
};
use serde::de::DeserializeOwned;
use worker::{Request, Response, Storage};

use crate::{
    alarm::{pull_earlier, rearm, rearm_after_failure},
    error::{CloudleaseError, CloudleaseResult},
    http::{HttpError, json_response, read_json},
    store::HoldingStore,
};

/// The path the sweep posts an [`OwnerQuery`] to
pub const SWEEP_PATH: &str = "/cloudlease/sweep";

/// What one alarm pass did
#[derive(Debug)]
pub struct Processed<K> {
    /// Holdings that reached `Released` during this pass, whose permits the app can release
    pub released: Vec<Holding<K>>,
    /// Provider failures, each already turned into a retry
    pub errors: Vec<ProviderError>,
}

/// Saves a holding and moves the alarm earlier if one of the holdings is due before it
///
/// The alarm is never moved later here: the owner may have deadlines of its own beyond its
/// holdings, and only its alarm handler, which knows them all, re-arms from scratch.
pub async fn save<K>(
    storage: &Storage,
    store: &HoldingStore,
    holding: &Holding<K>,
    now: UnixMillis,
) -> CloudleaseResult<()>
where
    K: ResourceKind + DeserializeOwned,
{
    store.put(holding, now)?;
    let holdings = store.list::<K>()?;

    pull_earlier(storage, holdings.iter().filter_map(Holding::wake), now).await
}

/// Creates one resource for a leased generation, saving the intent before the provider call
///
/// The `Requested` slot is saved first, so a sweep that runs while the create is in flight,
/// or after this object dies before recording the ID, finds the resource claimed by a live
/// lease. If the ID is never recorded, the lease's expiry turns the slot into a delete by
/// tags. The resource's ceiling tag is the lease's hard deadline.
///
/// The holding is read again after the provider call, and the clock too, since an alarm
/// may have ended the lease meanwhile or it may have expired without one yet; the ID is
/// then not recorded, the holding error is returned, and cleanup finds the resource by its
/// tags. A provider error leaves the slot requested for the same reason.
pub async fn create<K, P>(
    storage: &Storage,
    store: &HoldingStore,
    provider: &P,
    generation: Generation,
    spec: &P::Spec,
    now: UnixMillis,
) -> CloudleaseResult<(ResourceId, Holding<K>)>
where
    K: ResourceKind + DeserializeOwned,
    P: CloudProvider<Kind = K>,
{
    let kind = spec.kind();
    let holding = load(store, generation)?;
    let tags = holding.owner_tags()?;
    let requested = holding.request(kind, now)?;
    save(storage, store, &requested, now).await?;

    let id = provider.create(&tags, spec).await?;

    // a create can take seconds, so `now` from before it is too early to check the lease
    let after = crate::runtime::now().max(now);
    let created = load(store, generation)?.record_created(kind, id.clone(), after)?;
    save(storage, store, &created, after).await?;

    Ok((id, created))
}

fn load<K>(store: &HoldingStore, generation: Generation) -> CloudleaseResult<Holding<K>>
where
    K: ResourceKind + DeserializeOwned,
{
    store
        .get(generation)?
        .ok_or(CloudleaseError::UnknownGeneration(generation))
}

/// Expires overdue leases, runs a cleanup pass over each due holding, and re-arms the alarm
///
/// Every expiry is persisted before the first provider call, so a failing provider cannot
/// keep an expired lease alive.
pub async fn process_due<K, P>(
    storage: &Storage,
    store: &HoldingStore,
    provider: &P,
    now: UnixMillis,
    backoff: &Backoff,
) -> CloudleaseResult<Processed<K>>
where
    K: ResourceKind + DeserializeOwned,
    P: CloudProvider<Kind = K>,
{
    let mut released = Vec::new();
    let mut errors = Vec::new();

    let mut holdings = store.list::<K>()?;
    for holding in &mut holdings {
        if let Some(expired) = holding.expire(now) {
            store.put(&expired, now)?;
            // a lease with no resources goes straight to released
            if expired.is_released() {
                released.push(expired.clone());
            }
            *holding = expired;
        }
    }

    for holding in &mut holdings {
        if !is_due(holding.wake(), now) || holding.lease().is_some() {
            continue;
        }

        // provider calls for earlier holdings take time, and a released holding records
        // this as when its resources were confirmed gone
        let at = crate::runtime::now().max(now);
        let reaped = reap(provider, holding, at, backoff).await?;
        store.put(&reaped.holding, at)?;
        errors.extend(reaped.errors);
        if reaped.holding.is_released() {
            released.push(reaped.holding.clone());
        }
        *holding = reaped.holding;
    }

    // requests run while the provider calls above are awaited and may have saved new
    // holdings, so the alarm is re-armed from storage rather than from this pass's copy
    let current = store.list::<K>()?;
    rearm(storage, current.iter().filter_map(Holding::wake), now).await?;

    Ok(Processed { released, errors })
}

/// Runs [`process_due`] and leaves a retry alarm behind if it fails
pub async fn run_alarm<K, P>(
    storage: &Storage,
    store: &HoldingStore,
    provider: &P,
    now: UnixMillis,
    backoff: &Backoff,
) -> CloudleaseResult<Processed<K>>
where
    K: ResourceKind + DeserializeOwned,
    P: CloudProvider<Kind = K>,
{
    let result = process_due(storage, store, provider, now, backoff).await;
    if result.is_err() {
        rearm_after_failure(storage, now).await?;
    }

    result
}

/// Answers the sweep's question about this owner's resources
///
/// An owner with no holdings claims nothing, so the sweep deletes its resources: this is
/// what removes resources whose owner object never existed.
pub async fn answer_sweep<K>(
    store: &HoldingStore,
    request: &mut Request,
    now: UnixMillis,
) -> worker::Result<Response>
where
    K: ResourceKind + DeserializeOwned,
{
    let query = match read_json::<OwnerQuery<K>>(request).await {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let holdings = match store.list::<K>() {
        Ok(holdings) => holdings,
        Err(error) => return HttpError::from(error).into_response(),
    };

    json_response(200, &answer(&holdings, &query, now))
}

fn is_due(wake: Option<Wake>, now: UnixMillis) -> bool {
    match wake {
        Some(Wake::Now) => true,
        Some(Wake::At(at)) => at <= now,
        None => false,
    }
}
