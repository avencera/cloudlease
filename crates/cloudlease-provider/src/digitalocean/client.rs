//! A narrow DigitalOcean v2 REST client over an injected transport
//!
//! Ported from rembuild's client. It follows the shared [`rest`](crate::rest) rules; creates
//! are never replayed because DigitalOcean documents no idempotency for them, and pagination
//! is bounded.

use std::time::Duration;

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use url::Url;

use crate::{
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    http::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, Runtime},
    rest::{
        RetryPolicy, classify_status, normalize_base_url, parse_retry_after, request_timeout,
        transport_error, url_within,
    },
};

const DEFAULT_BASE_URL: &str = "https://api.digitalocean.com/v2/";
// droplet creation can take longer than ten seconds before DigitalOcean returns its ID
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PAGE_COUNT: usize = 100;
const PAGE_SIZE: &str = "200";

/// A DigitalOcean API token that never appears in debug output
#[derive(Clone, Eq, PartialEq)]
pub struct DigitalOceanToken(String);

impl DigitalOceanToken {
    /// Wraps a non-empty token
    pub fn parse(value: impl Into<String>) -> ProviderResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ProviderError::validation(
                "the DigitalOcean token must not be empty",
            ));
        }

        Ok(Self(value))
    }
}

impl std::fmt::Debug for DigitalOceanToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DigitalOceanToken(<redacted>)")
    }
}

/// Client settings
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientOptions {
    token: DigitalOceanToken,
    base_url: Url,
    request_timeout: Duration,
    retry: RetryPolicy,
}

impl ClientOptions {
    /// Creates settings for the public DigitalOcean API
    pub fn new(token: DigitalOceanToken) -> ProviderResult<Self> {
        let base_url = Url::parse(DEFAULT_BASE_URL)
            .map_err(|_| ProviderError::validation("the default API URL is invalid"))?;

        Ok(Self {
            token,
            base_url,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            retry: RetryPolicy::default(),
        })
    }

    /// Replaces the API base URL, such as a fake API in tests
    pub fn with_base_url(mut self, base_url: Url) -> ProviderResult<Self> {
        self.base_url = normalize_base_url(base_url)?;
        Ok(self)
    }

    /// Replaces the per-attempt timeout
    pub fn with_request_timeout(mut self, timeout: Duration) -> ProviderResult<Self> {
        self.request_timeout = request_timeout(timeout)?;
        Ok(self)
    }

    /// Replaces the retry policy
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> ProviderResult<Self> {
        self.retry = retry.validate()?;
        Ok(self)
    }
}

/// A droplet as the API returns it
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct WireDroplet {
    /// The numeric ID
    pub id: u64,
    /// The name
    pub name: String,
    /// The status, such as `new`, `active` or `off`
    pub status: String,
    /// The tags
    #[serde(default, deserialize_with = "null_as_empty")]
    pub tags: Vec<String>,
}

fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// A DigitalOcean v2 client
pub struct DigitalOceanClient<T, R> {
    options: ClientOptions,
    transport: T,
    runtime: R,
}

impl<T: HttpTransport, R: Runtime> DigitalOceanClient<T, R> {
    /// Creates a client over a transport and runtime
    pub const fn new(options: ClientOptions, transport: T, runtime: R) -> Self {
        Self {
            options,
            transport,
            runtime,
        }
    }

    /// Returns the runtime the client sleeps with
    pub const fn runtime(&self) -> &R {
        &self.runtime
    }

    /// Lists every droplet carrying one tag
    pub async fn list_droplets_by_tag(&self, tag: &str) -> ProviderResult<Vec<WireDroplet>> {
        let mut url = self.url_for("droplets")?;
        url.query_pairs_mut()
            .append_pair("tag_name", tag)
            .append_pair("per_page", PAGE_SIZE);

        self.list(url, "list droplets", "droplets").await
    }

    /// Reads one droplet, or `None` when it does not exist
    pub async fn get_droplet(&self, id: &str) -> ProviderResult<Option<WireDroplet>> {
        let url = self.resource_url("droplets", id)?;

        self.get_optional(url, "read droplet", "droplet").await
    }

    /// Creates a droplet and returns its ID; never replayed
    pub async fn create_droplet(&self, body: Value) -> ProviderResult<WireDroplet> {
        let url = self.url_for("droplets")?;
        let value = self
            .request(url, "create droplet", HttpMethod::Post, Some(body))
            .await?;

        decode_root(&value, "droplet")
    }

    /// Requests deletion of a droplet, treating an absent droplet as success
    pub async fn delete_droplet(&self, id: &str) -> ProviderResult<()> {
        let url = self.resource_url("droplets", id)?;

        self.delete(url, "delete droplet").await
    }

    async fn get_optional<Item: DeserializeOwned>(
        &self,
        url: Url,
        operation: &str,
        key: &str,
    ) -> ProviderResult<Option<Item>> {
        match self.request(url, operation, HttpMethod::Get, None).await {
            Ok(value) => decode_root(&value, key).map(Some),
            Err(error) if error.code == ProviderErrorCode::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn delete(&self, url: Url, operation: &str) -> ProviderResult<()> {
        match self.request(url, operation, HttpMethod::Delete, None).await {
            Ok(_) => Ok(()),
            Err(error) if error.code == ProviderErrorCode::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn list<Item: DeserializeOwned>(
        &self,
        first: Url,
        operation: &str,
        key: &str,
    ) -> ProviderResult<Vec<Item>> {
        let mut items = Vec::new();
        let mut next = Some(first);

        for _ in 0..MAX_PAGE_COUNT {
            let Some(url) = next.take() else {
                return Ok(items);
            };
            let value = self.request(url, operation, HttpMethod::Get, None).await?;
            let page: Vec<Item> = decode_root(&value, key)?;
            items.extend(page);

            next = value
                .pointer("/links/pages/next")
                .and_then(Value::as_str)
                .map(|next| self.url_for(next))
                .transpose()?;
        }

        if next.is_none() {
            return Ok(items);
        }

        Err(ProviderError::invalid_response(format!(
            "{operation} returned too many pages"
        )))
    }

    async fn request(
        &self,
        url: Url,
        operation: &str,
        method: HttpMethod,
        body: Option<Value>,
    ) -> ProviderResult<Value> {
        let body = body
            .map(|body| serde_json::to_vec(&body))
            .transpose()
            .map_err(|_| ProviderError::validation("the request body could not be encoded"))?;
        // DigitalOcean documents no create idempotency, so a lost POST response is found by
        // its tags instead of being replayed
        let attempts = if method == HttpMethod::Post {
            1
        } else {
            self.options.retry.max_attempts
        };

        let mut last_error = None;
        for attempt in 1..=attempts {
            if let Some(error) = &last_error {
                let delay = self
                    .options
                    .retry
                    .delay(attempt - 1, error, self.runtime.random_u64());
                self.runtime.sleep(delay).await;
            }

            let request = self.http_request(method, url.clone(), body.clone());
            let result = match self.transport.send(request).await {
                Ok(response) => self.decode_response(&response, operation),
                Err(error) => Err(transport_error(&error, operation)),
            };

            match result {
                Ok(value) => return Ok(value),
                Err(error) if error.retryable && attempt < attempts => last_error = Some(error),
                Err(error) => return Err(error),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            ProviderError::new(
                ProviderErrorCode::Network,
                true,
                format!("{operation} exceeded its retry limit"),
            )
        }))
    }

    fn http_request(&self, method: HttpMethod, url: Url, body: Option<Vec<u8>>) -> HttpRequest {
        let mut headers = vec![
            ("accept".to_owned(), "application/json".to_owned()),
            (
                "authorization".to_owned(),
                format!("Bearer {}", self.options.token.0),
            ),
        ];
        if body.is_some() {
            headers.push(("content-type".to_owned(), "application/json".to_owned()));
        }

        HttpRequest {
            method,
            url,
            headers,
            body,
            timeout: self.options.request_timeout,
        }
    }

    fn decode_response(&self, response: &HttpResponse, operation: &str) -> ProviderResult<Value> {
        if !response.is_success() {
            let body = serde_json::from_slice::<Value>(&response.body).ok();
            let message = body
                .as_ref()
                .and_then(|body| body.get("message"))
                .and_then(Value::as_str)
                .map_or_else(
                    || format!("{operation} failed with HTTP {}", response.status),
                    |message| format!("{operation} failed: {message}"),
                );
            let request_id = body
                .as_ref()
                .and_then(|body| body.get("request_id"))
                .and_then(Value::as_str)
                .or_else(|| response.header("x-request-id"))
                .map(str::to_owned);
            let retry_after = parse_retry_after(
                response.header("retry-after"),
                self.runtime.now_millis(),
                self.options.retry.max_retry_after,
            );

            return Err(classify_status(response.status, message).with_response(
                response.status,
                retry_after,
                request_id,
            ));
        }

        if response.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }

        serde_json::from_slice(&response.body).map_err(|_| {
            ProviderError::invalid_response(format!("{operation} returned invalid JSON"))
        })
    }

    fn url_for(&self, path: &str) -> ProviderResult<Url> {
        url_within(&self.options.base_url, path)
    }

    fn resource_url(&self, collection: &str, id: &str) -> ProviderResult<Url> {
        // IDs are validated as letters, digits, underscores and dashes before they get here
        self.url_for(&format!("{collection}/{id}"))
    }
}

fn decode_root<Item: DeserializeOwned>(value: &Value, key: &str) -> ProviderResult<Item> {
    let root = value
        .get(key)
        .ok_or_else(|| ProviderError::invalid_response(format!("the response has no {key}")))?;

    serde_json::from_value(root.clone())
        .map_err(|error| ProviderError::invalid_response(format!("invalid {key}: {error}")))
}
