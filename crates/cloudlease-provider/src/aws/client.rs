//! The EC2 Query API client over an injected transport
//!
//! Ported from rembuild. Every action is a signed form `POST` to the regional endpoint. Reads
//! and actions with a provider idempotency guarantee retry under the shared
//! [`RetryPolicy`]; actions whose replay could do something twice are sent once.

use std::time::Duration;

use quick_xml::{Reader, events::Event};
use url::{Url, form_urlencoded};

use super::signing::{AwsCredentials, AwsRegion, sign_query_request};
use crate::{
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    http::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, Runtime},
    rest::{RetryPolicy, parse_retry_after, request_timeout, transport_error},
};

const EC2_API_VERSION: &str = "2016-11-15";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// EC2 client settings
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ec2ClientOptions {
    credentials: AwsCredentials,
    region: AwsRegion,
    endpoint: Url,
    request_timeout: Duration,
    retry: RetryPolicy,
}

impl Ec2ClientOptions {
    /// Creates settings for the region's public EC2 endpoint
    pub fn new(credentials: AwsCredentials, region: AwsRegion) -> ProviderResult<Self> {
        let endpoint = Url::parse(&format!("https://ec2.{}.amazonaws.com/", region.as_str()))
            .map_err(|_| ProviderError::validation("the default AWS EC2 endpoint is invalid"))?;

        Ok(Self {
            credentials,
            region,
            endpoint,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            retry: RetryPolicy::default(),
        })
    }

    /// Replaces the endpoint, such as a fake API in tests
    pub fn with_endpoint(mut self, endpoint: Url) -> ProviderResult<Self> {
        validate_endpoint(&endpoint)?;
        self.endpoint = endpoint;
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

    /// Returns the region requests are signed for
    #[must_use]
    pub const fn region(&self) -> &AwsRegion {
        &self.region
    }
}

/// Whether a request may be sent again after a failure
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AwsRequestSafety {
    /// A read, safe to repeat
    ReadOnly,
    /// A write EC2 applies at most once, such as terminating an instance, or one that carries
    /// the same `ClientToken` on every attempt
    Idempotent,
    /// A write whose replay could act twice; sent once, and a lost response is left to the
    /// caller to resolve by reading state
    Ambiguous,
}

impl AwsRequestSafety {
    const fn allows_retry(self) -> bool {
        !matches!(self, Self::Ambiguous)
    }
}

/// A successful EC2 response
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ec2Response {
    /// The HTTP status
    pub status: u16,
    /// The raw XML body
    pub body: Vec<u8>,
    /// The EC2 request ID, from a header or the body
    pub request_id: Option<String>,
}

/// An EC2 Query client over an injected transport and runtime
pub struct Ec2Client<T, R> {
    options: Ec2ClientOptions,
    transport: T,
    runtime: R,
}

impl<T: HttpTransport, R: Runtime> Ec2Client<T, R> {
    /// Creates a client
    pub fn new(options: Ec2ClientOptions, transport: T, runtime: R) -> ProviderResult<Self> {
        validate_endpoint(&options.endpoint)?;

        Ok(Self {
            options,
            transport,
            runtime,
        })
    }

    /// Returns the client settings
    pub const fn options(&self) -> &Ec2ClientOptions {
        &self.options
    }

    /// Sends one EC2 action and returns its raw XML response
    ///
    /// Parameters are sent as a form body sorted by encoded pair; this method adds `Action`
    /// and `Version`. An [`AwsRequestSafety::Idempotent`] action that is not idempotent by
    /// itself must carry a `ClientToken`.
    pub async fn call(
        &self,
        action: &str,
        parameters: Vec<(String, String)>,
        safety: AwsRequestSafety,
    ) -> ProviderResult<Ec2Response> {
        let body = encode_query_body(action, parameters)?;
        let max_attempts = if safety.allows_retry() {
            self.options.retry.max_attempts
        } else {
            1
        };
        let mut attempt = 1_u8;

        loop {
            let error = match self.send_once(action, &body).await {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            if !error.retryable || attempt >= max_attempts {
                return Err(error);
            }

            let delay = self
                .options
                .retry
                .delay(attempt, &error, self.runtime.random_u64());
            self.runtime.sleep(delay).await;
            attempt = attempt.saturating_add(1);
        }
    }

    async fn send_once(&self, action: &str, body: &[u8]) -> ProviderResult<Ec2Response> {
        // each attempt is signed afresh because the signature covers the request time
        let headers = sign_query_request(
            &self.options.credentials,
            &self.options.region,
            &self.options.endpoint,
            body,
            self.runtime.now_millis(),
        )?;
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: self.options.endpoint.clone(),
            headers,
            body: Some(body.to_vec()),
            timeout: self.options.request_timeout,
        };

        let response = self
            .transport
            .send(request)
            .await
            .map_err(|error| transport_error(&error, &format!("AWS EC2 {action}")))?;
        if !response.is_success() {
            return Err(self.response_error(action, &response));
        }

        Ok(Ec2Response {
            status: response.status,
            request_id: response_request_id(&response),
            body: response.body,
        })
    }

    fn response_error(&self, action: &str, response: &HttpResponse) -> ProviderError {
        let xml = parse_error_fields(&response.body).ok();
        let request_id = response_request_id(response).or_else(|| xml.as_ref()?.request_id.clone());
        let provider_code = xml.as_ref().and_then(|fields| fields.code.as_deref());
        let (code, retryable) = classify_error(response.status, provider_code);
        let retry_after = parse_retry_after(
            response.header("retry-after"),
            self.runtime.now_millis(),
            self.options.retry.max_retry_after,
        );

        ProviderError::new(code, retryable, error_message(action, code, provider_code))
            .with_response(response.status, retry_after, request_id)
    }
}

fn encode_query_body(action: &str, parameters: Vec<(String, String)>) -> ProviderResult<Vec<u8>> {
    if action.trim().is_empty()
        || action
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte == b'&' || byte == b'=')
    {
        return Err(ProviderError::validation("AWS EC2 action is invalid"));
    }

    let mut encoded = Vec::with_capacity(parameters.len() + 2);
    encoded.push(encode_pair("Action", action));
    encoded.push(encode_pair("Version", EC2_API_VERSION));
    for (name, value) in parameters {
        if name.is_empty() || name == "Action" || name == "Version" {
            return Err(ProviderError::validation(
                "AWS EC2 parameter name is invalid",
            ));
        }
        encoded.push(encode_pair(&name, &value));
    }
    encoded.sort();

    Ok(encoded.join("&").into_bytes())
}

fn encode_pair(name: &str, value: &str) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    serializer.append_pair(name, value);

    serializer.finish()
}

fn response_request_id(response: &HttpResponse) -> Option<String> {
    ["x-amzn-requestid", "x-amzn-request-id", "x-amz-request-id"]
        .iter()
        .find_map(|name| response.header(name).map(str::to_owned))
        .or_else(|| parse_error_fields(&response.body).ok()?.request_id)
}

fn error_message(action: &str, code: ProviderErrorCode, provider_code: Option<&str>) -> String {
    // the provider code is echoed into messages, so keep only characters EC2 codes use
    let safe_code = provider_code
        .map(|value| {
            value
                .chars()
                .filter(|character| character.is_ascii_alphanumeric() || ".-_".contains(*character))
                .take(96)
                .collect::<String>()
        })
        .filter(|value| !value.is_empty());

    safe_code.map_or_else(
        || format!("AWS EC2 {action} request failed ({code:?})"),
        |value| format!("AWS EC2 {action} request failed with error code {value}"),
    )
}

/// Classifies an EC2 error code and HTTP status
///
/// Order matters: throttling and missing resources are recognized before the broad
/// `invalid` match, and Spot capacity before account limits, so a fleet pool refused for
/// capacity reads as [`ProviderErrorCode::NoCapacity`] but a quota does not. The adapter also
/// classifies each error of a fleet's HTTP 200 answer here.
pub(crate) fn classify_error(
    status: u16,
    provider_code: Option<&str>,
) -> (ProviderErrorCode, bool) {
    let code = provider_code.unwrap_or_default().to_ascii_lowercase();
    if status == 429 || code.contains("throttl") || code == "requestlimitexceeded" {
        return (ProviderErrorCode::RateLimited, true);
    }
    if code.contains("notfound") || status == 404 {
        return (ProviderErrorCode::NotFound, false);
    }
    if code.contains("unauthorized")
        || code.contains("accessdenied")
        || code.contains("authfailure")
        || code.contains("invalidclienttoken")
        || code.contains("signaturedoesnotmatch")
        || code.contains("expiredtoken")
        || status == 401
        || status == 403
    {
        return (ProviderErrorCode::PermissionDenied, false);
    }
    // retryable so a launch can fall through to the next instance type in its pool
    // a Spot price above the request's max price creates nothing either, so another type or
    // zone may still be under it
    if code.contains("insufficient") && (code.contains("capacity") || code.contains("spot"))
        || code.contains("unfulfillablecapacity")
        || code == "spotmaxpricetoolow"
        || code.contains("capacityreservation")
    {
        return (ProviderErrorCode::NoCapacity, true);
    }
    if code == "maxspotinstancecountexceeded" || code.contains("limitexceeded") {
        return (ProviderErrorCode::QuotaExceeded, false);
    }
    if code == "invalidpermission.duplicate" {
        return (ProviderErrorCode::Conflict, false);
    }
    if code.contains("dependencyviolation")
        || code == "invalidgroup.inuse"
        || code.contains("incorrectstate")
    {
        return (ProviderErrorCode::Conflict, true);
    }
    if code.contains("internalerror")
        || code.contains("serviceunavailable")
        || code.contains("requesttimeout")
        || status >= 500
    {
        return (ProviderErrorCode::ServerError, true);
    }
    if code.contains("invalid")
        || code.contains("malformed")
        || code.contains("missing")
        || code.contains("unsupported")
        || status == 400
    {
        return (ProviderErrorCode::Validation, false);
    }

    (ProviderErrorCode::ServerError, status >= 500)
}

#[derive(Default)]
struct ErrorFields {
    code: Option<String>,
    request_id: Option<String>,
}

fn parse_error_fields(body: &[u8]) -> Result<ErrorFields, ()> {
    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);
    let mut fields = ErrorFields::default();

    loop {
        match reader.read_event().map_err(|_| ())? {
            Event::Start(start) => {
                let name = start.local_name();
                if name.as_ref() == b"Code" && fields.code.is_none() {
                    fields.code = Some(read_text(&mut reader, &start)?);
                } else if matches!(name.as_ref(), b"RequestId" | b"RequestID" | b"requestId")
                    && fields.request_id.is_none()
                {
                    fields.request_id = Some(read_text(&mut reader, &start)?);
                }
            }
            Event::Eof => return Ok(fields),
            _ => {}
        }
    }
}

fn read_text(
    reader: &mut Reader<&[u8]>,
    start: &quick_xml::events::BytesStart<'_>,
) -> Result<String, ()> {
    let text = reader.read_text(start.name()).map_err(|_| ())?;

    text.decode()
        .map(|value| value.into_owned())
        .map_err(|_| ())
}

fn validate_endpoint(endpoint: &Url) -> ProviderResult<()> {
    // SigV4 signs the path `/`, so the endpoint must be a bare origin
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || (!endpoint.path().is_empty() && endpoint.path() != "/")
    {
        return Err(ProviderError::validation(
            "AWS EC2 endpoint must be an HTTPS origin without query or path",
        ));
    }

    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        future::Future,
        rc::Rc,
        task::{Context, Poll, Waker},
        time::Duration,
    };

    use super::{AwsRequestSafety, Ec2Client, Ec2ClientOptions};
    use crate::{
        aws::signing::{AwsCredentials, AwsRegion},
        error::ProviderErrorCode,
        http::{
            HttpMethod, HttpRequest, HttpResponse, HttpTransport, LocalFuture, Runtime,
            TransportError,
        },
        rest::RetryPolicy,
    };

    #[derive(Clone, Default)]
    pub(crate) struct FakeTransport {
        responses: Rc<RefCell<VecDeque<Result<HttpResponse, TransportError>>>>,
        pub(crate) requests: Rc<RefCell<Vec<HttpRequest>>>,
    }

    impl FakeTransport {
        pub(crate) fn with(
            responses: impl IntoIterator<Item = Result<HttpResponse, TransportError>>,
        ) -> Self {
            Self {
                responses: Rc::new(RefCell::new(responses.into_iter().collect())),
                requests: Rc::default(),
            }
        }

        pub(crate) fn bodies(&self) -> Vec<String> {
            self.requests
                .borrow()
                .iter()
                .map(|request| String::from_utf8_lossy(request.body.as_deref().unwrap()).into())
                .collect()
        }
    }

    impl HttpTransport for FakeTransport {
        fn send(
            &self,
            request: HttpRequest,
        ) -> LocalFuture<'_, Result<HttpResponse, TransportError>> {
            self.requests.borrow_mut().push(request);
            let response = self
                .responses
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(TransportError::network("missing fake response")));

            Box::pin(async move { response })
        }
    }

    #[derive(Clone, Default)]
    pub(crate) struct FakeRuntime {
        now: Rc<Cell<u64>>,
        sleeps: Rc<RefCell<Vec<Duration>>>,
    }

    impl Runtime for FakeRuntime {
        fn now_millis(&self) -> u64 {
            self.now.get()
        }

        fn sleep(&self, duration: Duration) -> LocalFuture<'_, ()> {
            self.sleeps.borrow_mut().push(duration);
            Box::pin(async {})
        }

        fn random_u64(&self) -> u64 {
            0
        }
    }

    pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
        let mut context = Context::from_waker(Waker::noop());
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
    }

    pub(crate) fn client(transport: FakeTransport) -> Ec2Client<FakeTransport, FakeRuntime> {
        client_with_runtime(transport, FakeRuntime::default())
    }

    fn client_with_runtime(
        transport: FakeTransport,
        runtime: FakeRuntime,
    ) -> Ec2Client<FakeTransport, FakeRuntime> {
        runtime.now.set(1_440_981_600_000);
        let credentials = AwsCredentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
        )
        .unwrap();
        let options = Ec2ClientOptions::new(credentials, AwsRegion::parse("us-east-1").unwrap())
            .unwrap()
            .with_retry_policy(RetryPolicy {
                max_attempts: 3,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(4),
                max_retry_after: Duration::from_secs(2),
            })
            .unwrap();

        Ec2Client::new(options, transport, runtime).unwrap()
    }

    pub(crate) fn ok(body: &[u8]) -> Result<HttpResponse, TransportError> {
        status(200, body)
    }

    pub(crate) fn status(status: u16, body: &[u8]) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        })
    }

    #[test]
    fn sends_a_sorted_query_body_and_returns_the_request_id() {
        let transport = FakeTransport::with([ok(
            br"<DescribeInstancesResponse><requestId>req-123</requestId></DescribeInstancesResponse>",
        )]);
        let client = client(transport.clone());

        let result = block_on(client.call(
            "DescribeInstances",
            vec![
                ("Z".to_owned(), "last".to_owned()),
                ("A".to_owned(), "first".to_owned()),
            ],
            AwsRequestSafety::ReadOnly,
        ))
        .unwrap();

        assert_eq!(result.request_id.as_deref(), Some("req-123"));
        let request = &transport.requests.borrow()[0];
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(
            transport.bodies()[0],
            "A=first&Action=DescribeInstances&Version=2016-11-15&Z=last"
        );
        assert!(request.headers.iter().any(|(name, value)| {
            name == "content-type" && value == "application/x-www-form-urlencoded; charset=UTF-8"
        }));
    }

    #[test]
    fn retries_read_transport_failures_with_a_finite_delay() {
        let transport = FakeTransport::with([
            Err(TransportError::network("temporary failure")),
            ok(b"<Response><requestId>ok</requestId></Response>"),
        ]);
        let runtime = FakeRuntime::default();
        let client = client_with_runtime(transport.clone(), runtime.clone());

        block_on(client.call("DescribeImages", Vec::new(), AwsRequestSafety::ReadOnly)).unwrap();

        assert_eq!(transport.requests.borrow().len(), 2);
        assert_eq!(
            runtime.sleeps.borrow().as_slice(),
            [Duration::from_millis(1)]
        );
    }

    #[test]
    fn does_not_replay_an_ambiguous_write() {
        let transport = FakeTransport::with([Err(TransportError::network("connection lost"))]);
        let client = client(transport.clone());

        let error = block_on(client.call("RunInstances", Vec::new(), AwsRequestSafety::Ambiguous))
            .unwrap_err();

        assert_eq!(error.code, ProviderErrorCode::Network);
        assert_eq!(transport.requests.borrow().len(), 1);
    }

    #[test]
    fn includes_the_ec2_action_in_provider_errors() {
        let transport = FakeTransport::with([status(
            403,
            br"<Response><Errors><Error><Code>UnauthorizedOperation</Code></Error></Errors></Response>",
        )]);
        let client = client(transport);

        let error = block_on(client.call(
            "CreateSecurityGroup",
            Vec::new(),
            AwsRequestSafety::ReadOnly,
        ))
        .unwrap_err();

        assert_eq!(error.code, ProviderErrorCode::PermissionDenied);
        assert!(error.message().contains("CreateSecurityGroup"));
    }

    #[test]
    fn classifies_duplicate_security_group_ingress_as_conflict() {
        let transport = FakeTransport::with([status(
            400,
            br"<Response><Errors><Error><Code>InvalidPermission.Duplicate</Code></Error></Errors></Response>",
        )]);
        let client = client(transport);

        let error = block_on(client.call(
            "AuthorizeSecurityGroupIngress",
            Vec::new(),
            AwsRequestSafety::Ambiguous,
        ))
        .unwrap_err();

        assert_eq!(error.code, ProviderErrorCode::Conflict);
        assert!(!error.retryable);
        assert!(error.message().contains("InvalidPermission.Duplicate"));
    }

    #[test]
    fn classifies_spot_capacity_separately_from_account_quota() {
        let transport = FakeTransport::with([
            status(
                500,
                br"<Response><Errors><Error><Code>InsufficientInstanceCapacity</Code></Error></Errors></Response>",
            ),
            status(
                400,
                br"<Response><Errors><Error><Code>MaxSpotInstanceCountExceeded</Code></Error></Errors></Response>",
            ),
            status(
                400,
                br"<Response><Errors><Error><Code>SpotMaxPriceTooLow</Code></Error></Errors></Response>",
            ),
        ]);
        let client = client(transport);

        let capacity =
            block_on(client.call("RunInstances", Vec::new(), AwsRequestSafety::Ambiguous))
                .unwrap_err();
        let quota = block_on(client.call("RunInstances", Vec::new(), AwsRequestSafety::Ambiguous))
            .unwrap_err();
        let price = block_on(client.call("RunInstances", Vec::new(), AwsRequestSafety::Ambiguous))
            .unwrap_err();

        assert_eq!(
            (capacity.code, capacity.retryable),
            (ProviderErrorCode::NoCapacity, true)
        );
        assert_eq!(
            (quota.code, quota.retryable),
            (ProviderErrorCode::QuotaExceeded, false)
        );
        // a price over the cap created nothing, so the launch may try another type
        assert_eq!(
            (price.code, price.retryable),
            (ProviderErrorCode::NoCapacity, true)
        );
    }
}
