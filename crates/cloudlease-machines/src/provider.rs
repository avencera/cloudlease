//! The providers a machine class can name, behind one [`CloudProvider`]
//!
//! A run object owns holdings of one kind enum, and the sweep lists one inventory, whatever
//! provider each class uses. [`Providers`] is that one provider: it dispatches each create
//! by its spec and each delete, read and tag search by its kind, and its inventory is every
//! configured provider's together, so absence is proven across all of them before the sweep
//! returns a permit.
//!
//! Adding a provider is one more [`MachineKind`] per resource kind, one more
//! [`MachineSpec`] variant and one more field here.
//!
//! A Latitude.sh machine is two resources: the user data record that names its owner, then
//! the server that boots from it. Only the server is billed.
//!
//! An AWS machine is one Spot instance whose root volume and network interface are deleted
//! with it. It is a one-time request that terminates on interruption, so EC2 never leaves a
//! stopped instance, a volume or an open Spot request behind for the sweep. One launch offers
//! EC2 every pool of the class at once, less the pools that just interrupted the run; see
//! [`choose_pools`].

use std::{fmt, time::Duration};

use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId},
    sweep::{Inventory, Observed, Unlisted},
    tags::{ManagedTags, OwnerTags, ResourceKind},
    time::UnixMillis,
};
use cloudlease_provider::{
    aws::{
        AvailabilityZone, AwsKind, AwsRegion, InstanceType, LaunchPools, RegionalSpec, SpotHistory,
        SpotInterruption, SpotPool, SubnetId,
    },
    digitalocean::{DoKind, DoSpec},
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    latitude::{self, LatKind, LatSpec},
    phoenixnap::{Location, PnapKind, PnapSpec, ServerSpec},
    provider::{CloudProvider, CreateSpec, Presence},
};
use serde::{Deserialize, Serialize};

use crate::{boot::InterruptionNotice, money::PaidTime};

/// The most locations a phoenixNAP class, or sites a Latitude.sh class, may try in turn
///
/// Each attempt can take the client's 30 second request timeout, and a create still in flight
/// when its lease ends is found only by the sweep once the core's tag search settles, so the
/// list stays short.
pub const MAX_LOCATIONS: usize = 3;

/// A provider a machine class can use
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderName {
    /// DigitalOcean droplets, with an optional volume
    DigitalOcean,
    /// phoenixNAP Bare Metal Cloud servers
    PhoenixNap,
    /// Latitude.sh bare metal servers
    Latitude,
    /// AWS EC2 Spot instances
    Aws,
}

impl ProviderName {
    /// Returns the name the configuration uses
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DigitalOcean => "digitalocean",
            Self::PhoenixNap => "phoenixnap",
            Self::Latitude => "latitude",
            Self::Aws => "aws",
        }
    }

    /// Reports whether the provider may take a machine back while its lease is live, as EC2
    /// does with a Spot instance when it needs the capacity
    ///
    /// The run object checks such a machine's presence while it is leased, so a lease whose
    /// machine was taken ends as `interrupted` instead of waiting for its heartbeat to lapse.
    #[must_use]
    pub const fn reclaims_machines(self) -> bool {
        matches!(self, Self::Aws)
    }

    /// Returns how a machine of this provider learns that the provider is about to take it
    /// back, for its `lease.json`
    #[must_use]
    pub const fn interruption_notice(self) -> Option<InterruptionNotice> {
        match self {
            Self::Aws => Some(InterruptionNotice::AwsImds),
            Self::DigitalOcean | Self::PhoenixNap | Self::Latitude => None,
        }
    }

    /// Returns the provider's billing increment when it is long enough for an idle machine to
    /// wait out, or `None` for one that bills by the second
    ///
    /// phoenixNAP bills every started hour from the server's `provisionedOn`, and Latitude.sh
    /// every started hour from within about a minute of the create (live tests of 2026-11-24).
    #[must_use]
    pub const fn paid_time(self) -> Option<PaidTime> {
        match self {
            Self::PhoenixNap | Self::Latitude => Some(PaidTime::HOURLY),
            Self::DigitalOcean | Self::Aws => None,
        }
    }
}

impl fmt::Display for ProviderName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Every resource kind a machine can hold, across providers
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MachineKind {
    /// A DigitalOcean droplet
    Droplet,
    /// A DigitalOcean block storage volume
    Volume,
    /// A phoenixNAP Bare Metal Cloud server
    PnapServer,
    /// A Latitude.sh server
    LatServer,
    /// The Latitude.sh user data record a server boots from, which names its owner
    LatUserData,
    /// An AWS EC2 Spot instance, with its root volume
    AwsInstance,
}

impl MachineKind {
    /// Returns the provider that holds resources of this kind
    #[must_use]
    pub const fn provider(self) -> ProviderName {
        match self {
            Self::Droplet | Self::Volume => ProviderName::DigitalOcean,
            Self::PnapServer => ProviderName::PhoenixNap,
            Self::LatServer | Self::LatUserData => ProviderName::Latitude,
            Self::AwsInstance => ProviderName::Aws,
        }
    }

    /// Reports whether this kind is the machine itself rather than its storage or records
    #[must_use]
    pub const fn is_machine(self) -> bool {
        matches!(
            self,
            Self::Droplet | Self::PnapServer | Self::LatServer | Self::AwsInstance
        )
    }

    /// Reports whether the provider bills for a resource of this kind
    ///
    /// A machine whose only created resources are unbilled, such as a Latitude.sh user data
    /// record before a server create refused for stock, has cost nothing.
    #[must_use]
    pub const fn is_billed(self) -> bool {
        !matches!(self, Self::LatUserData)
    }
}

impl ResourceKind for MachineKind {
    const ALL: &'static [Self] = &[
        Self::Droplet,
        Self::Volume,
        Self::PnapServer,
        Self::LatServer,
        Self::LatUserData,
        Self::AwsInstance,
    ];

    // each provider writes and parses its own kind tags, so these only have to match them;
    // Latitude.sh's `server` repeats phoenixNAP's, so nothing may parse a machine kind with
    // `from_tag_name`
    fn tag_name(self) -> &'static str {
        match self {
            Self::Droplet => DoKind::Droplet.tag_name(),
            Self::Volume => DoKind::Volume.tag_name(),
            Self::PnapServer => PnapKind::Server.tag_name(),
            Self::LatServer => LatKind::Server.tag_name(),
            Self::LatUserData => LatKind::UserData.tag_name(),
            Self::AwsInstance => AwsKind::Instance.tag_name(),
        }
    }

    fn delete_rank(self) -> u8 {
        match self {
            Self::Droplet => DoKind::Droplet.delete_rank(),
            Self::Volume => DoKind::Volume.delete_rank(),
            Self::PnapServer => PnapKind::Server.delete_rank(),
            Self::LatServer => LatKind::Server.delete_rank(),
            Self::LatUserData => LatKind::UserData.delete_rank(),
            Self::AwsInstance => AwsKind::Instance.delete_rank(),
        }
    }
}

impl From<DoKind> for MachineKind {
    fn from(kind: DoKind) -> Self {
        match kind {
            DoKind::Droplet => Self::Droplet,
            DoKind::Volume => Self::Volume,
        }
    }
}

impl From<PnapKind> for MachineKind {
    fn from(kind: PnapKind) -> Self {
        match kind {
            PnapKind::Server => Self::PnapServer,
        }
    }
}

impl From<AwsKind> for MachineKind {
    fn from(kind: AwsKind) -> Self {
        match kind {
            AwsKind::Instance => Self::AwsInstance,
        }
    }
}

impl From<LatKind> for MachineKind {
    fn from(kind: LatKind) -> Self {
        match kind {
            LatKind::Server => Self::LatServer,
            LatKind::UserData => Self::LatUserData,
        }
    }
}

/// The places a class tries, in order, each at most once: phoenixNAP locations or
/// Latitude.sh sites
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Locations<L = Location>(Vec<L>);

/// A rejected location list
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("locations must be 1 to {MAX_LOCATIONS} distinct location codes")]
pub struct LocationsError;

impl<L: PartialEq> Locations<L> {
    /// Checks a location list: not empty, at most [`MAX_LOCATIONS`], no repeats
    pub fn new(locations: Vec<L>) -> Result<Self, LocationsError> {
        let repeated = locations
            .iter()
            .enumerate()
            .any(|(index, location)| locations[..index].contains(location));
        if locations.is_empty() || locations.len() > MAX_LOCATIONS || repeated {
            return Err(LocationsError);
        }

        Ok(Self(locations))
    }

    /// Returns the locations in the order they are tried
    #[must_use]
    pub fn as_slice(&self) -> &[L] {
        &self.0
    }
}

/// A phoenixNAP server and the locations it may be created in
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PnapPlacement {
    /// The server; its `location` is replaced by each location in turn
    pub server: ServerSpec,
    /// Where to try, first to last
    pub locations: Locations,
}

/// A Latitude.sh server and the sites it may be created in
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatPlacement {
    /// The server; its `site` is replaced by each site in turn
    pub server: latitude::ServerSpec,
    /// Where to try, first to last
    pub sites: Locations<latitude::Slug>,
}

/// A Latitude.sh create request
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LatitudeSpec {
    /// The user data record, created first
    UserData(latitude::UserDataSpec),
    /// The server, tried in each site until one has stock
    Server(LatPlacement),
}

/// The most instance types an AWS class may name
///
/// Every type is offered in every zone, so with [`MAX_LOCATIONS`] zones one launch offers EC2
/// at most 12 pools.
pub const MAX_INSTANCE_TYPES: usize = 4;

/// How long a pool that interrupted one of a run's machines is left out of the run's next
/// launches
///
/// A pool that just took an instance back is short of Spot capacity, so a machine launched
/// into it again is likely to be interrupted again soon. An hour matches how long EC2 keeps
/// a terminated instance listed, which is all the history the stateless check can see.
pub const INTERRUPTION_LOOKBACK: Duration = Duration::from_secs(60 * 60);

/// The instance types of an AWS class: 1 to [`MAX_INSTANCE_TYPES`], each at most once
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceTypes(Vec<InstanceType>);

/// A rejected instance type list
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("instance_types must be 1 to {MAX_INSTANCE_TYPES} distinct instance types")]
pub struct InstanceTypesError;

impl InstanceTypes {
    /// Checks a type list: not empty, at most [`MAX_INSTANCE_TYPES`], no repeats
    pub fn new(types: Vec<InstanceType>) -> Result<Self, InstanceTypesError> {
        let repeated = types
            .iter()
            .enumerate()
            .any(|(index, instance_type)| types[..index].contains(instance_type));
        if types.is_empty() || types.len() > MAX_INSTANCE_TYPES || repeated {
            return Err(InstanceTypesError);
        }

        Ok(Self(types))
    }

    /// Returns the types in the order the class names them
    #[must_use]
    pub fn as_slice(&self) -> &[InstanceType] {
        &self.0
    }
}

/// An Availability Zone and the subnet an instance is launched into there
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AwsZone {
    /// The zone, such as `us-east-1a`
    pub zone: AvailabilityZone,
    /// A subnet in that zone, such as the default VPC's
    pub subnet_id: SubnetId,
}

/// The pools one AWS launch offers, after leaving out those that interrupted the run
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PoolChoice {
    /// No pool interrupted the run recently, so every pool is offered
    All(LaunchPools),
    /// Some pools interrupted the run recently and are left out
    Without {
        /// The pools offered
        pools: LaunchPools,
        /// The pools left out
        left_out: Vec<SpotPool>,
    },
    /// Every pool interrupted the run recently, so every pool is offered rather than none
    AllInterrupted(LaunchPools),
}

impl PoolChoice {
    /// Returns the pools to offer
    #[must_use]
    pub const fn pools(&self) -> &LaunchPools {
        match self {
            Self::All(pools) | Self::AllInterrupted(pools) | Self::Without { pools, .. } => pools,
        }
    }

    /// Returns the pools to offer, consuming the choice
    #[must_use]
    pub fn into_pools(self) -> LaunchPools {
        match self {
            Self::All(pools) | Self::AllInterrupted(pools) | Self::Without { pools, .. } => pools,
        }
    }
}

/// Leaves out of `pools` every pool that interrupted the run within [`INTERRUPTION_LOOKBACK`]
/// of `now`, unless that leaves none
///
/// An interruption without a time counts as recent: EC2 lists a terminated instance only for
/// about an hour, so having seen it at all bounds its age.
#[must_use]
pub fn choose_pools(
    pools: LaunchPools,
    interruptions: &[SpotInterruption],
    now: UnixMillis,
) -> PoolChoice {
    let recent = |interruption: &&SpotInterruption| {
        interruption.at.is_none_or(|at| {
            at.checked_add(INTERRUPTION_LOOKBACK)
                .is_none_or(|until| until >= now)
        })
    };
    let interrupted = |pool: &SpotPool| {
        interruptions.iter().filter(recent).any(|interruption| {
            pool.is(
                interruption.instance_type.as_str(),
                interruption.availability_zone.as_str(),
            )
        })
    };
    let (left_out, kept): (Vec<_>, Vec<_>) = pools
        .as_slice()
        .iter()
        .cloned()
        .partition(|pool| interrupted(pool));
    if left_out.is_empty() {
        return PoolChoice::All(pools);
    }

    // a non-empty subset of valid pools is valid too
    match LaunchPools::new(kept) {
        Ok(kept) => PoolChoice::Without {
            pools: kept,
            left_out,
        },
        Err(_) => PoolChoice::AllInterrupted(pools),
    }
}

/// A create request for one resource of a machine
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MachineSpec {
    /// A DigitalOcean droplet or volume
    DigitalOcean(DoSpec),
    /// A phoenixNAP server, tried in each location until one has stock
    PhoenixNap(PnapPlacement),
    /// A Latitude.sh user data record or server
    Latitude(LatitudeSpec),
    /// An AWS Spot instance, launched in whichever of its pools EC2 picks
    Aws(RegionalSpec),
}

impl CreateSpec for MachineSpec {
    type Kind = MachineKind;

    fn kind(&self) -> MachineKind {
        match self {
            Self::DigitalOcean(spec) => spec.kind().into(),
            Self::PhoenixNap(_) => MachineKind::PnapServer,
            Self::Latitude(LatitudeSpec::UserData(_)) => MachineKind::LatUserData,
            Self::Latitude(LatitudeSpec::Server(_)) => MachineKind::LatServer,
            Self::Aws(_) => MachineKind::AwsInstance,
        }
    }
}

/// Every provider the machines app can use, as one [`CloudProvider`]
///
/// DigitalOcean is always configured. phoenixNAP, Latitude.sh and AWS are optional: without
/// one, its creates, deletes, reads and tag searches fail with a validation error, and the
/// inventory holds the other providers' resources only.
pub struct Providers<D, P, L, A> {
    digitalocean: D,
    phoenixnap: Option<P>,
    latitude: Option<L>,
    aws: Option<A>,
}

impl<D, P, L, A> Providers<D, P, L, A>
where
    D: CloudProvider<Kind = DoKind, Spec = DoSpec>,
    P: CloudProvider<Kind = PnapKind, Spec = PnapSpec>,
    L: CloudProvider<Kind = LatKind, Spec = LatSpec>,
    A: CloudProvider<Kind = AwsKind, Spec = RegionalSpec> + SpotHistory,
{
    /// Combines the configured adapters
    pub const fn new(
        digitalocean: D,
        phoenixnap: Option<P>,
        latitude: Option<L>,
        aws: Option<A>,
    ) -> Self {
        Self {
            digitalocean,
            phoenixnap,
            latitude,
            aws,
        }
    }

    /// Reports whether resources of `provider` can be created and cleaned up
    #[must_use]
    pub const fn has(&self, provider: ProviderName) -> bool {
        match provider {
            ProviderName::DigitalOcean => true,
            ProviderName::PhoenixNap => self.phoenixnap.is_some(),
            ProviderName::Latitude => self.latitude.is_some(),
            ProviderName::Aws => self.aws.is_some(),
        }
    }

    fn aws(&self) -> ProviderResult<&A> {
        self.aws.as_ref().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorCode::Validation,
                false,
                "AWS is not configured: set AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY and \
                 AWS_REGIONS",
            )
        })
    }

    /// Returns the pools in `region` that interrupted any of `owner`'s machines in about
    /// the last hour
    pub async fn spot_interruptions(
        &self,
        region: &AwsRegion,
        owner: &OwnerId,
    ) -> ProviderResult<Vec<SpotInterruption>> {
        self.aws()?.spot_interruptions(region, owner).await
    }

    fn latitude(&self) -> ProviderResult<&L> {
        self.latitude.as_ref().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorCode::Validation,
                false,
                "Latitude.sh is not configured: set LATITUDE_API_KEY and LATITUDE_PROJECT",
            )
        })
    }

    /// Creates the server in each site in turn until one has stock
    ///
    /// Like phoenixNAP's locations, only `SERVERS_OUT_OF_STOCK` moves on, since it means
    /// Latitude.sh created nothing.
    async fn create_latitude_server(
        &self,
        owner: &OwnerTags,
        placement: &LatPlacement,
    ) -> ProviderResult<ResourceId> {
        let latitude = self.latitude()?;
        let mut out_of_stock = Vec::new();
        for site in placement.sites.as_slice() {
            let spec = LatSpec::Server(latitude::ServerSpec {
                site: site.clone(),
                ..placement.server.clone()
            });
            match latitude.create(owner, &spec).await {
                Err(error) if error.code == ProviderErrorCode::NoCapacity => {
                    out_of_stock.push(site.as_str());
                }
                result => return result,
            }
        }

        Err(ProviderError::new(
            ProviderErrorCode::NoCapacity,
            false,
            format!(
                "Latitude.sh has no {} in stock at {}",
                placement.server.plan.as_str(),
                out_of_stock.join(", ")
            ),
        ))
    }

    fn phoenixnap(&self) -> ProviderResult<&P> {
        self.phoenixnap.as_ref().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorCode::Validation,
                false,
                "phoenixNAP is not configured: set PNAP_CLIENT_ID and PNAP_CLIENT_SECRET",
            )
        })
    }

    /// Creates the server in each location in turn until one has stock
    ///
    /// Only a `NoCapacity` answer moves on: it means phoenixNAP created nothing. Any other
    /// failure may have created the server, so it ends the create for the lease's cleanup.
    async fn create_server(
        &self,
        owner: &OwnerTags,
        placement: &PnapPlacement,
    ) -> ProviderResult<ResourceId> {
        let phoenixnap = self.phoenixnap()?;
        let mut out_of_stock = Vec::new();
        for location in placement.locations.as_slice() {
            let spec = PnapSpec::Server(ServerSpec {
                location: location.clone(),
                ..placement.server.clone()
            });
            match phoenixnap.create(owner, &spec).await {
                Err(error) if error.code == ProviderErrorCode::NoCapacity => {
                    out_of_stock.push(location.as_str());
                }
                result => return result,
            }
        }

        Err(ProviderError::new(
            ProviderErrorCode::NoCapacity,
            false,
            format!(
                "phoenixNAP has no {} in stock at {}",
                placement.server.server_type.as_str(),
                out_of_stock.join(", ")
            ),
        ))
    }
}

/// One provider's own kind for a machine kind
enum ProviderKind {
    DigitalOcean(DoKind),
    PhoenixNap(PnapKind),
    Latitude(LatKind),
    Aws(AwsKind),
}

impl From<MachineKind> for ProviderKind {
    fn from(kind: MachineKind) -> Self {
        match kind {
            MachineKind::Droplet => Self::DigitalOcean(DoKind::Droplet),
            MachineKind::Volume => Self::DigitalOcean(DoKind::Volume),
            MachineKind::PnapServer => Self::PhoenixNap(PnapKind::Server),
            MachineKind::LatServer => Self::Latitude(LatKind::Server),
            MachineKind::LatUserData => Self::Latitude(LatKind::UserData),
            MachineKind::AwsInstance => Self::Aws(AwsKind::Instance),
        }
    }
}

/// Adds one provider's list to the inventory, or names the provider if its list failed
fn add<K: Into<MachineKind>>(
    into: &mut Inventory<MachineKind>,
    provider: ProviderName,
    listed: ProviderResult<Inventory<K>>,
) {
    let inventory = match listed {
        Ok(inventory) => inventory,
        Err(error) => {
            into.unlisted.push(Unlisted {
                provider: provider.as_str().to_owned(),
                error: error.to_string(),
            });
            return;
        }
    };

    into.managed
        .extend(inventory.managed.into_iter().map(|observed| Observed {
            id: observed.id,
            tags: ManagedTags {
                owner: observed.tags.owner,
                generation: observed.tags.generation,
                kind: observed.tags.kind.into(),
                ceiling: observed.tags.ceiling,
            },
        }));
    into.unrecognized.extend(inventory.unrecognized);
    into.unlisted.extend(inventory.unlisted);
}

impl<D, P, L, A> CloudProvider for Providers<D, P, L, A>
where
    D: CloudProvider<Kind = DoKind, Spec = DoSpec>,
    P: CloudProvider<Kind = PnapKind, Spec = PnapSpec>,
    L: CloudProvider<Kind = LatKind, Spec = LatSpec>,
    A: CloudProvider<Kind = AwsKind, Spec = RegionalSpec> + SpotHistory,
{
    type Kind = MachineKind;
    type Spec = MachineSpec;

    async fn create(&self, owner: &OwnerTags, spec: &MachineSpec) -> ProviderResult<ResourceId> {
        match spec {
            MachineSpec::DigitalOcean(spec) => self.digitalocean.create(owner, spec).await,
            MachineSpec::PhoenixNap(placement) => self.create_server(owner, placement).await,
            MachineSpec::Latitude(LatitudeSpec::UserData(spec)) => {
                self.latitude()?
                    .create(owner, &LatSpec::UserData(spec.clone()))
                    .await
            }
            MachineSpec::Latitude(LatitudeSpec::Server(placement)) => {
                self.create_latitude_server(owner, placement).await
            }
            // one launch offers every pool, so there is nothing to fall through to here
            MachineSpec::Aws(spec) => self.aws()?.create(owner, spec).await,
        }
    }

    async fn delete(&self, kind: MachineKind, id: &ResourceId) -> ProviderResult<()> {
        match kind.into() {
            ProviderKind::DigitalOcean(kind) => self.digitalocean.delete(kind, id).await,
            ProviderKind::PhoenixNap(kind) => self.phoenixnap()?.delete(kind, id).await,
            ProviderKind::Latitude(kind) => self.latitude()?.delete(kind, id).await,
            ProviderKind::Aws(kind) => self.aws()?.delete(kind, id).await,
        }
    }

    async fn presence(&self, kind: MachineKind, id: &ResourceId) -> ProviderResult<Presence> {
        match kind.into() {
            ProviderKind::DigitalOcean(kind) => self.digitalocean.presence(kind, id).await,
            ProviderKind::PhoenixNap(kind) => self.phoenixnap()?.presence(kind, id).await,
            ProviderKind::Latitude(kind) => self.latitude()?.presence(kind, id).await,
            ProviderKind::Aws(kind) => self.aws()?.presence(kind, id).await,
        }
    }

    async fn find(
        &self,
        owner: &OwnerId,
        generation: Generation,
        kind: MachineKind,
    ) -> ProviderResult<Vec<ResourceId>> {
        match kind.into() {
            ProviderKind::DigitalOcean(kind) => {
                self.digitalocean.find(owner, generation, kind).await
            }
            ProviderKind::PhoenixNap(kind) => {
                self.phoenixnap()?.find(owner, generation, kind).await
            }
            ProviderKind::Latitude(kind) => self.latitude()?.find(owner, generation, kind).await,
            ProviderKind::Aws(kind) => self.aws()?.find(owner, generation, kind).await,
        }
    }

    /// Lists every configured provider, naming each one whose list fails instead of failing
    ///
    /// A provider in the inventory's `unlisted` makes it incomplete: the sweep still deletes
    /// what the other providers listed, but returns no permit, since its resource may sit at
    /// the provider that failed. Each failing provider keeps its own alert streak, even when
    /// every list fails. Leases still end on their run objects' alarms meanwhile.
    async fn inventory(&self) -> ProviderResult<Inventory<MachineKind>> {
        let mut inventory = Inventory {
            managed: Vec::new(),
            unrecognized: Vec::new(),
            unlisted: Vec::new(),
        };
        let digitalocean = self.digitalocean.inventory().await;
        add(&mut inventory, ProviderName::DigitalOcean, digitalocean);
        if let Some(phoenixnap) = &self.phoenixnap {
            add(
                &mut inventory,
                ProviderName::PhoenixNap,
                phoenixnap.inventory().await,
            );
        }
        if let Some(latitude) = &self.latitude {
            add(
                &mut inventory,
                ProviderName::Latitude,
                latitude.inventory().await,
            );
        }
        if let Some(aws) = &self.aws {
            add(&mut inventory, ProviderName::Aws, aws.inventory().await);
        }

        Ok(inventory)
    }
}

#[cfg(test)]
mod tests {
    use cloudlease_core::{tags::ResourceKind, time::UnixMillis};
    use cloudlease_provider::{
        aws::{AvailabilityZone, InstanceType, LaunchPools, SpotInterruption, SpotPool, SubnetId},
        digitalocean::DoKind,
        phoenixnap::Location,
    };

    use super::{INTERRUPTION_LOOKBACK, Locations, MachineKind, PoolChoice, choose_pools};

    fn pool(instance_type: &str, zone: &str) -> SpotPool {
        SpotPool {
            instance_type: InstanceType::parse(instance_type).unwrap(),
            availability_zone: AvailabilityZone::parse(zone).unwrap(),
            subnet_id: SubnetId::parse("subnet-01234567").unwrap(),
        }
    }

    fn interruption(instance_type: &str, zone: &str, at: Option<u64>) -> SpotInterruption {
        SpotInterruption {
            instance_type: InstanceType::parse(instance_type).unwrap(),
            availability_zone: AvailabilityZone::parse(zone).unwrap(),
            at: at.and_then(UnixMillis::new),
        }
    }

    #[test]
    fn a_launch_leaves_out_pools_that_interrupted_the_run_within_the_hour() {
        let pools = LaunchPools::new(vec![
            pool("c8a.4xlarge", "us-east-1a"),
            pool("c8a.4xlarge", "us-east-1b"),
            pool("c7a.4xlarge", "us-east-1a"),
        ])
        .unwrap();
        let hour = u64::try_from(INTERRUPTION_LOOKBACK.as_millis()).unwrap();
        let now = UnixMillis::new(10 * hour).unwrap();

        // one recent, one with no time, which counts as recent, and one past the lookback
        let choice = choose_pools(
            pools.clone(),
            &[
                interruption("c8a.4xlarge", "us-east-1a", Some(10 * hour - 60_000)),
                interruption("c7a.4xlarge", "us-east-1a", None),
                interruption("c8a.4xlarge", "us-east-1b", Some(10 * hour - hour - 1)),
            ],
            now,
        );
        assert_eq!(
            choice,
            PoolChoice::Without {
                pools: LaunchPools::new(vec![pool("c8a.4xlarge", "us-east-1b")]).unwrap(),
                left_out: vec![
                    pool("c8a.4xlarge", "us-east-1a"),
                    pool("c7a.4xlarge", "us-east-1a")
                ],
            }
        );

        // with no pool left, every pool is offered rather than none
        let everywhere: Vec<_> = pools
            .as_slice()
            .iter()
            .map(|pool| {
                interruption(
                    pool.instance_type.as_str(),
                    pool.availability_zone.as_str(),
                    None,
                )
            })
            .collect();
        assert_eq!(
            choose_pools(pools.clone(), &everywhere, now),
            PoolChoice::AllInterrupted(pools.clone())
        );
        assert_eq!(
            choose_pools(pools.clone(), &[], now),
            PoolChoice::All(pools)
        );
    }

    #[test]
    fn machine_kinds_keep_their_providers_tags_and_delete_order() {
        // only the kinds whose tag names are unique round-trip; providers parse their own
        for kind in MachineKind::ALL {
            if *kind != MachineKind::LatServer {
                assert_eq!(MachineKind::from_tag_name(kind.tag_name()), Some(*kind));
            }
        }
        assert_eq!(MachineKind::PnapServer.tag_name(), "server");
        assert_eq!(MachineKind::LatServer.tag_name(), "server");
        // a Latitude.sh record is deleted only once its server is gone, and is never billed
        assert!(MachineKind::LatServer.delete_rank() < MachineKind::LatUserData.delete_rank());
        assert!(!MachineKind::LatUserData.is_billed() && MachineKind::LatServer.is_billed());
        assert!(MachineKind::Droplet.delete_rank() < MachineKind::Volume.delete_rank());
        assert_eq!(MachineKind::from(DoKind::Volume), MachineKind::Volume);
        assert!(MachineKind::PnapServer.is_machine() && !MachineKind::Volume.is_machine());
        assert_eq!(MachineKind::AwsInstance.tag_name(), "instance");
        assert!(MachineKind::AwsInstance.is_machine() && MachineKind::AwsInstance.is_billed());
    }

    #[test]
    fn a_location_list_is_short_and_has_no_repeats() {
        let location = |code: &str| Location::parse(code).unwrap();

        assert!(Locations::new(vec![location("SEA"), location("NLD")]).is_ok());
        assert!(Locations::<Location>::new(Vec::new()).is_err());
        assert!(Locations::new(vec![location("SEA"), location("SEA")]).is_err());
        assert!(
            Locations::new(vec![
                location("SEA"),
                location("NLD"),
                location("PHX"),
                location("ASH")
            ])
            .is_err()
        );
    }
}
