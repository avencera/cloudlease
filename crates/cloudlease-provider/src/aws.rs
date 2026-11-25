//! EC2 Spot instances on AWS
//!
//! [`Aws`] manages one kind, [`AwsKind::Instance`]: a one-time Spot instance that EC2
//! terminates when it takes the capacity back. [`Aws::launch`] makes one instant
//! `CreateFleet` call over every candidate [`SpotPool`], so EC2's `price-capacity-optimized`
//! strategy picks the pool least likely to be interrupted. The instance's root volume is
//! deleted with it, and EC2 deletes the fleet object on its own, so neither is a kind. AWS
//! tags are keys and values, so the adapter writes the scheme's
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

use std::{collections::BTreeMap, future::Future, num::NonZeroU16};

use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId},
    sweep::{Inventory, Observed, Unrecognized},
    tags::{ManagedTags, OwnerTags, ResourceKind, TagMatch, TagScheme, UnrecognizedReason},
    time::UnixMillis,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use client::{AwsRequestSafety, Ec2Client, Ec2ClientOptions, Ec2Response};
pub use ec2::{
    AmiId, AvailabilityZone, AwsImage, AwsInstance, AwsInstanceDetails, AwsInstanceState,
    AwsInstanceType, AwsInstanceVolume, AwsSecurityGroup, AwsSecurityGroupIngress,
    AwsSpotInstanceRequest, AwsSpotInstanceRequestState, AwsSpotInstanceRequestType,
    AwsSpotInterruptionBehavior, AwsStateReason, AwsSubnet, AwsTag, CreateFleetRequest,
    CreateFleetResponse, Ec2, FleetError, FleetInstances, FleetOverrides, InstanceFilter,
    InstanceName, InstanceType, LaunchTemplate, LaunchTemplateId, MARKED_FOR_TERMINATION,
    MAX_USER_DATA_BYTES, NAME_TAG_KEY, RetiredPlacement, SPOT_INSTANCE_TERMINATION,
    SecurityGroupId, SpotMaxPrice, SpotPool, SubnetId, TcpIngress, UserData, is_ambiguous_failure,
};
pub use ingress::{AwsIngress, SharedSecurityGroup};
pub use signing::{AwsCredentials, AwsRegion};

use crate::{
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    provider::{CloudProvider, CreateSpec, Presence},
    validated::validated,
};
use signing::hex_lower;

/// The most pools one launch may offer EC2
///
/// The EC2 API reference caps request and maintain fleets at 300 overrides across all launch
/// templates; it names no cap for instant fleets, so this assumes the same one. A launch over
/// it is refused before it reaches EC2.
pub const MAX_LAUNCH_POOLS: usize = 300;

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

/// The Spot pools one launch offers EC2: 1 to [`MAX_LAUNCH_POOLS`], each instance type and
/// zone at most once
///
/// EC2 picks among them, so their order is no preference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchPools(Vec<SpotPool>);

impl LaunchPools {
    /// Checks the pool count and that no instance type and zone repeats
    pub fn new(pools: Vec<SpotPool>) -> ProviderResult<Self> {
        if pools.is_empty() || pools.len() > MAX_LAUNCH_POOLS {
            return Err(ProviderError::validation(format!(
                "a launch must offer from 1 through {MAX_LAUNCH_POOLS} Spot pools"
            )));
        }
        let repeated = pools.iter().enumerate().any(|(index, pool)| {
            pools[..index].iter().any(|earlier| {
                earlier.is(pool.instance_type.as_str(), pool.availability_zone.as_str())
            })
        });
        if repeated {
            return Err(ProviderError::validation(
                "a launch must not offer an instance type in one zone twice",
            ));
        }

        Ok(Self(pools))
    }

    /// Returns the pools in the order they are sent
    #[must_use]
    pub fn as_slice(&self) -> &[SpotPool] {
        &self.0
    }

    /// Returns the pool that is `instance_type` in `availability_zone`
    #[must_use]
    pub fn find(&self, instance_type: &str, availability_zone: &str) -> Option<&SpotPool> {
        self.0
            .iter()
            .find(|pool| pool.is(instance_type, availability_zone))
    }
}

validated!(
    /// The caller's key for one logical launch, which makes repeating it return the same
    /// instance
    IdempotencyKey,
    "an idempotency key must be 1 to 512 characters",
    |value| !value.trim().is_empty() && value.len() <= 512
);

/// One Spot instance to launch
///
/// The adapter adds the parts that are not options: a one-time request that terminates on
/// interruption, IMDSv2 and an encrypted gp3 root volume deleted with the instance. The
/// launch template supplies the security group; the subnets give the public IPv4 address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsInstanceSpec {
    /// The `Name` tag
    pub name: InstanceName,
    /// The pools EC2 may launch in
    pub pools: LaunchPools,
    /// The launch template every pool shares
    pub launch_template: LaunchTemplate,
    /// The AMI
    pub image_id: AmiId,
    /// The AMI's root device name, from [`Ec2::describe_image`]
    pub root_device_name: String,
    /// The root volume size
    pub root_volume_gib: NonZeroU16,
    /// The boot user data
    pub user_data: UserData,
    /// The caller's key for this launch, from which its client token is derived
    pub idempotency_key: IdempotencyKey,
    /// The most the instance may cost per hour in any pool; EC2 refuses a pool whose Spot
    /// price is higher and reclaims the instance when it rises above it. `None` caps it at
    /// the On-Demand price
    pub max_price: Option<SpotMaxPrice>,
}

impl CreateSpec for AwsInstanceSpec {
    type Kind = AwsKind;

    fn kind(&self) -> AwsKind {
        AwsKind::Instance
    }
}

/// A launched instance and the pool EC2 chose for it
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchedInstance {
    /// The instance as the launch reported it
    pub instance: AwsInstance,
    /// The pool it launched in
    pub pool: SpotPool,
}

/// A Spot pool that took an owner's instance back
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotInterruption {
    /// The instance type
    pub instance_type: InstanceType,
    /// The Availability Zone
    pub availability_zone: AvailabilityZone,
    /// When EC2 terminated the instance, when its transition reason names a time; `None` too
    /// for an instance EC2 has given its interruption notice but not yet terminated, whose
    /// interruption is happening now
    pub at: Option<UnixMillis>,
}

/// Reads which Spot pools recently took an owner's instances back, or are taking one back
///
/// EC2 keeps a terminated instance visible for about an hour, so this sees only recent
/// interruptions.
pub trait SpotHistory {
    /// Returns the pools in `region` that interrupted any generation of `owner`
    fn spot_interruptions(
        &self,
        region: &AwsRegion,
        owner: &OwnerId,
    ) -> impl Future<Output = ProviderResult<Vec<SpotInterruption>>>;
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

    /// Returns the exact tags an instance, its root volume and its fleet get: the scheme's
    /// pairs and `Name`
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

    /// Launches a Spot instance in one of the spec's pools and returns it with the pool
    ///
    /// One instant `CreateFleet` call offers every pool, and EC2 picks the one least likely
    /// to be interrupted. The client token comes from the owner, generation and idempotency
    /// key, so repeating a launch returns the instance the first one created. A launch is
    /// never replayed after an [ambiguous failure](is_ambiguous_failure); the caller looks
    /// for the instance with [`Aws::find_instances`]. A fleet that launched nothing is a
    /// definite failure: [`ProviderErrorCode::NoCapacity`] when every pool refused for
    /// capacity or price, and the first other error's class otherwise.
    pub async fn launch(
        &self,
        owner: &OwnerTags,
        spec: &AwsInstanceSpec,
    ) -> ProviderResult<LaunchedInstance> {
        let tags = self.instance_tags(owner, &spec.name);
        let request = CreateFleetRequest {
            launch_template: spec.launch_template.clone(),
            pools: spec.pools.as_slice().to_vec(),
            image_id: spec.image_id.clone(),
            root_device_name: spec.root_device_name.clone(),
            root_volume_gib: spec.root_volume_gib,
            max_price: spec.max_price.clone(),
            user_data: spec.user_data.clone(),
            tags: tags.clone(),
            client_token: self.client_token(owner, &spec.idempotency_key),
        };
        let response = self.ec2.create_fleet(request).await?;
        let Some(launched) = response.instances.first() else {
            return Err(fleet_failure(&spec.pools, &response.errors));
        };

        // the fleet asks for one instance, so anything else is a response this adapter does
        // not understand; whatever launched carries the lease's tags for its cleanup to find
        let [instance_id] = launched.instance_ids.as_slice() else {
            return Err(ProviderError::invalid_response(
                "EC2 CreateFleet did not launch exactly one instance",
            ));
        };
        if response.instances.len() > 1 {
            return Err(ProviderError::invalid_response(
                "EC2 CreateFleet did not launch exactly one instance",
            ));
        }
        let pool = launched_pool(&spec.pools, launched)?;
        let instance = AwsInstance::present(
            instance_id.clone(),
            AwsInstanceState::Pending,
            AwsInstanceDetails {
                instance_type: pool.instance_type.as_str().to_owned(),
                image_id: spec.image_id.as_str().to_owned(),
                availability_zone: pool.availability_zone.as_str().to_owned(),
                subnet_id: pool.subnet_id.as_str().to_owned(),
                public_ipv4: None,
                volumes: Vec::new(),
                // the launch template holds the security group, so the launch does not name it
                security_group_ids: Vec::new(),
                spot: true,
                imds_v2_required: true,
            },
            tags,
        )?;

        Ok(LaunchedInstance { instance, pool })
    }

    /// Returns the pools that took any generation of `owner`'s instances back, or are taking
    /// one back now
    ///
    /// Terminated instances count, since EC2 lists them for about an hour after the
    /// interruption; an instance whose type or zone EC2 no longer reports is skipped. So does
    /// a live instance whose Spot request is [marked for termination](MARKED_FOR_TERMINATION):
    /// EC2 has given it the two-minute notice, and its replacement is asked for within that
    /// time, while it still runs. One `DescribeSpotInstanceRequests` call covers every such
    /// instance, and is skipped when there is none; if it fails, the terminated instances
    /// still count.
    pub async fn spot_interruptions(
        &self,
        owner: &OwnerId,
    ) -> ProviderResult<Vec<SpotInterruption>> {
        let filter = InstanceFilter::Tag(AwsTag::new(
            format!("{}-owner", self.scheme.prefix()),
            owner.as_str(),
        ));
        let instances = self.ec2.list_instances(filter).await?;
        let owned: Vec<_> = instances
            .iter()
            .filter(|instance| {
                matches!(
                    self.classify(instance.tags()),
                    TagMatch::Managed(tags) if tags.owner == *owner
                )
            })
            .collect();
        let mut interruptions: Vec<_> = owned.iter().copied().filter_map(interruption).collect();

        let live = owned
            .iter()
            .filter(|instance| !instance.state().is_absent() && interruption(instance).is_none());
        let by_request: BTreeMap<&str, &AwsInstance> = live
            .filter_map(|instance| Some((instance.spot_instance_request_id()?, *instance)))
            .collect();
        if by_request.is_empty() {
            return Ok(interruptions);
        }

        // the terminated instances above are history enough on their own, so a failed
        // read, such as a request EC2 no longer lists, must not discard them
        let Ok(requests) = self
            .ec2
            .describe_spot_instance_requests_with_status(
                by_request.keys().map(|id| (*id).to_owned()).collect(),
                MARKED_FOR_TERMINATION,
            )
            .await
        else {
            return Ok(interruptions);
        };
        interruptions.extend(
            requests
                .iter()
                .filter(|request| request.status_code.as_deref() == Some(MARKED_FOR_TERMINATION))
                .filter_map(|request| by_request.get(request.id.as_str()))
                .filter_map(|instance| pool_of(instance, None)),
        );

        Ok(interruptions)
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
    ///
    /// A fleet cannot tag the Spot request it opens, so an untagged one-time request is
    /// accepted as the instance's own: it closes with the instance and is never cancelled.
    /// A request with any of the scheme's tags must carry exactly the instance's.
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
            TagMatch::Foreign if request.request_type == AwsSpotInstanceRequestType::OneTime => {
                Ok(Some(request))
            }
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

        // a persistent request outlives its instance and would launch a replacement; a
        // one-time request closes with it, and a fleet's is untagged, so the policy would
        // refuse its cancel
        if let Some(request) = self.spot_request(&instance, &tags).await?
            && request.request_type != AwsSpotInstanceRequestType::OneTime
            && request.state.may_launch()
        {
            ignore_not_found(self.ec2.cancel_spot_instance_request(request.id).await)?;
        }
        if instance.state().is_absent() {
            return Ok(());
        }

        ignore_not_found(self.ec2.terminate_instance(id.as_str().to_owned()).await)
    }

    // one token per launch: a repeat with other pools is refused by EC2 as a parameter
    // mismatch rather than launching a second instance
    fn client_token(&self, owner: &OwnerTags, key: &IdempotencyKey) -> String {
        let identity = format!("{}:{}:{}", owner.owner, owner.generation, key.as_str());
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
        let launched = self.launch(owner, spec).await?;

        ResourceId::parse(launched.instance.id())
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

impl<C: Ec2> SpotHistory for AwsRegions<'_, C> {
    async fn spot_interruptions(
        &self,
        region: &AwsRegion,
        owner: &OwnerId,
    ) -> ProviderResult<Vec<SpotInterruption>> {
        let aws = self.region(region).ok_or_else(|| {
            ProviderError::validation(format!("AWS region {region} is not configured"))
        })?;

        aws.spot_interruptions(owner).await
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

/// Explains a fleet that launched nothing
///
/// EC2 created nothing, so the failure is never retryable as-is. When any pool refused for
/// capacity or price it is [`ProviderErrorCode::NoCapacity`], so the caller may try another
/// provider, even if other pools failed otherwise, such as a type a zone does not offer.
/// Only a fleet whose every error is something else keeps that error's class.
fn fleet_failure(pools: &LaunchPools, errors: &[FleetError]) -> ProviderError {
    let (refused, other): (Vec<_>, Vec<_>) = errors.iter().partition(|error| {
        client::classify_error(200, Some(&error.code)).0 == ProviderErrorCode::NoCapacity
    });
    if let ([], [first, ..]) = (refused.as_slice(), other.as_slice()) {
        let (code, _) = client::classify_error(200, Some(&first.code));
        let failures: Vec<_> = other.iter().map(|error| describe_failure(error)).collect();
        return ProviderError::new(
            code,
            false,
            format!("EC2 CreateFleet launched nothing: {}", failures.join(", ")),
        );
    }
    if errors.is_empty() {
        return ProviderError::invalid_response(
            "EC2 CreateFleet launched nothing and reported no error",
        );
    }

    let mut named: Vec<String> = refused
        .iter()
        .filter_map(|error| {
            let instance_type = error.overrides.instance_type.as_deref()?;
            let zone = error.overrides.availability_zone.as_deref()?;
            Some(format!("{instance_type} in {zone}"))
        })
        .collect();
    // in the order sent, each pool once
    let mut seen = Vec::new();
    named.retain(|pool| {
        let first = !seen.contains(pool);
        if first {
            seen.push(pool.clone());
        }
        first
    });
    if named.is_empty() {
        named = pools
            .as_slice()
            .iter()
            .map(|pool| {
                format!(
                    "{} in {}",
                    pool.instance_type.as_str(),
                    pool.availability_zone.as_str()
                )
            })
            .collect();
    }
    let mut message = format!(
        "AWS has no Spot capacity under the price cap for {}",
        named.join(", ")
    );
    if !other.is_empty() {
        let failures: Vec<_> = other.iter().map(|error| describe_failure(error)).collect();
        message.push_str(&format!("; other pools failed: {}", failures.join(", ")));
    }

    ProviderError::new(ProviderErrorCode::NoCapacity, false, message)
}

fn describe_failure(error: &FleetError) -> String {
    let pool = match (
        error.overrides.instance_type.as_deref(),
        error.overrides.availability_zone.as_deref(),
    ) {
        (Some(instance_type), Some(zone)) => format!("{instance_type} in {zone}"),
        (Some(instance_type), None) => instance_type.to_owned(),
        _ => "a pool".to_owned(),
    };
    let message = error
        .message
        .as_deref()
        .map(|message| format!(": {}", safe_message(message)))
        .unwrap_or_default();

    format!("{pool} ({}{message})", safe_code(&error.code))
}

/// Maps the pool EC2 reports back to the spec's pool
///
/// The override is read first, then the instance's own type, zone and subnet, which an
/// instant fleet reports outside the override, so a launch is never mistaken for a failed
/// create when one of the two is missing.
fn launched_pool(pools: &LaunchPools, launched: &FleetInstances) -> ProviderResult<SpotPool> {
    let instance_type = launched
        .overrides
        .instance_type
        .as_deref()
        .or(launched.instance_type.as_deref());
    let zone = launched
        .overrides
        .availability_zone
        .as_deref()
        .or(launched.availability_zone.as_deref());
    let subnet = launched
        .overrides
        .subnet_id
        .as_deref()
        .or(launched.subnet_id.as_deref());
    let by_zone = instance_type.zip(zone);
    let by_subnet = || {
        let instance_type = instance_type?;
        let subnet = subnet?;
        pools.as_slice().iter().find(|pool| {
            pool.instance_type.as_str() == instance_type && pool.subnet_id.as_str() == subnet
        })
    };

    by_zone
        .and_then(|(instance_type, zone)| pools.find(instance_type, zone))
        .or_else(by_subnet)
        .cloned()
        .ok_or_else(|| {
            ProviderError::invalid_response(
                "EC2 CreateFleet launched in a pool the request did not offer",
            )
        })
}

fn interruption(instance: &AwsInstance) -> Option<SpotInterruption> {
    let reason = instance.state_reason()?;
    if reason.code != SPOT_INSTANCE_TERMINATION {
        return None;
    }

    pool_of(instance, reason.at)
}

fn pool_of(instance: &AwsInstance, at: Option<UnixMillis>) -> Option<SpotInterruption> {
    Some(SpotInterruption {
        instance_type: InstanceType::parse(instance.instance_type()?).ok()?,
        availability_zone: AvailabilityZone::parse(instance.availability_zone()?).ok()?,
        at,
    })
}

// EC2 codes and messages are echoed into errors that reach alerts, so keep them printable
// and short
fn safe_code(code: &str) -> String {
    code.chars()
        .filter(|character| character.is_ascii_alphanumeric() || ".-_".contains(*character))
        .take(96)
        .collect()
}

fn safe_message(message: &str) -> String {
    message
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect()
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
