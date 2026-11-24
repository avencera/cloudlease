//! The cloud provider boundary for cloudlease, the cleanup that drives it, and the
//! DigitalOcean and phoenixNAP adapters
//!
//! Nothing here depends on a Worker runtime: clients take an injected [`http::HttpTransport`]
//! and [`http::Runtime`], so the cleanup rules run under plain `cargo test`.
//!
//! - [`provider::CloudProvider`]: create, delete, presence, find by tags, inventory
//! - [`reaper::reap`]: one delete-and-confirm pass over a releasing holding
//! - [`sweep::delete_doomed`]: deletes the resources a sweep doomed, lowest rank first
//! - [`digitalocean`]: droplets and volumes over the DigitalOcean v2 API
//! - [`phoenixnap`]: Bare Metal Cloud servers over the phoenixNAP BMC v1 API
//! - [`rest`]: retry, backoff and URL rules the REST clients share

pub mod digitalocean;
pub mod error;
pub mod http;
pub mod phoenixnap;
pub mod provider;
pub mod reaper;
pub mod rest;
pub mod sweep;
mod validated;
