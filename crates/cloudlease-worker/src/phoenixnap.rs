//! Builds the phoenixNAP adapter from Worker bindings

use cloudlease_core::{ids::TagPrefix, tags::TagScheme};
use cloudlease_provider::phoenixnap::{
    ClientOptions, PhoenixNap, PhoenixNapClient, PnapCredentials,
};
use worker::Env;

use crate::{
    error::{CloudleaseError, CloudleaseResult},
    runtime::{WorkersFetchTransport, WorkersRuntime},
};

/// The phoenixNAP adapter inside a Worker
pub type WorkersPhoenixNap = PhoenixNap<WorkersFetchTransport, WorkersRuntime>;

/// The binding names the adapter reads
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhoenixNapBindings<'a> {
    /// The secret holding the OAuth client ID
    pub client_id: &'a str,
    /// The secret holding the OAuth client secret
    pub client_secret: &'a str,
    /// A variable that overrides the API root, such as a fake API in tests
    pub api_url: Option<&'a str>,
    /// A variable that overrides the OAuth token endpoint, such as a fake one in tests
    pub token_url: Option<&'a str>,
    /// The variable holding the app's tag prefix
    pub tag_prefix: &'a str,
}

/// Builds the adapter from Worker bindings, or `None` when neither credential is set
///
/// phoenixNAP is optional for an app: one without credentials simply has no phoenixNAP. A
/// single credential without the other is a configuration error, not an absent provider.
pub fn from_env(
    env: &Env,
    bindings: PhoenixNapBindings<'_>,
) -> CloudleaseResult<Option<WorkersPhoenixNap>> {
    let secret = |name: &str| {
        env.secret(name)
            .or_else(|_| env.var(name))
            .ok()
            .map(|value| value.to_string())
    };
    let (client_id, client_secret) =
        match (secret(bindings.client_id), secret(bindings.client_secret)) {
            (None, None) => return Ok(None),
            (Some(id), Some(secret)) => (id, secret),
            (Some(_), None) | (None, Some(_)) => {
                return Err(CloudleaseError::Config(format!(
                    "set both {} and {}, or neither",
                    bindings.client_id, bindings.client_secret
                )));
            }
        };
    let prefix = env
        .var(bindings.tag_prefix)
        .map_err(|_| CloudleaseError::Config(format!("{} is not set", bindings.tag_prefix)))?
        .to_string();

    let mut options = ClientOptions::new(PnapCredentials::parse(client_id, client_secret)?)?;
    if let Some(api_url) = bindings.api_url.and_then(|name| env.var(name).ok()) {
        options = options.with_api_url(url_from(&api_url.to_string())?)?;
    }
    if let Some(token_url) = bindings.token_url.and_then(|name| env.var(name).ok()) {
        options = options.with_token_url(url_from(&token_url.to_string())?)?;
    }

    let client = PhoenixNapClient::new(options, WorkersFetchTransport, WorkersRuntime);

    Ok(Some(PhoenixNap::new(
        client,
        TagScheme::new(TagPrefix::parse(prefix)?),
    )))
}

fn url_from(value: &str) -> CloudleaseResult<worker::Url> {
    worker::Url::parse(value)
        .map_err(|error| CloudleaseError::Config(format!("invalid phoenixNAP URL: {error}")))
}
