//! Pure lease, ownership and cleanup rules for cloud resources whose lifetime a Cloudflare
//! Durable Object enforces
//!
//! Nothing here performs I/O or reads a clock: every transition takes the current time as an
//! argument, so the rules can be tested with plain `cargo test` and reused by any runtime.
//!
//! - [`lease`]: a client lease with a heartbeat deadline and a hard deadline
//! - [`holding`]: the resources one leased generation owns, from lease to confirmed deletion
//! - [`deletes`]: delete targets that leave only once the provider reports them absent
//! - [`tags`]: the exact provider tag set that marks a resource as managed
//! - [`capacity`]: global capacity permits that stay counted until absence is proven
//! - [`sweep`]: which tagged resources a sweep deletes and which permits it releases
//! - [`alarm`]: when a Durable Object alarm fires next and how retries back off

pub mod alarm;
pub mod capacity;
pub mod deletes;
pub mod holding;
pub mod ids;
pub mod lease;
pub mod sweep;
pub mod tags;
pub mod time;
