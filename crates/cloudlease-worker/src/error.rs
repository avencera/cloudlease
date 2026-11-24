//! Errors from the Worker glue

use std::time::Duration;

use cloudlease_core::{
    capacity::CapacityError,
    holding::HoldingError,
    ids::{Generation, IdError},
};
use cloudlease_provider::error::ProviderError;
use thiserror::Error;

/// A failure in cloudlease's Worker glue
#[derive(Debug, Error)]
pub enum CloudleaseError {
    /// The Workers runtime failed
    #[error("worker: {0}")]
    Worker(#[from] worker::Error),
    /// JSON did not encode or decode
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A holding transition was rejected
    #[error("holding: {0}")]
    Holding(#[from] HoldingError),
    /// A capacity value was rejected
    #[error("capacity: {0}")]
    Capacity(#[from] CapacityError),
    /// An identifier was invalid
    #[error("id: {0}")]
    Id(#[from] IdError),
    /// The provider failed
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    /// Stored data contradicts itself
    #[error("storage: {0}")]
    Storage(String),
    /// Another object answered with an error
    #[error("{object} answered HTTP {status}")]
    Remote {
        /// The object that answered
        object: String,
        /// The status it returned
        status: u16,
    },
    /// Another object did not answer in time
    #[error("{object} did not answer within {}ms", limit.as_millis())]
    Timeout {
        /// The object asked
        object: String,
        /// How long the caller waited
        limit: Duration,
    },
    /// The owner holds no record of this generation
    #[error("no holding for generation {0}")]
    UnknownGeneration(Generation),
    /// A binding or setting is missing or invalid
    #[error("configuration: {0}")]
    Config(String),
}

/// The result of a cloudlease Worker operation
pub type CloudleaseResult<T> = Result<T, CloudleaseError>;

impl From<CloudleaseError> for worker::Error {
    fn from(error: CloudleaseError) -> Self {
        match error {
            CloudleaseError::Worker(error) => error,
            other => Self::RustError(other.to_string()),
        }
    }
}
