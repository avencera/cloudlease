//! The injected HTTP transport and runtime services provider clients use
//!
//! Provider clients never depend on a particular HTTP stack or executor. A Worker supplies a
//! Fetch-based transport and a `Date.now()` runtime; tests supply fakes.

use std::{future::Future, pin::Pin, time::Duration};

use thiserror::Error;

/// An HTTP method used by provider clients
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpMethod {
    /// Read
    Get,
    /// Create
    Post,
    /// Delete
    Delete,
}

impl HttpMethod {
    /// Returns the method text
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Delete => "DELETE",
        }
    }
}

/// A fully prepared request
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpRequest {
    /// The method
    pub method: HttpMethod,
    /// The absolute, origin-checked URL
    pub url: url::Url,
    /// Headers with lowercase names
    pub headers: Vec<(String, String)>,
    /// The UTF-8 body, when the method has one
    pub body: Option<Vec<u8>>,
    /// The longest the transport may spend on this attempt
    pub timeout: Duration,
}

/// A provider response
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    /// The status code
    pub status: u16,
    /// Headers in any case
    pub headers: Vec<(String, String)>,
    /// The raw body
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Returns a header by case-insensitive name
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Reports whether the status is 2xx
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// Why a request produced no response
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportErrorKind {
    /// The transport could not complete the request
    Network,
    /// The transport aborted the request at its timeout
    Timeout,
}

/// A request that produced no response
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct TransportError {
    /// The failure class
    pub kind: TransportErrorKind,
    message: String,
}

impl TransportError {
    /// Creates a network failure
    #[must_use]
    pub fn network(message: impl Into<String>) -> Self {
        Self {
            kind: TransportErrorKind::Network,
            message: message.into(),
        }
    }

    /// Creates a timeout failure
    #[must_use]
    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            kind: TransportErrorKind::Timeout,
            message: message.into(),
        }
    }
}

/// A boxed future without a `Send` bound, since Worker futures are not `Send`
pub type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Sends one prepared request
pub trait HttpTransport {
    /// Sends the request and returns the response, whatever its status
    fn send(&self, request: HttpRequest) -> LocalFuture<'_, Result<HttpResponse, TransportError>>;
}

/// Time, delay and randomness for retries and polling
pub trait Runtime {
    /// Returns Unix time in milliseconds
    fn now_millis(&self) -> u64;

    /// Waits without blocking the isolate
    fn sleep(&self, duration: Duration) -> LocalFuture<'_, ()>;

    /// Returns a random value for retry jitter
    fn random_u64(&self) -> u64;
}
