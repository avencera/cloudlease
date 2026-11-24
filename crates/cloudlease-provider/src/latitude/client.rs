//! A narrow Latitude.sh REST client over an injected transport
//!
//! It follows the shared [`rest`](crate::rest) rules. Latitude.sh answers in JSON:API
//! form, documents no create idempotency, and pages lists with `page[number]` without a
//! next link, so a list reads pages until one comes back empty: stopping at a short page
//! would miss servers if the API capped the page size below the size asked for.

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

const DEFAULT_BASE_URL: &str = "https://api.latitude.sh/";
// a bare metal create can take a while to return its server
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PAGE_COUNT: u32 = 100;
const PAGE_SIZE: &str = "100";

/// A Latitude.sh API key that never appears in debug output
#[derive(Clone, Eq, PartialEq)]
pub struct LatitudeApiKey(String);

impl LatitudeApiKey {
    /// Wraps a non-empty key
    pub fn parse(value: impl Into<String>) -> ProviderResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ProviderError::validation(
                "the Latitude.sh API key must not be empty",
            ));
        }

        Ok(Self(value))
    }
}

impl std::fmt::Debug for LatitudeApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LatitudeApiKey(<redacted>)")
    }
}

/// Client settings
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientOptions {
    key: LatitudeApiKey,
    base_url: Url,
    request_timeout: Duration,
    retry: RetryPolicy,
}

impl ClientOptions {
    /// Creates settings for the public Latitude.sh API
    pub fn new(key: LatitudeApiKey) -> ProviderResult<Self> {
        let base_url = Url::parse(DEFAULT_BASE_URL)
            .map_err(|_| ProviderError::validation("the default API URL is invalid"))?;

        Ok(Self {
            key,
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

    /// Replaces the retry policy for reads and deletes
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> ProviderResult<Self> {
        self.retry = retry.validate()?;
        Ok(self)
    }
}

/// A JSON:API resource object
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct Resource<A> {
    /// The ID, such as `sv_ZWr75Zbjr5A91` or `ud_r0MK4O4kDa95w`
    pub id: String,
    /// The attributes
    pub attributes: A,
}

/// Server attributes as the API returns them
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ServerAttributes {
    /// The hostname
    pub hostname: String,
    /// The status, an open set such as `starting_deploy`, `deploying` or `on`
    #[serde(default)]
    pub status: Option<String>,
}

/// User data attributes as the API returns them
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct UserDataAttributes {
    /// The description, which the adapter uses to hold its tag set
    #[serde(default)]
    pub description: Option<String>,
}

/// A server as the API returns it
pub type WireServer = Resource<ServerAttributes>;

/// A user data resource as the API returns it
pub type WireUserData = Resource<UserDataAttributes>;

#[derive(Deserialize)]
struct Document<T> {
    data: T,
}

/// A Latitude.sh client
pub struct LatitudeClient<T, R> {
    options: ClientOptions,
    transport: T,
    runtime: R,
}

impl<T: HttpTransport, R: Runtime> LatitudeClient<T, R> {
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

    /// Lists every server in the team, or those Latitude.sh matches to `hostname`
    ///
    /// The filter only narrows the list; callers still compare hostnames themselves.
    pub async fn list_servers(&self, hostname: Option<&str>) -> ProviderResult<Vec<WireServer>> {
        let mut url = self.url_for("servers")?;
        if let Some(hostname) = hostname {
            url.query_pairs_mut()
                .append_pair("filter[hostname][eql]", hostname);
        }

        self.list(url, "list servers").await
    }

    /// Reads one server, or `None` when it does not exist
    pub async fn get_server(&self, id: &str) -> ProviderResult<Option<WireServer>> {
        let url = self.url_for(&format!("servers/{id}"))?;

        self.get_optional(url, "read server").await
    }

    /// Creates a server and returns it; never replayed
    pub async fn create_server(&self, body: Value) -> ProviderResult<WireServer> {
        let url = self.url_for("servers")?;

        self.document(url, "create server", HttpMethod::Post, Some(body))
            .await
    }

    /// Requests deletion of a server, treating an absent server as success
    pub async fn delete_server(&self, id: &str) -> ProviderResult<()> {
        let url = self.url_for(&format!("servers/{id}"))?;

        self.delete(url, "delete server").await
    }

    /// Lists every user data resource the key can see
    pub async fn list_user_data(&self) -> ProviderResult<Vec<WireUserData>> {
        let url = self.url_for("user_data")?;

        self.list(url, "list user data").await
    }

    /// Reads one user data resource, or `None` when it does not exist
    pub async fn get_user_data(&self, id: &str) -> ProviderResult<Option<WireUserData>> {
        let url = self.url_for(&format!("user_data/{id}"))?;

        self.get_optional(url, "read user data").await
    }

    /// Creates a user data resource and returns it; never replayed
    pub async fn create_user_data(&self, body: Value) -> ProviderResult<WireUserData> {
        let url = self.url_for("user_data")?;

        self.document(url, "create user data", HttpMethod::Post, Some(body))
            .await
    }

    /// Deletes a user data resource, treating an absent one as success
    pub async fn delete_user_data(&self, id: &str) -> ProviderResult<()> {
        let url = self.url_for(&format!("user_data/{id}"))?;

        self.delete(url, "delete user data").await
    }

    async fn get_optional<A: DeserializeOwned>(
        &self,
        url: Url,
        operation: &str,
    ) -> ProviderResult<Option<Resource<A>>> {
        match self.document(url, operation, HttpMethod::Get, None).await {
            Ok(resource) => Ok(Some(resource)),
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

    async fn document<Item: DeserializeOwned>(
        &self,
        url: Url,
        operation: &str,
        method: HttpMethod,
        body: Option<Value>,
    ) -> ProviderResult<Item> {
        let response = self.request(url, operation, method, body).await?;

        decode::<Document<Item>>(&response.body, operation).map(|document| document.data)
    }

    async fn list<Item: DeserializeOwned>(
        &self,
        first: Url,
        operation: &str,
    ) -> ProviderResult<Vec<Item>> {
        let mut items = Vec::new();

        for number in 1..=MAX_PAGE_COUNT {
            let mut url = first.clone();
            url.query_pairs_mut()
                .append_pair("page[size]", PAGE_SIZE)
                .append_pair("page[number]", &number.to_string());
            let response = self.request(url, operation, HttpMethod::Get, None).await?;
            let page: Document<Vec<Item>> = decode(&response.body, operation)?;
            if page.data.is_empty() {
                return Ok(items);
            }

            items.extend(page.data);
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
    ) -> ProviderResult<HttpResponse> {
        let body = body
            .map(|body| serde_json::to_vec(&body))
            .transpose()
            .map_err(|_| ProviderError::validation("the request body could not be encoded"))?;
        // Latitude.sh documents no create idempotency, so a lost POST response is found by
        // the adapter instead of being replayed
        let attempts = if method == HttpMethod::Post {
            1
        } else {
            self.options.retry.max_attempts
        };

        let mut last_error: Option<ProviderError> = None;
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
                Ok(response) => self.check(response, operation),
                Err(error) => Err(transport_error(&error, operation)),
            };

            match result {
                Ok(response) => return Ok(response),
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
                format!("Bearer {}", self.options.key.0),
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

    fn check(&self, response: HttpResponse, operation: &str) -> ProviderResult<HttpResponse> {
        if response.is_success() {
            return Ok(response);
        }

        let error = serde_json::from_slice::<Value>(&response.body)
            .ok()
            .and_then(|body| body.pointer("/errors/0").cloned())
            .unwrap_or(Value::Null);
        let detail = error
            .get("detail")
            .or_else(|| error.get("title"))
            .and_then(Value::as_str);
        let message = detail.map_or_else(
            || format!("{operation} failed with HTTP {}", response.status),
            |detail| format!("{operation} failed: {detail}"),
        );
        let code = error.get("code").and_then(Value::as_str).unwrap_or("");
        // the rate-limit docs give the wait only in the body, as `meta.retry_after` seconds
        let body_retry_after = error
            .pointer("/meta/retry_after")
            .and_then(Value::as_u64)
            .map(|seconds| seconds.to_string());
        let retry_after = parse_retry_after(
            response
                .header("retry-after")
                .or(body_retry_after.as_deref()),
            self.runtime.now_millis(),
            self.options.retry.max_retry_after,
        );
        let request_id = response.header("x-request-id").map(str::to_owned);
        let classified = if code.eq_ignore_ascii_case("SERVERS_OUT_OF_STOCK") {
            ProviderError::new(ProviderErrorCode::NoCapacity, false, message)
        } else {
            classify_status(response.status, message)
        };

        Err(classified.with_response(response.status, retry_after, request_id))
    }

    fn url_for(&self, path: &str) -> ProviderResult<Url> {
        // IDs are validated as letters, digits, underscores and dashes before they get here
        url_within(&self.options.base_url, path)
    }
}

fn decode<Item: DeserializeOwned>(body: &[u8], operation: &str) -> ProviderResult<Item> {
    serde_json::from_slice(body).map_err(|error| {
        ProviderError::invalid_response(format!("{operation} returned an invalid body: {error}"))
    })
}
