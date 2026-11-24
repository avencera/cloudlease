//! The cloudlease machines Worker: temporary DigitalOcean, phoenixNAP and Latitude.sh
//! machines whose lifetime Cloudflare enforces, so no machine depends on an agent to be
//! deleted
//!
//! It serves the temporary machines contract (`/v1/runs`, `/v1/leases`) to homebased:
//!
//! - **`Run`** Durable Object, one per run: the owner of every machine of the run. It keeps
//!   the run and machine records, the core holdings, and the hashes of the lease tokens. Its
//!   alarm ends leases at their heartbeat or hard deadline, releases the run's machines when
//!   they spend the run's cap, deletes and confirms their resources, and returns their
//!   permits.
//! - **`Gate`** Durable Object: the core capacity gate, weighted in cents per hour, whose
//!   limit is the hourly spend cap.
//! - **`Monitor`** Durable Object: consecutive sweep failures, so a failing owner alerts on
//!   its third sweep in a row rather than on every one.
//! - A cron sweep every 5 minutes that deletes every resource with the app's tag prefix that
//!   no live lease claims, droplets, volumes, phoenixNAP servers and Latitude.sh servers and
//!   their user data alike, and alerts on orphans.
//!
//! Each machine class names its provider. Alerts go to the ntfy topic in `NTFY_URL`. The
//! Worker has its own DigitalOcean token, optional phoenixNAP credentials, an optional
//! Latitude.sh key and project, and its own tag prefix, so it cannot touch another app's
//! resources.

#[cfg(target_arch = "wasm32")]
mod app;

use std::time::Duration;

use cloudlease_machines::config::MAX_RUN_LIFETIME;
use cloudlease_provider::sweep::ConfirmPolicy;
use cloudlease_worker::{
    capacity::CapacityConfig, digitalocean::DigitalOceanBindings, latitude::LatitudeBindings,
    phoenixnap::PhoenixNapBindings, sweep::SweepConfig,
};

/// The namespace binding of the run objects
pub const RUNS: &str = "RUNS";
/// The namespace binding of the hourly spend gate
pub const GATE: &str = "GATE";
/// The name of the one gate object
pub const GATE_OBJECT: &str = "global";
/// The namespace binding of the sweep monitor
pub const MONITOR: &str = "MONITOR";
/// The name of the one monitor object
pub const MONITOR_OBJECT: &str = "global";

/// The variable holding the JSON configuration
pub const CONFIG_VAR: &str = "MACHINES_CONFIG";
/// The secret mapping each client app to the SHA-256 of its token
pub const CLIENT_TOKENS: &str = "CLIENT_TOKENS";
/// The secret holding the ntfy topic URL
pub const NTFY_URL: &str = "NTFY_URL";
/// The optional secret holding an ntfy access token
pub const NTFY_TOKEN: &str = "NTFY_TOKEN";

/// The DigitalOcean bindings
pub const DIGITALOCEAN: DigitalOceanBindings<'static> = DigitalOceanBindings {
    token: "DIGITALOCEAN_TOKEN",
    api_url: Some("DIGITALOCEAN_API_URL"),
    tag_prefix: "CLOUDLEASE_TAG_PREFIX",
};

/// The phoenixNAP bindings; a Worker without the two secrets refuses phoenixNAP classes
pub const PHOENIXNAP: PhoenixNapBindings<'static> = PhoenixNapBindings {
    client_id: "PNAP_CLIENT_ID",
    client_secret: "PNAP_CLIENT_SECRET",
    api_url: Some("PNAP_API_URL"),
    token_url: Some("PNAP_TOKEN_URL"),
    tag_prefix: "CLOUDLEASE_TAG_PREFIX",
};

/// The Latitude.sh bindings; a Worker without the key and project refuses Latitude.sh classes
pub const LATITUDE: LatitudeBindings<'static> = LatitudeBindings {
    api_key: "LATITUDE_API_KEY",
    project: "LATITUDE_PROJECT",
    api_url: Some("LATITUDE_API_URL"),
    tag_prefix: "CLOUDLEASE_TAG_PREFIX",
};

/// The hourly spend gate: every permit weighs its class's price in cents per hour, and the
/// limit is the most the app's machines may cost per hour, counting each until its resources
/// are confirmed gone
pub const GATE_CONFIG: CapacityConfig = CapacityConfig {
    limit_var: "HOURLY_SPEND_CAP_CENTS",
    // a missing setting fails closed at $1/hour
    default_limit: 100,
    max_limit: u16::MAX,
    // a permit lives as long as its lease can, plus a margin for deletion
    max_ttl: MAX_RUN_LIFETIME.saturating_add(Duration::from_secs(60 * 60)),
};

/// The tag sweep
pub const SWEEP: SweepConfig<'static> = SweepConfig {
    owner_binding: RUNS,
    capacity: Some((GATE, GATE_OBJECT)),
    confirm: ConfirmPolicy {
        reads: 10,
        pause: Duration::from_secs(3),
    },
    // a run object answers from its own SQLite, so 5 seconds means it is stalled; a timeout
    // only keeps its resources below their ceiling until the next sweep
    owner_timeout: Duration::from_secs(5),
};

/// How long a pending create may wait on the gate before a retry of the same request takes
/// it over
pub const PENDING_RESUME: Duration = Duration::from_secs(30);

/// How long a pending create whose client never retried keeps its generation before the run
/// object drops it
pub const PENDING_EXPIRY: Duration = Duration::from_secs(10 * 60);

/// How long a permit outlives its lease's hard deadline before the gate calls it suspect
pub const PERMIT_MARGIN: Duration = Duration::from_secs(30 * 60);

/// How soon the run object retries returning a permit the gate did not take back
pub const PERMIT_RETRY: Duration = Duration::from_secs(60);
