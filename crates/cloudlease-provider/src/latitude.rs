//! Bare metal servers on Latitude.sh
//!
//! Latitude.sh tags do not fit the core tag set: a tag is a team-wide named resource of at
//! most 40 characters with no value, and a server only gets tags from a `PATCH` after it
//! exists. Tagging that way would need a new team tag per owner and ceiling, and a server
//! whose create returned but whose tag update did not would be invisible to the sweep.
//!
//! The adapter does not use Latitude.sh tags. Instead:
//!
//! - Every server boots from a user data resource, which the lease creates first. Its
//!   description holds the exact core tag set, space-separated, with kind `userdata`.
//! - The server's hostname is `{p}-` followed by 16 hex digits of a SHA-256 over the
//!   prefix, owner and generation, set atomically at create. [`CloudProvider::find`]
//!   locates a server by that hostname, so a server whose create response was lost is still
//!   found.
//! - The sweep reads a server's owner, generation and ceiling from the user data record
//!   whose hostname matches. A hostname in the app's `{p}-` namespace without exactly one
//!   consistent record is reported as unrecognized and never deleted automatically.
//!
//! Servers have delete rank 0 and user data rank 1, so cleanup confirms a server gone
//! before it deletes the record that names its owner.
//!
//! Every list is filtered to the adapter's project. Latitude.sh lists the whole team
//! otherwise, and another project's owner store does not know this app's owners, so a sweep
//! there must not see these servers or this one see theirs.

pub mod client;

use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId},
    sweep::{Inventory, Observed, Unrecognized},
    tags::{ManagedTags, OwnerTags, ResourceKind, TagMatch, TagScheme, UnrecognizedReason},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    error::{ProviderError, ProviderResult},
    http::{HttpTransport, Runtime},
    provider::{CloudProvider, CreateSpec, Presence},
    validated::validated,
};

pub use client::{ClientOptions, LatitudeApiKey, LatitudeClient, WireServer, WireUserData};

/// The largest user data the adapter sends, before base64 encoding
///
/// Latitude.sh documents no limit; this matches DigitalOcean's so one boot script fits all
/// providers.
pub const MAX_USER_DATA_BYTES: usize = 64 * 1024;

/// A Latitude.sh resource kind cloudlease manages
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LatKind {
    /// A bare metal server
    Server,
    /// The user data resource a server boots from, which also records its owner
    UserData,
}

impl ResourceKind for LatKind {
    const ALL: &'static [Self] = &[Self::Server, Self::UserData];

    fn tag_name(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::UserData => "userdata",
        }
    }

    fn delete_rank(self) -> u8 {
        match self {
            Self::Server => 0,
            Self::UserData => 1,
        }
    }
}

validated!(
    /// A project, plan or site identifier, such as `proj_ZWr75ZRzo5A91`, `m4-metal-medium`
    /// or `DAL`
    Slug,
    "a slug must be 1 to 64 letters, digits, dashes or underscores",
    |value| {
        !value.is_empty()
            && value.len() <= 64
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '-' || character == '_'
            })
    }
);

validated!(
    /// The cloud-init user data a server runs at first boot, such as a `#cloud-config`
    /// document
    BootScript,
    "user data must be non-empty and at most 64 KiB",
    |value| !value.is_empty() && value.len() <= MAX_USER_DATA_BYTES
);

/// A server to create, billed hourly
///
/// The adapter chooses the hostname from the owner and generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerSpec {
    /// The plan slug, such as `m4-metal-medium`
    pub plan: Slug,
    /// The site, such as `DAL`
    pub site: Slug,
    /// The operating system slug, such as `ubuntu_24_04_x64_lts`
    pub operating_system: Slug,
    /// The user data resource created for this server's lease generation
    pub user_data: ResourceId,
    /// IDs of SSH keys in the project
    pub ssh_key_ids: Vec<ResourceId>,
}

/// A user data resource to create
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserDataSpec {
    /// The first-boot user data
    pub boot: BootScript,
}

/// A Latitude.sh create request
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LatSpec {
    /// Create a server
    Server(ServerSpec),
    /// Create a user data resource
    UserData(UserDataSpec),
}

impl CreateSpec for LatSpec {
    type Kind = LatKind;

    fn kind(&self) -> LatKind {
        match self {
            Self::Server(_) => LatKind::Server,
            Self::UserData(_) => LatKind::UserData,
        }
    }
}

/// The Latitude.sh adapter for one app's tag scheme and project
pub struct Latitude<T, R> {
    client: LatitudeClient<T, R>,
    scheme: TagScheme,
    project: Slug,
}

impl<T: HttpTransport, R: Runtime> Latitude<T, R> {
    /// Creates an adapter that creates, finds and sweeps resources in `project` only, with
    /// `scheme`'s tags
    pub const fn new(client: LatitudeClient<T, R>, scheme: TagScheme, project: Slug) -> Self {
        Self {
            client,
            scheme,
            project,
        }
    }

    /// Returns the REST client
    pub const fn client(&self) -> &LatitudeClient<T, R> {
        &self.client
    }

    /// Returns the tag scheme
    pub const fn scheme(&self) -> &TagScheme {
        &self.scheme
    }

    /// Returns the hostname of the server for one owner and generation
    #[must_use]
    pub fn hostname(&self, owner: &OwnerId, generation: Generation) -> String {
        let prefix = self.scheme.prefix();
        let digest = Sha256::new()
            .chain_update(prefix.as_str())
            .chain_update([0])
            .chain_update(owner.as_str())
            .chain_update([0])
            .chain_update(generation.to_string())
            .finalize();
        let hex: String = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        format!("{prefix}-{hex}")
    }

    /// Returns the user data description that records a tag set
    #[must_use]
    pub fn record_description(&self, owner: &OwnerTags) -> String {
        self.scheme.render(owner, LatKind::UserData).join(" ")
    }

    fn classify_record(
        &self,
        record: WireUserData,
    ) -> Option<Result<Observed<LatKind>, Unrecognized>> {
        let description = record.attributes.description.unwrap_or_default();
        let tags: Vec<String> = description.split_whitespace().map(str::to_owned).collect();
        let reason = match self.scheme.classify::<LatKind>(&tags) {
            TagMatch::Foreign => return None,
            TagMatch::Managed(parsed) if parsed.kind == LatKind::UserData => {
                return Some(match ResourceId::parse(&record.id) {
                    Ok(id) => Ok(Observed { id, tags: parsed }),
                    Err(_) => Err(unrecognized(
                        LatKind::UserData,
                        record.id,
                        description,
                        UnrecognizedReason::InvalidValue,
                        tags,
                    )),
                });
            }
            TagMatch::Managed(_) => UnrecognizedReason::KindMismatch,
            TagMatch::Unrecognized(reason) => reason,
        };

        Some(Err(unrecognized(
            LatKind::UserData,
            record.id,
            description,
            reason,
            tags,
        )))
    }

    fn classify_server(
        &self,
        server: WireServer,
        records: &BTreeMap<String, Vec<ManagedTags<LatKind>>>,
    ) -> Option<Result<Observed<LatKind>, Unrecognized>> {
        let namespace = format!("{}-", self.scheme.prefix());
        let hostname = server.attributes.hostname;
        if !hostname.starts_with(&namespace) {
            return None;
        }

        let reason = match records.get(&hostname).map(Vec::as_slice) {
            None | Some([]) => UnrecognizedReason::WrongShape,
            // a lost user data create can leave two records for one generation; they name
            // the same server only if they agree
            Some([first, rest @ ..]) if rest.iter().all(|other| other == first) => {
                let tags = ManagedTags {
                    kind: LatKind::Server,
                    ..first.clone()
                };
                return Some(match ResourceId::parse(&server.id) {
                    Ok(id) => Ok(Observed { id, tags }),
                    Err(_) => Err(unrecognized(
                        LatKind::Server,
                        server.id,
                        hostname,
                        UnrecognizedReason::InvalidValue,
                        Vec::new(),
                    )),
                });
            }
            Some(_) => UnrecognizedReason::DuplicateTag,
        };

        Some(Err(unrecognized(
            LatKind::Server,
            server.id,
            hostname,
            reason,
            Vec::new(),
        )))
    }

    async fn classified(&self) -> ProviderResult<Vec<Result<Observed<LatKind>, Unrecognized>>> {
        let records = self.client.list_user_data(self.project.as_str()).await?;
        let servers = self
            .client
            .list_servers(self.project.as_str(), None)
            .await?;

        let records: Vec<_> = records
            .into_iter()
            .filter_map(|record| self.classify_record(record))
            .collect();
        let mut by_hostname: BTreeMap<String, Vec<ManagedTags<LatKind>>> = BTreeMap::new();
        for observed in records.iter().filter_map(|entry| entry.as_ref().ok()) {
            by_hostname
                .entry(self.hostname(&observed.tags.owner, observed.tags.generation))
                .or_default()
                .push(observed.tags.clone());
        }
        let servers: Vec<_> = servers
            .into_iter()
            .filter_map(|server| self.classify_server(server, &by_hostname))
            .collect();

        Ok(servers.into_iter().chain(records).collect())
    }

    async fn find_servers(
        &self,
        owner: &OwnerId,
        generation: Generation,
    ) -> ProviderResult<Vec<ResourceId>> {
        let hostname = self.hostname(owner, generation);
        let servers = self
            .client
            .list_servers(self.project.as_str(), Some(&hostname))
            .await?;

        servers
            .into_iter()
            .filter(|server| server.attributes.hostname == hostname)
            .map(|server| resource_id(server.id))
            .collect()
    }

    async fn find_records(
        &self,
        owner: &OwnerId,
        generation: Generation,
    ) -> ProviderResult<Vec<ResourceId>> {
        let records = self.client.list_user_data(self.project.as_str()).await?;

        Ok(records
            .into_iter()
            .filter_map(|record| self.classify_record(record))
            .filter_map(Result::ok)
            .filter(|observed| {
                observed.tags.owner == *owner && observed.tags.generation == generation
            })
            .map(|observed| observed.id)
            .collect())
    }
}

fn unrecognized(
    kind: LatKind,
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

fn resource_id(value: String) -> ProviderResult<ResourceId> {
    ResourceId::parse(value)
        .map_err(|_| ProviderError::invalid_response("Latitude.sh returned an invalid ID"))
}

fn presence(present: bool) -> Presence {
    if present {
        Presence::Present
    } else {
        Presence::Absent
    }
}

impl<T: HttpTransport, R: Runtime> CloudProvider for Latitude<T, R> {
    type Kind = LatKind;
    type Spec = LatSpec;

    async fn create(&self, owner: &OwnerTags, spec: &LatSpec) -> ProviderResult<ResourceId> {
        match spec {
            LatSpec::UserData(spec) => {
                let body = json!({ "data": { "type": "user_data", "attributes": {
                    "description": self.record_description(owner),
                    "project": self.project.as_str(),
                    "content": STANDARD.encode(spec.boot.as_str()),
                } } });
                resource_id(self.client.create_user_data(body).await?.id)
            }
            LatSpec::Server(spec) => {
                let body: Value = json!({ "data": { "type": "servers", "attributes": {
                    "project": self.project.as_str(),
                    "plan": spec.plan.as_str(),
                    "site": spec.site.as_str(),
                    "operating_system": spec.operating_system.as_str(),
                    "hostname": self.hostname(&owner.owner, owner.generation),
                    "ssh_keys": spec.ssh_key_ids.iter().map(ResourceId::as_str).collect::<Vec<_>>(),
                    "user_data": spec.user_data.as_str(),
                    // the API's default when this is left out is unclear, and monthly is
                    // paid up front with no refund
                    "billing": "hourly",
                } } });
                resource_id(self.client.create_server(body).await?.id)
            }
        }
    }

    async fn delete(&self, kind: LatKind, id: &ResourceId) -> ProviderResult<()> {
        match kind {
            LatKind::Server => self.client.delete_server(id.as_str()).await,
            LatKind::UserData => self.client.delete_user_data(id.as_str()).await,
        }
    }

    async fn presence(&self, kind: LatKind, id: &ResourceId) -> ProviderResult<Presence> {
        let present = match kind {
            LatKind::Server => self.client.get_server(id.as_str()).await?.is_some(),
            LatKind::UserData => self.client.get_user_data(id.as_str()).await?.is_some(),
        };

        Ok(presence(present))
    }

    async fn find(
        &self,
        owner: &OwnerId,
        generation: Generation,
        kind: LatKind,
    ) -> ProviderResult<Vec<ResourceId>> {
        match kind {
            LatKind::Server => self.find_servers(owner, generation).await,
            LatKind::UserData => self.find_records(owner, generation).await,
        }
    }

    async fn inventory(&self) -> ProviderResult<Inventory<LatKind>> {
        let mut inventory = Inventory {
            managed: Vec::new(),
            unrecognized: Vec::new(),
        };
        for entry in self.classified().await? {
            match entry {
                Ok(observed) => inventory.managed.push(observed),
                Err(unrecognized) => inventory.unrecognized.push(unrecognized),
            }
        }

        Ok(inventory)
    }
}
