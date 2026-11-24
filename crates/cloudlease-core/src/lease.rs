//! A client lease with a heartbeat deadline and a hard deadline
//!
//! A lease ends when the client misses a heartbeat or when its hard deadline passes,
//! whichever comes first. Heartbeats move the heartbeat deadline forward but never past the
//! hard deadline, so no client can keep a resource beyond the policy's maximum duration.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ids::LeaseId, time::UnixMillis};

/// Limits applied to every lease an owner grants
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeasePolicy {
    heartbeat_timeout: Duration,
    boot_grace: Duration,
    max_duration: Duration,
}

impl LeasePolicy {
    /// Creates a policy where a lease without a heartbeat for `heartbeat_timeout` expires and
    /// no lease lasts longer than `max_duration`
    pub fn new(heartbeat_timeout: Duration, max_duration: Duration) -> Result<Self, LeaseError> {
        if heartbeat_timeout.is_zero() || heartbeat_timeout > max_duration {
            return Err(LeaseError::InvalidPolicy);
        }

        Ok(Self {
            heartbeat_timeout,
            boot_grace: heartbeat_timeout,
            max_duration,
        })
    }

    /// Allows a longer wait for the first heartbeat, while a machine boots
    pub fn with_boot_grace(self, boot_grace: Duration) -> Result<Self, LeaseError> {
        if boot_grace.is_zero() || boot_grace > self.max_duration {
            return Err(LeaseError::InvalidPolicy);
        }

        Ok(Self { boot_grace, ..self })
    }

    /// Returns the longest allowed gap between heartbeats
    #[must_use]
    pub const fn heartbeat_timeout(&self) -> Duration {
        self.heartbeat_timeout
    }

    /// Returns the longest wait for the first heartbeat
    #[must_use]
    pub const fn boot_grace(&self) -> Duration {
        self.boot_grace
    }

    /// Returns the longest allowed lease
    #[must_use]
    pub const fn max_duration(&self) -> Duration {
        self.max_duration
    }
}

/// Why a lease ended without a release
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Expiry {
    /// The client stopped sending heartbeats
    HeartbeatMissed,
    /// The lease reached its maximum duration
    HardDeadline,
}

/// A rejected lease operation
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LeaseError {
    /// The heartbeat timeout is zero or longer than the maximum duration
    #[error("the heartbeat timeout must be positive and no longer than the maximum duration")]
    InvalidPolicy,
    /// The request names a different lease
    #[error("the request names a different lease")]
    WrongLease,
    /// The heartbeat sequence did not increase
    #[error("heartbeat sequence numbers must increase")]
    StaleSequence,
    /// The request time is before the last accepted heartbeat
    #[error("the request time is before the last accepted heartbeat")]
    TimeWentBackwards,
    /// The lease has already ended
    #[error("the lease has expired: {0:?}")]
    Expired(Expiry),
    /// A deadline does not fit the safe integer range
    #[error("a lease deadline is out of range")]
    OutOfRange,
    /// Persisted lease fields contradict each other
    #[error("stored lease fields are inconsistent")]
    Inconsistent,
}

/// A live or ended client lease
///
/// The fields are private so every value satisfies
/// `started_at <= last_heartbeat_at` and `heartbeat_due_at <= hard_deadline`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    id: LeaseId,
    started_at: UnixMillis,
    last_heartbeat_at: UnixMillis,
    last_sequence: u64,
    heartbeat_due_at: UnixMillis,
    hard_deadline: UnixMillis,
}

impl Lease {
    /// Starts a lease at `now`
    ///
    /// The hard deadline is `now` plus the policy's maximum duration, or `not_after` when that
    /// is earlier, such as the deadline of a batch run the lease belongs to.
    pub fn start(
        id: LeaseId,
        policy: &LeasePolicy,
        now: UnixMillis,
        not_after: Option<UnixMillis>,
    ) -> Result<Self, LeaseError> {
        let longest = now
            .checked_add(policy.max_duration)
            .ok_or(LeaseError::OutOfRange)?;
        let hard_deadline = not_after.map_or(longest, |not_after| not_after.min(longest));
        if hard_deadline <= now {
            return Err(LeaseError::Expired(Expiry::HardDeadline));
        }

        let heartbeat_due_at = now
            .checked_add(policy.boot_grace)
            .ok_or(LeaseError::OutOfRange)?
            .min(hard_deadline);

        Ok(Self {
            id,
            started_at: now,
            last_heartbeat_at: now,
            last_sequence: 0,
            heartbeat_due_at,
            hard_deadline,
        })
    }

    /// Returns the lease ID
    #[must_use]
    pub const fn id(&self) -> &LeaseId {
        &self.id
    }

    /// Returns the start time
    #[must_use]
    pub const fn started_at(&self) -> UnixMillis {
        self.started_at
    }

    /// Returns the time of the last accepted heartbeat
    #[must_use]
    pub const fn last_heartbeat_at(&self) -> UnixMillis {
        self.last_heartbeat_at
    }

    /// Returns the last accepted heartbeat sequence number, zero before the first heartbeat
    #[must_use]
    pub const fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Returns the time by which the next heartbeat must arrive
    #[must_use]
    pub const fn heartbeat_due_at(&self) -> UnixMillis {
        self.heartbeat_due_at
    }

    /// Returns the time the lease ends regardless of heartbeats
    #[must_use]
    pub const fn hard_deadline(&self) -> UnixMillis {
        self.hard_deadline
    }

    /// Returns the earliest time the lease can expire, which is when its owner must check it
    #[must_use]
    pub const fn next_deadline(&self) -> UnixMillis {
        // the constructor keeps the heartbeat deadline at or before the hard deadline
        self.heartbeat_due_at
    }

    /// Reports whether and why the lease has ended at `now`
    #[must_use]
    pub fn expiry(&self, now: UnixMillis) -> Option<Expiry> {
        if now >= self.hard_deadline {
            return Some(Expiry::HardDeadline);
        }
        if now >= self.heartbeat_due_at {
            return Some(Expiry::HeartbeatMissed);
        }

        None
    }

    /// Checks that `id` names this lease and that it is still live at `now`
    pub fn ensure_live(&self, id: &LeaseId, now: UnixMillis) -> Result<(), LeaseError> {
        if id != &self.id {
            return Err(LeaseError::WrongLease);
        }
        if now < self.last_heartbeat_at {
            return Err(LeaseError::TimeWentBackwards);
        }
        if let Some(expiry) = self.expiry(now) {
            return Err(LeaseError::Expired(expiry));
        }

        Ok(())
    }

    /// Accepts a heartbeat with a sequence number greater than the last one
    pub fn heartbeat(
        &self,
        id: &LeaseId,
        sequence: u64,
        now: UnixMillis,
        policy: &LeasePolicy,
    ) -> Result<Self, LeaseError> {
        self.ensure_live(id, now)?;
        if sequence <= self.last_sequence {
            return Err(LeaseError::StaleSequence);
        }

        let heartbeat_due_at = now
            .checked_add(policy.heartbeat_timeout)
            .ok_or(LeaseError::OutOfRange)?
            .min(self.hard_deadline);

        Ok(Self {
            last_heartbeat_at: now,
            last_sequence: sequence,
            heartbeat_due_at,
            ..self.clone()
        })
    }

    /// Accepts a heartbeat from a client that sends no sequence numbers
    ///
    /// It counts as the next sequence number and sets the deadline to `now` plus the
    /// heartbeat timeout, never past the hard deadline. The boot grace covers only the time
    /// before the first heartbeat, so a first renew can bring the deadline earlier.
    pub fn renew(
        &self,
        id: &LeaseId,
        now: UnixMillis,
        policy: &LeasePolicy,
    ) -> Result<Self, LeaseError> {
        let sequence = self
            .last_sequence
            .checked_add(1)
            .ok_or(LeaseError::OutOfRange)?;

        self.heartbeat(id, sequence, now, policy)
    }
}

/// The stored fields of a [`Lease`], for apps that keep a lease in their own record shape
///
/// [`Lease::try_from`] checks the same invariants as deserializing a lease, so a record
/// written by an app under its own field names cannot produce an inconsistent lease.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LeaseParts {
    /// The lease identifier
    pub id: LeaseId,
    /// When the lease started
    pub started_at: UnixMillis,
    /// When the last heartbeat was accepted, or the start time
    pub last_heartbeat_at: UnixMillis,
    /// The last accepted heartbeat sequence number, or zero
    pub last_sequence: u64,
    /// When the lease expires without another heartbeat
    pub heartbeat_due_at: UnixMillis,
    /// When the lease expires regardless of heartbeats
    pub hard_deadline: UnixMillis,
}

impl From<&Lease> for LeaseParts {
    fn from(lease: &Lease) -> Self {
        Self {
            id: lease.id.clone(),
            started_at: lease.started_at,
            last_heartbeat_at: lease.last_heartbeat_at,
            last_sequence: lease.last_sequence,
            heartbeat_due_at: lease.heartbeat_due_at,
            hard_deadline: lease.hard_deadline,
        }
    }
}

impl TryFrom<LeaseParts> for Lease {
    type Error = LeaseError;

    fn try_from(wire: LeaseParts) -> Result<Self, Self::Error> {
        if wire.started_at > wire.last_heartbeat_at || wire.heartbeat_due_at > wire.hard_deadline {
            return Err(LeaseError::Inconsistent);
        }

        Ok(Self {
            id: wire.id,
            started_at: wire.started_at,
            last_heartbeat_at: wire.last_heartbeat_at,
            last_sequence: wire.last_sequence,
            heartbeat_due_at: wire.heartbeat_due_at,
            hard_deadline: wire.hard_deadline,
        })
    }
}

impl<'de> Deserialize<'de> for Lease {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::try_from(LeaseParts::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Expiry, Lease, LeaseError, LeaseParts, LeasePolicy};
    use crate::{ids::LeaseId, time::UnixMillis};

    const MINUTE: Duration = Duration::from_secs(60);

    fn at(millis: u64) -> UnixMillis {
        UnixMillis::new(millis).unwrap()
    }

    fn policy() -> LeasePolicy {
        LeasePolicy::new(2 * MINUTE, 240 * MINUTE).unwrap()
    }

    fn lease_id() -> LeaseId {
        LeaseId::parse("lease_1").unwrap()
    }

    fn start() -> Lease {
        Lease::start(lease_id(), &policy(), at(1_000_000), None).unwrap()
    }

    #[test]
    fn a_lease_expires_at_its_heartbeat_or_hard_deadline() {
        let lease = start();

        assert_eq!(lease.expiry(at(1_000_000)), None);
        assert_eq!(lease.expiry(at(1_119_999)), None);
        assert_eq!(lease.expiry(at(1_120_000)), Some(Expiry::HeartbeatMissed));

        // heartbeats extend the heartbeat deadline but never past the hard deadline
        let mut lease = lease;
        let mut now = at(1_000_000);
        for sequence in 1.. {
            now = now.checked_add(MINUTE).unwrap();
            if now >= lease.hard_deadline() {
                break;
            }
            lease = lease
                .heartbeat(&lease_id(), sequence, now, &policy())
                .unwrap();
        }

        assert_eq!(lease.heartbeat_due_at(), lease.hard_deadline());
        assert_eq!(
            lease.expiry(lease.hard_deadline()),
            Some(Expiry::HardDeadline)
        );
        assert_eq!(
            lease.heartbeat(&lease_id(), u64::MAX, lease.hard_deadline(), &policy()),
            Err(LeaseError::Expired(Expiry::HardDeadline))
        );
    }

    #[test]
    fn accepts_only_newer_heartbeats_from_the_same_lease() {
        let first = start()
            .heartbeat(&lease_id(), 1, at(1_030_000), &policy())
            .unwrap();
        let second = first
            .heartbeat(&lease_id(), 2, at(1_031_000), &policy())
            .unwrap();

        assert_eq!(second.last_sequence(), 2);
        assert!(second.heartbeat_due_at() >= first.heartbeat_due_at());
        assert_eq!(
            second.heartbeat(&lease_id(), 2, at(1_032_000), &policy()),
            Err(LeaseError::StaleSequence)
        );
        assert_eq!(
            second.heartbeat(
                &LeaseId::parse("lease_stale").unwrap(),
                3,
                at(1_032_000),
                &policy()
            ),
            Err(LeaseError::WrongLease)
        );
        assert_eq!(
            second.heartbeat(&lease_id(), 3, at(1_030_500), &policy()),
            Err(LeaseError::TimeWentBackwards)
        );

        // a heartbeat at the deadline is late, not a renewal
        assert_eq!(
            second.heartbeat(&lease_id(), 3, second.heartbeat_due_at(), &policy()),
            Err(LeaseError::Expired(Expiry::HeartbeatMissed))
        );
    }

    #[test]
    fn a_boot_grace_covers_only_the_first_heartbeat_and_a_cap_shortens_the_lease() {
        let policy = policy().with_boot_grace(15 * MINUTE).unwrap();
        let lease = Lease::start(lease_id(), &policy, at(0), None).unwrap();
        assert_eq!(lease.heartbeat_due_at(), at(900_000));

        let renewed = lease
            .heartbeat(&lease_id(), 1, at(60_000), &policy)
            .unwrap();
        assert_eq!(renewed.heartbeat_due_at(), at(180_000));

        let capped = Lease::start(lease_id(), &policy, at(0), Some(at(600_000))).unwrap();
        assert_eq!(capped.hard_deadline(), at(600_000));
        assert_eq!(capped.heartbeat_due_at(), at(600_000));
        assert_eq!(
            Lease::start(lease_id(), &policy, at(600_000), Some(at(600_000))),
            Err(LeaseError::Expired(Expiry::HardDeadline))
        );
        assert_eq!(
            policy.with_boot_grace(241 * MINUTE),
            Err(LeaseError::InvalidPolicy)
        );
    }

    #[test]
    fn a_renew_extends_from_now_up_to_the_hard_deadline() {
        let policy = policy().with_boot_grace(15 * MINUTE).unwrap();
        // the run deadline caps the lease, and with it the boot grace
        let lease = Lease::start(lease_id(), &policy, at(1_000_000), Some(at(1_200_000))).unwrap();
        assert_eq!(lease.hard_deadline(), at(1_200_000));
        assert_eq!(lease.heartbeat_due_at(), at(1_200_000));

        let renewed = lease.renew(&lease_id(), at(1_050_000), &policy).unwrap();
        assert_eq!(renewed.heartbeat_due_at(), at(1_170_000));
        assert_eq!(renewed.last_sequence(), 1);

        let capped = renewed.renew(&lease_id(), at(1_100_000), &policy).unwrap();
        assert_eq!(capped.heartbeat_due_at(), at(1_200_000));
        assert_eq!(capped.last_sequence(), 2);
        assert_eq!(
            capped.renew(&lease_id(), at(1_200_000), &policy),
            Err(LeaseError::Expired(Expiry::HardDeadline))
        );
        assert_eq!(
            lease.renew(&LeaseId::parse("lease_2").unwrap(), at(1_050_000), &policy),
            Err(LeaseError::WrongLease)
        );
    }

    #[test]
    fn policies_and_stored_leases_reject_contradictions() {
        assert_eq!(
            LeasePolicy::new(Duration::ZERO, MINUTE),
            Err(LeaseError::InvalidPolicy)
        );
        assert_eq!(
            LeasePolicy::new(2 * MINUTE, MINUTE),
            Err(LeaseError::InvalidPolicy)
        );

        let lease = start();
        let json = serde_json::to_value(&lease).unwrap();
        assert_eq!(
            serde_json::from_value::<Lease>(json.clone()).unwrap(),
            lease
        );

        let mut broken = json;
        broken["heartbeatDueAt"] = (lease.hard_deadline().as_millis() + 1).into();
        assert!(serde_json::from_value::<Lease>(broken).is_err());

        // an app's own record converts through the same checks
        let parts = LeaseParts::from(&lease);
        assert_eq!(Lease::try_from(parts.clone()), Ok(lease.clone()));
        let backwards = LeaseParts {
            last_heartbeat_at: at(0),
            started_at: at(1),
            ..parts
        };
        assert_eq!(Lease::try_from(backwards), Err(LeaseError::Inconsistent));
    }
}
