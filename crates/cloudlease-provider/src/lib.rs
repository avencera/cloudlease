//! The cloud provider boundary for cloudlease, the cleanup that drives it, and the
//! DigitalOcean, phoenixNAP, Latitude.sh and AWS adapters
//!
//! Nothing here depends on a Worker runtime: clients take an injected [`http::HttpTransport`]
//! and [`http::Runtime`], so the cleanup rules run under plain `cargo test`.
//!
//! - [`provider::CloudProvider`]: create, delete, presence, find by tags, inventory
//! - [`reaper::reap`]: one delete-and-confirm pass over a releasing holding
//! - [`sweep::delete_doomed`]: deletes the resources a sweep doomed, lowest rank first
//! - [`firewall::IngressPolicy`]: inbound access for one generation, an optional capability
//! - [`digitalocean`]: droplets over the DigitalOcean v2 API
//! - [`phoenixnap`]: Bare Metal Cloud servers over the phoenixNAP BMC v1 API
//! - [`latitude`]: bare metal servers and their user data over the Latitude.sh API
//! - [`aws`]: EC2 Spot instances over the EC2 Query API, and security group ingress
//! - [`rest`]: retry, backoff and URL rules the REST clients share

pub mod aws;
pub mod digitalocean;
pub mod error;
pub mod firewall;
pub mod http;
pub mod latitude;
pub mod phoenixnap;
pub mod provider;
pub mod reaper;
pub mod rest;
pub mod sweep;
mod validated;
