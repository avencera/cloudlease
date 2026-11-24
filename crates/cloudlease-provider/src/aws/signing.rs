//! AWS credential validation and SigV4 signing for EC2 Query requests

use std::fmt;

use chrono::DateTime;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::{ProviderError, ProviderResult};

const AWS_ALGORITHM: &str = "AWS4-HMAC-SHA256";
const AWS_SERVICE: &str = "ec2";
const AWS_TERMINATOR: &str = "aws4_request";
pub(super) const CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=UTF-8";

/// Credentials that sign AWS requests
///
/// The secret access key and session token never appear in debug output. A session token is
/// required when the credentials come from a temporary role session.
#[derive(Clone, Eq, PartialEq)]
pub struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

impl AwsCredentials {
    /// Validates static or temporary AWS credentials
    pub fn new(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: Option<String>,
    ) -> ProviderResult<Self> {
        let access_key_id = access_key_id.into();
        let secret_access_key = secret_access_key.into();

        if access_key_id.trim().is_empty() {
            return Err(ProviderError::validation(
                "AWS access key ID must not be empty",
            ));
        }
        if secret_access_key.is_empty() {
            return Err(ProviderError::validation(
                "AWS secret access key must not be empty",
            ));
        }
        if access_key_id.chars().any(char::is_whitespace)
            || secret_access_key.chars().any(char::is_whitespace)
        {
            return Err(ProviderError::validation(
                "AWS credentials must not contain whitespace",
            ));
        }

        let session_token = session_token
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty());
        if session_token
            .as_deref()
            .is_some_and(|token| token.chars().any(char::is_whitespace))
        {
            return Err(ProviderError::validation(
                "AWS session token must not contain whitespace",
            ));
        }

        Ok(Self {
            access_key_id,
            secret_access_key,
            session_token,
        })
    }

    /// Returns the non-secret access key ID
    #[must_use]
    pub fn access_key_id(&self) -> &str {
        &self.access_key_id
    }
}

impl fmt::Debug for AwsCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// An AWS region name, which is also the SigV4 credential scope
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AwsRegion(String);

impl AwsRegion {
    /// Validates a region name such as `us-east-1`
    pub fn parse(value: impl Into<String>) -> ProviderResult<Self> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 63
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !valid {
            return Err(ProviderError::validation("AWS region is invalid"));
        }

        Ok(Self(value))
    }

    /// Returns the region name
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AwsRegion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Signs an EC2 Query request and returns the headers to send with it
pub(super) fn sign_query_request(
    credentials: &AwsCredentials,
    region: &AwsRegion,
    endpoint: &Url,
    body: &[u8],
    now_millis: u64,
) -> ProviderResult<Vec<(String, String)>> {
    let host = canonical_host(endpoint)?;
    let seconds = i64::try_from(now_millis / 1_000).map_err(|_| {
        ProviderError::validation("AWS signing time is outside the supported range")
    })?;
    let timestamp = DateTime::from_timestamp(seconds, 0)
        .ok_or_else(|| ProviderError::validation("AWS signing time is invalid"))?;
    let amz_date = timestamp.format("%Y%m%dT%H%M%SZ").to_string();
    let short_date = timestamp.format("%Y%m%d").to_string();
    let payload_hash = sha256_hex(body);

    let mut canonical_headers = vec![
        ("content-type", CONTENT_TYPE.to_owned()),
        ("host", host),
        ("x-amz-date", amz_date.clone()),
    ];
    if let Some(session_token) = &credentials.session_token {
        canonical_headers.push((
            "x-amz-security-token",
            normalize_header_value(session_token),
        ));
    }
    canonical_headers.sort_by(|left, right| left.0.cmp(right.0));

    let canonical_headers_text = canonical_headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    let signed_headers = canonical_headers
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request =
        format!("POST\n/\n\n{canonical_headers_text}\n{signed_headers}\n{payload_hash}");
    let credential_scope = format!(
        "{short_date}/{}/{AWS_SERVICE}/{AWS_TERMINATOR}",
        region.as_str()
    );
    let string_to_sign = format!(
        "{AWS_ALGORITHM}\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let signing_key = derive_signing_key(
        &credentials.secret_access_key,
        &short_date,
        region.as_str(),
        AWS_SERVICE,
    );
    let signature = hex_lower(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "{AWS_ALGORITHM} Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    );

    // the Fetch API sets `host` itself and refuses it as a request header
    let mut headers = vec![
        ("content-type".to_owned(), CONTENT_TYPE.to_owned()),
        ("x-amz-date".to_owned(), amz_date),
        ("authorization".to_owned(), authorization),
    ];
    if let Some(session_token) = &credentials.session_token {
        headers.push(("x-amz-security-token".to_owned(), session_token.clone()));
    }

    Ok(headers)
}

fn canonical_host(endpoint: &Url) -> ProviderResult<String> {
    let host = endpoint
        .host_str()
        .ok_or_else(|| ProviderError::validation("AWS endpoint must include a host"))?
        .to_ascii_lowercase();
    let port = endpoint.port().filter(|port| *port != 443);

    Ok(port.map_or(host.clone(), |port| format!("{host}:{port}")))
}

fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let date_key = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let region_key = hmac_sha256(&date_key, region.as_bytes());
    let service_key = hmac_sha256(&region_key, service.as_bytes());

    hmac_sha256(&service_key, AWS_TERMINATOR.as_bytes())
}

fn hmac_sha256(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts every key length");
    mac.update(value);

    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(value: &[u8]) -> String {
    hex_lower(&Sha256::digest(value))
}

pub(super) fn hex_lower(value: &[u8]) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value {
        // writing to a String cannot fail
        let _ = write!(encoded, "{byte:02x}");
    }

    encoded
}

fn normalize_header_value(value: &str) -> String {
    value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{
        AwsCredentials, AwsRegion, derive_signing_key, hex_lower, hmac_sha256, sign_query_request,
    };

    #[test]
    fn matches_the_published_sigv4_key_vector() {
        let signing_key = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        let string_to_sign = "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/iam/aws4_request\nf536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59";

        assert_eq!(
            hex_lower(&hmac_sha256(&signing_key, string_to_sign.as_bytes())),
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn redacts_secret_material_from_debug_output() {
        let credentials = AwsCredentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            Some("session-secret".to_owned()),
        )
        .unwrap();

        let debug = format!("{credentials:?}");
        assert!(debug.contains("AKIDEXAMPLE"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("session-secret"));
        assert!(!debug.contains("wJalrXUtnFEMI"));
    }

    #[test]
    fn rejects_invalid_regions_and_credentials() {
        assert!(AwsRegion::parse("us east 1").is_err());
        assert!(AwsCredentials::new("", "secret", None).is_err());
        assert!(AwsCredentials::new("access", "secret key", None).is_err());
    }

    #[test]
    fn signs_temporary_credentials_without_emitting_a_host_header() {
        let credentials = AwsCredentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            Some("token".to_owned()),
        )
        .unwrap();
        let region = AwsRegion::parse("us-east-1").unwrap();
        let endpoint = Url::parse("https://ec2.us-east-1.amazonaws.com/").unwrap();
        let headers = sign_query_request(
            &credentials,
            &region,
            &endpoint,
            b"Action=DescribeInstances&Version=2016-11-15",
            1_440_981_600_000,
        )
        .unwrap();

        let header = |name: &str| {
            headers
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("x-amz-date"), Some("20150831T004000Z"));
        assert!(
            header("authorization").is_some_and(|value| value.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150831/us-east-1/ec2/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, Signature="
            ))
        );
        assert_eq!(header("x-amz-security-token"), Some("token"));
        assert_eq!(header("host"), None);
    }
}
