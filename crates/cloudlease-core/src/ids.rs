//! Validated identifiers shared by leases, tags, permits and providers

use std::{fmt, num::NonZeroU64};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use thiserror::Error;

use crate::time::MAX_SAFE_INTEGER;

/// An identifier that failed validation
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{kind} is invalid: {rule}")]
pub struct IdError {
    /// The identifier type
    pub kind: &'static str,
    /// The rule the value broke
    pub rule: &'static str,
}

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident, $rule:literal, $valid:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps a value
            pub fn parse(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                let valid: fn(&str) -> bool = $valid;
                if !valid(&value) {
                    return Err(IdError {
                        kind: stringify!($name),
                        rule: $rule,
                    });
                }

                Ok(Self(value))
            }

            /// Returns the validated value
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }
    };
}

fn bounded(value: &str, max: usize, allowed: impl Fn(char) -> bool) -> bool {
    !value.is_empty() && value.len() <= max && value.chars().all(allowed)
}

const fn is_word(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '-'
}

string_id!(
    /// The app-specific prefix that every managed tag starts with, such as `cl` or `rb`
    ///
    /// The prefix holds no dash, so `{prefix}-` cannot be confused with another app's longer
    /// prefix.
    TagPrefix,
    "must be a lowercase letter followed by up to 15 lowercase letters or digits",
    |value| {
        value.len() <= 16
            && value.starts_with(|character: char| character.is_ascii_lowercase())
            && value
                .chars()
                .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
    }
);

string_id!(
    /// The name of the Durable Object that owns a resource, such as a job or project ID
    OwnerId,
    "must be 1 to 64 ASCII letters, digits, underscores or dashes",
    |value| bounded(value, 64, is_word)
);

string_id!(
    /// A provider resource identifier, such as a droplet number or a volume UUID
    ResourceId,
    "must be 1 to 128 ASCII letters, digits, underscores or dashes",
    // no dots or slashes, so an ID can never change the path of a provider URL
    |value| bounded(value, 128, is_word)
);

string_id!(
    /// A client lease identifier
    LeaseId,
    "must be 1 to 128 ASCII letters, digits, underscores or dashes",
    |value| bounded(value, 128, is_word)
);

string_id!(
    /// A capacity permit identifier
    PermitId,
    "must be 1 to 128 ASCII letters, digits, underscores or dashes",
    |value| bounded(value, 128, is_word)
);

/// A positive lifecycle counter, unique within one owner
///
/// Each lease an owner grants uses a new generation, and every resource created for that
/// lease carries it in its tags, so a stale resource can never be mistaken for a current one.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Generation(NonZeroU64);

impl Generation {
    /// The first generation
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// Creates a generation from a positive safe integer
    pub fn new(value: u64) -> Result<Self, IdError> {
        NonZeroU64::new(value)
            .filter(|value| value.get() <= MAX_SAFE_INTEGER)
            .map(Self)
            .ok_or(IdError {
                kind: "Generation",
                rule: "must be a positive safe integer",
            })
    }

    /// Returns the numeric value
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Returns the next generation, or `None` past the safe integer range
    #[must_use]
    pub fn next(self) -> Option<Self> {
        Self::new(self.get().checked_add(1)?).ok()
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl<'de> Deserialize<'de> for Generation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::{Generation, OwnerId, ResourceId, TagPrefix};

    #[test]
    fn identifiers_reject_values_that_would_break_tags_or_storage() {
        assert!(TagPrefix::parse("cl").is_ok());
        assert!(TagPrefix::parse("cl-job").is_err());
        assert!(TagPrefix::parse("1cl").is_err());
        assert!(TagPrefix::parse("").is_err());
        assert!(OwnerId::parse("job_7f3a-2").is_ok());
        assert!(OwnerId::parse("job 7").is_err());
        assert!(OwnerId::parse("x".repeat(65)).is_err());
        assert!(ResourceId::parse("506f78a4-e098-11e5-ad9f-000f53306ae1").is_ok());
        assert!(ResourceId::parse("../droplets").is_err());
        assert!(ResourceId::parse("..").is_err());
        assert!(Generation::new(0).is_err());
        assert!(Generation::new(9_007_199_254_740_992).is_err());
        assert!(serde_json::from_str::<OwnerId>("\"bad owner\"").is_err());
    }
}
