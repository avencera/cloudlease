//! Droplets and block storage volumes on DigitalOcean
//!
//! Droplets have delete rank 0 and volumes rank 1, so cleanup confirms a droplet gone before
//! it deletes the volumes the droplet may still hold. DigitalOcean's volume list has no tag
//! filter, so inventory reads every volume and keeps those with the app's managed tag.

pub mod client;

use std::num::{NonZeroU32, NonZeroU64};

use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId},
    sweep::{Inventory, Observed, Unrecognized},
    tags::{OwnerTags, ResourceKind, TagMatch, TagScheme, UnrecognizedReason},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    error::{ProviderError, ProviderResult},
    http::{HttpTransport, Runtime},
    provider::{CloudProvider, CreateSpec, Presence},
};

pub use client::{ClientOptions, DigitalOceanClient, DigitalOceanToken, RetryPolicy};

/// The largest user data DigitalOcean accepts
pub const MAX_USER_DATA_BYTES: usize = 64 * 1024;

/// A DigitalOcean resource kind cloudlease manages
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DoKind {
    /// A droplet
    Droplet,
    /// A block storage volume
    Volume,
}

impl ResourceKind for DoKind {
    const ALL: &'static [Self] = &[Self::Droplet, Self::Volume];

    fn tag_name(self) -> &'static str {
        match self {
            Self::Droplet => "droplet",
            Self::Volume => "volume",
        }
    }

    fn delete_rank(self) -> u8 {
        match self {
            Self::Droplet => 0,
            Self::Volume => 1,
        }
    }
}

macro_rules! validated {
    ($(#[$meta:meta])* $name:ident, $message:literal, $valid:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps a value
            pub fn parse(value: impl Into<String>) -> ProviderResult<Self> {
                let value = value.into();
                let valid: fn(&str) -> bool = $valid;
                if !valid(&value) {
                    return Err(ProviderError::validation($message));
                }

                Ok(Self(value))
            }

            /// Returns the value
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

validated!(
    /// A region or size slug, such as `nyc3` or `c-16`
    Slug,
    "a slug must be 1 to 64 lowercase letters, digits or dashes",
    |value| {
        !value.is_empty()
            && value.len() <= 64
            && value.chars().all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
            })
    }
);

validated!(
    /// A resource name valid for both droplets and volumes
    ResourceName,
    "a name must be a lowercase letter followed by up to 62 lowercase letters, digits or dashes",
    |value| {
        value.len() <= 63
            && value.starts_with(|character: char| character.is_ascii_lowercase())
            && value.chars().all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
            })
    }
);

validated!(
    /// The user data a droplet runs at first boot, such as a cloud-init document
    BootScript,
    "user data must be non-empty and at most 64 KiB",
    |value| !value.is_empty() && value.len() <= MAX_USER_DATA_BYTES
);

/// The image a droplet boots
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DropletImage {
    /// A public image slug, such as `ubuntu-24-04-x64`
    Slug(Slug),
    /// A custom image or snapshot ID
    Id(NonZeroU64),
}

/// A droplet to create
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DropletSpec {
    /// The name
    pub name: ResourceName,
    /// The region slug
    pub region: Slug,
    /// The size slug
    pub size: Slug,
    /// The boot image
    pub image: DropletImage,
    /// The first-boot user data
    pub boot: BootScript,
    /// SSH key IDs to install
    pub ssh_key_ids: Vec<NonZeroU64>,
    /// Volumes to attach at create
    pub volume_ids: Vec<ResourceId>,
    /// The VPC, when not the region default
    pub vpc_uuid: Option<ResourceId>,
}

/// A block storage volume to create
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeSpec {
    /// The name, unique within the region
    pub name: ResourceName,
    /// The region slug, which must match the droplet it attaches to
    pub region: Slug,
    /// The size in GiB
    pub size_gib: NonZeroU32,
}

/// A DigitalOcean create request
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DoSpec {
    /// Create a droplet
    Droplet(DropletSpec),
    /// Create a volume
    Volume(VolumeSpec),
}

impl CreateSpec for DoSpec {
    type Kind = DoKind;

    fn kind(&self) -> DoKind {
        match self {
            Self::Droplet(_) => DoKind::Droplet,
            Self::Volume(_) => DoKind::Volume,
        }
    }
}

/// The DigitalOcean adapter for one app's tag scheme
pub struct DigitalOcean<T, R> {
    client: DigitalOceanClient<T, R>,
    scheme: TagScheme,
}

impl<T: HttpTransport, R: Runtime> DigitalOcean<T, R> {
    /// Creates an adapter that manages resources with `scheme`'s tags
    pub const fn new(client: DigitalOceanClient<T, R>, scheme: TagScheme) -> Self {
        Self { client, scheme }
    }

    /// Returns the REST client
    pub const fn client(&self) -> &DigitalOceanClient<T, R> {
        &self.client
    }

    /// Returns the tag scheme
    pub const fn scheme(&self) -> &TagScheme {
        &self.scheme
    }

    fn classify(
        &self,
        actual: DoKind,
        id: String,
        name: String,
        tags: Vec<String>,
    ) -> Option<Result<Observed<DoKind>, Unrecognized>> {
        let reason = match self.scheme.classify::<DoKind>(&tags) {
            TagMatch::Foreign => return None,
            TagMatch::Managed(parsed) if parsed.kind == actual => {
                return Some(match ResourceId::parse(&id) {
                    Ok(id) => Ok(Observed { id, tags: parsed }),
                    Err(_) => Err(unrecognized(
                        actual,
                        id,
                        name,
                        UnrecognizedReason::InvalidValue,
                        tags,
                    )),
                });
            }
            TagMatch::Managed(_) => UnrecognizedReason::KindMismatch,
            TagMatch::Unrecognized(reason) => reason,
        };

        Some(Err(unrecognized(actual, id, name, reason, tags)))
    }

    async fn classified(
        &self,
        droplet_tag: &str,
    ) -> ProviderResult<Vec<Result<Observed<DoKind>, Unrecognized>>> {
        let droplets = self.client.list_droplets_by_tag(droplet_tag).await?;
        let volumes = self.client.list_volumes().await?;

        let droplets = droplets.into_iter().filter_map(|droplet| {
            self.classify(
                DoKind::Droplet,
                droplet.id.to_string(),
                droplet.name,
                droplet.tags,
            )
        });
        let volumes = volumes.into_iter().filter_map(|volume| {
            self.classify(DoKind::Volume, volume.id, volume.name, volume.tags)
        });

        Ok(droplets.chain(volumes).collect())
    }
}

fn unrecognized(
    kind: DoKind,
    id: String,
    name: String,
    reason: UnrecognizedReason,
    tags: Vec<String>,
) -> Unrecognized {
    Unrecognized {
        provider_kind: kind.tag_name().to_owned(),
        id,
        name,
        reason,
        tags,
    }
}

fn droplet_body(spec: &DropletSpec, tags: Vec<String>) -> Value {
    let image = match &spec.image {
        DropletImage::Slug(slug) => Value::String(slug.as_str().to_owned()),
        DropletImage::Id(id) => Value::Number(id.get().into()),
    };
    let mut body = json!({
        "name": spec.name.as_str(),
        "region": spec.region.as_str(),
        "size": spec.size.as_str(),
        "image": image,
        "user_data": spec.boot.as_str(),
        "ssh_keys": spec.ssh_key_ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
        "volumes": spec.volume_ids.iter().map(ResourceId::as_str).collect::<Vec<_>>(),
        "tags": tags,
        "monitoring": true,
        "ipv6": true,
    });
    if let Some(vpc_uuid) = &spec.vpc_uuid {
        body["vpc_uuid"] = Value::String(vpc_uuid.as_str().to_owned());
    }

    body
}

fn volume_body(spec: &VolumeSpec, tags: Vec<String>) -> Value {
    json!({
        "name": spec.name.as_str(),
        "region": spec.region.as_str(),
        "size_gigabytes": spec.size_gib.get(),
        "tags": tags,
    })
}

fn resource_id(value: impl Into<String>) -> ProviderResult<ResourceId> {
    ResourceId::parse(value)
        .map_err(|_| ProviderError::invalid_response("the provider returned an invalid ID"))
}

impl<T: HttpTransport, R: Runtime> CloudProvider for DigitalOcean<T, R> {
    type Kind = DoKind;
    type Spec = DoSpec;

    async fn create(&self, owner: &OwnerTags, spec: &DoSpec) -> ProviderResult<ResourceId> {
        let tags = self.scheme.render(owner, spec.kind());

        match spec {
            DoSpec::Droplet(spec) => {
                let droplet = self.client.create_droplet(droplet_body(spec, tags)).await?;
                resource_id(droplet.id.to_string())
            }
            DoSpec::Volume(spec) => {
                let volume = self.client.create_volume(volume_body(spec, tags)).await?;
                resource_id(volume.id)
            }
        }
    }

    async fn delete(&self, kind: DoKind, id: &ResourceId) -> ProviderResult<()> {
        match kind {
            DoKind::Droplet => self.client.delete_droplet(id.as_str()).await,
            DoKind::Volume => self.client.delete_volume(id.as_str()).await,
        }
    }

    async fn presence(&self, kind: DoKind, id: &ResourceId) -> ProviderResult<Presence> {
        let present = match kind {
            DoKind::Droplet => self.client.get_droplet(id.as_str()).await?.is_some(),
            DoKind::Volume => self.client.get_volume(id.as_str()).await?.is_some(),
        };

        Ok(if present {
            Presence::Present
        } else {
            Presence::Absent
        })
    }

    async fn find(
        &self,
        owner: &OwnerId,
        generation: Generation,
        kind: DoKind,
    ) -> ProviderResult<Vec<ResourceId>> {
        let classified = self.classified(&self.scheme.owner_tag(owner)).await?;

        Ok(classified
            .into_iter()
            .filter_map(Result::ok)
            .filter(|observed| {
                observed.tags.owner == *owner
                    && observed.tags.generation == generation
                    && observed.tags.kind == kind
            })
            .map(|observed| observed.id)
            .collect())
    }

    async fn inventory(&self) -> ProviderResult<Inventory<DoKind>> {
        let classified = self.classified(&self.scheme.managed_tag()).await?;
        let mut inventory = Inventory {
            managed: Vec::new(),
            unrecognized: Vec::new(),
        };
        for entry in classified {
            match entry {
                Ok(observed) => inventory.managed.push(observed),
                Err(unrecognized) => inventory.unrecognized.push(unrecognized),
            }
        }

        Ok(inventory)
    }
}
