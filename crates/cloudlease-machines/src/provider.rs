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

use std::fmt;

use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId},
    sweep::{Inventory, Observed},
    tags::{ManagedTags, OwnerTags, ResourceKind},
};
use cloudlease_provider::{
    digitalocean::{DoKind, DoSpec},
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    phoenixnap::{Location, PnapKind, PnapSpec, ServerSpec},
    provider::{CloudProvider, CreateSpec, Presence},
};
use serde::{Deserialize, Serialize};

/// The most locations a phoenixNAP class may try in turn
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
}

impl ProviderName {
    /// Returns the name the configuration uses
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DigitalOcean => "digitalocean",
            Self::PhoenixNap => "phoenixnap",
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
}

impl MachineKind {
    /// Returns the provider that holds resources of this kind
    #[must_use]
    pub const fn provider(self) -> ProviderName {
        match self {
            Self::Droplet | Self::Volume => ProviderName::DigitalOcean,
            Self::PnapServer => ProviderName::PhoenixNap,
        }
    }

    /// Reports whether this kind is the machine itself rather than its storage
    #[must_use]
    pub const fn is_machine(self) -> bool {
        matches!(self, Self::Droplet | Self::PnapServer)
    }
}

impl ResourceKind for MachineKind {
    const ALL: &'static [Self] = &[Self::Droplet, Self::Volume, Self::PnapServer];

    // each provider writes and parses its own kind tags, so these only have to match them;
    // the names are distinct today, and a later provider whose tag repeats one must not rely
    // on `from_tag_name`
    fn tag_name(self) -> &'static str {
        match self {
            Self::Droplet => DoKind::Droplet.tag_name(),
            Self::Volume => DoKind::Volume.tag_name(),
            Self::PnapServer => PnapKind::Server.tag_name(),
        }
    }

    fn delete_rank(self) -> u8 {
        match self {
            Self::Droplet => DoKind::Droplet.delete_rank(),
            Self::Volume => DoKind::Volume.delete_rank(),
            Self::PnapServer => PnapKind::Server.delete_rank(),
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

/// The phoenixNAP locations a class tries, in order, each at most once
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Locations(Vec<Location>);

/// A rejected location list
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("locations must be 1 to {MAX_LOCATIONS} distinct location codes")]
pub struct LocationsError;

impl Locations {
    /// Checks a location list: not empty, at most [`MAX_LOCATIONS`], no repeats
    pub fn new(locations: Vec<Location>) -> Result<Self, LocationsError> {
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
    pub fn as_slice(&self) -> &[Location] {
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

/// A create request for one resource of a machine
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MachineSpec {
    /// A DigitalOcean droplet or volume
    DigitalOcean(DoSpec),
    /// A phoenixNAP server, tried in each location until one has stock
    PhoenixNap(PnapPlacement),
}

impl CreateSpec for MachineSpec {
    type Kind = MachineKind;

    fn kind(&self) -> MachineKind {
        match self {
            Self::DigitalOcean(spec) => spec.kind().into(),
            Self::PhoenixNap(_) => MachineKind::PnapServer,
        }
    }
}

/// Every provider the machines app can use, as one [`CloudProvider`]
///
/// DigitalOcean is always configured. phoenixNAP is optional: without it, a phoenixNAP
/// create, delete, read or tag search fails with a validation error, and the inventory
/// holds DigitalOcean resources only.
pub struct Providers<D, P> {
    digitalocean: D,
    phoenixnap: Option<P>,
}

impl<D, P> Providers<D, P>
where
    D: CloudProvider<Kind = DoKind, Spec = DoSpec>,
    P: CloudProvider<Kind = PnapKind, Spec = PnapSpec>,
{
    /// Combines the configured adapters
    pub const fn new(digitalocean: D, phoenixnap: Option<P>) -> Self {
        Self {
            digitalocean,
            phoenixnap,
        }
    }

    /// Reports whether resources of `provider` can be created and cleaned up
    #[must_use]
    pub const fn has(&self, provider: ProviderName) -> bool {
        match provider {
            ProviderName::DigitalOcean => true,
            ProviderName::PhoenixNap => self.phoenixnap.is_some(),
        }
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

fn digitalocean_kind(kind: MachineKind) -> Option<DoKind> {
    match kind {
        MachineKind::Droplet => Some(DoKind::Droplet),
        MachineKind::Volume => Some(DoKind::Volume),
        MachineKind::PnapServer => None,
    }
}

fn widen<K: Into<MachineKind>>(inventory: Inventory<K>, into: &mut Inventory<MachineKind>) {
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
}

impl<D, P> CloudProvider for Providers<D, P>
where
    D: CloudProvider<Kind = DoKind, Spec = DoSpec>,
    P: CloudProvider<Kind = PnapKind, Spec = PnapSpec>,
{
    type Kind = MachineKind;
    type Spec = MachineSpec;

    async fn create(&self, owner: &OwnerTags, spec: &MachineSpec) -> ProviderResult<ResourceId> {
        match spec {
            MachineSpec::DigitalOcean(spec) => self.digitalocean.create(owner, spec).await,
            MachineSpec::PhoenixNap(placement) => self.create_server(owner, placement).await,
        }
    }

    async fn delete(&self, kind: MachineKind, id: &ResourceId) -> ProviderResult<()> {
        match digitalocean_kind(kind) {
            Some(kind) => self.digitalocean.delete(kind, id).await,
            None => self.phoenixnap()?.delete(PnapKind::Server, id).await,
        }
    }

    async fn presence(&self, kind: MachineKind, id: &ResourceId) -> ProviderResult<Presence> {
        match digitalocean_kind(kind) {
            Some(kind) => self.digitalocean.presence(kind, id).await,
            None => self.phoenixnap()?.presence(PnapKind::Server, id).await,
        }
    }

    async fn find(
        &self,
        owner: &OwnerId,
        generation: Generation,
        kind: MachineKind,
    ) -> ProviderResult<Vec<ResourceId>> {
        match digitalocean_kind(kind) {
            Some(kind) => self.digitalocean.find(owner, generation, kind).await,
            None => {
                self.phoenixnap()?
                    .find(owner, generation, PnapKind::Server)
                    .await
            }
        }
    }

    /// Lists every configured provider; if any list fails, the whole inventory fails
    ///
    /// A partial inventory would let the sweep return a permit whose resource sits at the
    /// provider that failed to list, so a failure stops the sweep from deleting anything,
    /// as a failed DigitalOcean list always did. Leases still end on their run objects'
    /// alarms meanwhile.
    async fn inventory(&self) -> ProviderResult<Inventory<MachineKind>> {
        let mut inventory = Inventory {
            managed: Vec::new(),
            unrecognized: Vec::new(),
        };
        widen(self.digitalocean.inventory().await?, &mut inventory);
        if let Some(phoenixnap) = &self.phoenixnap {
            widen(phoenixnap.inventory().await?, &mut inventory);
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
        for kind in MachineKind::ALL {
            assert_eq!(MachineKind::from_tag_name(kind.tag_name()), Some(*kind));
        }
        assert_eq!(MachineKind::PnapServer.tag_name(), "server");
        assert!(MachineKind::Droplet.delete_rank() < MachineKind::Volume.delete_rank());
        assert_eq!(MachineKind::from(DoKind::Volume), MachineKind::Volume);
        assert!(MachineKind::PnapServer.is_machine() && !MachineKind::Volume.is_machine());
    }

    #[test]
    fn a_location_list_is_short_and_has_no_repeats() {
        let location = |code: &str| Location::parse(code).unwrap();

        assert!(Locations::new(vec![location("SEA"), location("NLD")]).is_ok());
        assert!(Locations::new(Vec::new()).is_err());
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
