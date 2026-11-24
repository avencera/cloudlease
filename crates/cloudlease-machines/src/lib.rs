//! Rules of the cloudlease machines app: temporary machines for batch jobs, each one a
//! lease that Cloudflare ends even when no agent is running
//!
//! A client app (homebased) opens a **run** with a machine class, a size limit and a
//! deadline, then asks for **machines**. Each machine is one generation of the run's owner
//! object, so the core's holdings, tags, sweep and capacity rules apply unchanged. This crate
//! holds everything that needs no Worker runtime:
//!
//! - [`alert`]: what the Worker alerts on, and how often
//! - [`config`]: classes, providers, prices, caps and the lease policy
//! - [`money`]: prices, billing, accrued spend and when a run reaches its cap
//! - [`provider`]: the providers a class can name, behind one cloud provider
//! - [`ids`]: run and lease identifiers that route themselves to their run object
//! - [`token`]: client and lease tokens, stored only as hashes
//! - [`boot`]: validating the client's boot data and rendering the droplet's user data
//! - [`run`]: run and machine records, admission, release reasons and the wire views
//! - [`wire`]: request and response bodies of the HTTP contract and its error codes
//! - [`time`]: RFC 3339 times on the wire

pub mod alert;
pub mod boot;
pub mod config;
pub mod ids;
pub mod money;
pub mod provider;
pub mod run;
pub mod time;
pub mod token;
pub mod wire;
