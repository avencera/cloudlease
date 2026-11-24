//! Client and lease tokens, stored only as hashes
//!
//! Tokens carry 256 random bits, so a plain SHA-256 is enough to store them: nothing can be
//! guessed from the hash, and a leaked hash cannot be presented as a token.

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::ids::AppName;

/// The prefix of a lease token, so a leaked one is recognizable
pub const LEASE_TOKEN_PREFIX: &str = "cll_";

/// A rejected token setting
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TokenError {
    /// A hash is not 64 lowercase hex digits
    #[error("a token hash must be 64 lowercase hex digits (sha256)")]
    InvalidHash,
    /// The client token map does not parse
    #[error("CLIENT_TOKENS must be a JSON object of app name to sha256 hex: {0}")]
    InvalidClientTokens(String),
}

/// The SHA-256 of a token
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// Hashes a presented token
    #[must_use]
    pub fn of(token: &str) -> Self {
        Self(Sha256::digest(token.as_bytes()).into())
    }

    /// Parses a hash written as 64 lowercase hex digits
    pub fn parse_hex(value: &str) -> Result<Self, TokenError> {
        if value.len() != 64 {
            return Err(TokenError::InvalidHash);
        }

        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let pair = value
                .get(index * 2..index * 2 + 2)
                .ok_or(TokenError::InvalidHash)?;
            if pair.chars().any(|c| c.is_ascii_uppercase()) {
                return Err(TokenError::InvalidHash);
            }
            *byte = u8::from_str_radix(pair, 16).map_err(|_| TokenError::InvalidHash)?;
        }

        Ok(Self(bytes))
    }

    /// Compares in constant time, so the comparison leaks nothing about the stored hash
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }

    fn hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

impl fmt::Debug for TokenHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "TokenHash({}…)", &self.hex()[..8])
    }
}

impl Serialize for TokenHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for TokenHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse_hex(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// A freshly minted lease token, which exists in plain text only until it is written into
/// the machine's boot data
pub struct LeaseToken(String);

impl LeaseToken {
    /// Builds a token from 32 random bytes supplied by the caller
    #[must_use]
    pub fn from_random(bytes: [u8; 32]) -> Self {
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();

        Self(format!("{LEASE_TOKEN_PREFIX}{hex}"))
    }

    /// Returns the token text
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Returns the hash to store
    #[must_use]
    pub fn hash(&self) -> TokenHash {
        TokenHash::of(&self.0)
    }
}

impl fmt::Debug for LeaseToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LeaseToken(..)")
    }
}

/// The client token hash of every app allowed to call the API
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientTokens(BTreeMap<AppName, TokenHash>);

impl ClientTokens {
    /// Parses the `CLIENT_TOKENS` secret: `{ "<app>": "<sha256 hex of its token>" }`
    pub fn parse(json: &str) -> Result<Self, TokenError> {
        serde_json::from_str::<BTreeMap<AppName, TokenHash>>(json)
            .map(Self)
            .map_err(|error| TokenError::InvalidClientTokens(error.to_string()))
    }

    /// Returns the app whose client token `token` is
    ///
    /// Every stored hash is compared, so the time taken does not depend on which app matched.
    #[must_use]
    pub fn app_for(&self, token: &str) -> Option<&AppName> {
        let presented = TokenHash::of(token);

        self.0.iter().fold(None, |found, (app, hash)| {
            if hash.matches(&presented) {
                Some(app)
            } else {
                found
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientTokens, LeaseToken, TokenHash};

    #[test]
    fn tokens_are_checked_against_their_hash_only() {
        let token = LeaseToken::from_random([7; 32]);
        assert!(token.expose().starts_with("cll_"));
        assert_eq!(token.expose().len(), 4 + 64);
        assert!(token.hash().matches(&TokenHash::of(token.expose())));
        assert!(!token.hash().matches(&TokenHash::of("cll_other")));
        assert_eq!(format!("{token:?}"), "LeaseToken(..)");

        let hash = serde_json::to_string(&token.hash()).unwrap();
        assert_eq!(
            serde_json::from_str::<TokenHash>(&hash).unwrap(),
            token.hash()
        );
        assert!(TokenHash::parse_hex(&"A".repeat(64)).is_err());
        assert!(TokenHash::parse_hex("abcd").is_err());
    }

    #[test]
    fn client_tokens_name_the_app_they_belong_to() {
        let hash = |token: &str| serde_json::to_value(TokenHash::of(token)).unwrap();
        let config = serde_json::json!({ "homebased": hash("clc_one"), "other": hash("clc_two") });
        let tokens = ClientTokens::parse(&config.to_string()).unwrap();

        assert_eq!(tokens.app_for("clc_one").unwrap().as_str(), "homebased");
        assert_eq!(tokens.app_for("clc_two").unwrap().as_str(), "other");
        assert_eq!(tokens.app_for("clc_three"), None);
        assert!(ClientTokens::parse(r#"{ "homebased": "plaintext" }"#).is_err());
        assert!(ClientTokens::parse(r#"{ "Bad App": "00" }"#).is_err());
    }
}
