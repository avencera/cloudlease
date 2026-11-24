//! Global capacity permits that stay counted until absence is proven
//!
//! An owner acquires a permit before it creates resources and releases it only with proof
//! that they are gone. Each permit takes [`PermitUnits`] of the limit: one for a gate that
//! counts machines, or a price for a gate that caps hourly spend. A permit whose time-to-live passes turns [`PermitStatus::Suspect`]
//! rather than disappearing: its owner may have crashed with resources still running, so the
//! permit keeps counting against the limit until a cleanup proves absence.

use std::{num::NonZeroU16, time::Duration};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use thiserror::Error;

use crate::{
    ids::{Generation, OwnerId, PermitId},
    time::UnixMillis,
};

/// A rejected capacity value
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CapacityError {
    /// The limit is zero, above the maximum, or not an integer
    #[error("the capacity limit must be an integer from 1 through {max}")]
    InvalidLimit {
        /// The largest accepted limit
        max: u16,
    },
    /// The permit lifetime is out of range
    #[error("the permit lifetime must be positive and at most the maximum")]
    InvalidTtl,
    /// The permit weight is zero or out of range
    #[error("permit units must be an integer from 1 through 65535")]
    InvalidUnits,
}

/// The number of permits that may be held at once
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CapacityLimit(NonZeroU16);

impl CapacityLimit {
    /// Creates a limit from 1 through `max`
    pub fn new(value: u16, max: u16) -> Result<Self, CapacityError> {
        NonZeroU16::new(value)
            .filter(|value| value.get() <= max)
            .map(Self)
            .ok_or(CapacityError::InvalidLimit { max })
    }

    /// Parses a configured limit, using `default` when none is configured
    pub fn parse_configured(
        value: Option<&str>,
        default: Self,
        max: u16,
    ) -> Result<Self, CapacityError> {
        let Some(value) = value else {
            return Ok(default);
        };

        let parsed = value
            .trim()
            .parse::<u16>()
            .map_err(|_| CapacityError::InvalidLimit { max })?;

        Self::new(parsed, max)
    }

    /// Returns the numeric limit
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

/// Whether a permit is within its lifetime
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermitStatus {
    /// The owner is expected to be working within the permit's lifetime
    Active,
    /// The lifetime passed without a release, so resources may have leaked
    Suspect,
}

impl PermitStatus {
    /// Returns the stored name
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspect => "suspect",
        }
    }
}

/// The owner and generation a permit covers, unique among held permits
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermitKey {
    /// The owner that acquired the permit
    pub owner_id: OwnerId,
    /// The generation the permit covers
    pub generation: Generation,
}

/// How much of the limit one permit takes, one by default
///
/// A gate that counts machines leaves every permit at one unit. A gate that caps spend
/// weighs each permit by its resource's price, such as cents per hour, so the limit bounds
/// the hourly cost of everything not yet proven gone.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PermitUnits(NonZeroU16);

impl PermitUnits {
    /// One unit, the weight of a permit that counts one machine
    pub const ONE: Self = Self(NonZeroU16::MIN);

    /// Creates a positive weight
    pub fn new(value: u16) -> Result<Self, CapacityError> {
        NonZeroU16::new(value)
            .map(Self)
            .ok_or(CapacityError::InvalidUnits)
    }

    /// Returns the weight
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl Default for PermitUnits {
    fn default() -> Self {
        Self::ONE
    }
}

impl<'de> Deserialize<'de> for PermitUnits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u16::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// One held share of capacity
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Permit {
    /// The permit ID, required to release it
    pub permit_id: PermitId,
    /// The owner that acquired the permit
    pub owner_id: OwnerId,
    /// The generation the permit covers
    pub generation: Generation,
    /// How much of the limit the permit takes
    #[serde(default)]
    pub units: PermitUnits,
    /// When the permit turns suspect unless released
    pub expires_at: UnixMillis,
    /// Whether the permit is within its lifetime
    pub status: PermitStatus,
}

impl Permit {
    /// Grants a new active permit
    pub fn grant(
        permit_id: PermitId,
        key: PermitKey,
        units: PermitUnits,
        now: UnixMillis,
        ttl: PermitTtl,
    ) -> Result<Self, CapacityError> {
        Ok(Self {
            permit_id,
            owner_id: key.owner_id,
            generation: key.generation,
            units,
            expires_at: now.checked_add(ttl.0).ok_or(CapacityError::InvalidTtl)?,
            status: PermitStatus::Active,
        })
    }

    /// Returns the owner and generation the permit covers
    #[must_use]
    pub fn key(&self) -> PermitKey {
        PermitKey {
            owner_id: self.owner_id.clone(),
            generation: self.generation,
        }
    }

    /// Returns the permit as it stands at `now`: suspect once its lifetime has passed
    #[must_use]
    pub fn at(&self, now: UnixMillis) -> Self {
        let status = if now >= self.expires_at {
            PermitStatus::Suspect
        } else {
            self.status
        };

        Self {
            status,
            ..self.clone()
        }
    }
}

/// How long a permit stays active without a release
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermitTtl(Duration);

impl PermitTtl {
    /// Creates a lifetime from 1 millisecond through `max`
    pub fn new(ttl: Duration, max: Duration) -> Result<Self, CapacityError> {
        if ttl.is_zero() || ttl > max {
            return Err(CapacityError::InvalidTtl);
        }

        Ok(Self(ttl))
    }

    /// Returns the lifetime
    #[must_use]
    pub const fn get(self) -> Duration {
        self.0
    }
}

/// What an acquire request should do
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcquireDecision<'a> {
    /// The owner already holds a permit for this generation, so a retried acquire returns it
    Existing(&'a Permit),
    /// Grant a new permit
    Grant,
    /// The new permit's units would take the held units past the limit, counting active and
    /// suspect permits
    Exhausted,
}

/// Decides an acquire from the owner's existing permit, the units already held and the
/// units the new permit would take
#[must_use]
pub fn decide_acquire(
    existing: Option<&Permit>,
    held_units: u64,
    units: PermitUnits,
    limit: CapacityLimit,
) -> AcquireDecision<'_> {
    if let Some(permit) = existing {
        return AcquireDecision::Existing(permit);
    }
    if held_units.saturating_add(u64::from(units.get())) > u64::from(limit.get()) {
        return AcquireDecision::Exhausted;
    }

    AcquireDecision::Grant
}

/// A request for a permit
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcquireRequest {
    /// The owner acquiring the permit
    pub owner_id: OwnerId,
    /// The generation the permit covers
    pub generation: Generation,
    /// The requested lifetime in milliseconds
    pub ttl_ms: u64,
    /// How much of the limit the permit takes, one when absent
    #[serde(default)]
    pub units: PermitUnits,
}

impl AcquireRequest {
    /// Returns the owner and generation the request covers
    #[must_use]
    pub fn key(&self) -> PermitKey {
        PermitKey {
            owner_id: self.owner_id.clone(),
            generation: self.generation,
        }
    }
}

/// A request to release a permit, which must state that its resources are absent
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseRequest {
    /// The permit to release
    pub permit_id: PermitId,
    /// The owner that holds it
    pub owner_id: OwnerId,
    /// The generation it covers
    pub generation: Generation,
    /// The caller's statement that every resource of this generation is confirmed gone
    pub owner_absent: AbsenceProof,
}

/// A statement that an owner's resources are confirmed gone
///
/// It deserializes only from `true`, so a release request without it fails to parse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AbsenceProof;

impl Serialize for AbsenceProof {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for AbsenceProof {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        if bool::deserialize(deserializer)? {
            return Ok(Self);
        }

        Err(D::Error::custom(
            "a permit is released only after its resources are absent",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::{
        AcquireDecision, AcquireRequest, CapacityError, CapacityLimit, Permit, PermitKey,
        PermitStatus, PermitTtl, PermitUnits, ReleaseRequest, decide_acquire,
    };
    use crate::{
        ids::{Generation, OwnerId, PermitId},
        time::UnixMillis,
    };

    fn key() -> PermitKey {
        PermitKey {
            owner_id: OwnerId::parse("job_1").unwrap(),
            generation: Generation::FIRST,
        }
    }

    #[test]
    fn expired_permits_turn_suspect_and_keep_counting() {
        let ttl = PermitTtl::new(Duration::from_secs(60), Duration::from_secs(3_600)).unwrap();
        let now = UnixMillis::new(1_000).unwrap();
        let one = PermitUnits::ONE;
        let permit =
            Permit::grant(PermitId::parse("cap_1").unwrap(), key(), one, now, ttl).unwrap();
        let limit = CapacityLimit::new(1, 100).unwrap();

        assert_eq!(permit.at(now).status, PermitStatus::Active);
        let expired = permit.at(UnixMillis::new(61_000).unwrap());
        assert_eq!(expired.status, PermitStatus::Suspect);

        // a suspect permit is still held, so it blocks new owners and is returned on retry
        assert_eq!(
            decide_acquire(None, 1, one, limit),
            AcquireDecision::Exhausted
        );
        assert_eq!(
            decide_acquire(Some(&expired), 1, one, limit),
            AcquireDecision::Existing(&expired)
        );
        assert_eq!(decide_acquire(None, 0, one, limit), AcquireDecision::Grant);
    }

    #[test]
    fn weighted_permits_fill_the_limit_by_their_units() {
        // a spend gate in cents per hour: $15/h, with machines at $4.76/h each
        let limit = CapacityLimit::new(1_500, u16::MAX).unwrap();
        let machine = PermitUnits::new(476).unwrap();

        assert_eq!(
            decide_acquire(None, 952, machine, limit),
            AcquireDecision::Grant
        );
        assert_eq!(
            decide_acquire(None, 1_428, machine, limit),
            AcquireDecision::Exhausted
        );
        // a permit heavier than the whole limit can never be granted
        assert_eq!(
            decide_acquire(None, 0, PermitUnits::new(1_501).unwrap(), limit),
            AcquireDecision::Exhausted
        );
        assert_eq!(PermitUnits::new(0), Err(CapacityError::InvalidUnits));

        // requests and stored permits without units weigh one, as before weights existed
        let request: AcquireRequest =
            serde_json::from_value(json!({ "ownerId": "job_1", "generation": 1, "ttlMs": 1 }))
                .unwrap();
        assert_eq!(request.units, PermitUnits::ONE);
        assert!(
            serde_json::from_value::<AcquireRequest>(
                json!({ "ownerId": "job_1", "generation": 1, "ttlMs": 1, "units": 0 })
            )
            .is_err()
        );
    }

    #[test]
    fn limits_default_and_reject_out_of_range_values() {
        let default = CapacityLimit::new(2, 100).unwrap();

        assert_eq!(
            CapacityLimit::parse_configured(None, default, 100),
            Ok(default)
        );
        assert_eq!(
            CapacityLimit::parse_configured(Some(" 100 "), default, 100)
                .unwrap()
                .get(),
            100
        );
        for invalid in ["0", "101", "1.5", "-1", "ten"] {
            assert!(CapacityLimit::parse_configured(Some(invalid), default, 100).is_err());
        }
        assert!(PermitTtl::new(Duration::ZERO, Duration::from_secs(1)).is_err());
        assert!(PermitTtl::new(Duration::from_secs(2), Duration::from_secs(1)).is_err());
    }

    #[test]
    fn release_requires_proof_that_resources_are_absent() {
        let mut body = json!({
            "permitId": "cap_1",
            "ownerId": "job_1",
            "generation": 1,
            "ownerAbsent": true,
        });
        assert!(serde_json::from_value::<ReleaseRequest>(body.clone()).is_ok());

        body["ownerAbsent"] = json!(false);
        assert!(serde_json::from_value::<ReleaseRequest>(body.clone()).is_err());

        body.as_object_mut().unwrap().remove("ownerAbsent");
        assert!(serde_json::from_value::<ReleaseRequest>(body).is_err());
    }
}
