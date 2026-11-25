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
//! EC2 every pool of the class at once, and EC2's `price-capacity-optimized` allocation picks
//! the pool least likely to be interrupted.

use std::fmt;

use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId},
    sweep::{Inventory, Observed, Unlisted},
    tags::{ManagedTags, OwnerTags, ResourceKind},
};
use cloudlease_provider::{
    aws::{AvailabilityZone, AwsKind, InstanceType, RegionalSpec, SubnetId},
    digitalocean::{DoKind, DoSpec},
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    latitude::{self, LatKind, LatSpec},
    phoenixnap::{Location, PnapKind, PnapSpec, ServerSpec},
    provider::{CloudProvider, CreateSpec, Presence},
};
use serde::{Deserialize, Serialize};

use crate::money::PaidTime;

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
            Self::Droplet => ProviderName::DigitalOcean,
            Self::PnapServer => ProviderName::PhoenixNap,
            Self::LatServer | Self::LatUserData => ProviderName::Latitude,
            Self::AwsInstance => ProviderName::Aws,
        }
    }

    /// Reports whether this kind is the machine itself rather than a record it boots from
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
            Self::PnapServer => PnapKind::Server.tag_name(),
            Self::LatServer => LatKind::Server.tag_name(),
            Self::LatUserData => LatKind::UserData.tag_name(),
            Self::AwsInstance => AwsKind::Instance.tag_name(),
        }
    }

    fn delete_rank(self) -> u8 {
        match self {
            Self::Droplet => DoKind::Droplet.delete_rank(),
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

/// A create request for one resource of a machine
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MachineSpec {
    /// A DigitalOcean droplet
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

/// An optional provider's adapter, or why the Worker has none
#[derive(Clone, Debug)]
pub enum Optional<P> {
    /// Its settings are complete
    Configured(P),
    /// None of its settings are set, so this deployment never used it
    Unset,
    /// Some of its settings are set but they do not build an adapter, such as one secret
    /// without the other; it may still have machines, which nothing can then see
    Broken(String),
}

impl<P> Optional<P> {
    /// Sorts the result of reading a provider's settings: an adapter, no settings, or an
    /// error
    pub fn from_settings<E: fmt::Display>(settings: Result<Option<P>, E>) -> Self {
        match settings {
            Ok(Some(adapter)) => Self::Configured(adapter),
            Ok(None) => Self::Unset,
            Err(error) => Self::Broken(error.to_string()),
        }
    }

    fn get(&self, name: &str, settings: &str) -> ProviderResult<&P> {
        match self {
            Self::Configured(adapter) => Ok(adapter),
            Self::Unset => Err(ProviderError::new(
                ProviderErrorCode::Validation,
                false,
                format!("{name} is not configured: set {settings}"),
            )),
            Self::Broken(error) => Err(broken(name, error)),
        }
    }
}

fn broken(name: &str, error: &str) -> ProviderError {
    ProviderError::new(
        ProviderErrorCode::Validation,
        false,
        format!("{name} settings are broken: {error}"),
    )
}

/// Every provider the machines app can use, as one [`CloudProvider`]
///
/// DigitalOcean is always configured. phoenixNAP, Latitude.sh and AWS are optional: without
/// one, its creates, deletes, reads and tag searches fail with a validation error. The
/// inventory leaves an unset provider out, and lists a broken one as unlisted, since it may
/// still have machines.
pub struct Providers<D, P, L, A> {
    digitalocean: D,
    phoenixnap: Optional<P>,
    latitude: Optional<L>,
    aws: Optional<A>,
}

impl<D, P, L, A> Providers<D, P, L, A>
where
    D: CloudProvider<Kind = DoKind, Spec = DoSpec>,
    P: CloudProvider<Kind = PnapKind, Spec = PnapSpec>,
    L: CloudProvider<Kind = LatKind, Spec = LatSpec>,
    A: CloudProvider<Kind = AwsKind, Spec = RegionalSpec>,
{
    /// Combines the configured adapters
    pub const fn new(
        digitalocean: D,
        phoenixnap: Optional<P>,
        latitude: Optional<L>,
        aws: Optional<A>,
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
            ProviderName::PhoenixNap => matches!(self.phoenixnap, Optional::Configured(_)),
            ProviderName::Latitude => matches!(self.latitude, Optional::Configured(_)),
            ProviderName::Aws => matches!(self.aws, Optional::Configured(_)),
        }
    }

    fn aws(&self) -> ProviderResult<&A> {
        self.aws.get(
            "AWS",
            "AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY and AWS_REGIONS",
        )
    }

    fn latitude(&self) -> ProviderResult<&L> {
        self.latitude
            .get("Latitude.sh", "LATITUDE_API_KEY and LATITUDE_PROJECT")
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
        self.phoenixnap
            .get("phoenixNAP", "PNAP_CLIENT_ID and PNAP_CLIENT_SECRET")
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
            MachineKind::PnapServer => Self::PhoenixNap(PnapKind::Server),
            MachineKind::LatServer => Self::Latitude(LatKind::Server),
            MachineKind::LatUserData => Self::Latitude(LatKind::UserData),
            MachineKind::AwsInstance => Self::Aws(AwsKind::Instance),
        }
    }
}

/// Adds one provider's list to the inventory, or names the provider if its list failed
async fn add_optional<C>(
    into: &mut Inventory<MachineKind>,
    provider: ProviderName,
    adapter: &Optional<C>,
) where
    C: CloudProvider,
    C::Kind: Into<MachineKind>,
{
    let listed = match adapter {
        Optional::Configured(adapter) => adapter.inventory().await,
        Optional::Unset => return,
        Optional::Broken(error) => Err(broken(provider.as_str(), error)),
    };

    add(into, provider, listed);
}

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
    A: CloudProvider<Kind = AwsKind, Spec = RegionalSpec>,
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
        add_optional(&mut inventory, ProviderName::PhoenixNap, &self.phoenixnap).await;
        add_optional(&mut inventory, ProviderName::Latitude, &self.latitude).await;
        add_optional(&mut inventory, ProviderName::Aws, &self.aws).await;

        Ok(inventory)
    }
}

#[cfg(test)]
mod tests {
    use cloudlease_core::tags::ResourceKind;
    use cloudlease_provider::phoenixnap::Location;

    use super::{Locations, MachineKind};

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
        assert!(MachineKind::PnapServer.is_machine() && !MachineKind::LatUserData.is_machine());
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
