//! The Worker entry points and Durable Object classes

mod alerts;
mod api;
mod monitor;
mod run;
mod settings;
mod store;
mod sweep;

use cloudlease_worker::capacity::CapacityGate;
use worker::{
    Context, DurableObject, Env, Request, Response, ScheduleContext, ScheduledEvent, State,
    durable_object, event,
};

use crate::GATE_CONFIG;

/// Serves the contract
#[event(fetch)]
pub async fn fetch(request: Request, env: Env, _context: Context) -> worker::Result<Response> {
    api::serve(request, &env).await
}

/// Runs the tag sweep on the cron trigger
#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, env: Env, _context: ScheduleContext) {
    sweep::run(&env).await;
}

/// The hourly spend gate
#[durable_object(alarm)]
pub struct Gate(CapacityGate);

impl DurableObject for Gate {
    fn new(state: State, env: Env) -> Self {
        Self(CapacityGate::new(state, env, GATE_CONFIG))
    }

    async fn fetch(&self, request: Request) -> worker::Result<Response> {
        self.0.fetch(request).await
    }

    async fn alarm(&self) -> worker::Result<Response> {
        self.0.alarm().await
    }
}
