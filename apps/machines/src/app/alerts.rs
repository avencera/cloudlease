//! Sending alerts to the ntfy topic
//!
//! An alert that cannot be sent is logged and dropped: alerting never blocks a cap, a release
//! or a sweep.

use std::time::Duration;

use cloudlease_machines::alert::Alert;
use cloudlease_provider::http::{HttpMethod, HttpRequest, HttpTransport};
use cloudlease_worker::runtime::WorkersFetchTransport;
use worker::Env;

use crate::{NTFY_TOKEN, NTFY_URL, app::settings::secret};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Posts an alert to `NTFY_URL`, and to the log either way
pub async fn notify(env: &Env, alert: &Alert) {
    worker::console_warn!("cloudlease alert: {}: {}", alert.title, alert.message);

    let Some(topic) = secret(env, NTFY_URL) else {
        worker::console_error!("{NTFY_URL} is not set, so the alert above was not sent");
        return;
    };
    let url = match worker::Url::parse(&topic) {
        Ok(url) => url,
        Err(error) => {
            worker::console_error!("{NTFY_URL} is not a URL: {error}");
            return;
        }
    };

    let mut headers = vec![
        ("title".to_owned(), alert.title.clone()),
        ("priority".to_owned(), alert.priority.header().to_owned()),
        ("tags".to_owned(), "cloudlease".to_owned()),
    ];
    if let Some(token) = secret(env, NTFY_TOKEN) {
        headers.push(("authorization".to_owned(), format!("Bearer {token}")));
    }
    let request = HttpRequest {
        method: HttpMethod::Post,
        url,
        headers,
        body: Some(alert.message.clone().into_bytes()),
        timeout: TIMEOUT,
    };

    match WorkersFetchTransport.send(request).await {
        Ok(response) if (200..300).contains(&response.status) => {}
        Ok(response) => worker::console_error!("ntfy answered HTTP {}", response.status),
        Err(error) => worker::console_error!("ntfy failed: {error}"),
    }
}
