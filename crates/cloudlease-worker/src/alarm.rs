//! Re-arms a Durable Object alarm for its earliest deadline

use cloudlease_core::{
    alarm::{FAILED_ALARM_RETRY, Wake, next_alarm},
    time::UnixMillis,
};
use worker::{ScheduledTime, Storage, js_sys};

use crate::error::CloudleaseResult;

/// Sets the alarm for the earliest wake, or deletes it when nothing is pending
pub async fn rearm(
    storage: &Storage,
    wakes: impl IntoIterator<Item = Wake>,
    now: UnixMillis,
) -> CloudleaseResult<()> {
    match next_alarm(wakes, now) {
        Some(at) => set_at(storage, at).await,
        None => Ok(storage.delete_alarm().await?),
    }
}

/// Moves the alarm earlier when a wake is due before it, and otherwise leaves it as it is
///
/// For a change that can only add deadlines, such as saving one holding: the owner may have
/// wakes of its own beyond its holdings, such as a spend check, which a full [`rearm`] from
/// holdings alone would push back. An alarm left earlier than needed only runs a pass that
/// re-arms from everything the owner knows.
pub async fn pull_earlier(
    storage: &Storage,
    wakes: impl IntoIterator<Item = Wake>,
    now: UnixMillis,
) -> CloudleaseResult<()> {
    let Some(at) = next_alarm(wakes, now) else {
        return Ok(());
    };
    let current = storage.get_alarm().await?;
    if current.is_some_and(|current| current <= i64::try_from(at.as_millis()).unwrap_or(i64::MAX)) {
        return Ok(());
    }

    set_at(storage, at).await
}

/// Sets the alarm a few seconds out after a handler failed
///
/// A failing alarm handler must leave an alarm behind, or nothing enforces an expired lease
/// until the next request reaches the object.
pub async fn rearm_after_failure(storage: &Storage, now: UnixMillis) -> CloudleaseResult<()> {
    set_at(storage, now.saturating_add(FAILED_ALARM_RETRY)).await
}

async fn set_at(storage: &Storage, at: UnixMillis) -> CloudleaseResult<()> {
    #[allow(clippy::cast_precision_loss)]
    // UnixMillis stays within the safe integer range, which f64 holds exactly
    let date = js_sys::Date::new(&js_sys::Number::from(at.as_millis() as f64));

    Ok(storage.set_alarm(ScheduledTime::new(date)).await?)
}
