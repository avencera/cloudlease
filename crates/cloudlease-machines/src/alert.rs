//! What the Worker alerts on, and how often
//!
//! Alerts go to an ntfy topic. Each one is a cap hit, a failed create, a sweep deleting an
//! orphan, or a failure that keeps happening:
//!
//! - a run reaching its spend cap, once per run, when its machines are released
//! - a create refused by the hourly spend cap, on each refusal (the client ends its pool on
//!   the first one, so these do not repeat)
//! - a provider create failure, once per run
//! - a sweep deleting resources that no owner was releasing, on each sweep that does
//! - an owner object that fails to answer the sweep, or a sweep that fails outright, on the
//!   third consecutive failure (15 minutes at a 5 minute cadence) and hourly after that

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Consecutive failures before the first alert
pub const ALERT_AFTER: u32 = 3;

/// Consecutive failures between repeated alerts after the first
pub const REALERT_EVERY: u32 = 12;

/// The streak key of a sweep that failed before it could ask any owner
pub const SWEEP_KEY: &str = "sweep";

/// The streak key prefix of a provider the sweep could not list, followed by its name
pub const UNLISTED_KEY_PREFIX: &str = "unlisted:";

/// How urgent an alert is, as ntfy priorities
///
/// There is no priority 5: the alerts topic may be one that a Superalert Mac app follows, and
/// it sounds its alarm for every priority 5 message. cloudlease alerts are notifications.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Priority {
    /// ntfy priority 3, for something to look at
    Default,
    /// ntfy priority 4, for a cap that stopped machines or a sweep that keeps failing
    High,
}

impl Priority {
    /// Returns the ntfy `Priority` header value
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Default => "3",
            Self::High => "4",
        }
    }
}

/// One notification
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Alert {
    /// The title
    pub title: String,
    /// The body
    pub message: String,
    /// The urgency
    pub priority: Priority,
}

/// Consecutive failure counts per key, such as an owner ID or [`SWEEP_KEY`]
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Streaks(BTreeMap<String, u32>);

impl Streaks {
    /// Rebuilds streaks from stored counts
    #[must_use]
    pub const fn from_counts(counts: BTreeMap<String, u32>) -> Self {
        Self(counts)
    }

    /// Returns the stored counts
    #[must_use]
    pub const fn counts(&self) -> &BTreeMap<String, u32> {
        &self.0
    }

    /// Records one sweep: the keys in `failing` failed again, every other key succeeded
    ///
    /// Returns the new streaks and the keys due an alert, with their counts.
    #[must_use]
    pub fn record(&self, failing: &BTreeSet<String>) -> (Self, Vec<(String, u32)>) {
        let next: BTreeMap<String, u32> = failing
            .iter()
            .map(|key| {
                let count = self.0.get(key).copied().unwrap_or(0).saturating_add(1);
                (key.clone(), count)
            })
            .collect();
        let due = next
            .iter()
            .filter(|(_, count)| is_due(**count))
            .map(|(key, count)| (key.clone(), *count))
            .collect();

        (Self(next), due)
    }
}

const fn is_due(count: u32) -> bool {
    count == ALERT_AFTER
        || (count > ALERT_AFTER && (count - ALERT_AFTER).is_multiple_of(REALERT_EVERY))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::Streaks;

    fn keys(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn a_failure_alerts_on_its_third_sweep_in_a_row_then_hourly() {
        let mut streaks = Streaks::default();
        let mut alerted_at = Vec::new();
        for sweep in 1..=28 {
            // run_b fails once in the middle, which never alerts
            let failing = if sweep == 10 {
                keys(&["run_a", "run_b"])
            } else {
                keys(&["run_a"])
            };
            let (next, due) = streaks.record(&failing);
            alerted_at.extend(due.into_iter().map(|(key, count)| (sweep, key, count)));
            streaks = next;
        }

        assert_eq!(
            alerted_at,
            vec![
                (3, "run_a".to_owned(), 3),
                (15, "run_a".to_owned(), 15),
                (27, "run_a".to_owned(), 27),
            ]
        );

        // one good sweep resets the streak
        let (reset, due) = streaks.record(&BTreeSet::new());
        assert!(due.is_empty());
        assert!(reset.counts().is_empty());
    }
}
