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
    alarm::{rearm, rearm_after_failure},
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

/// Saves a holding and re-arms the alarm for the earliest deadline of all holdings
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

    rearm(storage, holdings.iter().filter_map(Holding::wake), now).await
}

/// Creates one resource for a leased generation, saving the intent before the provider call
///
/// The `Requested` slot is saved first, so a sweep that runs while the create is in flight,
/// or after this object dies before recording the ID, finds the resource claimed by a live
/// lease. If the ID is never recorded, the lease's expiry turns the slot into a delete by
/// tags. The resource's ceiling tag is the lease's hard deadline.
///
/// The holding is read again after the provider call, since an alarm may have ended the
/// lease meanwhile; the ID is then not recorded, the holding error is returned, and cleanup
/// finds the resource by its tags. A provider error leaves the slot requested for the same
/// reason.
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

    let created = load(store, generation)?.record_created(kind, id.clone())?;
    save(storage, store, &created, now).await?;

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
    let mut holdings = store.list::<K>()?;
    for holding in &mut holdings {
        if let Some(expired) = holding.expire(now) {
            store.put(&expired, now)?;
            *holding = expired;
        }
    }

    let mut released = Vec::new();
    let mut errors = Vec::new();
    for holding in &mut holdings {
        if !is_due(holding.wake(), now) || holding.lease().is_some() {
            continue;
        }

        let reaped = reap(provider, holding, now, backoff).await?;
        store.put(&reaped.holding, now)?;
        errors.extend(reaped.errors);
        if reaped.holding.is_released() {
            released.push(reaped.holding.clone());
        }
        *holding = reaped.holding;
    }

    rearm(storage, holdings.iter().filter_map(Holding::wake), now).await?;

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
