//! Delete targets that leave only once the provider reports them absent
//!
//! A delete request is not proof of deletion: providers accept the request and remove the
//! resource later, and a request can fail after the provider acted. A target therefore stays
//! pending until a read shows the resource is gone.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ids::ResourceId, tags::ResourceKind};

/// One resource to delete
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(
    tag = "target",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DeleteTarget<K> {
    /// A resource whose provider ID is known
    Known {
        /// The resource kind
        kind: K,
        /// The provider ID
        id: ResourceId,
    },
    /// A resource whose create request was sent but whose ID was never recorded
    ///
    /// The cleanup finds it by its exact owner, generation and kind tags.
    Tagged {
        /// The resource kind
        kind: K,
    },
}

impl<K: ResourceKind> DeleteTarget<K> {
    /// Returns the resource kind
    #[must_use]
    pub const fn kind(&self) -> K {
        match self {
            Self::Known { kind, .. } | Self::Tagged { kind } => *kind,
        }
    }
}

/// A rejected change to pending deletes
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DeletesError {
    /// The target is not pending
    #[error("the delete target is not pending")]
    UnknownTarget,
    /// The target belongs to a later rank than the one being deleted
    #[error("a lower-rank resource must be confirmed gone first")]
    RankNotReady,
}

/// A non-empty set of resources that still have to be confirmed gone
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PendingDeletes<K>(BTreeSet<DeleteTarget<K>>);

impl<K: ResourceKind> PendingDeletes<K> {
    /// Creates a pending set, or `None` when there is nothing to delete
    #[must_use]
    pub fn new(targets: impl IntoIterator<Item = DeleteTarget<K>>) -> Option<Self> {
        let targets: BTreeSet<_> = targets.into_iter().collect();

        (!targets.is_empty()).then_some(Self(targets))
    }

    /// Returns every pending target
    pub fn iter(&self) -> impl Iterator<Item = &DeleteTarget<K>> {
        self.0.iter()
    }

    /// Returns the lowest delete rank still pending
    #[must_use]
    pub fn current_rank(&self) -> u8 {
        self.0
            .iter()
            .map(|target| target.kind().delete_rank())
            .min()
            .unwrap_or_default()
    }

    /// Returns the targets that may be deleted now: those of the lowest pending rank
    #[must_use]
    pub fn ready(&self) -> Vec<DeleteTarget<K>> {
        let rank = self.current_rank();

        self.0
            .iter()
            .filter(|target| target.kind().delete_rank() == rank)
            .cloned()
            .collect()
    }

    /// Removes a target the provider reported absent, returning `None` once nothing is left
    pub fn confirm_gone(&self, target: &DeleteTarget<K>) -> Result<Option<Self>, DeletesError> {
        if !self.0.contains(target) {
            return Err(DeletesError::UnknownTarget);
        }
        if target.kind().delete_rank() != self.current_rank() {
            return Err(DeletesError::RankNotReady);
        }

        let mut remaining = self.0.clone();
        remaining.remove(target);

        Ok(Self::new(remaining))
    }

    /// Replaces a tagged target with the resources a tag search found
    ///
    /// An empty search confirms the tagged target absent.
    pub fn resolve_tagged(
        &self,
        kind: K,
        found: impl IntoIterator<Item = ResourceId>,
    ) -> Result<Option<Self>, DeletesError> {
        let tagged = DeleteTarget::Tagged { kind };
        if !self.0.contains(&tagged) {
            return Err(DeletesError::UnknownTarget);
        }

        let mut remaining = self.0.clone();
        remaining.remove(&tagged);
        remaining.extend(found.into_iter().map(|id| DeleteTarget::Known { kind, id }));

        Ok(Self::new(remaining))
    }
}

impl<'de, K> Deserialize<'de> for PendingDeletes<K>
where
    K: ResourceKind,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let targets = Vec::<DeleteTarget<K>>::deserialize(deserializer)?;

        Self::new(targets).ok_or_else(|| serde::de::Error::custom("pending deletes are empty"))
    }
}
