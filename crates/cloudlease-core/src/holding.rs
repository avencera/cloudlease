//! The resources one leased generation owns, from lease to confirmed deletion
//!
//! ```text
//! Leased { lease, slots } --release or expiry--> Releasing { deletes, retry } --last confirm--> Released
//! ```
//!
//! An owner records a slot as [`Slot::Requested`] and persists it *before* it sends a create
//! request, then records [`Slot::Created`] once the provider returns an ID. If the response is
//! lost, the requested slot becomes a [`DeleteTarget::Tagged`] cleanup target, so the resource
//! is still found by its tags and deleted.
//!
//! Expiry is a pure transition ([`Holding::expire`]). Owners apply and persist it before any
//! provider call, so a failing provider can never keep an expired lease alive.

use std::{collections::BTreeMap, time::Duration};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{
    alarm::{Backoff, Wake},
    deletes::{DeleteTarget, DeletesError, PendingDeletes},
    ids::{Generation, LeaseId, MachineIdentity, OwnerId, ResourceId},
    lease::{Expiry, Lease, LeaseError, LeasePolicy},
    tags::{Ceiling, ManagedTags, OwnerTags, ResourceKind},
    time::UnixMillis,
};

/// One resource kind within a leased generation
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Slot {
    /// The create request may have been sent, but no ID is recorded
    Requested,
    /// The provider returned this ID
    Created {
        /// The provider ID
        id: ResourceId,
    },
}

/// Why a holding stopped being leased
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReleaseReason {
    /// The client released its lease
    Released,
    /// The client stopped sending heartbeats
    HeartbeatMissed,
    /// The lease reached its maximum duration
    HardDeadline,
    /// The owner ended the lease for its own reason, such as a spend cap or a failed create
    ///
    /// Apps that need finer reasons keep them in their own records.
    Revoked,
}

impl From<Expiry> for ReleaseReason {
    fn from(expiry: Expiry) -> Self {
        match expiry {
            Expiry::HeartbeatMissed => Self::HeartbeatMissed,
            Expiry::HardDeadline => Self::HardDeadline,
        }
    }
}

/// The next permitted cleanup attempt after a failed or unconfirmed one
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Retry {
    /// The number of attempts that did not finish, starting at one
    pub attempt: u32,
    /// The earliest time to try again
    pub at: UnixMillis,
}

/// The lifecycle state of one leased generation
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    bound(
        serialize = "K: ResourceKind",
        deserialize = "K: ResourceKind + DeserializeOwned"
    )
)]
pub enum HoldingState<K> {
    /// A live or not yet checked lease owns the slots
    Leased {
        /// The client lease
        lease: Lease,
        /// The resources requested or created for the lease
        slots: BTreeMap<K, Slot>,
    },
    /// The lease ended and resources are being deleted
    Releasing {
        /// Why the lease ended
        reason: ReleaseReason,
        /// When the lease ended
        since: UnixMillis,
        /// Resources not yet confirmed gone
        deletes: PendingDeletes<K>,
        /// When to try again after an attempt that did not finish
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry: Option<Retry>,
    },
    /// Every resource is confirmed gone
    Released {
        /// Why the lease ended
        reason: ReleaseReason,
        /// When the last resource was confirmed gone
        at: UnixMillis,
    },
}

/// How long after a lease ends an empty tag search still proves nothing
///
/// A create sent just before the lease ended can still be in flight. This is well past the
/// DigitalOcean client's 30 second request timeout, so such a create has finished or failed
/// by then; if one still lands later, the sweep deletes it as unclaimed.
pub const TAG_SEARCH_SETTLE: Duration = Duration::from_secs(120);

/// A rejected holding transition
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum HoldingError {
    /// The action does not apply to the current state
    #[error("cannot {action} a holding that is {state}")]
    InvalidTransition {
        /// The current state name
        state: &'static str,
        /// The rejected action
        action: &'static str,
    },
    /// The kind already has a slot
    #[error("the resource kind already has a slot")]
    SlotTaken,
    /// The kind was not requested, or was created with a different ID
    #[error("the resource kind was not requested or has a different ID")]
    SlotMismatch,
    /// The lease rejected the request
    #[error(transparent)]
    Lease(#[from] LeaseError),
    /// The pending deletes rejected the change
    #[error(transparent)]
    Deletes(#[from] DeletesError),
}

/// The resources one owner holds for one lease generation
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    rename_all = "camelCase",
    bound(
        serialize = "K: ResourceKind",
        deserialize = "K: ResourceKind + DeserializeOwned"
    )
)]
pub struct Holding<K> {
    owner: OwnerId,
    generation: Generation,
    state: HoldingState<K>,
}

impl<K: ResourceKind> Holding<K> {
    /// Starts a holding for a new lease with no resources yet
    #[must_use]
    pub const fn leased(owner: OwnerId, generation: Generation, lease: Lease) -> Self {
        Self {
            owner,
            generation,
            state: HoldingState::Leased {
                lease,
                slots: BTreeMap::new(),
            },
        }
    }

    /// Returns the owner
    #[must_use]
    pub const fn owner(&self) -> &OwnerId {
        &self.owner
    }

    /// Returns the generation
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// Returns the lifecycle state
    #[must_use]
    pub const fn state(&self) -> &HoldingState<K> {
        &self.state
    }

    /// Returns the lease while the holding is leased
    #[must_use]
    pub const fn lease(&self) -> Option<&Lease> {
        match &self.state {
            HoldingState::Leased { lease, .. } => Some(lease),
            _ => None,
        }
    }

    /// Returns the tags every resource of this leased generation is created with
    ///
    /// The ceiling is the lease's hard deadline, so the sweep deletes the resources at that
    /// time even if this owner object is broken.
    pub fn owner_tags(&self) -> Result<OwnerTags, HoldingError> {
        let (lease, _) = self.leased_parts("tag resources for")?;

        Ok(OwnerTags {
            owner: self.owner.clone(),
            generation: self.generation,
            ceiling: Some(Ceiling::at_or_before(lease.hard_deadline())),
        })
    }

    /// Records that a create request for `kind` is about to be sent
    ///
    /// Persist the result before sending the request.
    pub fn request(&self, kind: K, now: UnixMillis) -> Result<Self, HoldingError> {
        let (lease, slots) = self.leased_parts("request a resource for")?;
        if let Some(expiry) = lease.expiry(now) {
            return Err(LeaseError::Expired(expiry).into());
        }
        if slots.contains_key(&kind) {
            return Err(HoldingError::SlotTaken);
        }

        let mut slots = slots.clone();
        slots.insert(kind, Slot::Requested);

        Ok(self.with_state(HoldingState::Leased {
            lease: lease.clone(),
            slots,
        }))
    }

    /// Records the ID the provider returned for a requested kind
    ///
    /// Recording the same ID twice is accepted, so a retried request is harmless.
    pub fn record_created(&self, kind: K, id: ResourceId) -> Result<Self, HoldingError> {
        let (lease, slots) = self.leased_parts("record a resource for")?;
        match slots.get(&kind) {
            Some(Slot::Requested) => {}
            Some(Slot::Created { id: existing }) if existing == &id => return Ok(self.clone()),
            _ => return Err(HoldingError::SlotMismatch),
        }

        let mut slots = slots.clone();
        slots.insert(kind, Slot::Created { id });

        Ok(self.with_state(HoldingState::Leased {
            lease: lease.clone(),
            slots,
        }))
    }

    /// Accepts a client heartbeat
    pub fn heartbeat(
        &self,
        lease_id: &LeaseId,
        sequence: u64,
        now: UnixMillis,
        policy: &LeasePolicy,
    ) -> Result<Self, HoldingError> {
        let (lease, slots) = self.leased_parts("heartbeat")?;

        Ok(self.with_state(HoldingState::Leased {
            lease: lease.heartbeat(lease_id, sequence, now, policy)?,
            slots: slots.clone(),
        }))
    }

    /// Binds the identity the leased machine reports, while the lease is live
    pub fn bind_machine(
        &self,
        lease_id: &LeaseId,
        machine: MachineIdentity,
        now: UnixMillis,
    ) -> Result<Self, HoldingError> {
        let (lease, slots) = self.leased_parts("bind a machine to")?;

        Ok(self.with_state(HoldingState::Leased {
            lease: lease.bind_machine(lease_id, machine, now)?,
            slots: slots.clone(),
        }))
    }

    /// Ends a live lease at the client's request and starts deleting its resources
    pub fn release(&self, lease_id: &LeaseId, now: UnixMillis) -> Result<Self, HoldingError> {
        let (lease, _) = self.leased_parts("release")?;
        lease.ensure_live(lease_id, now)?;

        Ok(self.end_lease(ReleaseReason::Released, now))
    }

    /// Ends a lease on the owner's authority, without the client's lease ID
    ///
    /// A holding that is already releasing or released is returned unchanged, so revoking
    /// twice is harmless.
    #[must_use]
    pub fn revoke(&self, now: UnixMillis) -> Self {
        self.end_lease(ReleaseReason::Revoked, now)
    }

    /// Ends an expired lease, or returns `None` when the lease is live or already ended
    ///
    /// Apply and persist this before any provider call.
    #[must_use]
    pub fn expire(&self, now: UnixMillis) -> Option<Self> {
        let expiry = self.lease()?.expiry(now)?;

        Some(self.end_lease(expiry.into(), now))
    }

    /// Returns the targets the next cleanup attempt may delete, empty unless releasing
    #[must_use]
    pub fn ready_deletes(&self) -> Vec<DeleteTarget<K>> {
        match &self.state {
            HoldingState::Releasing { deletes, .. } => deletes.ready(),
            _ => Vec::new(),
        }
    }

    /// Removes a target the provider reported absent
    pub fn confirm_gone(
        &self,
        target: &DeleteTarget<K>,
        now: UnixMillis,
    ) -> Result<Self, HoldingError> {
        let (reason, since, deletes) = self.releasing("confirm a deletion for")?;

        Ok(self.after_progress(reason, since, deletes.confirm_gone(target)?, now))
    }

    /// Replaces a tagged target with the IDs a tag search found
    ///
    /// Returns `None` for an empty search within [`TAG_SEARCH_SETTLE`] of the lease's end:
    /// a create sent just before then may not be visible yet, so the target stays pending
    /// and the owner retries.
    pub fn resolve_tagged(
        &self,
        kind: K,
        found: impl IntoIterator<Item = ResourceId>,
        now: UnixMillis,
    ) -> Result<Option<Self>, HoldingError> {
        let (reason, since, deletes) = self.releasing("resolve a tagged deletion for")?;
        let found: Vec<_> = found.into_iter().collect();
        if found.is_empty() && now < since.saturating_add(TAG_SEARCH_SETTLE) {
            return Ok(None);
        }

        let remaining = deletes.resolve_tagged(kind, found)?;

        Ok(Some(self.after_progress(reason, since, remaining, now)))
    }

    /// Schedules the next cleanup attempt after one that failed or did not confirm absence
    pub fn retry_later(&self, now: UnixMillis, backoff: &Backoff) -> Result<Self, HoldingError> {
        let HoldingState::Releasing {
            reason,
            since,
            deletes,
            retry,
        } = &self.state
        else {
            return Err(self.invalid("schedule a cleanup retry for"));
        };

        let attempt = retry.map_or(1, |retry| retry.attempt.saturating_add(1));

        Ok(self.with_state(HoldingState::Releasing {
            reason: *reason,
            since: *since,
            deletes: deletes.clone(),
            retry: Some(Retry {
                attempt,
                at: now.saturating_add(backoff.delay(attempt)),
            }),
        }))
    }

    /// Returns when the owner must next act on this holding, or `None` once released
    #[must_use]
    pub fn wake(&self) -> Option<Wake> {
        match &self.state {
            HoldingState::Leased { lease, .. } => Some(Wake::At(lease.next_deadline())),
            HoldingState::Releasing { retry: None, .. } => Some(Wake::Now),
            HoldingState::Releasing {
                retry: Some(retry), ..
            } => Some(Wake::At(retry.at)),
            HoldingState::Released { .. } => None,
        }
    }

    /// Reports whether a live lease holds this generation at `now`
    #[must_use]
    pub fn is_live(&self, now: UnixMillis) -> bool {
        self.lease()
            .is_some_and(|lease| lease.expiry(now).is_none())
    }

    /// Reports whether every resource of this generation is confirmed gone
    #[must_use]
    pub const fn is_released(&self) -> bool {
        matches!(self.state, HoldingState::Released { .. })
    }

    /// Reports whether this holding keeps an observed resource alive
    ///
    /// Only a live lease of the same owner and generation claims a resource, and only for a
    /// kind it requested: by the recorded ID once created, or any ID while the create is
    /// pending. Everything else is unclaimed and the sweep deletes it.
    #[must_use]
    pub fn claims(&self, id: &ResourceId, tags: &ManagedTags<K>, now: UnixMillis) -> bool {
        if tags.owner != self.owner || tags.generation != self.generation || !self.is_live(now) {
            return false;
        }
        let HoldingState::Leased { slots, .. } = &self.state else {
            return false;
        };

        match slots.get(&tags.kind) {
            Some(Slot::Requested) => true,
            Some(Slot::Created { id: recorded }) => recorded == id,
            None => false,
        }
    }

    fn end_lease(&self, reason: ReleaseReason, now: UnixMillis) -> Self {
        let HoldingState::Leased { slots, .. } = &self.state else {
            return self.clone();
        };
        let targets = slots.iter().map(|(kind, slot)| match slot {
            Slot::Requested => DeleteTarget::Tagged { kind: *kind },
            Slot::Created { id } => DeleteTarget::Known {
                kind: *kind,
                id: id.clone(),
            },
        });

        let state = match PendingDeletes::new(targets) {
            Some(deletes) => HoldingState::Releasing {
                reason,
                since: now,
                deletes,
                retry: None,
            },
            None => HoldingState::Released { reason, at: now },
        };

        self.with_state(state)
    }

    fn after_progress(
        &self,
        reason: ReleaseReason,
        since: UnixMillis,
        remaining: Option<PendingDeletes<K>>,
        now: UnixMillis,
    ) -> Self {
        // progress clears the retry, so the next rank starts at once
        let state = match remaining {
            Some(deletes) => HoldingState::Releasing {
                reason,
                since,
                deletes,
                retry: None,
            },
            None => HoldingState::Released { reason, at: now },
        };

        self.with_state(state)
    }

    fn leased_parts(
        &self,
        action: &'static str,
    ) -> Result<(&Lease, &BTreeMap<K, Slot>), HoldingError> {
        match &self.state {
            HoldingState::Leased { lease, slots } => Ok((lease, slots)),
            _ => Err(self.invalid(action)),
        }
    }

    fn releasing(
        &self,
        action: &'static str,
    ) -> Result<(ReleaseReason, UnixMillis, &PendingDeletes<K>), HoldingError> {
        match &self.state {
            HoldingState::Releasing {
                reason,
                since,
                deletes,
                ..
            } => Ok((*reason, *since, deletes)),
            _ => Err(self.invalid(action)),
        }
    }

    fn with_state(&self, state: HoldingState<K>) -> Self {
        Self {
            owner: self.owner.clone(),
            generation: self.generation,
            state,
        }
    }

    const fn invalid(&self, action: &'static str) -> HoldingError {
        let state = match self.state {
            HoldingState::Leased { .. } => "leased",
            HoldingState::Releasing { .. } => "releasing",
            HoldingState::Released { .. } => "released",
        };

        HoldingError::InvalidTransition { state, action }
    }
}
