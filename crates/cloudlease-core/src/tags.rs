//! The exact provider tag set that marks a resource as managed by one app
//!
//! A managed resource carries exactly these tags, where `{p}` is the app's [`TagPrefix`]:
//!
//! - `{p}-managed`
//! - `{p}-owner-{owner}`
//! - `{p}-gen-{generation}`
//! - `{p}-kind-{kind}`
//! - optionally `{p}-until-{unix seconds}`, the resource's ceiling
//!
//! A resource carrying `{p}-managed` with any other shape is [`TagMatch::Unrecognized`]: it
//! is reported and never deleted automatically, because something other than this app may
//! have made it.

use std::{collections::HashSet, fmt::Debug, hash::Hash};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    ids::{Generation, OwnerId, TagPrefix},
    time::UnixMillis,
};

/// A provider resource kind that an app manages, such as a droplet or a volume
pub trait ResourceKind:
    Copy + Debug + Eq + Hash + Ord + Serialize + DeserializeOwned + 'static
{
    /// Every kind, used to parse tags
    const ALL: &'static [Self];

    /// The kind's tag value, lowercase letters and digits only
    fn tag_name(self) -> &'static str;

    /// Cleanup order: every resource of a lower rank in one generation is deleted and
    /// confirmed gone before deletion of a higher rank starts
    ///
    /// Use this for dependencies such as a volume that cannot be deleted while attached to a
    /// droplet.
    fn delete_rank(self) -> u8;

    /// Parses a kind from its tag value
    #[must_use]
    fn from_tag_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.tag_name() == name)
    }
}

/// The latest time a resource may exist, in whole seconds so it fits a tag
///
/// The sweep deletes a resource past its ceiling without asking its owner, so a broken owner
/// object cannot keep a resource forever.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Ceiling(u64);

impl Ceiling {
    /// Rounds a time down to whole seconds, so the ceiling is never later than requested
    #[must_use]
    pub const fn at_or_before(at: UnixMillis) -> Self {
        Self(at.as_millis() / 1_000)
    }

    /// Returns the ceiling as Unix seconds
    #[must_use]
    pub const fn as_unix_seconds(self) -> u64 {
        self.0
    }

    /// Reports whether the ceiling has been reached
    #[must_use]
    pub const fn is_reached_at(self, now: UnixMillis) -> bool {
        now.as_millis() / 1_000 >= self.0
    }
}

/// The ownership part of a tag set, shared by every resource of one generation
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OwnerTags {
    /// The owning Durable Object's name
    pub owner: OwnerId,
    /// The lease generation the resources belong to
    pub generation: Generation,
    /// The latest time the resources may exist, if the app sets one
    pub ceiling: Option<Ceiling>,
}

impl OwnerTags {
    /// Adds a resource kind to form a complete tag set
    #[must_use]
    pub fn with_kind<K: ResourceKind>(&self, kind: K) -> ManagedTags<K> {
        ManagedTags {
            owner: self.owner.clone(),
            generation: self.generation,
            kind,
            ceiling: self.ceiling,
        }
    }
}

/// A complete, parsed managed tag set
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedTags<K> {
    /// The owning Durable Object's name
    pub owner: OwnerId,
    /// The lease generation the resource belongs to
    pub generation: Generation,
    /// The resource kind
    pub kind: K,
    /// The latest time the resource may exist, if the app set one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<Ceiling>,
}

/// Why a resource with the managed tag is not safe to manage automatically
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UnrecognizedReason {
    /// A tag appears twice
    DuplicateTag,
    /// A tag outside the managed set is present
    ExtraTag,
    /// A required tag is missing or appears with two values
    WrongShape,
    /// A tag value does not parse
    InvalidValue,
}

/// How a provider tag list relates to an app's tag scheme
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TagMatch<K> {
    /// The resource does not carry the app's managed tag
    Foreign,
    /// The resource carries exactly one managed tag set
    Managed(ManagedTags<K>),
    /// The resource carries the managed tag but not an exact managed set
    Unrecognized(UnrecognizedReason),
}

/// One app's tag prefix and the rules for rendering and matching its tags
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TagScheme {
    prefix: TagPrefix,
}

impl TagScheme {
    /// Creates a scheme for one app prefix
    #[must_use]
    pub const fn new(prefix: TagPrefix) -> Self {
        Self { prefix }
    }

    /// Returns the app prefix
    #[must_use]
    pub const fn prefix(&self) -> &TagPrefix {
        &self.prefix
    }

    /// Returns the tag every managed resource carries, used to list inventory
    #[must_use]
    pub fn managed_tag(&self) -> String {
        format!("{}-managed", self.prefix)
    }

    /// Returns the tag that every resource of one owner carries
    #[must_use]
    pub fn owner_tag(&self, owner: &OwnerId) -> String {
        format!("{}-owner-{owner}", self.prefix)
    }

    /// Renders the exact tag set for one resource
    #[must_use]
    pub fn render<K: ResourceKind>(&self, tags: &ManagedTags<K>) -> Vec<String> {
        let prefix = &self.prefix;
        let mut rendered = vec![
            self.managed_tag(),
            self.owner_tag(&tags.owner),
            format!("{prefix}-gen-{}", tags.generation),
            format!("{prefix}-kind-{}", tags.kind.tag_name()),
        ];
        if let Some(ceiling) = tags.ceiling {
            rendered.push(format!("{prefix}-until-{}", ceiling.as_unix_seconds()));
        }

        rendered
    }

    /// Classifies a provider tag list as foreign, exactly managed or unrecognized
    #[must_use]
    pub fn classify<K: ResourceKind>(&self, tags: &[String]) -> TagMatch<K> {
        let managed = self.managed_tag();
        if !tags.iter().any(|tag| tag == &managed) {
            return TagMatch::Foreign;
        }

        match self.parse_managed(tags) {
            Ok(parsed) => TagMatch::Managed(parsed),
            Err(reason) => TagMatch::Unrecognized(reason),
        }
    }

    fn parse_managed<K: ResourceKind>(
        &self,
        tags: &[String],
    ) -> Result<ManagedTags<K>, UnrecognizedReason> {
        if tags.iter().collect::<HashSet<_>>().len() != tags.len() {
            return Err(UnrecognizedReason::DuplicateTag);
        }

        let managed = self.managed_tag();
        let mut owner = None;
        let mut generation = None;
        let mut kind = None;
        let mut ceiling = None;

        for tag in tags.iter().filter(|tag| **tag != managed) {
            let Some(rest) = tag
                .strip_prefix(self.prefix.as_str())
                .and_then(|rest| rest.strip_prefix('-'))
            else {
                return Err(UnrecognizedReason::ExtraTag);
            };
            let (slot, value) = if let Some(value) = rest.strip_prefix("owner-") {
                (&mut owner, value)
            } else if let Some(value) = rest.strip_prefix("gen-") {
                (&mut generation, value)
            } else if let Some(value) = rest.strip_prefix("kind-") {
                (&mut kind, value)
            } else if let Some(value) = rest.strip_prefix("until-") {
                (&mut ceiling, value)
            } else {
                return Err(UnrecognizedReason::ExtraTag);
            };
            if slot.replace(value).is_some() {
                return Err(UnrecognizedReason::WrongShape);
            }
        }

        let (Some(owner), Some(generation), Some(kind)) = (owner, generation, kind) else {
            return Err(UnrecognizedReason::WrongShape);
        };

        let parsed = ManagedTags {
            owner: OwnerId::parse(owner).map_err(|_| UnrecognizedReason::InvalidValue)?,
            generation: parse_canonical_u64(generation)
                .and_then(|value| Generation::new(value).ok())
                .ok_or(UnrecognizedReason::InvalidValue)?,
            kind: K::from_tag_name(kind).ok_or(UnrecognizedReason::InvalidValue)?,
            ceiling: match ceiling {
                Some(value) => Some(Ceiling(
                    parse_canonical_u64(value).ok_or(UnrecognizedReason::InvalidValue)?,
                )),
                None => None,
            },
        };

        // a value such as `gen-07` parses but renders differently, so only a set that
        // round-trips is exact
        let mut rendered = self.render(&parsed);
        let mut actual = tags.to_vec();
        rendered.sort();
        actual.sort();
        if rendered != actual {
            return Err(UnrecognizedReason::InvalidValue);
        }

        Ok(parsed)
    }
}

fn parse_canonical_u64(value: &str) -> Option<u64> {
    let parsed = value.parse::<u64>().ok()?;

    (parsed.to_string() == value).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::{
        Ceiling, ManagedTags, OwnerTags, ResourceKind, TagMatch, TagScheme, UnrecognizedReason,
    };
    use crate::{
        ids::{Generation, OwnerId, TagPrefix},
        time::UnixMillis,
    };

    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
    enum Kind {
        Machine,
        Disk,
    }

    impl ResourceKind for Kind {
        const ALL: &'static [Self] = &[Self::Machine, Self::Disk];

        fn tag_name(self) -> &'static str {
            match self {
                Self::Machine => "machine",
                Self::Disk => "disk",
            }
        }

        fn delete_rank(self) -> u8 {
            match self {
                Self::Machine => 0,
                Self::Disk => 1,
            }
        }
    }

    fn scheme() -> TagScheme {
        TagScheme::new(TagPrefix::parse("cl").unwrap())
    }

    fn tags(ceiling: Option<Ceiling>) -> ManagedTags<Kind> {
        OwnerTags {
            owner: OwnerId::parse("job_1").unwrap(),
            generation: Generation::new(7).unwrap(),
            ceiling,
        }
        .with_kind(Kind::Disk)
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn rendered_tags_round_trip_with_and_without_a_ceiling() {
        let ceiling = Ceiling::at_or_before(UnixMillis::new(1_764_000_000_999).unwrap());
        for expected in [tags(None), tags(Some(ceiling))] {
            let rendered = scheme().render(&expected);

            assert_eq!(scheme().classify(&rendered), TagMatch::Managed(expected));
        }

        assert_eq!(
            scheme().render(&tags(Some(ceiling))),
            strings(&[
                "cl-managed",
                "cl-owner-job_1",
                "cl-gen-7",
                "cl-kind-disk",
                "cl-until-1764000000",
            ])
        );
    }

    #[test]
    fn only_exact_tag_sets_are_managed() {
        let foreign = strings(&["cl-owner-job_1", "cl-gen-7", "cl-kind-disk", "team-x"]);
        assert_eq!(scheme().classify::<Kind>(&foreign), TagMatch::Foreign);

        let cases = [
            (
                strings(&[
                    "cl-managed",
                    "cl-owner-job_1",
                    "cl-gen-7",
                    "cl-kind-disk",
                    "team",
                ]),
                UnrecognizedReason::ExtraTag,
            ),
            (
                strings(&[
                    "cl-managed",
                    "cl-managed",
                    "cl-owner-a",
                    "cl-gen-7",
                    "cl-kind-disk",
                ]),
                UnrecognizedReason::DuplicateTag,
            ),
            (
                strings(&["cl-managed", "cl-owner-a", "cl-kind-disk"]),
                UnrecognizedReason::WrongShape,
            ),
            (
                strings(&[
                    "cl-managed",
                    "cl-owner-a",
                    "cl-owner-b",
                    "cl-gen-7",
                    "cl-kind-disk",
                ]),
                UnrecognizedReason::WrongShape,
            ),
            (
                strings(&["cl-managed", "cl-owner-a", "cl-gen-07", "cl-kind-disk"]),
                UnrecognizedReason::InvalidValue,
            ),
            (
                strings(&["cl-managed", "cl-owner-a", "cl-gen-0", "cl-kind-disk"]),
                UnrecognizedReason::InvalidValue,
            ),
            (
                strings(&["cl-managed", "cl-owner-a", "cl-gen-7", "cl-kind-firewall"]),
                UnrecognizedReason::InvalidValue,
            ),
            (
                strings(&["cl-managed", "clx-owner-a", "cl-gen-7", "cl-kind-disk"]),
                UnrecognizedReason::ExtraTag,
            ),
        ];

        for (tags, reason) in cases {
            assert_eq!(
                scheme().classify::<Kind>(&tags),
                TagMatch::Unrecognized(reason),
                "{tags:?}"
            );
        }
    }

    #[test]
    fn another_prefix_never_matches() {
        let other = TagScheme::new(TagPrefix::parse("rb").unwrap());
        let rendered = other.render(&tags(None));

        assert_eq!(scheme().classify::<Kind>(&rendered), TagMatch::Foreign);
    }

    #[test]
    fn a_ceiling_rounds_down_and_is_reached_at_its_second() {
        let ceiling = Ceiling::at_or_before(UnixMillis::new(10_999).unwrap());

        assert_eq!(ceiling.as_unix_seconds(), 10);
        assert!(!ceiling.is_reached_at(UnixMillis::new(9_999).unwrap()));
        assert!(ceiling.is_reached_at(UnixMillis::new(10_000).unwrap()));
    }
}
