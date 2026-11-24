//! Builds the DigitalOcean adapter from Worker bindings

use cloudlease_core::{ids::TagPrefix, tags::TagScheme};
use cloudlease_provider::digitalocean::{
    ClientOptions, DigitalOcean, DigitalOceanClient, DigitalOceanToken,
};
use worker::Env;

use crate::{
    error::{CloudleaseError, CloudleaseResult},
    runtime::{WorkersFetchTransport, WorkersRuntime},
};

/// The DigitalOcean adapter inside a Worker
pub type WorkersDigitalOcean = DigitalOcean<WorkersFetchTransport, WorkersRuntime>;

/// The binding names the adapter reads
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DigitalOceanBindings<'a> {
    /// The secret holding the API token
    pub token: &'a str,
    /// A variable that overrides the API base URL, such as a fake API in tests
    pub api_url: Option<&'a str>,
    /// The variable holding the app's tag prefix
    pub tag_prefix: &'a str,
}

/// Builds the adapter from Worker bindings
pub fn from_env(
    env: &Env,
    bindings: DigitalOceanBindings<'_>,
) -> CloudleaseResult<WorkersDigitalOcean> {
    let token = env
        .secret(bindings.token)
        .or_else(|_| env.var(bindings.token))
        .map_err(|_| CloudleaseError::Config(format!("{} is not set", bindings.token)))?
        .to_string();
    let prefix = env
        .var(bindings.tag_prefix)
        .map_err(|_| CloudleaseError::Config(format!("{} is not set", bindings.tag_prefix)))?
        .to_string();

    let mut options = ClientOptions::new(DigitalOceanToken::parse(token)?)?;
    if let Some(api_url) = bindings.api_url.and_then(|name| env.var(name).ok()) {
        let url = url_from(&api_url.to_string())?;
        options = options.with_base_url(url)?;
    }

    let client = DigitalOceanClient::new(options, WorkersFetchTransport, WorkersRuntime);

    Ok(DigitalOcean::new(
        client,
        TagScheme::new(TagPrefix::parse(prefix)?),
    ))
}

fn url_from(value: &str) -> CloudleaseResult<worker::Url> {
    worker::Url::parse(value)
        .map_err(|error| CloudleaseError::Config(format!("invalid API URL: {error}")))
}
