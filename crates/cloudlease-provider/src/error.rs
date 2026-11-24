//! Typed provider failures with retry information

use std::time::Duration;

use thiserror::Error;

/// The result of a provider operation
pub type ProviderResult<T> = Result<T, ProviderError>;

/// Stable classes of provider failure
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderErrorCode {
    /// A finite deadline passed
    Timeout,
    /// The transport could not complete a request
    Network,
    /// The provider asked the caller to slow down
    RateLimited,
    /// The provider failed on its side
    ServerError,
    /// The resource is absent
    NotFound,
    /// The credentials lack permission
    PermissionDenied,
    /// The request is invalid
    Validation,
    /// The request conflicts with provider state, such as deleting an attached volume
    Conflict,
    /// The provider has no stock of the requested machine type at the requested location
    ///
    /// Not retryable as-is; a caller may try another location, type or provider.
    NoCapacity,
    /// An account quota or limit blocks the request, such as a vCPU or Spot instance limit
    QuotaExceeded,
    /// The provider response did not match the expected schema
    InvalidResponse,
    /// Provider state broke an ownership invariant
    InvariantViolation,
}

/// A provider failure
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct ProviderError {
    /// The failure class
    pub code: ProviderErrorCode,
    /// Whether the same operation may succeed later
    pub retryable: bool,
    /// The HTTP status, when the provider answered
    pub status: Option<u16>,
    /// The provider's requested delay, when it sent one
    pub retry_after: Option<Duration>,
    /// The provider request ID, when it sent one
    pub request_id: Option<String>,
    message: String,
}

impl ProviderError {
    /// Creates a failure without HTTP metadata
    #[must_use]
    pub fn new(code: ProviderErrorCode, retryable: bool, message: impl Into<String>) -> Self {
        Self {
            code,
            retryable,
            status: None,
            retry_after: None,
            request_id: None,
            message: message.into(),
        }
    }

    /// Adds provider response metadata
    #[must_use]
    pub fn with_response(
        mut self,
        status: u16,
        retry_after: Option<Duration>,
        request_id: Option<String>,
    ) -> Self {
        self.status = Some(status);
        self.retry_after = retry_after;
        self.request_id = request_id;
        self
    }

    /// Returns the message
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn invalid_response(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorCode::InvalidResponse, false, message)
    }

    pub(crate) fn validation(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorCode::Validation, false, message)
    }
}
