//! Retry, backoff and URL rules shared by the provider REST clients
//!
//! Reads and deletes retry with bounded exponential backoff and honor `Retry-After`; creates
//! are never replayed; URLs, including pagination links, may not leave the API origin.

use std::time::Duration;

use url::Url;

use crate::{
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    http::{TransportError, TransportErrorKind},
};

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

impl RetryPolicy {
    /// Checks that every bound is positive and the base backoff fits under the maximum
    pub fn validate(self) -> ProviderResult<Self> {
        if self.max_attempts == 0
            || self.backoff_base.is_zero()
            || self.backoff_base > self.backoff_max
        {
            return Err(ProviderError::validation(
                "the retry policy must use positive bounded values",
            ));
        }

        Ok(self)
    }

    /// Returns the wait before retry `attempt` (1-based) after `error`
    ///
    /// A provider's `Retry-After` wins; otherwise the wait doubles from the base, plus up to
    /// one base of jitter, and never exceeds the maximum.
    #[must_use]
    pub fn delay(&self, attempt: u8, error: &ProviderError, random: u64) -> Duration {
        if let Some(retry_after) = error.retry_after {
            return retry_after;
        }

        let base = millis(self.backoff_base);
        let max = millis(self.backoff_max);
        let exponential = base
            .saturating_mul(2_u64.saturating_pow(u32::from(attempt.saturating_sub(1))))
            .min(max);
        let jitter = random % base.max(1);

        Duration::from_millis(exponential.saturating_add(jitter).min(max))
    }
}

/// Turns a transport failure into a retryable provider failure
pub(crate) fn transport_error(error: &TransportError, operation: &str) -> ProviderError {
    ProviderError::new(
        match error.kind {
            TransportErrorKind::Network => ProviderErrorCode::Network,
            TransportErrorKind::Timeout => ProviderErrorCode::Timeout,
        },
        true,
        format!("{operation} failed: {error}"),
    )
}

/// Validates a positive per-attempt timeout
pub(crate) fn request_timeout(timeout: Duration) -> ProviderResult<Duration> {
    if timeout.is_zero() {
        return Err(ProviderError::validation(
            "the request timeout must be positive",
        ));
    }

    Ok(timeout)
}

/// Resolves an API path or absolute link against `base`, refusing any other origin
pub(crate) fn url_within(base: &Url, path: &str) -> ProviderResult<Url> {
    let url = match Url::parse(path) {
        Ok(url) => url,
        Err(url::ParseError::RelativeUrlWithoutBase) => base
            .join(path.trim_start_matches('/'))
            .map_err(|_| ProviderError::validation("the API path is invalid"))?,
        Err(_) => return Err(ProviderError::validation("the API URL is invalid")),
    };

    if url.origin() != base.origin() {
        return Err(ProviderError::invalid_response(
            "a pagination link left the API origin",
        ));
    }

    Ok(url)
}

pub(crate) fn normalize_base_url(mut url: Url) -> ProviderResult<Url> {
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

pub(crate) fn millis(duration: Duration) -> u64 {
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
