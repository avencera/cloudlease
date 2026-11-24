//! Unix time in milliseconds supplied by the caller

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

/// The largest integer a JavaScript number holds exactly
///
/// Times and generations cross JSON and Durable Object SQLite boundaries, so values above this
/// would silently lose precision on the JavaScript side.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// A point in time as milliseconds since the Unix epoch
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct UnixMillis(u64);

impl UnixMillis {
    /// The Unix epoch
    pub const EPOCH: Self = Self(0);

    /// Creates a time from milliseconds, or `None` above [`MAX_SAFE_INTEGER`]
    #[must_use]
    pub const fn new(millis: u64) -> Option<Self> {
        if millis > MAX_SAFE_INTEGER {
            return None;
        }

        Some(Self(millis))
    }

    /// Returns the milliseconds since the Unix epoch
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// Adds a duration, or returns `None` when the result is not a safe integer
    #[must_use]
    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        let millis = u64::try_from(duration.as_millis()).ok()?;

        Self::new(self.0.checked_add(millis)?)
    }

    /// Adds a duration and clamps the result to [`MAX_SAFE_INTEGER`]
    #[must_use]
    pub fn saturating_add(self, duration: Duration) -> Self {
        self.checked_add(duration).unwrap_or(Self(MAX_SAFE_INTEGER))
    }
}

impl<'de> Deserialize<'de> for UnixMillis {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = u64::deserialize(deserializer)?;

        Self::new(millis).ok_or_else(|| D::Error::custom("time exceeds the safe integer range"))
    }
}
