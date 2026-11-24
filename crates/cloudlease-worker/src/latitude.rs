//! Builds the Latitude.sh adapter from Worker bindings

use cloudlease_core::{ids::TagPrefix, tags::TagScheme};
use cloudlease_provider::latitude::{
    ClientOptions, Latitude, LatitudeApiKey, LatitudeClient, Slug,
};
use worker::Env;

use crate::{
    error::{CloudleaseError, CloudleaseResult},
    runtime::{WorkersFetchTransport, WorkersRuntime},
};

/// The Latitude.sh adapter inside a Worker
pub type WorkersLatitude = Latitude<WorkersFetchTransport, WorkersRuntime>;

/// The binding names the adapter reads
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LatitudeBindings<'a> {
    /// The secret holding the API key
    pub api_key: &'a str,
    /// The variable holding the project every resource is created, found and swept in, such
    /// as `proj_ZWr75ZRzo5A91`
    pub project: &'a str,
    /// A variable that overrides the API base URL, such as a fake API in tests
    pub api_url: Option<&'a str>,
    /// The variable holding the app's tag prefix
    pub tag_prefix: &'a str,
}

/// Builds the adapter from Worker bindings, or `None` when neither the key nor the project
/// is set
///
/// Latitude.sh is optional for an app. A key without a project, or a project without a key,
/// is a configuration error, not an absent provider: the sweep must never list a whole team.
pub fn from_env(
    env: &Env,
    bindings: LatitudeBindings<'_>,
) -> CloudleaseResult<Option<WorkersLatitude>> {
    let setting = |name: &str| {
        env.secret(name)
            .or_else(|_| env.var(name))
            .ok()
            .map(|value| value.to_string())
    };
    let (key, project) = match (setting(bindings.api_key), setting(bindings.project)) {
        (None, None) => return Ok(None),
        (Some(key), Some(project)) => (key, project),
        (Some(_), None) | (None, Some(_)) => {
            return Err(CloudleaseError::Config(format!(
                "set both {} and {}, or neither",
                bindings.api_key, bindings.project
            )));
        }
    };
    let prefix = env
        .var(bindings.tag_prefix)
        .map_err(|_| CloudleaseError::Config(format!("{} is not set", bindings.tag_prefix)))?
        .to_string();

    let mut options = ClientOptions::new(LatitudeApiKey::parse(key)?)?;
    if let Some(api_url) = bindings.api_url.and_then(|name| env.var(name).ok()) {
        let url = worker::Url::parse(&api_url.to_string()).map_err(|error| {
            CloudleaseError::Config(format!("invalid Latitude.sh URL: {error}"))
        })?;
        options = options.with_base_url(url)?;
    }

    let client = LatitudeClient::new(options, WorkersFetchTransport, WorkersRuntime);

    Ok(Some(Latitude::new(
        client,
        TagScheme::new(TagPrefix::parse(prefix)?),
        Slug::parse(project)?,
    )))
}
