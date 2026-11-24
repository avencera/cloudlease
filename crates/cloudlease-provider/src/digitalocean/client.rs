//! A narrow DigitalOcean v2 REST client over an injected transport
//!
//! Ported from rembuild's client: reads and deletes retry with bounded exponential backoff
//! and honor `Retry-After`; creates are never replayed, because DigitalOcean documents no
//! idempotency for them; pagination is bounded and may not leave the API origin.

use std::time::Duration;

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use url::Url;

use crate::{
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    http::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, Runtime, TransportErrorKind},
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

/// Bounded retries for reads and deletes
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    /// Attempts per request, at least one
    pub max_attempts: u8,
    /// The first backoff, before jitter
    pub backoff_base: Duration,
    /// The longest backoff, including jitter
    pub backoff_max: Duration,
    /// The longest `Retry-After` honored
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            backoff_base: Duration::from_millis(200),
            backoff_max: Duration::from_secs(5),
            max_retry_after: Duration::from_secs(30),
        }
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
        if timeout.is_zero() {
            return Err(ProviderError::validation(
                "the request timeout must be positive",
            ));
        }

        self.request_timeout = timeout;
        Ok(self)
    }

    /// Replaces the retry policy
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> ProviderResult<Self> {
        if retry.max_attempts == 0
            || retry.backoff_base.is_zero()
            || retry.backoff_base > retry.backoff_max
        {
            return Err(ProviderError::validation(
                "the retry policy must use positive bounded values",
            ));
        }

        self.retry = retry;
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
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A block storage volume as the API returns it
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct WireVolume {
    /// The UUID
    pub id: String,
    /// The name
    pub name: String,
    /// The droplets it is attached to
    #[serde(default)]
    pub droplet_ids: Vec<u64>,
    /// The tags, which the API may return as `null`
    #[serde(default, deserialize_with = "null_as_empty")]
    pub tags: Vec<String>,
}

fn null_as_empty<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
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

    /// Lists every volume in the account
    ///
    /// The volumes endpoint has no tag filter, so callers filter by tag.
    pub async fn list_volumes(&self) -> ProviderResult<Vec<WireVolume>> {
        let mut url = self.url_for("volumes")?;
        url.query_pairs_mut().append_pair("per_page", PAGE_SIZE);

        self.list(url, "list volumes", "volumes").await
    }

    /// Reads one volume, or `None` when it does not exist
    pub async fn get_volume(&self, id: &str) -> ProviderResult<Option<WireVolume>> {
        let url = self.resource_url("volumes", id)?;

        self.get_optional(url, "read volume", "volume").await
    }

    /// Creates a volume and returns it; never replayed
    pub async fn create_volume(&self, body: Value) -> ProviderResult<WireVolume> {
        let url = self.url_for("volumes")?;
        let value = self
            .request(url, "create volume", HttpMethod::Post, Some(body))
            .await?;

        decode_root(&value, "volume")
    }

    /// Requests deletion of a volume, treating an absent volume as success
    pub async fn delete_volume(&self, id: &str) -> ProviderResult<()> {
        let url = self.resource_url("volumes", id)?;

        self.delete(url, "delete volume").await
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
                self.runtime
                    .sleep(self.retry_delay(attempt - 1, error))
                    .await;
            }

            let request = self.http_request(method, url.clone(), body.clone());
            let result = match self.transport.send(request).await {
                Ok(response) => self.decode_response(&response, operation),
                Err(error) => Err(ProviderError::new(
                    match error.kind {
                        TransportErrorKind::Network => ProviderErrorCode::Network,
                        TransportErrorKind::Timeout => ProviderErrorCode::Timeout,
                    },
                    true,
                    format!("{operation} failed: {error}"),
                )),
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

    fn retry_delay(&self, attempt: u8, error: &ProviderError) -> Duration {
        if let Some(retry_after) = error.retry_after {
            return retry_after;
        }

        let retry = self.options.retry;
        let base = millis(retry.backoff_base);
        let max = millis(retry.backoff_max);
        let exponential = base
            .saturating_mul(2_u64.saturating_pow(u32::from(attempt.saturating_sub(1))))
            .min(max);
        let jitter = self.runtime.random_u64() % base.max(1);

        Duration::from_millis(exponential.saturating_add(jitter).min(max))
    }

    fn url_for(&self, path: &str) -> ProviderResult<Url> {
        let url = match Url::parse(path) {
            Ok(url) => url,
            Err(url::ParseError::RelativeUrlWithoutBase) => self
                .options
                .base_url
                .join(path.trim_start_matches('/'))
                .map_err(|_| ProviderError::validation("the API path is invalid"))?,
            Err(_) => return Err(ProviderError::validation("the API URL is invalid")),
        };

        if url.origin() != self.options.base_url.origin() {
            return Err(ProviderError::invalid_response(
                "a pagination link left the API origin",
            ));
        }

        Ok(url)
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

fn normalize_base_url(mut url: Url) -> ProviderResult<Url> {
    if url.scheme() != "https"
        || url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::validation(
            "the API base URL must be an HTTPS URL without credentials, query or fragment",
        ));
    }

    // a trailing slash makes `join` append to the path instead of replacing its last segment
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }

    Ok(url)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Classifies an HTTP status with safe retry semantics
#[must_use]
pub fn classify_status(status: u16, message: impl Into<String>) -> ProviderError {
    let (code, retryable) = match status {
        404 => (ProviderErrorCode::NotFound, false),
        401 | 403 => (ProviderErrorCode::PermissionDenied, false),
        409 => (ProviderErrorCode::Conflict, false),
        429 => (ProviderErrorCode::RateLimited, true),
        500..=599 => (ProviderErrorCode::ServerError, true),
        _ => (ProviderErrorCode::Validation, false),
    };

    ProviderError::new(code, retryable, message)
}

/// Parses a `Retry-After` header given in seconds or as an HTTP date, capped at `maximum`
#[must_use]
pub fn parse_retry_after(
    value: Option<&str>,
    now_millis: u64,
    maximum: Duration,
) -> Option<Duration> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }

    if let Ok(seconds) = value.parse::<f64>()
        && seconds.is_finite()
        && seconds >= 0.0
    {
        // a huge value saturates to the cap rather than overflowing
        return Some(
            Duration::try_from_secs_f64(seconds)
                .unwrap_or(maximum)
                .min(maximum),
        );
    }

    let retry_at = parse_imf_fixdate_millis(value)?;
    Some(Duration::from_millis(retry_at.saturating_sub(now_millis)).min(maximum))
}

fn parse_imf_fixdate_millis(value: &str) -> Option<u64> {
    let mut parts = value.split_ascii_whitespace();
    if !parts.next()?.ends_with(',') {
        return None;
    }

    let day = parts.next()?.parse::<u8>().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = parts.next()?.parse::<i64>().ok()?;
    let time = parts.next()?;
    if parts.next()? != "GMT" || parts.next().is_some() {
        return None;
    }

    let mut clock = time.split(':');
    let hour = clock.next()?.parse::<u8>().ok()?;
    let minute = clock.next()?.parse::<u8>().ok()?;
    let second = clock.next()?.parse::<u8>().ok()?;
    if clock.next().is_some() || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 59
    {
        return None;
    }

    let days = days_from_civil(year, month, day)?;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second))?;

    u64::try_from(seconds).ok()?.checked_mul(1_000)
}

fn days_from_civil(year: i64, month: u8, day: u8) -> Option<i64> {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    if !(0..=365).contains(&day_of_year) {
        return None;
    }

    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{classify_status, parse_retry_after};
    use crate::error::ProviderErrorCode;

    #[test]
    fn statuses_map_to_safe_retry_classes() {
        let cases = [
            (404, ProviderErrorCode::NotFound, false),
            (401, ProviderErrorCode::PermissionDenied, false),
            (409, ProviderErrorCode::Conflict, false),
            (422, ProviderErrorCode::Validation, false),
            (429, ProviderErrorCode::RateLimited, true),
            (503, ProviderErrorCode::ServerError, true),
        ];

        for (status, code, retryable) in cases {
            let error = classify_status(status, "failed");
            assert_eq!((error.code, error.retryable), (code, retryable), "{status}");
        }
    }

    #[test]
    fn retry_after_accepts_seconds_and_dates_within_the_cap() {
        let cap = Duration::from_secs(30);
        // Tue, 24 Nov 2026 12:00:00 GMT
        let now = 1_795_521_600_000;

        assert_eq!(
            parse_retry_after(Some("2"), now, cap),
            Some(Duration::from_secs(2))
        );
        assert_eq!(parse_retry_after(Some("600"), now, cap), Some(cap));
        assert_eq!(parse_retry_after(Some("1e300"), now, cap), Some(cap));
        assert_eq!(
            parse_retry_after(Some("Tue, 24 Nov 2026 12:00:10 GMT"), now, cap),
            Some(Duration::from_secs(10))
        );
        assert_eq!(parse_retry_after(Some("soon"), now, cap), None);
        assert_eq!(parse_retry_after(None, now, cap), None);
    }
}
