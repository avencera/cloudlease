//! A test-only Worker that runs cloudlease in miniflare against a fake DigitalOcean API
//!
//! It wires the library the way an app would: an `Owner` Durable Object per owner ID that
//! keeps holdings and answers the sweep, a `Capacity` gate object, and a scheduled sweep.
//! The `/test/sweep` route and the owner's `holdings` routes let the vitest suite run the
//! sweep on demand and seed any holding state; a real app would not expose them.

#[cfg(target_arch = "wasm32")]
mod app;

use std::time::Duration;

use cloudlease_provider::sweep::ConfirmPolicy;
use cloudlease_worker::{
    capacity::CapacityConfig, digitalocean::DigitalOceanBindings, sweep::SweepConfig,
};

/// The namespace binding of the owner objects
pub const OWNERS: &str = "OWNERS";
/// The namespace binding of the capacity gate
pub const CAPACITY: &str = "CAPACITY";
/// The name of the one capacity gate object
pub const CAPACITY_OBJECT: &str = "global";

/// The DigitalOcean bindings the test Worker reads
pub const DIGITALOCEAN: DigitalOceanBindings<'static> = DigitalOceanBindings {
    token: "DIGITALOCEAN_TOKEN",
    api_url: Some("DIGITALOCEAN_API_URL"),
    tag_prefix: "CLOUDLEASE_TAG_PREFIX",
};

/// The capacity gate settings
pub const CAPACITY_CONFIG: CapacityConfig = CapacityConfig {
    limit_var: "MAX_ACTIVE_MACHINES",
    default_limit: 2,
    max_limit: 100,
    max_ttl: Duration::from_secs(6 * 60 * 60),
};

/// The sweep settings, with short confirm pauses so the suite stays fast
pub const SWEEP: SweepConfig<'static> = SweepConfig {
    owner_binding: OWNERS,
    capacity: Some((CAPACITY, CAPACITY_OBJECT)),
    confirm: ConfirmPolicy {
        reads: 5,
        pause: Duration::from_millis(50),
    },
};
