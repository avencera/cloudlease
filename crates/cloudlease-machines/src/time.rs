//! RFC 3339 times on the wire
//!
//! Internally every time is a [`UnixMillis`]; the HTTP contract speaks RFC 3339 in UTC.

use chrono::{DateTime, SecondsFormat, Utc};
use cloudlease_core::time::UnixMillis;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// A time that crosses the HTTP contract as RFC 3339 in UTC
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(pub UnixMillis);

impl Timestamp {
    /// Formats the time, such as `2026-11-25T02:00:00Z`
    #[must_use]
    pub fn to_rfc3339(self) -> String {
        let millis = i64::try_from(self.0.as_millis()).unwrap_or(i64::MAX);
        DateTime::<Utc>::from_timestamp_millis(millis)
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true)
    }

    /// Parses an RFC 3339 time with any offset, refusing times before 1970
    pub fn parse(value: &str) -> Result<Self, String> {
        let parsed = DateTime::parse_from_rfc3339(value)
            .map_err(|error| format!("{value:?} is not an RFC 3339 time: {error}"))?;
        let millis = u64::try_from(parsed.timestamp_millis())
            .ok()
            .and_then(UnixMillis::new)
            .ok_or_else(|| format!("{value:?} is out of range"))?;

        Ok(Self(millis))
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use cloudlease_core::time::UnixMillis;

    use super::Timestamp;

    #[test]
    fn times_round_trip_through_rfc3339_in_utc() {
        let time = Timestamp::parse("2026-11-25T02:00:00Z").unwrap();
        assert_eq!(time.0, UnixMillis::new(1_795_572_000_000).unwrap());
        assert_eq!(time.to_rfc3339(), "2026-11-25T02:00:00Z");
        assert_eq!(
            Timestamp::parse("2026-11-24T20:00:00.250-06:00")
                .unwrap()
                .to_rfc3339(),
            "2026-11-25T02:00:00.250Z"
        );
        assert!(Timestamp::parse("1969-12-31T23:59:59Z").is_err());
        assert!(Timestamp::parse("tomorrow").is_err());
    }
}
