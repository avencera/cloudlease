//! Run and lease identifiers that route themselves to their run object
//!
//! A run ID is derived from the client app and the create's `request_id`, so a retried
//! create reaches the same run object without a global index. A lease ID is the run ID and
//! the machine's generation, so a lease call reaches its run object the same way.

use std::{fmt, str::FromStr};

use cloudlease_core::ids::{Generation, IdError, LeaseId, OwnerId};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

const RUN_PREFIX: &str = "run_";
const RUN_HASH_HEX: usize = 24;

/// A rejected identifier
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{kind} {value:?} is invalid: {rule}")]
pub struct NameError {
    /// The identifier type
    pub kind: &'static str,
    /// The rejected value
    pub value: String,
    /// The rule it broke
    pub rule: &'static str,
}

macro_rules! name {
    ($(#[$meta:meta])* $name:ident, $rule:literal, $valid:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps a value
            pub fn parse(value: impl Into<String>) -> Result<Self, NameError> {
                let value = value.into();
                let valid: fn(&str) -> bool = $valid;
                if !valid(&value) {
                    return Err(NameError {
                        kind: stringify!($name),
                        value,
                        rule: $rule,
                    });
                }

                Ok(Self(value))
            }

            /// Returns the value
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
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }
    };
}

fn slug(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.starts_with(|character: char| character.is_ascii_lowercase())
        && value.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

name!(
    /// A client app allowed to call the API, such as `homebased`
    AppName,
    "must be a lowercase letter followed by up to 31 lowercase letters, digits or dashes",
    |value| slug(value, 32)
);

name!(
    /// A machine class, such as `do-c16`
    ClassName,
    "must be a lowercase letter followed by up to 62 lowercase letters, digits or dashes",
    |value| slug(value, 63)
);

name!(
    /// A run's display name, such as the homebased pool's name
    RunName,
    "must be 1 to 63 ASCII letters, digits, dots, underscores or dashes",
    |value| {
        !value.is_empty()
            && value.len() <= 63
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
            })
    }
);

name!(
    /// A machine's hostname, which is also its droplet name
    Hostname,
    "must be a lowercase letter followed by up to 62 lowercase letters, digits or dashes",
    |value| slug(value, 63)
);

/// A client's idempotency key: a UUID, written in lowercase with hyphens
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RequestId(Uuid);

impl RequestId {
    /// Parses any UUID spelling
    pub fn parse(value: &str) -> Result<Self, NameError> {
        Uuid::parse_str(value).map(Self).map_err(|_| NameError {
            kind: "RequestId",
            value: value.to_owned(),
            rule: "must be a UUID",
        })
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.as_hyphenated().fmt(formatter)
    }
}

impl Serialize for RequestId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// A run, which is one owner object: `run_` and 24 lowercase hex digits
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Derives the run that a create with `request_id` from `app` names
    ///
    /// The app is part of the hash, so two apps that pick the same request ID get
    /// different runs.
    #[must_use]
    pub fn derive(app: &AppName, request_id: RequestId) -> Self {
        let digest = Sha256::new()
            .chain_update(b"cloudlease-machines/run\0")
            .chain_update(app.as_str())
            .chain_update(b"\0")
            .chain_update(request_id.to_string())
            .finalize();
        let hex: String = digest
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 0xf])
            .take(RUN_HASH_HEX)
            .map(|nibble| char::from_digit(u32::from(nibble), 16).unwrap_or('0'))
            .collect();

        Self(format!("{RUN_PREFIX}{hex}"))
    }

    /// Parses a run ID from a URL
    pub fn parse(value: &str) -> Result<Self, NameError> {
        let valid = value.strip_prefix(RUN_PREFIX).is_some_and(|hex| {
            hex.len() == RUN_HASH_HEX
                && hex
                    .chars()
                    .all(|character| matches!(character, '0'..='9' | 'a'..='f'))
        });
        if !valid {
            return Err(NameError {
                kind: "RunId",
                value: value.to_owned(),
                rule: "must be run_ followed by 24 lowercase hex digits",
            });
        }

        Ok(Self(value.to_owned()))
    }

    /// Returns the value
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the owner ID the run's resources are tagged with
    pub fn owner(&self) -> Result<OwnerId, IdError> {
        OwnerId::parse(self.0.clone())
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// One machine's lease: its run and its generation, written `{run_id}-{generation}`
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MachineId {
    /// The run, whose object holds the lease
    pub run: RunId,
    /// The machine's generation within the run
    pub generation: Generation,
}

impl MachineId {
    /// Returns the core lease ID, which is the same text
    pub fn lease_id(&self) -> Result<LeaseId, IdError> {
        LeaseId::parse(self.to_string())
    }
}

impl fmt::Display for MachineId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}-{}", self.run, self.generation)
    }
}

impl FromStr for MachineId {
    type Err = NameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || NameError {
            kind: "MachineId",
            value: value.to_owned(),
            rule: "must be a run ID, a dash and a positive generation",
        };
        let (run, generation) = value.split_once('-').ok_or_else(invalid)?;
        // a canonical number only, so one lease has exactly one spelling
        if generation.starts_with('0') || !generation.chars().all(|c| c.is_ascii_digit()) {
            return Err(invalid());
        }
        let generation = generation
            .parse()
            .ok()
            .and_then(|value| Generation::new(value).ok())
            .ok_or_else(invalid)?;

        Ok(Self {
            run: RunId::parse(run).map_err(|_| invalid())?,
            generation,
        })
    }
}

impl Serialize for MachineId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MachineId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use cloudlease_core::ids::Generation;

    use super::{AppName, Hostname, MachineId, RequestId, RunId};

    #[test]
    fn runs_derive_from_app_and_request_and_leases_route_back_to_their_run() {
        let app = AppName::parse("homebased").unwrap();
        let request = RequestId::parse("0193A2B4-1C2D-7E3F-8A9B-0C1D2E3F4A5B").unwrap();
        let run = RunId::derive(&app, request);

        assert_eq!(run, RunId::derive(&app, request));
        assert_ne!(
            run,
            RunId::derive(&AppName::parse("other").unwrap(), request)
        );
        assert_eq!(RunId::parse(run.as_str()).unwrap(), run);
        assert!(run.owner().is_ok());
        assert_eq!(request.to_string(), "0193a2b4-1c2d-7e3f-8a9b-0c1d2e3f4a5b");

        let machine = MachineId {
            run: run.clone(),
            generation: Generation::new(12).unwrap(),
        };
        let text = machine.to_string();
        assert_eq!(text.parse::<MachineId>().unwrap(), machine);
        assert_eq!(machine.lease_id().unwrap().as_str(), text);
        for invalid in [
            format!("{run}-0"),
            format!("{run}-012"),
            format!("{run}-x"),
            format!("{run}"),
            "run_zz-1".to_owned(),
            format!("{run}-1-2"),
        ] {
            assert!(invalid.parse::<MachineId>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn names_reject_values_that_would_break_droplet_names_or_tags() {
        assert!(Hostname::parse("afv-01a2b3c4").is_ok());
        assert!(Hostname::parse("AFV").is_err());
        assert!(Hostname::parse("1afv").is_err());
        assert!(Hostname::parse("a".repeat(64)).is_err());
        assert!(AppName::parse("home_based").is_err());
        assert!(RequestId::parse("not-a-uuid").is_err());
    }
}
