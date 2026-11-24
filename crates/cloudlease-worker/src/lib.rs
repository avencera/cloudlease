//! workers-rs glue for cloudlease
//!
//! - [`runtime`]: a Fetch transport and `Date.now()` runtime for provider clients
//! - [`alarm`]: re-arms a Durable Object alarm for its earliest deadline
//! - [`store`]: holdings in a Durable Object's SQLite storage
//! - [`owner`]: the owner side of a lease: processing due holdings and answering the sweep
//! - [`capacity`]: the capacity gate Durable Object logic and a client for it
//! - [`sweep`]: the scheduled tag sweep
//! - [`digitalocean`]: builds the DigitalOcean adapter from Worker bindings
//! - [`phoenixnap`]: builds the phoenixNAP adapter from Worker bindings, when it has them
//!
//! Durable Object classes are declared by the app, which wraps the handlers here, so each
//! app keeps its own class names, bindings and settings.

pub mod alarm;
pub mod capacity;
pub mod digitalocean;
pub mod error;
pub mod http;
pub mod owner;
pub mod phoenixnap;
pub mod runtime;
pub mod store;
pub mod sweep;
