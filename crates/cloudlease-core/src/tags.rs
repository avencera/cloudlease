//! The exact provider tag set that marks a resource as managed by one app
//!
//! A managed resource carries exactly these tags, where `{p}` is the app's [`TagPrefix`]:
//!
//! - `{p}-managed`
//! - `{p}-owner-{owner}`
//! - `{p}-gen-{generation}`
//! - `{p}-kind-{kind}`
//! - optionally `{p}-until-{unix seconds}`, the resource's ceiling
//! - with [`TagScheme::with_selector`], `{p}-sel-{owner}-{generation}`, which a provider
//!   object that selects resources by tag (a DigitalOcean firewall) can name on its own
//!
//! Providers with key and value tags (AWS) use [`TagScheme::render_pairs`] and
//! [`TagScheme::classify_pairs`]: each tag above becomes the key `{p}-{field}` with the rest
//! as its value, and `{p}-managed` takes the value [`MANAGED_VALUE`].
//!
//! A resource carrying `{p}-managed` with any other shape is [`TagMatch::Unrecognized`]: it
//! is reported and never deleted automatically, because something other than this app may
//! have made it. The one exception is a malformed ceiling on an otherwise exact set, which
//! is [`CeilingTag::Malformed`]: the owner is still known, so the sweep asks it as if there
//! were no ceiling, and never treats the resource as having no deadline.

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

/// The ceiling tag as observed on a provider resource
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CeilingTag {
    /// No ceiling tag
    #[default]
    Absent,
    /// One ceiling tag in canonical form
    At(Ceiling),
    /// Ceiling tags that do not parse exactly, such as `until-07`, `until-soon` or two of
    /// them
    ///
    /// The sweep cannot act on it, so it asks the owner instead; the owner's lease still
    /// has its own hard deadline.
    Malformed,
}

impl CeilingTag {
    /// Reports whether the tag gives a ceiling that has been reached
    ///
    /// Only an exact ceiling can be reached; an absent or malformed one never lets the
    /// sweep skip asking the owner.
    #[must_use]
    pub const fn is_reached_at(self, now: UnixMillis) -> bool {
        match self {
            Self::At(ceiling) => ceiling.is_reached_at(now),
            Self::Absent | Self::Malformed => false,
        }
    }
}

impl From<Option<Ceiling>> for CeilingTag {
    fn from(ceiling: Option<Ceiling>) -> Self {
        ceiling.map_or(Self::Absent, Self::At)
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
            ceiling: self.ceiling.into(),
        }
    }
}

/// A managed tag set as observed on a provider resource
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedTags<K> {
    /// The owning Durable Object's name
    pub owner: OwnerId,
    /// The lease generation the resource belongs to
    pub generation: Generation,
    /// The resource kind
    pub kind: K,
    /// The latest time the resource may exist, as tagged
    #[serde(default)]
    pub ceiling: CeilingTag,
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
    /// The kind tag names a different resource type than the provider reports
    KindMismatch,
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

/// The value of the `{p}-managed` key in the key and value encoding
pub const MANAGED_VALUE: &str = "true";

// the fields after `{p}-` that carry a value; `managed` carries none
const VALUE_FIELDS: [&str; 5] = ["owner", "gen", "kind", "until", "sel"];

/// One app's tag prefix and the rules for rendering and matching its tags
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TagScheme {
    prefix: TagPrefix,
    selector: bool,
}

impl TagScheme {
    /// Creates a scheme for one app prefix
    #[must_use]
    pub const fn new(prefix: TagPrefix) -> Self {
        Self {
            prefix,
            selector: false,
        }
    }

    /// Makes every managed resource carry `{p}-sel-{owner}-{generation}`
    ///
    /// A provider object that applies to resources by tag, such as a DigitalOcean firewall,
    /// can then select exactly one generation's resources and carry that one tag as its own
    /// identity. The selector is part of the exact set: once enabled, a resource without it
    /// is unrecognized, and without it, a resource carrying one is.
    #[must_use]
    pub fn with_selector(self) -> Self {
        Self {
            selector: true,
            ..self
        }
    }

    /// Returns whether managed resources carry a selector tag
    #[must_use]
    pub const fn has_selector(&self) -> bool {
        self.selector
    }

    /// Returns the selector tag for one owner and generation
    #[must_use]
    pub fn selector_tag(&self, owner: &OwnerId, generation: Generation) -> String {
        format!("{}-sel-{owner}-{generation}", self.prefix)
    }

    /// Reads the owner and generation back from a selector tag
    ///
    /// Owners may contain dashes but generations never do, so the last dash splits them;
    /// only a tag that renders back exactly is accepted.
    #[must_use]
    pub fn parse_selector(&self, tag: &str) -> Option<(OwnerId, Generation)> {
        let rest = tag
            .strip_prefix(self.prefix.as_str())?
            .strip_prefix("-sel-")?;
        let (owner, generation) = rest.rsplit_once('-')?;
        let owner = OwnerId::parse(owner).ok()?;
        let generation = Generation::new(parse_canonical_u64(generation)?).ok()?;

        (self.selector_tag(&owner, generation) == tag).then_some((owner, generation))
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
    ///
    /// It takes [`OwnerTags`] rather than observed tags, so a malformed ceiling can never
    /// be written.
    #[must_use]
    pub fn render<K: ResourceKind>(&self, owner: &OwnerTags, kind: K) -> Vec<String> {
        let prefix = &self.prefix;
        let mut rendered = vec![
            self.managed_tag(),
            self.owner_tag(&owner.owner),
            format!("{prefix}-gen-{}", owner.generation),
            format!("{prefix}-kind-{}", kind.tag_name()),
        ];
        if let Some(ceiling) = owner.ceiling {
            rendered.push(format!("{prefix}-until-{}", ceiling.as_unix_seconds()));
        }
        if self.selector {
            rendered.push(self.selector_tag(&owner.owner, owner.generation));
        }

        rendered
    }

    /// Renders the exact tag set as keys and values, for providers whose tags are pairs
    #[must_use]
    pub fn render_pairs<K: ResourceKind>(
        &self,
        owner: &OwnerTags,
        kind: K,
    ) -> Vec<(String, String)> {
        let managed = self.managed_tag();

        self.render(owner, kind)
            .into_iter()
            .map(|tag| {
                if tag == managed {
                    return (tag, MANAGED_VALUE.to_owned());
                }

                // every rendered tag other than the managed one is `{p}-{field}-{value}`
                let rest = &tag[self.prefix.as_str().len() + 1..];
                let (field, value) = rest.split_once('-').unwrap_or((rest, ""));
                (format!("{}-{field}", self.prefix), value.to_owned())
            })
            .collect()
    }

    /// Classifies key and value tags as foreign, exactly managed or unrecognized
    ///
    /// The caller leaves out keys the provider reserves for display, such as AWS `Name`.
    #[must_use]
    pub fn classify_pairs<K: ResourceKind>(&self, pairs: &[(String, String)]) -> TagMatch<K> {
        let managed = self.managed_tag();
        let Some((_, marker)) = pairs.iter().find(|(key, _)| key == &managed) else {
            return TagMatch::Foreign;
        };
        if marker != MANAGED_VALUE {
            return TagMatch::Unrecognized(UnrecognizedReason::InvalidValue);
        }

        let mut tags = Vec::with_capacity(pairs.len());
        for (key, value) in pairs {
            if key == &managed {
                tags.push(managed.clone());
                continue;
            }

            let field = key
                .strip_prefix(self.prefix.as_str())
                .and_then(|rest| rest.strip_prefix('-'));
            match field {
                // with exact field keys, `{key}-{value}` maps back to exactly one pair
                Some(field) if VALUE_FIELDS.contains(&field) => tags.push(format!("{key}-{value}")),
                _ => return TagMatch::Unrecognized(UnrecognizedReason::ExtraTag),
            }
        }

        self.classify(&tags)
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
        let mut ceilings = Vec::new();

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
                ceilings.push(value);
                continue;
            } else if self.selector && rest.starts_with("sel-") {
                // the round trip below checks it names this owner and generation
                continue;
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

        let ceiling = match ceilings.as_slice() {
            [] => CeilingTag::Absent,
            [value] => parse_canonical_u64(value).map_or(CeilingTag::Malformed, |seconds| {
                CeilingTag::At(Ceiling(seconds))
            }),
            _ => CeilingTag::Malformed,
        };
        let owner = OwnerTags {
            owner: OwnerId::parse(owner).map_err(|_| UnrecognizedReason::InvalidValue)?,
            generation: parse_canonical_u64(generation)
                .and_then(|value| Generation::new(value).ok())
                .ok_or(UnrecognizedReason::InvalidValue)?,
            ceiling: match ceiling {
                CeilingTag::At(ceiling) => Some(ceiling),
                CeilingTag::Absent | CeilingTag::Malformed => None,
            },
        };
        let kind = K::from_tag_name(kind).ok_or(UnrecognizedReason::InvalidValue)?;

        // a value such as `gen-07` parses but renders differently, so only a set that
        // round-trips is exact; malformed ceiling tags are left out of the comparison
        let until = format!("{}-until-", self.prefix);
        let mut rendered = self.render(&owner, kind);
        let mut actual: Vec<_> = tags
            .iter()
            .filter(|tag| ceiling != CeilingTag::Malformed || !tag.starts_with(&until))
            .cloned()
            .collect();
        rendered.sort();
        actual.sort();
        if rendered != actual {
            return Err(UnrecognizedReason::InvalidValue);
        }

        Ok(ManagedTags {
            ceiling,
            ..owner.with_kind(kind)
        })
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
        Ceiling, CeilingTag, ManagedTags, OwnerTags, ResourceKind, TagMatch, TagScheme,
        UnrecognizedReason,
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

    fn owner(ceiling: Option<Ceiling>) -> OwnerTags {
        OwnerTags {
            owner: OwnerId::parse("job_1").unwrap(),
            generation: Generation::new(7).unwrap(),
            ceiling,
        }
    }

    fn tags(ceiling: Option<Ceiling>) -> ManagedTags<Kind> {
        owner(ceiling).with_kind(Kind::Disk)
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn rendered_tags_round_trip_with_and_without_a_ceiling() {
        let ceiling = Ceiling::at_or_before(UnixMillis::new(1_764_000_000_999).unwrap());
        for ceiling in [None, Some(ceiling)] {
            let rendered = scheme().render(&owner(ceiling), Kind::Disk);

            assert_eq!(
                scheme().classify(&rendered),
                TagMatch::Managed(tags(ceiling))
            );
        }

        assert_eq!(
            scheme().render(&owner(Some(ceiling)), Kind::Disk),
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
    fn a_malformed_ceiling_keeps_the_owner_but_never_parses_as_a_deadline() {
        let base = ["cl-managed", "cl-owner-job_1", "cl-gen-7", "cl-kind-disk"];
        let malformed = [
            vec!["cl-until-soon"],
            vec!["cl-until-0100"],
            vec!["cl-until-"],
            vec!["cl-until-99999999999999999999"],
            vec!["cl-until-100", "cl-until-200"],
        ];

        for until in malformed {
            let tags_with_until: Vec<&str> = base.iter().copied().chain(until).collect();
            let expected = ManagedTags {
                ceiling: CeilingTag::Malformed,
                ..tags(None)
            };

            assert_eq!(
                scheme().classify(&strings(&tags_with_until)),
                TagMatch::Managed(expected),
                "{tags_with_until:?}"
            );
        }

        let latest = UnixMillis::new(crate::time::MAX_SAFE_INTEGER).unwrap();
        assert!(!CeilingTag::Malformed.is_reached_at(latest));

        // a malformed ceiling does not excuse any other tag
        let extra = strings(&[
            "cl-managed",
            "cl-owner-job_1",
            "cl-gen-07",
            "cl-kind-disk",
            "cl-until-soon",
        ]);
        assert_eq!(
            scheme().classify::<Kind>(&extra),
            TagMatch::Unrecognized(UnrecognizedReason::InvalidValue)
        );
    }

    #[test]
    fn another_prefix_never_matches() {
        let other = TagScheme::new(TagPrefix::parse("rb").unwrap());
        let rendered = other.render(&owner(None), Kind::Disk);

        assert_eq!(scheme().classify::<Kind>(&rendered), TagMatch::Foreign);
    }

    #[test]
    fn a_ceiling_rounds_down_and_is_reached_at_its_second() {
        let ceiling = Ceiling::at_or_before(UnixMillis::new(10_999).unwrap());

        assert_eq!(ceiling.as_unix_seconds(), 10);
        assert!(!ceiling.is_reached_at(UnixMillis::new(9_999).unwrap()));
        assert!(ceiling.is_reached_at(UnixMillis::new(10_000).unwrap()));
    }

    #[test]
    fn a_selector_scheme_requires_its_own_selector_and_names_it_alone() {
        let plain = scheme();
        let selecting = scheme().with_selector();
        let owner = OwnerTags {
            owner: OwnerId::parse("prj_a-1").unwrap(),
            generation: Generation::new(12).unwrap(),
            ceiling: None,
        };
        let rendered = selecting.render(&owner, Kind::Machine);
        assert!(rendered.contains(&"cl-sel-prj_a-1-12".to_owned()));
        assert_eq!(
            selecting.classify::<Kind>(&rendered),
            TagMatch::Managed(owner.with_kind(Kind::Machine))
        );

        // the selector is part of the exact set in both directions
        assert_eq!(
            plain.classify::<Kind>(&rendered),
            TagMatch::Unrecognized(UnrecognizedReason::ExtraTag)
        );
        let without = plain.render(&owner, Kind::Machine);
        assert!(matches!(
            selecting.classify::<Kind>(&without),
            TagMatch::Unrecognized(_)
        ));
        let mut other = rendered.clone();
        other.retain(|tag| !tag.starts_with("cl-sel-"));
        other.push("cl-sel-prj_a-1-13".to_owned());
        assert!(matches!(
            selecting.classify::<Kind>(&other),
            TagMatch::Unrecognized(_)
        ));

        assert_eq!(
            selecting.parse_selector("cl-sel-prj_a-1-12"),
            Some((owner.owner.clone(), owner.generation))
        );
        assert_eq!(selecting.parse_selector("cl-sel-prj_a-1-012"), None);
        assert_eq!(selecting.parse_selector("cl-sel-12"), None);
        assert_eq!(selecting.parse_selector("rb-sel-prj_a-1-12"), None);
    }

    #[test]
    fn pairs_round_trip_and_reject_foreign_keys_and_markers() {
        let scheme = scheme();
        let ceiling = Ceiling::at_or_before(UnixMillis::new(1_764_000_000_999).unwrap());
        let pairs = scheme.render_pairs(&owner(Some(ceiling)), Kind::Disk);
        let pair = |key: &str, value: &str| (key.to_owned(), value.to_owned());
        assert_eq!(
            pairs,
            vec![
                pair("cl-managed", "true"),
                pair("cl-owner", "job_1"),
                pair("cl-gen", "7"),
                pair("cl-kind", "disk"),
                pair("cl-until", "1764000000"),
            ]
        );
        assert_eq!(
            scheme.classify_pairs::<Kind>(&pairs),
            TagMatch::Managed(tags(Some(ceiling)))
        );

        let mut foreign_key = pairs.clone();
        foreign_key.push(pair("cl-lane", "lane_1"));
        assert_eq!(
            scheme.classify_pairs::<Kind>(&foreign_key),
            TagMatch::Unrecognized(UnrecognizedReason::ExtraTag)
        );

        // an older shape that reused the managed key keeps its resources visible
        let legacy = vec![pair("cl-managed", "rembuild"), pair("cl-project", "x")];
        assert_eq!(
            scheme.classify_pairs::<Kind>(&legacy),
            TagMatch::Unrecognized(UnrecognizedReason::InvalidValue)
        );
        assert_eq!(
            scheme.classify_pairs::<Kind>(&[pair("Name", "x")]),
            TagMatch::Foreign
        );

        // a value that would only split differently as a string is still exact
        let mut moved = pairs;
        moved[1] = pair("cl-owner", "job_1-gen-8");
        assert!(matches!(
            scheme.classify_pairs::<Kind>(&moved),
            TagMatch::Managed(parsed) if parsed.owner.as_str() == "job_1-gen-8"
        ));
    }
}
