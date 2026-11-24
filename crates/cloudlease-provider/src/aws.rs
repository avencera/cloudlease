//! EC2 Spot instances on AWS
//!
//! [`Aws`] manages one kind, [`AwsKind::Instance`]: a persistent Spot instance that stops
//! when EC2 reclaims its capacity. Its root volume is deleted with it, so there is no volume
//! kind. AWS tags are keys and values, so the adapter writes the scheme's
//! [pairs](TagScheme::render_pairs) plus a `Name`, and reads them back with
//! [`TagScheme::classify_pairs`], leaving `Name` out.
//!
//! - [`client`]: the signed EC2 Query client and its retry rules
//! - [`ec2`]: the typed [`Ec2`] seam the adapter and tests use
//! - [`ingress`]: [`IngressPolicy`](crate::firewall::IngressPolicy) through one shared
//!   security group
//! - [`AwsRegions`]: several regions behind one [`CloudProvider`]

pub mod client;
pub mod ec2;
pub mod ingress;
mod signing;
mod xml;

use std::{collections::BTreeMap, num::NonZeroU16};

use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId},
    sweep::{Inventory, Observed, Unrecognized},
    tags::{ManagedTags, OwnerTags, ResourceKind, TagMatch, TagScheme, UnrecognizedReason},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use client::{AwsRequestSafety, Ec2Client, Ec2ClientOptions, Ec2Response};
pub use ec2::{
    AmiId, AvailabilityZone, AwsImage, AwsInstance, AwsInstanceDetails, AwsInstanceState,
    AwsInstanceType, AwsInstanceVolume, AwsSecurityGroup, AwsSecurityGroupIngress,
    AwsSpotInstanceRequest, AwsSpotInstanceRequestState, AwsSpotInstanceRequestType,
    AwsSpotInterruptionBehavior, AwsSubnet, AwsTag, Ec2, InstanceFilter, InstanceName,
    InstanceType, NAME_TAG_KEY, RunInstancesRequest, SecurityGroupId, SubnetId, TcpIngress,
    UserData, is_ambiguous_failure,
};
pub use ingress::{AwsIngress, SharedSecurityGroup};
pub use signing::{AwsCredentials, AwsRegion};

use crate::{
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    provider::{CloudProvider, CreateSpec, Presence},
    validated::validated,
};
use signing::hex_lower;

/// The most instance types one launch may try
pub const MAX_INSTANCE_TYPE_CANDIDATES: usize = 20;

/// An AWS resource kind cloudlease manages
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AwsKind {
    /// An EC2 instance with its Spot request and root volume
    Instance,
}

impl ResourceKind for AwsKind {
    const ALL: &'static [Self] = &[Self::Instance];

    fn tag_name(self) -> &'static str {
        match self {
            Self::Instance => "instance",
        }
    }

    fn delete_rank(self) -> u8 {
        match self {
            Self::Instance => 0,
        }
    }
}

/// The instance types one launch tries, in order
///
/// A launch moves to the next type only when EC2 has no Spot capacity for the current one;
/// any other failure ends it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceTypeCandidates(Vec<InstanceType>);

impl InstanceTypeCandidates {
    /// Checks that there are 1 to 20 distinct types
    pub fn new(candidates: Vec<InstanceType>) -> ProviderResult<Self> {
        if candidates.is_empty() || candidates.len() > MAX_INSTANCE_TYPE_CANDIDATES {
            return Err(ProviderError::validation(
                "a launch must try from 1 through 20 instance types",
            ));
        }
        for (index, candidate) in candidates.iter().enumerate() {
            if candidates[..index].contains(candidate) {
                return Err(ProviderError::validation(
                    "a launch must not try an instance type twice",
                ));
            }
        }

        Ok(Self(candidates))
    }

    /// Returns the types in launch order
    #[must_use]
    pub fn as_slice(&self) -> &[InstanceType] {
        &self.0
    }
}

validated!(
    /// The caller's key for one logical launch, which makes repeating it return the same
    /// instance
    IdempotencyKey,
    "an idempotency key must be 1 to 512 characters",
    |value| !value.trim().is_empty() && value.len() <= 512
);

/// One persistent Spot instance to launch
///
/// The adapter adds the parts that are not options: a public IPv4 address, IMDSv2, an
/// encrypted gp3 root volume deleted with the instance, and a persistent Spot request that
/// stops the instance on interruption.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsInstanceSpec {
    /// The `Name` tag
    pub name: InstanceName,
    /// The instance types to try, in order
    pub instance_types: InstanceTypeCandidates,
    /// The AMI
    pub image_id: AmiId,
    /// The AMI's root device name, from [`Ec2::describe_image`]
    pub root_device_name: String,
    /// The root volume size
    pub root_volume_gib: NonZeroU16,
    /// The subnet
    pub subnet_id: SubnetId,
    /// The Availability Zone, which must be the subnet's
    pub availability_zone: AvailabilityZone,
    /// The security group
    pub security_group_id: SecurityGroupId,
    /// The boot user data
    pub user_data: UserData,
    /// The caller's key for this launch; each instance type gets its own client token
    /// derived from it
    pub idempotency_key: IdempotencyKey,
}

impl CreateSpec for AwsInstanceSpec {
    type Kind = AwsKind;

    fn kind(&self) -> AwsKind {
        AwsKind::Instance
    }
}

/// A managed instance with its parsed tags and everything EC2 reported
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedInstance {
    /// The ID and parsed tags
    pub observed: Observed<AwsKind>,
    /// The instance as EC2 described it
    pub instance: AwsInstance,
}

/// Every live instance carrying the managed tag key
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AwsInventory {
    /// Instances with an exact managed tag set
    pub managed: Vec<ManagedInstance>,
    /// Instances with the managed key but another shape, such as an older tag scheme
    pub unrecognized: Vec<Unrecognized>,
}

impl From<AwsInventory> for Inventory<AwsKind> {
    fn from(inventory: AwsInventory) -> Self {
        Self {
            managed: inventory
                .managed
                .into_iter()
                .map(|managed| managed.observed)
                .collect(),
            unrecognized: inventory.unrecognized,
            // a region that fails to list fails the whole inventory, so none is ever unlisted
            unlisted: Vec::new(),
        }
    }
}

/// The AWS adapter for one region and one app's tag scheme
pub struct Aws<C> {
    ec2: C,
    scheme: TagScheme,
}

impl<C: Ec2> Aws<C> {
    /// Creates an adapter that manages instances with `scheme`'s tags
    pub const fn new(ec2: C, scheme: TagScheme) -> Self {
        Self { ec2, scheme }
    }

    /// Returns the EC2 seam, for the lifecycle steps outside [`CloudProvider`] such as stop
    /// and start
    pub const fn ec2(&self) -> &C {
        &self.ec2
    }

    /// Returns the tag scheme
    pub const fn scheme(&self) -> &TagScheme {
        &self.scheme
    }

    /// Returns the exact tags an instance and its Spot request get: the scheme's pairs and
    /// `Name`
    #[must_use]
    pub fn instance_tags(&self, owner: &OwnerTags, name: &InstanceName) -> Vec<AwsTag> {
        self.scheme
            .render_pairs(owner, AwsKind::Instance)
            .into_iter()
            .map(|(key, value)| AwsTag::new(key, value))
            .chain([AwsTag::new(NAME_TAG_KEY, name.as_str())])
            .collect()
    }

    /// Classifies a resource's tags, leaving out `Name`
    #[must_use]
    pub fn classify(&self, tags: &[AwsTag]) -> TagMatch<AwsKind> {
        let pairs: Vec<_> = tags
            .iter()
            .filter(|tag| tag.key != NAME_TAG_KEY)
            .map(|tag| (tag.key.clone(), tag.value.clone()))
            .collect();

        self.scheme.classify_pairs(&pairs)
    }

    /// Launches a persistent Spot instance and returns it as EC2 reported the launch
    ///
    /// Tries each instance type in order while EC2 reports no Spot capacity. Each type's
    /// client token comes from the owner, generation, idempotency key and type, so repeating
    /// a launch returns the instance the first one created. A launch is never replayed after
    /// an [ambiguous failure](is_ambiguous_failure); the caller looks for the instance with
    /// [`Aws::find_instances`].
    pub async fn launch(
        &self,
        owner: &OwnerTags,
        spec: &AwsInstanceSpec,
    ) -> ProviderResult<AwsInstance> {
        let tags = self.instance_tags(owner, &spec.name);
        let mut last_capacity_error = None;

        for instance_type in spec.instance_types.as_slice() {
            let request = RunInstancesRequest {
                image_id: spec.image_id.clone(),
                root_device_name: spec.root_device_name.clone(),
                root_volume_gib: spec.root_volume_gib,
                instance_type: instance_type.clone(),
                subnet_id: spec.subnet_id.clone(),
                availability_zone: spec.availability_zone.clone(),
                security_group_id: spec.security_group_id.clone(),
                tags: tags.clone(),
                user_data: spec.user_data.clone(),
                client_token: self.client_token(owner, &spec.idempotency_key, instance_type),
            };
            match self.ec2.run_instances(request).await {
                Ok(instance) => return Ok(instance),
                Err(error) if error.code == ProviderErrorCode::NoCapacity && error.retryable => {
                    last_capacity_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }

        // candidates are never empty, so a capacity error was recorded
        Err(last_capacity_error.unwrap_or_else(|| {
            ProviderError::validation("a launch must try at least one instance type")
        }))
    }

    /// Returns the live instances with an exact tag set for one owner and generation
    pub async fn find_instances(
        &self,
        owner: &OwnerId,
        generation: Generation,
    ) -> ProviderResult<Vec<AwsInstance>> {
        let filter = InstanceFilter::Tag(AwsTag::new(
            format!("{}-owner", self.scheme.prefix()),
            owner.as_str(),
        ));
        let instances = self.ec2.list_instances(filter).await?;

        Ok(instances
            .into_iter()
            .filter(|instance| !instance.state().is_absent())
            .filter(|instance| {
                matches!(
                    self.classify(instance.tags()),
                    TagMatch::Managed(tags) if tags.owner == *owner && tags.generation == generation
                )
            })
            .collect())
    }

    /// Lists every live instance carrying the managed tag key, whatever its value
    ///
    /// Listing by key rather than by `{prefix}-managed=true` keeps instances tagged by an
    /// older scheme visible as unrecognized, so they are reported instead of forgotten.
    pub async fn instance_inventory(&self) -> ProviderResult<AwsInventory> {
        let filter = InstanceFilter::TagKey(self.scheme.managed_tag());
        let instances = self.ec2.list_instances(filter).await?;
        let mut inventory = AwsInventory::default();

        for instance in instances {
            if instance.state().is_absent() {
                continue;
            }

            let reason = match self.classify(instance.tags()) {
                TagMatch::Foreign => continue,
                TagMatch::Managed(tags) => match ResourceId::parse(instance.id()) {
                    Ok(id) => {
                        inventory.managed.push(ManagedInstance {
                            observed: Observed { id, tags },
                            instance,
                        });
                        continue;
                    }
                    Err(_) => UnrecognizedReason::InvalidValue,
                },
                TagMatch::Unrecognized(reason) => reason,
            };
            inventory.unrecognized.push(unrecognized(&instance, reason));
        }

        Ok(inventory)
    }

    /// Returns the Spot request that launched an instance, checking it carries `expected`
    pub async fn spot_request(
        &self,
        instance: &AwsInstance,
        expected: &ManagedTags<AwsKind>,
    ) -> ProviderResult<Option<AwsSpotInstanceRequest>> {
        let requests = match instance.spot_instance_request_id() {
            Some(request_id) => {
                self.ec2
                    .describe_spot_instance_requests(request_id.to_owned())
                    .await
            }
            None => {
                self.ec2
                    .describe_spot_instance_requests_for_instance(instance.id().to_owned())
                    .await
            }
        };
        let requests = match requests {
            Ok(requests) => requests,
            Err(error) if error.code == ProviderErrorCode::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        let Some(request) = requests.into_iter().next() else {
            return Ok(None);
        };

        match self.classify(&request.tags) {
            TagMatch::Managed(tags) if tags == *expected => Ok(Some(request)),
            _ => Err(invariant_violation(
                "the Spot request does not carry its instance's exact managed tags",
            )),
        }
    }

    /// Cancels an instance's Spot request and terminates it, refusing an instance that does
    /// not carry exactly `owner`'s tags
    ///
    /// An absent instance counts as deleted.
    pub async fn delete_owned(&self, owner: &OwnerTags, id: &ResourceId) -> ProviderResult<()> {
        self.terminate(id, Some(owner)).await
    }

    async fn terminate(&self, id: &ResourceId, owner: Option<&OwnerTags>) -> ProviderResult<()> {
        let instance = match self.ec2.describe_instance(id.as_str().to_owned()).await {
            Ok(instance) => instance,
            Err(error) if error.code == ProviderErrorCode::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let tags = match self.classify(instance.tags()) {
            TagMatch::Managed(tags)
                if owner.is_none_or(|owner| tags == owner.with_kind(AwsKind::Instance)) =>
            {
                tags
            }
            _ => {
                return Err(invariant_violation(
                    "the EC2 instance is not owned by the requested owner and generation",
                ));
            }
        };

        // a persistent request outlives its instance and would launch a replacement
        if let Some(request) = self.spot_request(&instance, &tags).await?
            && request.state.may_launch()
        {
            ignore_not_found(self.ec2.cancel_spot_instance_request(request.id).await)?;
        }
        if instance.state().is_absent() {
            return Ok(());
        }

        ignore_not_found(self.ec2.terminate_instance(id.as_str().to_owned()).await)
    }

    fn client_token(
        &self,
        owner: &OwnerTags,
        key: &IdempotencyKey,
        instance_type: &InstanceType,
    ) -> String {
        let identity = format!(
            "{}:{}:{}:{}",
            owner.owner,
            owner.generation,
            key.as_str(),
            instance_type.as_str()
        );
        let digest = Sha256::digest(identity.as_bytes());

        // EC2 accepts up to 64 ASCII characters; this is at most 16 + 10 + 32
        format!(
            "{}-instance-{}",
            self.scheme.prefix(),
            hex_lower(&digest[..16])
        )
    }
}

impl<C: Ec2> CloudProvider for Aws<C> {
    type Kind = AwsKind;
    type Spec = AwsInstanceSpec;

    async fn create(
        &self,
        owner: &OwnerTags,
        spec: &AwsInstanceSpec,
    ) -> ProviderResult<ResourceId> {
        let instance = self.launch(owner, spec).await?;

        ResourceId::parse(instance.id())
            .map_err(|_| ProviderError::invalid_response("EC2 returned an invalid instance ID"))
    }

    async fn delete(&self, kind: AwsKind, id: &ResourceId) -> ProviderResult<()> {
        match kind {
            AwsKind::Instance => self.terminate(id, None).await,
        }
    }

    async fn presence(&self, kind: AwsKind, id: &ResourceId) -> ProviderResult<Presence> {
        let AwsKind::Instance = kind;
        match self.ec2.describe_instance(id.as_str().to_owned()).await {
            Ok(instance) if instance.state().is_absent() => Ok(Presence::Absent),
            Ok(_) => Ok(Presence::Present),
            Err(error) if error.code == ProviderErrorCode::NotFound => Ok(Presence::Absent),
            Err(error) => Err(error),
        }
    }

    async fn find(
        &self,
        owner: &OwnerId,
        generation: Generation,
        kind: AwsKind,
    ) -> ProviderResult<Vec<ResourceId>> {
        let AwsKind::Instance = kind;
        self.find_instances(owner, generation)
            .await?
            .iter()
            .map(|instance| {
                ResourceId::parse(instance.id()).map_err(|_| {
                    ProviderError::invalid_response("EC2 returned an invalid instance ID")
                })
            })
            .collect()
    }

    async fn inventory(&self) -> ProviderResult<Inventory<AwsKind>> {
        Ok(self.instance_inventory().await?.into())
    }
}

/// A launch in one region of an [`AwsRegions`] set
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegionalSpec {
    /// The region to launch in
    pub region: AwsRegion,
    /// The instance
    pub instance: AwsInstanceSpec,
}

impl CreateSpec for RegionalSpec {
    type Kind = AwsKind;

    fn kind(&self) -> AwsKind {
        AwsKind::Instance
    }
}

/// Several regions behind one [`CloudProvider`], for a sweep or reaper that does not know
/// where an instance lives
///
/// Inventory and find merge every region. Delete and presence ask every region, since an
/// instance ID does not name its region, and a failure in any region is returned rather
/// than read as absence.
pub struct AwsRegions<'a, C> {
    regions: BTreeMap<AwsRegion, &'a Aws<C>>,
}

impl<'a, C: Ec2> AwsRegions<'a, C> {
    /// Groups at least one region, each at most once
    pub fn new(regions: impl IntoIterator<Item = (AwsRegion, &'a Aws<C>)>) -> ProviderResult<Self> {
        let mut grouped = BTreeMap::new();
        for (region, aws) in regions {
            if grouped.insert(region, aws).is_some() {
                return Err(ProviderError::validation(
                    "an AWS region set must not repeat a region",
                ));
            }
        }
        if grouped.is_empty() {
            return Err(ProviderError::validation(
                "an AWS region set needs at least one region",
            ));
        }

        Ok(Self { regions: grouped })
    }

    /// Returns one region's adapter
    #[must_use]
    pub fn region(&self, region: &AwsRegion) -> Option<&'a Aws<C>> {
        self.regions.get(region).copied()
    }
}

impl<C: Ec2> CloudProvider for AwsRegions<'_, C> {
    type Kind = AwsKind;
    type Spec = RegionalSpec;

    async fn create(&self, owner: &OwnerTags, spec: &RegionalSpec) -> ProviderResult<ResourceId> {
        let aws = self.region(&spec.region).ok_or_else(|| {
            ProviderError::validation(format!("AWS region {} is not configured", spec.region))
        })?;

        aws.create(owner, &spec.instance).await
    }

    async fn delete(&self, kind: AwsKind, id: &ResourceId) -> ProviderResult<()> {
        // a region that never had the instance answers not found, which delete treats as done
        let mut first_error = None;
        for aws in self.regions.values() {
            if let Err(error) = aws.delete(kind, id).await {
                first_error.get_or_insert(error);
            }
        }

        first_error.map_or(Ok(()), Err)
    }

    async fn presence(&self, kind: AwsKind, id: &ResourceId) -> ProviderResult<Presence> {
        let mut first_error = None;
        for aws in self.regions.values() {
            match aws.presence(kind, id).await {
                Ok(Presence::Present) => return Ok(Presence::Present),
                Ok(Presence::Absent) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }

        first_error.map_or(Ok(Presence::Absent), Err)
    }

    async fn find(
        &self,
        owner: &OwnerId,
        generation: Generation,
        kind: AwsKind,
    ) -> ProviderResult<Vec<ResourceId>> {
        let mut found = Vec::new();
        for aws in self.regions.values() {
            found.extend(aws.find(owner, generation, kind).await?);
        }

        Ok(found)
    }

    async fn inventory(&self) -> ProviderResult<Inventory<AwsKind>> {
        let mut merged = Inventory {
            managed: Vec::new(),
            unrecognized: Vec::new(),
            unlisted: Vec::new(),
        };
        // one region's list failure fails the whole inventory: a deployment's regions are one
        // provider to its app, and a partial list must not read as absence
        for aws in self.regions.values() {
            let inventory = aws.inventory().await?;
            merged.managed.extend(inventory.managed);
            merged.unrecognized.extend(inventory.unrecognized);
            merged.unlisted.extend(inventory.unlisted);
        }

        Ok(merged)
    }
}

fn unrecognized(instance: &AwsInstance, reason: UnrecognizedReason) -> Unrecognized {
    Unrecognized {
        provider_kind: AwsKind::Instance.tag_name().to_owned(),
        id: instance.id().to_owned(),
        name: instance.name().unwrap_or(instance.id()).to_owned(),
        reason,
        tags: instance
            .tags()
            .iter()
            .map(|tag| format!("{}={}", tag.key, tag.value))
            .collect(),
    }
}

fn ignore_not_found(result: ProviderResult<()>) -> ProviderResult<()> {
    match result {
        Err(error) if error.code == ProviderErrorCode::NotFound => Ok(()),
        result => result,
    }
}

fn invariant_violation(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorCode::InvariantViolation, false, message)
}
