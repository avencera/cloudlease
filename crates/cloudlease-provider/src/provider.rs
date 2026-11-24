//! The provider boundary every cloud adapter implements

use std::future::Future;

use cloudlease_core::{
    ids::{Generation, OwnerId, ResourceId},
    sweep::Inventory,
    tags::{OwnerTags, ResourceKind},
};

use crate::error::ProviderResult;

/// A create request for one resource, which knows its own kind
pub trait CreateSpec {
    /// The kind enum the spec belongs to
    type Kind: ResourceKind;

    /// Returns the kind this spec creates
    fn kind(&self) -> Self::Kind;
}

/// Whether a resource exists
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Presence {
    /// The provider still reports the resource, possibly while deleting it
    Present,
    /// The provider reports the resource does not exist
    Absent,
}

/// The operations cloudlease needs from a cloud provider
///
/// An adapter is configured with one app's [`TagScheme`](cloudlease_core::tags::TagScheme)
/// and applies its exact tags to everything it creates, so a caller cannot create an
/// untagged resource or label it with the wrong kind. Optional capabilities, such as
/// firewalls, belong in extension traits rather than here.
pub trait CloudProvider {
    /// The resource kinds this provider manages
    type Kind: ResourceKind;
    /// The create request for one resource
    type Spec: CreateSpec<Kind = Self::Kind>;

    /// Creates one resource with the owner's tags and the spec's kind tag
    ///
    /// A create is never retried automatically: if the response is lost, the resource may
    /// exist, and [`CloudProvider::find`] finds it by its tags.
    fn create(
        &self,
        owner: &OwnerTags,
        spec: &Self::Spec,
    ) -> impl Future<Output = ProviderResult<ResourceId>>;

    /// Requests deletion, treating an absent resource as success
    ///
    /// Success means the provider accepted the request, not that the resource is gone; use
    /// [`CloudProvider::presence`] to confirm.
    fn delete(&self, kind: Self::Kind, id: &ResourceId)
    -> impl Future<Output = ProviderResult<()>>;

    /// Reads whether a resource exists
    fn presence(
        &self,
        kind: Self::Kind,
        id: &ResourceId,
    ) -> impl Future<Output = ProviderResult<Presence>>;

    /// Finds the resources of one kind with an exact tag set for an owner and generation
    fn find(
        &self,
        owner: &OwnerId,
        generation: Generation,
        kind: Self::Kind,
    ) -> impl Future<Output = ProviderResult<Vec<ResourceId>>>;

    /// Lists every resource with the app's managed tag
    fn inventory(&self) -> impl Future<Output = ProviderResult<Inventory<Self::Kind>>>;
}
