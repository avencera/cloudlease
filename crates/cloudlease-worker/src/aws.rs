//! Builds the AWS EC2 adapter from Worker bindings, one client per region

use std::collections::BTreeMap;

use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId, TagPrefix},
    sweep::Inventory,
    tags::{OwnerTags, TagScheme},
};
use cloudlease_provider::{
    aws::{
        Aws, AwsCredentials, AwsKind, AwsRegion, AwsRegions, Ec2Client, Ec2ClientOptions,
        RegionalSpec, SpotHistory, SpotInterruption,
    },
    error::ProviderResult,
    provider::{CloudProvider, Presence},
};
use worker::Env;

use crate::{
    error::{CloudleaseError, CloudleaseResult},
    runtime::{WorkersFetchTransport, WorkersRuntime},
};

/// The AWS adapter of one region inside a Worker
pub type WorkersAwsRegion = Aws<Ec2Client<WorkersFetchTransport, WorkersRuntime>>;

/// The binding names the adapter reads
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AwsBindings<'a> {
    /// The secret holding the access key ID
    pub access_key_id: &'a str,
    /// The secret holding the secret access key
    pub secret_access_key: &'a str,
    /// The variable listing every region the app creates in, comma separated, such as
    /// `us-east-1`; the sweep lists each of them
    pub regions: &'a str,
    /// A variable that overrides every region's EC2 endpoint, such as a fake API in tests
    pub endpoint: Option<&'a str>,
    /// The variable holding the app's tag prefix
    pub tag_prefix: &'a str,
}

/// Every region an app creates EC2 instances in, as one [`CloudProvider`]
///
/// The regions come from the Worker's bindings, not from its classes, so removing a class
/// never takes its region out of the sweep.
pub struct WorkersAws {
    regions: BTreeMap<AwsRegion, WorkersAwsRegion>,
}

impl WorkersAws {
    /// Reports whether `region` is one the Worker creates and sweeps in
    #[must_use]
    pub fn has_region(&self, region: &AwsRegion) -> bool {
        self.regions.contains_key(region)
    }

    fn regions(
        &self,
    ) -> ProviderResult<AwsRegions<'_, Ec2Client<WorkersFetchTransport, WorkersRuntime>>> {
        AwsRegions::new(
            self.regions
                .iter()
                .map(|(region, aws)| (region.clone(), aws)),
        )
    }
}

impl CloudProvider for WorkersAws {
    type Kind = AwsKind;
    type Spec = RegionalSpec;

    async fn create(&self, owner: &OwnerTags, spec: &RegionalSpec) -> ProviderResult<ResourceId> {
        self.regions()?.create(owner, spec).await
    }

    async fn delete(&self, kind: AwsKind, id: &ResourceId) -> ProviderResult<()> {
        self.regions()?.delete(kind, id).await
    }

    async fn presence(&self, kind: AwsKind, id: &ResourceId) -> ProviderResult<Presence> {
        self.regions()?.presence(kind, id).await
    }

    async fn find(
        &self,
        owner: &OwnerId,
        generation: Generation,
        kind: AwsKind,
    ) -> ProviderResult<Vec<ResourceId>> {
        self.regions()?.find(owner, generation, kind).await
    }

    async fn inventory(&self) -> ProviderResult<Inventory<AwsKind>> {
        self.regions()?.inventory().await
    }
}

impl SpotHistory for WorkersAws {
    async fn spot_interruptions(
        &self,
        region: &AwsRegion,
        owner: &OwnerId,
    ) -> ProviderResult<Vec<SpotInterruption>> {
        self.regions()?.spot_interruptions(region, owner).await
    }
}

/// Builds the adapter from Worker bindings, or `None` when neither key secret is set
///
/// AWS is optional for an app. One key secret without the other, or keys without regions,
/// is a configuration error rather than an absent provider.
pub fn from_env(env: &Env, bindings: AwsBindings<'_>) -> CloudleaseResult<Option<WorkersAws>> {
    let setting = |name: &str| {
        env.secret(name)
            .or_else(|_| env.var(name))
            .ok()
            .map(|value| value.to_string())
    };
    let (access_key_id, secret_access_key) = match (
        setting(bindings.access_key_id),
        setting(bindings.secret_access_key),
    ) {
        (None, None) => return Ok(None),
        (Some(id), Some(secret)) => (id, secret),
        (Some(_), None) | (None, Some(_)) => {
            return Err(CloudleaseError::Config(format!(
                "set both {} and {}, or neither",
                bindings.access_key_id, bindings.secret_access_key
            )));
        }
    };
    let regions = setting(bindings.regions)
        .filter(|regions| !regions.trim().is_empty())
        .ok_or_else(|| {
            CloudleaseError::Config(format!(
                "{} must list the AWS regions when AWS keys are set",
                bindings.regions
            ))
        })?;
    let prefix = env
        .var(bindings.tag_prefix)
        .map_err(|_| CloudleaseError::Config(format!("{} is not set", bindings.tag_prefix)))?
        .to_string();
    let scheme = TagScheme::new(TagPrefix::parse(prefix)?);
    let credentials = AwsCredentials::new(access_key_id, secret_access_key, None)?;
    let endpoint = match bindings.endpoint.and_then(|name| env.var(name).ok()) {
        Some(url) => Some(worker::Url::parse(&url.to_string()).map_err(|error| {
            CloudleaseError::Config(format!("invalid AWS EC2 endpoint: {error}"))
        })?),
        None => None,
    };

    let mut built = BTreeMap::new();
    for region in regions.split(',') {
        let region = AwsRegion::parse(region.trim())?;
        let mut options = Ec2ClientOptions::new(credentials.clone(), region.clone())?;
        if let Some(endpoint) = &endpoint {
            options = options.with_endpoint(endpoint.clone())?;
        }
        let client = Ec2Client::new(options, WorkersFetchTransport, WorkersRuntime)?;
        if built
            .insert(region.clone(), Aws::new(client, scheme.clone()))
            .is_some()
        {
            return Err(CloudleaseError::Config(format!(
                "{} lists {region} twice",
                bindings.regions
            )));
        }
    }

    Ok(Some(WorkersAws { regions: built }))
}
