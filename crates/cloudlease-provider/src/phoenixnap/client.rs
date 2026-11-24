//! A narrow phoenixNAP Bare Metal Cloud REST client over an injected transport
//!
//! It follows the shared [`rest`](crate::rest) rules with two phoenixNAP specifics:
//!
//! - Every request carries an OAuth bearer token from [`auth`](super::auth). When the API
//!   answers 401, the token is dropped and the request is sent once more with a new one.
//!   This also applies to a create, because a 401 comes from the API gateway before
//!   phoenixNAP acts on the request.
//! - phoenixNAP answers 403, not 404, for a server ID it does not know, so a read never
//!   treats an error status as proof of absence; callers confirm absence with a list.
//!
//! The server and tag lists are bare JSON arrays with no pagination (checked against the
//! BMC and Tag Manager v1 specs and live on 2026-11-24).

use std::time::Duration;

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use url::{Url, form_urlencoded};

use super::auth::{PnapCredentials, TokenCache, TokenResponse};
use crate::{
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    http::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, Runtime},
    rest::{
        RetryPolicy, classify_status, normalize_base_url, parse_retry_after, request_timeout,
        transport_error, url_within,
    },
};

const DEFAULT_API_URL: &str = "https://api.phoenixnap.com/";
const DEFAULT_TOKEN_URL: &str =
    "https://auth.phoenixnap.com/auth/realms/BMC/protocol/openid-connect/token";
// a bare metal create can take a while to return its server
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
// a token is replaced this long before it expires, so one fetched just before a slow
// request does not expire in flight
const DEFAULT_TOKEN_MARGIN: Duration = Duration::from_secs(60);

/// Client settings
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientOptions {
    credentials: PnapCredentials,
    api_url: Url,
    token_url: Url,
    request_timeout: Duration,
    token_margin: Duration,
    retry: RetryPolicy,
}

impl ClientOptions {
    /// Creates settings for the public phoenixNAP API
    pub fn new(credentials: PnapCredentials) -> ProviderResult<Self> {
        let invalid = |_| ProviderError::validation("a default phoenixNAP URL is invalid");

        Ok(Self {
            credentials,
            api_url: Url::parse(DEFAULT_API_URL).map_err(invalid)?,
            token_url: Url::parse(DEFAULT_TOKEN_URL).map_err(invalid)?,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            token_margin: DEFAULT_TOKEN_MARGIN,
            retry: RetryPolicy::default(),
        })
    }

    /// Replaces the API root that `bmc/v1/` and `tag-manager/v1/` hang off, such as a fake
    /// API in tests
    pub fn with_api_url(mut self, api_url: Url) -> ProviderResult<Self> {
        self.api_url = normalize_base_url(api_url)?;
        Ok(self)
    }

    /// Replaces the OAuth token endpoint
    pub fn with_token_url(mut self, token_url: Url) -> ProviderResult<Self> {
        if token_url.scheme() != "https"
            || !token_url.username().is_empty()
            || token_url.password().is_some()
            || token_url.fragment().is_some()
        {
            return Err(ProviderError::validation(
                "the token URL must be an HTTPS URL without credentials or fragment",
            ));
        }

        self.token_url = token_url;
        Ok(self)
    }

    /// Replaces the per-attempt timeout
    pub fn with_request_timeout(mut self, timeout: Duration) -> ProviderResult<Self> {
        self.request_timeout = request_timeout(timeout)?;
        Ok(self)
    }

    /// Replaces the retry policy for reads, deletes and token fetches
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> ProviderResult<Self> {
        self.retry = retry.validate()?;
        Ok(self)
    }
}

/// A tag on a server as the API returns it
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct WireTag {
    /// The tag name
    pub name: String,
    /// The value, which phoenixNAP may omit or send as `null`
    #[serde(default)]
    pub value: Option<String>,
}

/// A server as the API returns it
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct WireServer {
    /// The 24-hex-digit ID
    pub id: String,
    /// The hostname
    pub hostname: String,
    /// The status, such as `creating`, `powered-on` or `deleting`
    pub status: String,
    /// The tag assignments
    #[serde(default, deserialize_with = "null_as_empty")]
    pub tags: Vec<WireTag>,
}

#[derive(Deserialize)]
struct WireTagDefinition {
    name: String,
}

fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// How a request may be repeated
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Replay {
    // reads, deletes and token fetches retry under the retry policy
    Retry,
    // a create is sent once, apart from the 401 resend
    Once,
}

/// A phoenixNAP Bare Metal Cloud client
pub struct PhoenixNapClient<T, R> {
    options: ClientOptions,
    transport: T,
    runtime: R,
    token: TokenCache,
}

impl<T: HttpTransport, R: Runtime> PhoenixNapClient<T, R> {
    /// Creates a client over a transport and runtime
    pub fn new(options: ClientOptions, transport: T, runtime: R) -> Self {
        Self {
            options,
            transport,
            runtime,
            token: TokenCache::default(),
        }
    }

    /// Returns the runtime the client sleeps with
    pub const fn runtime(&self) -> &R {
        &self.runtime
    }

    /// Lists every server in the account
    pub async fn list_servers(&self) -> ProviderResult<Vec<WireServer>> {
        let url = self.url_for("bmc/v1/servers")?;

        self.json(url, "list servers", HttpMethod::Get, None, Replay::Retry)
            .await
    }

    /// Reads one server, or `None` when the API refuses it
    ///
    /// `None` is not proof of absence: phoenixNAP answers 403 both for an unknown ID and
    /// for a server the credentials may not read.
    pub async fn get_server(&self, id: &str) -> ProviderResult<Option<WireServer>> {
        let url = self.url_for(&format!("bmc/v1/servers/{id}"))?;

        match self
            .json(url, "read server", HttpMethod::Get, None, Replay::Retry)
            .await
        {
            Ok(server) => Ok(Some(server)),
            Err(error) if refused(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Creates a server and returns it; never replayed
    pub async fn create_server(&self, body: Value) -> ProviderResult<WireServer> {
        let url = self.url_for("bmc/v1/servers")?;

        self.json(
            url,
            "create server",
            HttpMethod::Post,
            Some(body),
            Replay::Once,
        )
        .await
    }

    /// Requests deprovisioning of a server and deletes its public IP blocks
    ///
    /// The deprecated `DELETE /servers/{id}` is not used. Without `deleteIpBlocks` the IP
    /// block bought at create would stay on the account and keep billing.
    pub async fn deprovision_server(&self, id: &str) -> ProviderResult<()> {
        let url = self.url_for(&format!("bmc/v1/servers/{id}/actions/deprovision"))?;
        let body = json!({ "deleteIpBlocks": true });

        // deprovisioning twice ends the same way, so a lost response may be resent
        self.request(
            url,
            "deprovision server",
            HttpMethod::Post,
            Some(body),
            Replay::Retry,
        )
        .await
        .map(drop)
    }

    /// Creates the Tag Manager tag `name` unless it already exists
    ///
    /// phoenixNAP's docs point to creating a tag before assigning it and do not say whether
    /// assignment creates one, so the adapter makes sure each of its tag names exists.
    pub async fn ensure_tag(&self, name: &str) -> ProviderResult<()> {
        let mut url = self.url_for("tag-manager/v1/tags")?;
        url.query_pairs_mut().append_pair("name", name);
        let existing: Vec<WireTagDefinition> = self
            .json(url, "list tags", HttpMethod::Get, None, Replay::Retry)
            .await?;
        if existing.iter().any(|tag| tag.name == name) {
            return Ok(());
        }

        let url = self.url_for("tag-manager/v1/tags")?;
        let body = json!({
            "name": name,
            "description": "Managed by cloudlease",
            "isBillingTag": false,
        });
        // a create that raced another one, or whose response was lost, reports 409
        match self
            .request(
                url,
                "create tag",
                HttpMethod::Post,
                Some(body),
                Replay::Once,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if error.code == ProviderErrorCode::Conflict => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn json<Item: DeserializeOwned>(
        &self,
        url: Url,
        operation: &str,
        method: HttpMethod,
        body: Option<Value>,
        replay: Replay,
    ) -> ProviderResult<Item> {
        let response = self.request(url, operation, method, body, replay).await?;

        serde_json::from_slice(&response.body).map_err(|error| {
            ProviderError::invalid_response(format!(
                "{operation} returned an invalid body: {error}"
            ))
        })
    }

    async fn request(
        &self,
        url: Url,
        operation: &str,
        method: HttpMethod,
        body: Option<Value>,
        replay: Replay,
    ) -> ProviderResult<HttpResponse> {
        let body = body
            .map(|body| serde_json::to_vec(&body))
            .transpose()
            .map_err(|_| ProviderError::validation("the request body could not be encoded"))?;
        let attempts = match replay {
            Replay::Retry => self.options.retry.max_attempts,
            Replay::Once => 1,
        };

        let mut last_error: Option<ProviderError> = None;
        let mut resent_after_401 = false;
        let mut attempt = 0;
        while attempt < attempts {
            attempt += 1;
            if let Some(error) = &last_error {
                let delay = self
                    .options
                    .retry
                    .delay(attempt - 1, error, self.runtime.random_u64());
                self.runtime.sleep(delay).await;
            }

            let token = self.bearer().await?;
            let request = self.http_request(method, url.clone(), &token, body.clone());
            let result = match self.transport.send(request).await {
                Ok(response) => self.check(response, operation),
                Err(error) => Err(transport_error(&error, operation)),
            };

            match result {
                Ok(response) => return Ok(response),
                Err(error) if error.status == Some(401) && !resent_after_401 => {
                    // the token expired or was revoked early; a resend with a new token
                    // does not use up a retry
                    self.token.clear();
                    resent_after_401 = true;
                    attempt -= 1;
                    last_error = None;
                }
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

    async fn bearer(&self) -> ProviderResult<String> {
        if let Some(token) = self.token.current(self.runtime.now_millis()) {
            return Ok(token);
        }

        let issued = self.runtime.now_millis();
        let response: TokenResponse = self.fetch_token().await?;
        let value = response.access_token.clone();
        self.token
            .store(response, issued, self.options.token_margin);

        Ok(value)
    }

    async fn fetch_token(&self) -> ProviderResult<TokenResponse> {
        let operation = "fetch token";
        let body = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "client_credentials")
            .finish()
            .into_bytes();

        let mut last_error: Option<ProviderError> = None;
        for attempt in 1..=self.options.retry.max_attempts {
            if let Some(error) = &last_error {
                let delay = self
                    .options
                    .retry
                    .delay(attempt - 1, error, self.runtime.random_u64());
                self.runtime.sleep(delay).await;
            }

            let request = HttpRequest {
                method: HttpMethod::Post,
                url: self.options.token_url.clone(),
                headers: vec![
                    ("accept".to_owned(), "application/json".to_owned()),
                    (
                        "authorization".to_owned(),
                        self.options.credentials.basic_authorization(),
                    ),
                    (
                        "content-type".to_owned(),
                        "application/x-www-form-urlencoded".to_owned(),
                    ),
                ],
                body: Some(body.clone()),
                timeout: self.options.request_timeout,
            };
            let result = match self.transport.send(request).await {
                Ok(response) => self.check(response, operation),
                Err(error) => Err(transport_error(&error, operation)),
            };

            match result {
                Ok(response) => {
                    return serde_json::from_slice(&response.body).map_err(|_| {
                        ProviderError::invalid_response("the token endpoint returned no token")
                    });
                }
                Err(error) if error.retryable => last_error = Some(error),
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

    fn http_request(
        &self,
        method: HttpMethod,
        url: Url,
        token: &str,
        body: Option<Vec<u8>>,
    ) -> HttpRequest {
        let mut headers = vec![
            ("accept".to_owned(), "application/json".to_owned()),
            ("authorization".to_owned(), format!("Bearer {token}")),
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

        let message = error_message(&response.body).map_or_else(
            || format!("{operation} failed with HTTP {}", response.status),
            |message| format!("{operation} failed: {message}"),
        );
        let retry_after = parse_retry_after(
            response.header("retry-after"),
            self.runtime.now_millis(),
            self.options.retry.max_retry_after,
        );
        let error = match response.status {
            // phoenixNAP answers 406 when the server type has no stock at the location
            406 => ProviderError::new(ProviderErrorCode::NoCapacity, false, message),
            status => classify_status(status, message),
        };

        Err(error.with_response(response.status, retry_after, None))
    }

    fn url_for(&self, path: &str) -> ProviderResult<Url> {
        url_within(&self.options.api_url, path)
    }
}

/// Reports whether a read failed in a way phoenixNAP also uses for an unknown ID
fn refused(error: &ProviderError) -> bool {
    matches!(error.status, Some(400 | 403 | 404))
}

/// Reads the message from either phoenixNAP's error body or its API gateway's fault body
fn error_message(body: &[u8]) -> Option<String> {
    let body: Value = serde_json::from_slice(body).ok()?;
    let message = body
        .get("message")
        .or_else(|| body.pointer("/fault/faultstring"))
        .and_then(Value::as_str)?;
    let details: Vec<_> = body
        .get("validationErrors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();

    Some(if details.is_empty() {
        message.to_owned()
    } else {
        format!("{message} ({})", details.join("; "))
    })
}
