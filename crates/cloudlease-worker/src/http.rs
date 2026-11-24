//! JSON requests and responses between Workers and Durable Objects

use serde::{Serialize, de::DeserializeOwned};
use worker::{Headers, Method, Request, RequestInit, Response, Stub};

use crate::error::{CloudleaseError, CloudleaseResult};

/// The largest JSON body cloudlease reads
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// An HTTP error with a stable code, sent as `{ "error": { code, message, retryable } }`
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpError {
    /// The status
    pub status: u16,
    /// A stable code
    pub code: &'static str,
    /// A message for operators
    pub message: String,
    /// Whether retrying may succeed
    pub retryable: bool,
}

impl HttpError {
    /// Creates an error
    #[must_use]
    pub fn new(
        status: u16,
        code: &'static str,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retryable,
        }
    }

    /// A malformed request
    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message, false)
    }

    /// An unknown route
    #[must_use]
    pub fn not_found() -> Self {
        Self::new(404, "not_found", "no such route", false)
    }

    /// Converts the error to a response
    pub fn into_response(self) -> worker::Result<Response> {
        json_response(
            self.status,
            &serde_json::json!({
                "error": {
                    "code": self.code,
                    "message": self.message,
                    "retryable": self.retryable,
                }
            }),
        )
    }
}

impl From<CloudleaseError> for HttpError {
    fn from(error: CloudleaseError) -> Self {
        Self::new(500, "internal", error.to_string(), true)
    }
}

impl From<worker::Error> for HttpError {
    fn from(error: worker::Error) -> Self {
        CloudleaseError::from(error).into()
    }
}

/// Builds a JSON response that is never cached
pub fn json_response<T: Serialize>(status: u16, body: &T) -> worker::Result<Response> {
    let headers = Headers::new();
    headers.set("cache-control", "no-store")?;

    Ok(Response::from_json(body)?
        .with_status(status)
        .with_headers(headers))
}

/// Reads a bounded JSON request body
pub async fn read_json<T: DeserializeOwned>(request: &mut Request) -> Result<T, HttpError> {
    let bytes = request
        .bytes()
        .await
        .map_err(|error| HttpError::invalid_request(format!("unreadable body: {error}")))?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(HttpError::new(
            413,
            "too_large",
            "the body is too large",
            false,
        ));
    }

    serde_json::from_slice(&bytes)
        .map_err(|error| HttpError::invalid_request(format!("invalid body: {error}")))
}

/// Sends a JSON request to a Durable Object and decodes a 2xx JSON answer
pub async fn call_json<B, T>(
    stub: &Stub,
    object: &str,
    method: Method,
    url: &str,
    body: Option<&B>,
) -> CloudleaseResult<T>
where
    B: Serialize,
    T: DeserializeOwned,
{
    let mut init = RequestInit::new();
    init.with_method(method);
    if let Some(body) = body {
        let headers = Headers::new();
        headers.set("content-type", "application/json")?;
        init.with_headers(headers);
        init.with_body(Some(serde_json::to_string(body)?.into()));
    }

    let request = Request::new_with_init(url, &init)?;
    let mut response = stub.fetch_with_request(request).await?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        return Err(CloudleaseError::Remote {
            object: object.to_owned(),
            status,
        });
    }

    Ok(response.json().await?)
}
