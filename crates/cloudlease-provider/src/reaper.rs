//! Drives a releasing holding through delete and confirm

use cloudlease_core::{
    alarm::Backoff,
    deletes::DeleteTarget,
    holding::{Holding, HoldingError},
    time::UnixMillis,
};

use crate::{
    error::ProviderError,
    provider::{CloudProvider, Presence},
};

/// The result of one cleanup pass
#[derive(Debug)]
pub struct Reaped<K> {
    /// The holding to persist
    pub holding: Holding<K>,
    /// Provider failures seen during the pass, already turned into a retry
    pub errors: Vec<ProviderError>,
}

/// Runs one cleanup pass over a holding
///
/// The pass deletes the targets of the lowest pending rank, confirms each one gone with a
/// read, and moves to the next rank only when every target of the current rank is gone. A
/// tagged target is first resolved by a tag search. When a target fails or is still present,
/// the pass stops and schedules a retry with `backoff`; the caller persists the returned
/// holding and sets its alarm. A holding that is not releasing is returned unchanged.
///
/// `clock` is read after each provider answer, since a released holding records when its
/// last resource was confirmed gone and spend is counted up to then: a time from before slow
/// delete and confirm calls would undercount it.
pub async fn reap<P: CloudProvider>(
    provider: &P,
    holding: &Holding<P::Kind>,
    clock: impl Fn() -> UnixMillis,
    backoff: &Backoff,
) -> Result<Reaped<P::Kind>, HoldingError> {
    let mut current = holding.clone();
    let mut errors = Vec::new();

    loop {
        let ready = current.ready_deletes();
        if ready.is_empty() {
            return Ok(Reaped {
                holding: current,
                errors,
            });
        }

        let mut unfinished = false;
        for target in ready {
            match step(provider, &current, &target, &clock).await {
                Ok(Some(next)) => current = next?,
                Ok(None) => unfinished = true,
                Err(error) => {
                    errors.push(error);
                    unfinished = true;
                }
            }
        }

        if unfinished {
            return Ok(Reaped {
                holding: current.retry_later(clock(), backoff)?,
                errors,
            });
        }
    }
}

/// Advances one target, returning `None` when it is still present
async fn step<P: CloudProvider>(
    provider: &P,
    holding: &Holding<P::Kind>,
    target: &DeleteTarget<P::Kind>,
    clock: &impl Fn() -> UnixMillis,
) -> Result<Option<Result<Holding<P::Kind>, HoldingError>>, ProviderError> {
    match target {
        DeleteTarget::Tagged { kind } => {
            let found = provider
                .find(holding.owner(), holding.generation(), *kind)
                .await?;

            Ok(holding.resolve_tagged(*kind, found, clock()).transpose())
        }
        DeleteTarget::Known { kind, id } => {
            provider.delete(*kind, id).await?;
            match provider.presence(*kind, id).await? {
                Presence::Absent => Ok(Some(holding.confirm_gone(target, clock()))),
                Presence::Present => Ok(None),
            }
        }
    }
}
