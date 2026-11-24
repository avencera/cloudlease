//! Bare Metal Cloud servers on phoenixNAP
//!
//! phoenixNAP has no attachable block volumes in this API, so a server is the only kind; its
//! scratch space is the type's local NVMe.
//!
//! phoenixNAP tags are `{name, value}` pairs, and names may not contain dots. The adapter
//! stores the app's tag set as five fixed names with per-server values, so the account's
//! Tag Manager holds five tags rather than one per owner and generation:
//!
//! | core tag | phoenixNAP tag |
//! |---|---|
//! | `{p}-managed` | `{p}-managed`, no value |
//! | `{p}-owner-{owner}` | `{p}-owner` = `{owner}` |
//! | `{p}-gen-{generation}` | `{p}-gen` = `{generation}` |
//! | `{p}-kind-{kind}` | `{p}-kind` = `{kind}` |
//! | `{p}-until-{seconds}` | `{p}-until` = `{seconds}` |
//!
//! Reading tags reverses the table and hands the result to the core
//! [`TagScheme`](cloudlease_core::tags::TagScheme), so exact matching is the core's. Any
//! assignment outside the table, such as a valueless `{p}-owner-x` or `{p}-managed` with a
//! value, becomes `.{name}.{value}`. The core reads any tag without the `{p}-` prefix as an
//! extra tag, so a near miss is never read as an exact set, and a valueless `{p}-until-x`
//! next to a real ceiling is not taken for a second, malformed ceiling.
//!
//! The server list is not paginated, so [`CloudProvider::find`] and
//! [`CloudProvider::inventory`] list every server and match tags locally instead of relying
//! on phoenixNAP's tag filter.

pub mod auth;
pub mod client;

use std::cell::Cell;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId},
    sweep::{Inventory, Observed, Unrecognized},
    tags::{OwnerTags, ResourceKind, TagMatch, TagScheme, UnrecognizedReason},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    error::{ProviderError, ProviderErrorCode, ProviderResult},
    http::{HttpTransport, Runtime},
    provider::{CloudProvider, CreateSpec, Presence},
    validated::validated,
};

pub use auth::PnapCredentials;
pub use client::{ClientOptions, PhoenixNapClient, WireServer, WireTag};

/// The largest user data the adapter sends, before base64 encoding
///
/// phoenixNAP documents no limit; this matches DigitalOcean's so one boot script fits both.
pub const MAX_USER_DATA_BYTES: usize = 64 * 1024;

/// A phoenixNAP resource kind cloudlease manages
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PnapKind {
    /// A Bare Metal Cloud server
    Server,
}

impl ResourceKind for PnapKind {
    const ALL: &'static [Self] = &[Self::Server];

    fn tag_name(self) -> &'static str {
        match self {
            Self::Server => "server",
        }
    }

    fn delete_rank(self) -> u8 {
        match self {
            Self::Server => 0,
        }
    }
}

validated!(
    /// A server hostname: 1 to 100 letters, digits, dots or dashes with at least one letter
    Hostname,
    "a hostname must be 1 to 100 letters, digits, dots or dashes with at least one letter",
    |value| {
        value.len() <= 100
            && value
                .chars()
                .any(|character| character.is_ascii_alphabetic())
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '.' || character == '-'
            })
    }
);

validated!(
    /// A location code, such as `PHX`, `ASH` or `NLD2`
    Location,
    "a location must be 2 to 8 uppercase letters or digits",
    |value| {
        (2..=8).contains(&value.len())
            && value
                .chars()
                .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
    }
);

validated!(
    /// A server type, such as `s3.c3.medium`
    ServerType,
    "a server type must be 1 to 64 lowercase letters, digits or dots",
    |value| {
        !value.is_empty()
            && value.len() <= 64
            && value.chars().all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '.'
            })
    }
);

validated!(
    /// An OS image, such as `ubuntu/jammy`
    OsImage,
    "an OS image must be 1 to 64 lowercase letters, digits or the characters / + . _ -",
    |value| {
        !value.is_empty()
            && value.len() <= 64
            && value.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '/' | '+' | '.' | '_' | '-')
            })
    }
);

validated!(
    /// The cloud-init user data a server runs at first boot, such as a `#cloud-config`
    /// document or a `#!` script
    BootScript,
    "user data must be non-empty and at most 64 KiB",
    |value| !value.is_empty() && value.len() <= MAX_USER_DATA_BYTES
);

/// The account SSH keys a server installs, at least one
///
/// phoenixNAP refuses a Linux server with no SSH key, and the adapter never adds the
/// account's default keys, so a create must name its keys.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshKeyIds(Vec<ResourceId>);

/// An SSH key list with no key
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("phoenixNAP needs at least one account SSH key ID to create a Linux server")]
pub struct NoSshKeys;

impl SshKeyIds {
    /// Checks the list is not empty
    pub fn new(ids: Vec<ResourceId>) -> Result<Self, NoSshKeys> {
        if ids.is_empty() {
            return Err(NoSshKeys);
        }

        Ok(Self(ids))
    }

    /// Returns the key IDs
    #[must_use]
    pub fn as_slice(&self) -> &[ResourceId] {
        &self.0
    }
}

/// A server to create, billed hourly
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerSpec {
    /// The hostname
    pub hostname: Hostname,
    /// The location code
    pub location: Location,
    /// The server type
    pub server_type: ServerType,
    /// The OS image, which must support cloud-init
    pub os: OsImage,
    /// The first-boot user data
    pub boot: BootScript,
    /// IDs of SSH keys stored in the account; the account's default keys are not added
    pub ssh_key_ids: SshKeyIds,
}

/// A phoenixNAP create request
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PnapSpec {
    /// Create a server
    Server(ServerSpec),
}

impl CreateSpec for PnapSpec {
    type Kind = PnapKind;

    fn kind(&self) -> PnapKind {
        match self {
            Self::Server(_) => PnapKind::Server,
        }
    }
}

/// A tag assignment to send to phoenixNAP
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PnapTag {
    /// The tag name
    pub name: String,
    /// The value, absent for the managed marker
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// The fixed tag names one tag scheme uses on phoenixNAP
struct TagNames {
    managed: String,
    owner: String,
    generation: String,
    kind: String,
    until: String,
}

impl TagNames {
    fn new(scheme: &TagScheme) -> Self {
        let prefix = scheme.prefix();

        Self {
            managed: scheme.managed_tag(),
            owner: format!("{prefix}-owner"),
            generation: format!("{prefix}-gen"),
            kind: format!("{prefix}-kind"),
            until: format!("{prefix}-until"),
        }
    }

    fn all(&self) -> [&str; 5] {
        [
            &self.managed,
            &self.owner,
            &self.generation,
            &self.kind,
            &self.until,
        ]
    }

    fn render<K: ResourceKind>(&self, owner: &OwnerTags, kind: K) -> Vec<PnapTag> {
        let pair = |name: &str, value: String| PnapTag {
            name: name.to_owned(),
            value: Some(value),
        };
        let mut tags = vec![
            PnapTag {
                name: self.managed.clone(),
                value: None,
            },
            pair(&self.owner, owner.owner.to_string()),
            pair(&self.generation, owner.generation.to_string()),
            pair(&self.kind, kind.tag_name().to_owned()),
        ];
        if let Some(ceiling) = owner.ceiling {
            tags.push(pair(&self.until, ceiling.as_unix_seconds().to_string()));
        }

        tags
    }

    /// Maps phoenixNAP assignments back to core tag strings, escaping anything off the table
    fn flatten(&self, tags: &[WireTag]) -> Vec<String> {
        let keyed = [&self.owner, &self.generation, &self.kind, &self.until];
        let mut flat = Vec::with_capacity(tags.len());
        for tag in tags {
            let value = tag.value.as_deref().filter(|value| !value.is_empty());
            match value {
                None if tag.name == self.managed => flat.push(self.managed.clone()),
                Some(value) if keyed.contains(&&tag.name) => {
                    flat.push(format!("{}-{value}", tag.name));
                }
                // the marker with a value still puts the server in the app's namespace, so
                // it is reported as unrecognized rather than ignored as foreign
                Some(value) if tag.name == self.managed => {
                    flat.push(self.managed.clone());
                    flat.push(escaped(&tag.name, Some(value)));
                }
                value => flat.push(escaped(&tag.name, value)),
            }
        }

        flat
    }
}

/// Renders an assignment outside the tag table so the core can never parse it as one of its own
///
/// phoenixNAP tag names cannot start with a dot, so the result also never equals a real tag.
fn escaped(name: &str, value: Option<&str>) -> String {
    format!(".{name}.{}", value.unwrap_or_default())
}

/// The phoenixNAP adapter for one app's tag scheme
pub struct PhoenixNap<T, R> {
    client: PhoenixNapClient<T, R>,
    scheme: TagScheme,
    names: TagNames,
    // set once this adapter has made sure its Tag Manager tags exist
    tags_ready: Cell<bool>,
}

impl<T: HttpTransport, R: Runtime> PhoenixNap<T, R> {
    /// Creates an adapter that manages servers with `scheme`'s tags
    pub fn new(client: PhoenixNapClient<T, R>, scheme: TagScheme) -> Self {
        let names = TagNames::new(&scheme);

        Self {
            client,
            scheme,
            names,
            tags_ready: Cell::new(false),
        }
    }

    /// Returns the REST client
    pub const fn client(&self) -> &PhoenixNapClient<T, R> {
        &self.client
    }

    /// Returns the tag scheme
    pub const fn scheme(&self) -> &TagScheme {
        &self.scheme
    }

    /// Returns the tag assignments a create sends for `owner` and `kind`
    #[must_use]
    pub fn render_tags(&self, owner: &OwnerTags, kind: PnapKind) -> Vec<PnapTag> {
        self.names.render(owner, kind)
    }

    /// Classifies a server's phoenixNAP tags as foreign, exactly managed or unrecognized
    #[must_use]
    pub fn classify_tags(&self, tags: &[WireTag]) -> TagMatch<PnapKind> {
        self.scheme.classify(&self.names.flatten(tags))
    }

    async fn ensure_tags(&self) -> ProviderResult<()> {
        if self.tags_ready.get() {
            return Ok(());
        }

        for name in self.names.all() {
            self.client.ensure_tag(name).await?;
        }
        self.tags_ready.set(true);

        Ok(())
    }

    fn classify(&self, server: WireServer) -> Option<Result<Observed<PnapKind>, Unrecognized>> {
        let reason = match self.classify_tags(&server.tags) {
            TagMatch::Foreign => return None,
            TagMatch::Managed(parsed) if parsed.kind == PnapKind::Server => {
                return Some(match ResourceId::parse(&server.id) {
                    Ok(id) => Ok(Observed { id, tags: parsed }),
                    Err(_) => Err(unrecognized(server, UnrecognizedReason::InvalidValue)),
                });
            }
            TagMatch::Managed(_) => UnrecognizedReason::KindMismatch,
            TagMatch::Unrecognized(reason) => reason,
        };

        Some(Err(unrecognized(server, reason)))
    }

    async fn classified(&self) -> ProviderResult<Vec<Result<Observed<PnapKind>, Unrecognized>>> {
        let servers = self.client.list_servers().await?;

        Ok(servers
            .into_iter()
            .filter_map(|server| self.classify(server))
            .collect())
    }

    /// Reads whether `id` is in the server list, which is how absence is confirmed
    async fn listed(&self, id: &ResourceId) -> ProviderResult<Option<WireServer>> {
        let servers = self.client.list_servers().await?;

        Ok(servers.into_iter().find(|server| server.id == id.as_str()))
    }
}

fn unrecognized(server: WireServer, reason: UnrecognizedReason) -> Unrecognized {
    Unrecognized {
        provider_kind: PnapKind::Server.tag_name().to_owned(),
        id: server.id,
        name: server.hostname,
        reason,
        tags: server
            .tags
            .into_iter()
            .map(|tag| match tag.value {
                Some(value) => format!("{}={value}", tag.name),
                None => tag.name,
            })
            .collect(),
    }
}

fn server_body(spec: &ServerSpec, tags: Vec<PnapTag>) -> Value {
    json!({
        "hostname": spec.hostname.as_str(),
        "location": spec.location.as_str(),
        "type": spec.server_type.as_str(),
        "os": spec.os.as_str(),
        "pricingModel": "HOURLY",
        // phoenixNAP adds every default account key unless told not to
        "installDefaultSshKeys": false,
        "sshKeyIds": spec
            .ssh_key_ids
            .as_slice()
            .iter()
            .map(ResourceId::as_str)
            .collect::<Vec<_>>(),
        "osConfiguration": {
            "cloudInit": { "userData": STANDARD.encode(spec.boot.as_str()) },
        },
        "tags": tags,
    })
}

impl<T: HttpTransport, R: Runtime> CloudProvider for PhoenixNap<T, R> {
    type Kind = PnapKind;
    type Spec = PnapSpec;

    async fn create(&self, owner: &OwnerTags, spec: &PnapSpec) -> ProviderResult<ResourceId> {
        self.ensure_tags().await?;
        let tags = self.names.render(owner, spec.kind());

        match spec {
            PnapSpec::Server(spec) => {
                let server = self.client.create_server(server_body(spec, tags)).await?;
                ResourceId::parse(server.id).map_err(|_| {
                    ProviderError::invalid_response("phoenixNAP returned an invalid server ID")
                })
            }
        }
    }

    async fn delete(&self, kind: PnapKind, id: &ResourceId) -> ProviderResult<()> {
        let PnapKind::Server = kind;
        let Err(error) = self.client.deprovision_server(id.as_str()).await else {
            return Ok(());
        };
        // phoenixNAP refuses a deprovision of an unknown server with 403 and of one that is
        // already going with 409, so the list decides whether either is success
        if !matches!(error.status, Some(400 | 403 | 404 | 409)) {
            return Err(error);
        }

        match self.listed(id).await? {
            None => Ok(()),
            Some(server) if server.status == "deleting" => Ok(()),
            Some(_) => Err(error),
        }
    }

    async fn presence(&self, kind: PnapKind, id: &ResourceId) -> ProviderResult<Presence> {
        let PnapKind::Server = kind;
        if self.client.get_server(id.as_str()).await?.is_some() {
            return Ok(Presence::Present);
        }

        // a refused read is only absence when the list agrees
        match self.listed(id).await? {
            None => Ok(Presence::Absent),
            Some(_) => Err(ProviderError::new(
                ProviderErrorCode::PermissionDenied,
                false,
                "phoenixNAP refused to read a server that it lists",
            )),
        }
    }

    async fn find(
        &self,
        owner: &OwnerId,
        generation: Generation,
        kind: PnapKind,
    ) -> ProviderResult<Vec<ResourceId>> {
        let classified = self.classified().await?;

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

    async fn inventory(&self) -> ProviderResult<Inventory<PnapKind>> {
        let mut inventory = Inventory {
            managed: Vec::new(),
            unrecognized: Vec::new(),
            unlisted: Vec::new(),
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

#[cfg(test)]
mod tests {
    use cloudlease_core::{
        ids::{Generation, OwnerId, TagPrefix},
        tags::{Ceiling, OwnerTags, TagScheme},
        time::UnixMillis,
    };

    use super::{PnapKind, TagNames, WireTag};

    fn wire(tags: &[(&str, Option<&str>)]) -> Vec<WireTag> {
        tags.iter()
            .map(|(name, value)| WireTag {
                name: (*name).to_owned(),
                value: value.map(str::to_owned),
            })
            .collect()
    }

    #[test]
    fn rendered_pairs_flatten_to_the_core_tag_set() {
        let scheme = TagScheme::new(TagPrefix::parse("clm").unwrap());
        let names = TagNames::new(&scheme);
        let ceiling = Ceiling::at_or_before(UnixMillis::new(1_795_521_600_000).unwrap());
        for ceiling in [None, Some(ceiling)] {
            let owner = OwnerTags {
                owner: OwnerId::parse("run_1").unwrap(),
                generation: Generation::new(3).unwrap(),
                ceiling,
            };
            let wire: Vec<_> = names
                .render(&owner, PnapKind::Server)
                .into_iter()
                .map(|tag| WireTag {
                    name: tag.name,
                    value: tag.value,
                })
                .collect();

            assert_eq!(
                names.flatten(&wire),
                scheme.render(&owner, PnapKind::Server)
            );
        }
    }

    #[test]
    fn off_table_assignments_flatten_to_strings_no_core_tag_can_equal() {
        let scheme = TagScheme::new(TagPrefix::parse("clm").unwrap());
        let names = TagNames::new(&scheme);
        let flat = names.flatten(&wire(&[
            ("clm-owner-run_1", None),
            ("clm-managed", Some("yes")),
            ("clm-gen", None),
            ("team", Some("ops")),
        ]));

        assert_eq!(
            flat,
            [
                ".clm-owner-run_1.",
                "clm-managed",
                ".clm-managed.yes",
                ".clm-gen.",
                ".team.ops"
            ]
        );
    }
}
