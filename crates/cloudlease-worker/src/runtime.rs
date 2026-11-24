//! A Fetch transport and runtime for provider clients inside a Worker

use std::{
    future::{Future, poll_fn},
    task::Poll,
    time::Duration,
};

use cloudlease_core::time::UnixMillis;
use cloudlease_provider::http::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, LocalFuture, Runtime, TransportError,
};
use worker::{AbortController, Date, Delay, Fetch, Headers, Method, RequestInit};

/// Returns the current time from `Date.now()`
#[must_use]
pub fn now() -> UnixMillis {
    UnixMillis::new(Date::now().as_millis()).unwrap_or(UnixMillis::EPOCH)
}

/// Sends provider requests with `fetch`, aborting each at its timeout
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkersFetchTransport;

impl HttpTransport for WorkersFetchTransport {
    fn send(&self, request: HttpRequest) -> LocalFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(send(request))
    }
}

async fn send(request: HttpRequest) -> Result<HttpResponse, TransportError> {
    let headers = Headers::new();
    for (name, value) in &request.headers {
        headers
            .set(name, value)
            .map_err(|error| TransportError::network(format!("set header: {error}")))?;
    }

    let mut init = RequestInit::new();
    init.with_method(method(request.method))
        .with_headers(headers);
    if let Some(body) = request.body {
        let body = String::from_utf8(body)
            .map_err(|_| TransportError::network("the request body is not UTF-8"))?;
        init.with_body(Some(body.into()));
    }

    let outbound = worker::Request::new_with_init(request.url.as_str(), &init)
        .map_err(|error| TransportError::network(format!("build request: {error}")))?;
    let controller = AbortController::default();
    let signal = controller.signal();
    let fetch = Fetch::Request(outbound);
    let mut response = std::pin::pin!(fetch.send_with_signal(&signal));
    let mut timeout = std::pin::pin!(Delay::from(request.timeout));
    let mut controller = Some(controller);
    let mut timed_out = false;

    // poll the fetch first so a response that is already ready wins over the timeout
    let result = poll_fn(|context| {
        if let Poll::Ready(result) = response.as_mut().poll(context) {
            return Poll::Ready(result);
        }
        if !timed_out && timeout.as_mut().poll(context).is_ready() {
            timed_out = true;
            if let Some(controller) = controller.take() {
                controller.abort();
            }
        }

        Poll::Pending
    })
    .await;
    if timed_out {
        return Err(TransportError::timeout(format!(
            "the request timed out after {}ms",
            request.timeout.as_millis()
        )));
    }

    let mut response =
        result.map_err(|error| TransportError::network(format!("send request: {error}")))?;
    let status = response.status_code();
    let headers = response.headers().entries().collect();
    let body = response
        .bytes()
        .await
        .map_err(|error| TransportError::network(format!("read response: {error}")))?;

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

const fn method(method: HttpMethod) -> Method {
    match method {
        HttpMethod::Get => Method::Get,
        HttpMethod::Post => Method::Post,
        HttpMethod::Delete => Method::Delete,
    }
}

/// `Date.now()`, timer delays and `crypto.getRandomValues`
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkersRuntime;

impl Runtime for WorkersRuntime {
    fn now_millis(&self) -> u64 {
        Date::now().as_millis()
    }

    fn sleep(&self, duration: Duration) -> LocalFuture<'_, ()> {
        Box::pin(Delay::from(duration))
    }

    fn random_u64(&self) -> u64 {
        let mut bytes = [0_u8; 8];
        // jitter only spreads retries, so the clock is an acceptable fallback
        if getrandom::fill(&mut bytes).is_ok() {
            u64::from_le_bytes(bytes)
        } else {
            self.now_millis()
        }
    }
}
