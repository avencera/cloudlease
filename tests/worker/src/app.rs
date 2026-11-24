//! The Worker entry points and Durable Object classes

use std::time::Duration;

use cloudlease_core::{
    alarm::Backoff,
    holding::Holding,
    ids::{Generation, LeaseId, OwnerId},
    lease::{Lease, LeasePolicy},
};
use cloudlease_provider::digitalocean::{
    BootScript, DoKind, DoSpec, DropletImage, DropletSpec, ResourceName, Slug,
};
use cloudlease_worker::{
    capacity::CapacityGate,
    digitalocean::{self, WorkersDigitalOcean},
    error::CloudleaseResult,
    http::{HttpError, json_response, read_json},
    owner::{self, SWEEP_PATH},
    runtime::{WorkersRuntime, now},
    store::HoldingStore,
    sweep::run_sweep,
};
use serde::Deserialize;
use worker::{
    Context, DurableObject, Env, Method, Request, Response, ScheduleContext, ScheduledEvent, State,
    durable_object, event,
};

use crate::{CAPACITY_CONFIG, DIGITALOCEAN, SWEEP};

fn provider(env: &Env) -> CloudleaseResult<WorkersDigitalOcean> {
    digitalocean::from_env(env, DIGITALOCEAN)
}

async fn sweep(env: &Env) -> worker::Result<Response> {
    let provider = provider(env)?;
    let report = run_sweep(env, &provider, &WorkersRuntime, SWEEP, now()).await?;

    json_response(200, &report)
}

/// Runs the sweep on demand so the suite can read its report
#[event(fetch)]
pub async fn fetch(request: Request, env: Env, _context: Context) -> worker::Result<Response> {
    if request.method() == Method::Post && request.path() == "/test/sweep" {
        return sweep(&env).await;
    }

    HttpError::not_found().into_response()
}

/// Runs the tag sweep on the cron trigger
#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, env: Env, _context: ScheduleContext) {
    if let Err(error) = sweep(&env).await {
        worker::console_error!("cloudlease sweep failed: {error}");
    }
}

/// The capacity gate
#[durable_object(alarm)]
pub struct Capacity(CapacityGate);

impl DurableObject for Capacity {
    fn new(state: State, env: Env) -> Self {
        Self(CapacityGate::new(state, env, CAPACITY_CONFIG))
    }

    async fn fetch(&self, request: Request) -> worker::Result<Response> {
        self.0.fetch(request).await
    }

    async fn alarm(&self) -> worker::Result<Response> {
        self.0.alarm().await
    }
}

#[derive(Deserialize)]
struct CreateRequest {
    generation: Generation,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeaseRequest {
    owner_id: OwnerId,
    generation: Generation,
    lease_id: LeaseId,
}

/// One owner's holdings
#[durable_object(alarm)]
pub struct Owner {
    state: State,
    env: Env,
}

impl DurableObject for Owner {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut request: Request) -> worker::Result<Response> {
        let store = HoldingStore::open(self.state.storage().sql())?;
        let path = request.path();

        if request.method() == Method::Post && path == SWEEP_PATH {
            return owner::answer_sweep::<DoKind>(&store, &mut request, now()).await;
        }

        let result = match (request.method(), path.rsplit('/').next()) {
            (Method::Get, Some("holdings")) => self.list(&store),
            (Method::Post, Some("holdings")) => self.seed(&store, &mut request).await,
            (Method::Post, Some("lease")) => self.lease(&store, &mut request).await,
            (Method::Post, Some("droplet")) => self.create_droplet(&store, &mut request).await,
            _ => Err(HttpError::not_found()),
        };

        match result {
            Ok(response) => Ok(response),
            Err(error) => error.into_response(),
        }
    }

    async fn alarm(&self) -> worker::Result<Response> {
        let store = HoldingStore::open(self.state.storage().sql())?;
        let provider = provider(&self.env)?;
        let processed = owner::run_alarm::<DoKind, _>(
            &self.state.storage(),
            &store,
            &provider,
            now(),
            &Backoff::default(),
        )
        .await?;
        for error in processed.errors {
            worker::console_warn!("cleanup will retry: {error}");
        }

        Response::empty()
    }
}

impl Owner {
    fn list(&self, store: &HoldingStore) -> Result<Response, HttpError> {
        let holdings = store.list::<DoKind>()?;

        Ok(json_response(200, &holdings)?)
    }

    /// Stores a holding as given, so a test can start from any state
    ///
    /// It arms no alarm: a seeded holding is often already due, and an alarm armed here could
    /// fire before the test has added its fake resources, so tests arm alarms themselves
    async fn seed(
        &self,
        store: &HoldingStore,
        request: &mut Request,
    ) -> Result<Response, HttpError> {
        let holding: Holding<DoKind> = read_json(request).await?;
        store.put(&holding, now())?;

        Ok(json_response(201, &holding)?)
    }

    /// Starts a lease the way an app would
    async fn lease(
        &self,
        store: &HoldingStore,
        request: &mut Request,
    ) -> Result<Response, HttpError> {
        let body: LeaseRequest = read_json(request).await?;
        let policy = LeasePolicy::new(Duration::from_secs(120), Duration::from_secs(3_600))
            .map_err(|error| HttpError::invalid_request(error.to_string()))?;
        let lease = Lease::start(body.lease_id, &policy, now(), None)
            .map_err(|error| HttpError::invalid_request(error.to_string()))?;
        let holding = Holding::<DoKind>::leased(body.owner_id, body.generation, lease);
        owner::save(&self.state.storage(), store, &holding, now()).await?;

        Ok(json_response(201, &holding)?)
    }

    /// Creates a droplet for a leased generation the way an app would
    async fn create_droplet(
        &self,
        store: &HoldingStore,
        request: &mut Request,
    ) -> Result<Response, HttpError> {
        let body: CreateRequest = read_json(request).await?;
        let provider = provider(&self.env)?;
        let spec = droplet_spec().map_err(|error| HttpError::invalid_request(error.to_string()))?;
        let (id, holding) = owner::create::<DoKind, _>(
            &self.state.storage(),
            store,
            &provider,
            body.generation,
            &spec,
            now(),
        )
        .await?;

        Ok(json_response(
            201,
            &serde_json::json!({ "id": id, "holding": holding }),
        )?)
    }
}

fn droplet_spec() -> cloudlease_provider::error::ProviderResult<DoSpec> {
    Ok(DoSpec::Droplet(DropletSpec {
        name: ResourceName::parse("cloudlease-test")?,
        region: Slug::parse("nyc3")?,
        size: Slug::parse("s-1vcpu-1gb")?,
        image: DropletImage::Slug(Slug::parse("ubuntu-24-04-x64")?),
        boot: BootScript::parse("#!/bin/sh\ntrue\n")?,
        ssh_key_ids: Vec::new(),
        volume_ids: Vec::new(),
        vpc_uuid: None,
    }))
}
