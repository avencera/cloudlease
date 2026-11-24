//! OAuth client credentials for the phoenixNAP API
//!
//! phoenixNAP issues short bearer tokens (900 seconds when checked on 2026-11-24) from a
//! Keycloak token endpoint. The client caches one token, fetches a new one shortly before it
//! expires, and drops it when the API answers 401, so an expired or revoked token costs one
//! extra request rather than a failed operation.

use std::{cell::RefCell, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use url::form_urlencoded;

use crate::error::{ProviderError, ProviderResult};

/// A phoenixNAP OAuth client ID and secret that never appear in debug output
#[derive(Clone, Eq, PartialEq)]
pub struct PnapCredentials {
    client_id: String,
    client_secret: String,
}

impl PnapCredentials {
    /// Wraps a non-empty client ID and secret
    pub fn parse(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> ProviderResult<Self> {
        let client_id = client_id.into();
        let client_secret = client_secret.into();
        if client_id.trim().is_empty() || client_secret.trim().is_empty() {
            return Err(ProviderError::validation(
                "the phoenixNAP client ID and secret must not be empty",
            ));
        }

        Ok(Self {
            client_id,
            client_secret,
        })
    }

    /// Returns the HTTP Basic credentials for the token endpoint
    ///
    /// RFC 6749 section 2.3.1 form-encodes the ID and secret before joining them, so a
    /// colon or percent sign in either survives.
    pub(crate) fn basic_authorization(&self) -> String {
        let id: String = form_urlencoded::byte_serialize(self.client_id.as_bytes()).collect();
        let secret: String =
            form_urlencoded::byte_serialize(self.client_secret.as_bytes()).collect();

        format!("Basic {}", STANDARD.encode(format!("{id}:{secret}")))
    }
}

impl std::fmt::Debug for PnapCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PnapCredentials(<redacted>)")
    }
}

/// The token endpoint's success body
#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: String,
    pub(crate) expires_in: u64,
}

/// A bearer token and the time it stops being used
#[derive(Clone)]
struct CachedToken {
    value: String,
    // already moved earlier by the refresh margin
    use_until_millis: u64,
}

/// One cached bearer token
#[derive(Default)]
pub(crate) struct TokenCache(RefCell<Option<CachedToken>>);

impl TokenCache {
    /// Returns the cached token if it is still usable at `now_millis`
    pub(crate) fn current(&self, now_millis: u64) -> Option<String> {
        self.0
            .borrow()
            .as_ref()
            .filter(|token| now_millis < token.use_until_millis)
            .map(|token| token.value.clone())
    }

    /// Stores a token issued at `issued_millis`, to be replaced `margin` before it expires
    ///
    /// A token whose lifetime is shorter than the margin is used once and never cached.
    pub(crate) fn store(&self, response: TokenResponse, issued_millis: u64, margin: Duration) {
        let lifetime = Duration::from_secs(response.expires_in).saturating_sub(margin);
        let use_until_millis = issued_millis.saturating_add(crate::rest::millis(lifetime));
        *self.0.borrow_mut() = Some(CachedToken {
            value: response.access_token,
            use_until_millis,
        });
    }

    /// Drops the cached token, such as after the API rejected it
    pub(crate) fn clear(&self) {
        self.0.borrow_mut().take();
    }
}
