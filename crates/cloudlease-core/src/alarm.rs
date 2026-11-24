//! When a Durable Object alarm fires next and how retries back off

use std::time::Duration;

use crate::time::UnixMillis;

/// The delay before a failed alarm handler runs again
///
/// A handler that fails must leave an alarm behind, or an expired lease waits for the next
/// client request before anyone enforces it.
pub const FAILED_ALARM_RETRY: Duration = Duration::from_secs(5);

/// When an owner must next act
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wake {
    /// As soon as possible
    Now,
    /// At a stored deadline
    At(UnixMillis),
}

/// Exponential delays for cleanup retries
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Backoff {
    base: Duration,
    max: Duration,
}

impl Backoff {
    /// Creates a backoff that starts at `base` and doubles up to `max`, or `None` when
    /// `base` is zero or above `max`
    #[must_use]
    pub fn new(base: Duration, max: Duration) -> Option<Self> {
        (!base.is_zero() && base <= max).then_some(Self { base, max })
    }

    /// Returns the delay before retry number `attempt`, starting at one
    #[must_use]
    pub fn delay(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(31);

        self.base
            .checked_mul(1_u32 << exponent)
            .unwrap_or(self.max)
            .min(self.max)
    }
}

impl Default for Backoff {
    /// One second, doubling up to 15 minutes, as in rembuild
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            max: Duration::from_secs(15 * 60),
        }
    }
}

/// Returns the alarm time for the earliest wake, or `None` when nothing is pending
///
/// The result is always after `now`, because a Durable Object alarm set in the past fires at
/// once and a handler that keeps setting one would spin.
#[must_use]
pub fn next_alarm(wakes: impl IntoIterator<Item = Wake>, now: UnixMillis) -> Option<UnixMillis> {
    let earliest_allowed = now.saturating_add(Duration::from_millis(1));

    wakes
        .into_iter()
        .map(|wake| match wake {
            Wake::Now => earliest_allowed,
            Wake::At(at) => at.max(earliest_allowed),
        })
        .min()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Backoff, Wake, next_alarm};
    use crate::time::UnixMillis;

    fn at(millis: u64) -> UnixMillis {
        UnixMillis::new(millis).unwrap()
    }

    #[test]
    fn alarms_pick_the_earliest_wake_but_never_the_past() {
        let now = at(1_000);

        assert_eq!(next_alarm([], now), None);
        assert_eq!(
            next_alarm([Wake::At(at(5_000)), Wake::At(at(3_000))], now),
            Some(at(3_000))
        );
        assert_eq!(next_alarm([Wake::At(at(10))], now), Some(at(1_001)));
        assert_eq!(
            next_alarm([Wake::At(at(5_000)), Wake::Now], now),
            Some(at(1_001))
        );
    }

    #[test]
    fn retries_double_up_to_the_cap() {
        let backoff = Backoff::default();

        assert_eq!(backoff.delay(1), Duration::from_secs(1));
        assert_eq!(backoff.delay(2), Duration::from_secs(2));
        assert_eq!(backoff.delay(11), Duration::from_secs(15 * 60));
        assert_eq!(backoff.delay(u32::MAX), Duration::from_secs(15 * 60));
        assert!(Backoff::new(Duration::ZERO, Duration::from_secs(1)).is_none());
        assert!(Backoff::new(Duration::from_secs(2), Duration::from_secs(1)).is_none());
    }
}
