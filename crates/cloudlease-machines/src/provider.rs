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

use std::fmt;

use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId},
    sweep::{Inventory, Observed, Unlisted},
    tags::{ManagedTags, OwnerTags, ResourceKind},
};
use cloudlease_provider::{
    digitalocean::{DoKind, DoSpec},
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    latitude::{self, LatKind, LatSpec},
    phoenixnap::{Location, PnapKind, PnapSpec, ServerSpec},
    provider::{CloudProvider, CreateSpec, Presence},
};
use serde::{Deserialize, Serialize};

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
}

impl ProviderName {
    /// Returns the name the configuration uses
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DigitalOcean => "digitalocean",
            Self::PhoenixNap => "phoenixnap",
            Self::Latitude => "latitude",
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
}

impl MachineKind {
    /// Returns the provider that holds resources of this kind
    #[must_use]
    pub const fn provider(self) -> ProviderName {
        match self {
            Self::Droplet | Self::Volume => ProviderName::DigitalOcean,
            Self::PnapServer => ProviderName::PhoenixNap,
            Self::LatServer | Self::LatUserData => ProviderName::Latitude,
        }
    }

    /// Reports whether this kind is the machine itself rather than its storage or records
    #[must_use]
    pub const fn is_machine(self) -> bool {
        matches!(self, Self::Droplet | Self::PnapServer | Self::LatServer)
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
        }
    }

    fn delete_rank(self) -> u8 {
        match self {
            Self::Droplet => DoKind::Droplet.delete_rank(),
            Self::Volume => DoKind::Volume.delete_rank(),
            Self::PnapServer => PnapKind::Server.delete_rank(),
            Self::LatServer => LatKind::Server.delete_rank(),
            Self::LatUserData => LatKind::UserData.delete_rank(),
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

/// A create request for one resource of a machine
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MachineSpec {
    /// A DigitalOcean droplet or volume
    DigitalOcean(DoSpec),
    /// A phoenixNAP server, tried in each location until one has stock
    PhoenixNap(PnapPlacement),
    /// A Latitude.sh user data record or server
    Latitude(LatitudeSpec),
}

impl CreateSpec for MachineSpec {
    type Kind = MachineKind;

    fn kind(&self) -> MachineKind {
        match self {
            Self::DigitalOcean(spec) => spec.kind().into(),
            Self::PhoenixNap(_) => MachineKind::PnapServer,
            Self::Latitude(LatitudeSpec::UserData(_)) => MachineKind::LatUserData,
            Self::Latitude(LatitudeSpec::Server(_)) => MachineKind::LatServer,
        }
    }
}

/// Every provider the machines app can use, as one [`CloudProvider`]
///
/// DigitalOcean is always configured. phoenixNAP and Latitude.sh are optional: without one,
/// its creates, deletes, reads and tag searches fail with a validation error, and the
/// inventory holds the other providers' resources only.
pub struct Providers<D, P, L> {
    digitalocean: D,
    phoenixnap: Option<P>,
    latitude: Option<L>,
}

impl<D, P, L> Providers<D, P, L>
where
    D: CloudProvider<Kind = DoKind, Spec = DoSpec>,
    P: CloudProvider<Kind = PnapKind, Spec = PnapSpec>,
    L: CloudProvider<Kind = LatKind, Spec = LatSpec>,
{
    /// Combines the configured adapters
    pub const fn new(digitalocean: D, phoenixnap: Option<P>, latitude: Option<L>) -> Self {
        Self {
            digitalocean,
            phoenixnap,
            latitude,
        }
    }

    /// Reports whether resources of `provider` can be created and cleaned up
    #[must_use]
    pub const fn has(&self, provider: ProviderName) -> bool {
        match provider {
            ProviderName::DigitalOcean => true,
            ProviderName::PhoenixNap => self.phoenixnap.is_some(),
            ProviderName::Latitude => self.latitude.is_some(),
        }
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
}

impl From<MachineKind> for ProviderKind {
    fn from(kind: MachineKind) -> Self {
        match kind {
            MachineKind::Droplet => Self::DigitalOcean(DoKind::Droplet),
            MachineKind::Volume => Self::DigitalOcean(DoKind::Volume),
            MachineKind::PnapServer => Self::PhoenixNap(PnapKind::Server),
            MachineKind::LatServer => Self::Latitude(LatKind::Server),
            MachineKind::LatUserData => Self::Latitude(LatKind::UserData),
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

impl<D, P, L> CloudProvider for Providers<D, P, L>
where
    D: CloudProvider<Kind = DoKind, Spec = DoSpec>,
    P: CloudProvider<Kind = PnapKind, Spec = PnapSpec>,
    L: CloudProvider<Kind = LatKind, Spec = LatSpec>,
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
        }
    }

    async fn delete(&self, kind: MachineKind, id: &ResourceId) -> ProviderResult<()> {
        match kind.into() {
            ProviderKind::DigitalOcean(kind) => self.digitalocean.delete(kind, id).await,
            ProviderKind::PhoenixNap(kind) => self.phoenixnap()?.delete(kind, id).await,
            ProviderKind::Latitude(kind) => self.latitude()?.delete(kind, id).await,
        }
    }

    async fn presence(&self, kind: MachineKind, id: &ResourceId) -> ProviderResult<Presence> {
        match kind.into() {
            ProviderKind::DigitalOcean(kind) => self.digitalocean.presence(kind, id).await,
            ProviderKind::PhoenixNap(kind) => self.phoenixnap()?.presence(kind, id).await,
            ProviderKind::Latitude(kind) => self.latitude()?.presence(kind, id).await,
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

        Ok(inventory)
    }
}

#[cfg(test)]
mod tests {
    use cloudlease_core::tags::ResourceKind;
    use cloudlease_provider::{digitalocean::DoKind, phoenixnap::Location};

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
        assert!(MachineKind::Droplet.delete_rank() < MachineKind::Volume.delete_rank());
        assert_eq!(MachineKind::from(DoKind::Volume), MachineKind::Volume);
        assert!(MachineKind::PnapServer.is_machine() && !MachineKind::Volume.is_machine());
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
