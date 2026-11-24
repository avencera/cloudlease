//! The public routes: authenticate, parse the body into a typed action, and send it to the
//! run object that owns it
//!
//! | Route | Credential |
//! | --- | --- |
//! | `POST /v1/runs` | client |
//! | `GET /v1/runs/{run_id}` | client |
//! | `POST /v1/runs/{run_id}/release` | client |
//! | `POST /v1/runs/{run_id}/machines` | client |
//! | `GET /v1/leases/{lease_id}` | client or that lease's token |
//! | `POST /v1/leases/{lease_id}/release` | client or that lease's token |
//! | `POST /v1/leases/{lease_id}/renew` | that lease's token |
//! | `GET /v1/health` | none; says only whether each setting works |
//!
//! A bearer that is no client token is passed to the run object as a hash, which compares it
//! with the lease's stored hash; only the run object knows lease tokens.

use cloudlease_machines::{
    ids::{MachineId, RunId},
    provider::ProviderName,
    token::TokenHash,
    wire::{ApiError, Caller, CreateRunRequest, ErrorCode, RenewRequest, RunAction, RunCall},
};
use cloudlease_worker::http::{json_response, read_json};
use serde::de::DeserializeOwned;
use worker::{Env, Headers, Method, Request, RequestInit, Response};

use crate::{
    NTFY_URL, RUNS,
    app::settings::{
        client_tokens, config, digitalocean, error_response, internal, latitude, phoenixnap, secret,
    },
};

/// The path a run object serves calls on
pub const CALL_PATH: &str = "/machines/call";

/// Serves one public request
pub async fn serve(mut request: Request, env: &Env) -> worker::Result<Response> {
    match route(&mut request, env).await {
        Ok(response) => Ok(response),
        Err(error) => error_response(&error),
    }
}

async fn route(request: &mut Request, env: &Env) -> Result<Response, ApiError> {
    let method = request.method();
    let path = request.path();
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    if let (Method::Get, ["v1", "health"]) = (&method, segments.as_slice()) {
        return health(env);
    }

    let caller = authenticate(request, env)?;
    let call = match (method, segments.as_slice()) {
        (Method::Post, ["v1", "runs"]) => {
            let Caller::Client(app) = &caller else {
                return Err(unauthorized());
            };
            let body: CreateRunRequest = body(request).await?;
            RunCall {
                run: RunId::derive(app, body.request_id),
                caller,
                action: RunAction::Create(body),
            }
        }
        (Method::Get, ["v1", "runs", run]) => run_call(caller, run, RunAction::Read)?,
        (Method::Post, ["v1", "runs", run, "release"]) => {
            let action = RunAction::Release(body(request).await?);
            run_call(caller, run, action)?
        }
        (Method::Post, ["v1", "runs", run, "machines"]) => {
            let action = RunAction::CreateMachine(body(request).await?);
            run_call(caller, run, action)?
        }
        (Method::Get, ["v1", "leases", lease]) => {
            let machine = machine_id(lease)?;
            lease_call(
                caller,
                machine.run,
                RunAction::ReadLease(machine.generation),
            )
        }
        (Method::Post, ["v1", "leases", lease, "release"]) => {
            let machine = machine_id(lease)?;
            let action = RunAction::ReleaseLease(machine.generation, body(request).await?);
            lease_call(caller, machine.run, action)
        }
        (Method::Post, ["v1", "leases", lease, "renew"]) => {
            let machine = machine_id(lease)?;
            let _: RenewRequest = body(request).await?;
            lease_call(
                caller,
                machine.run,
                RunAction::RenewLease(machine.generation),
            )
        }
        _ => return Err(ApiError::new(ErrorCode::NotFound, "no such route")),
    };

    forward(env, &call).await
}

fn authenticate(request: &Request, env: &Env) -> Result<Caller, ApiError> {
    let header = request
        .headers()
        .get("authorization")
        .map_err(internal)?
        .ok_or_else(unauthorized)?;
    let token = header
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(unauthorized)?;

    let tokens = client_tokens(env)?;
    Ok(match tokens.app_for(token) {
        Some(app) => Caller::Client(app.clone()),
        None => Caller::Lease(TokenHash::of(token)),
    })
}

fn unauthorized() -> ApiError {
    ApiError::new(ErrorCode::Unauthorized, "a valid bearer token is required")
}

fn run_call(caller: Caller, run: &str, action: RunAction) -> Result<RunCall, ApiError> {
    if !matches!(caller, Caller::Client(_)) {
        return Err(unauthorized());
    }
    // an ID that cannot name a run names no run of this app
    let run =
        RunId::parse(run).map_err(|_| ApiError::new(ErrorCode::RunNotFound, "no such run"))?;

    Ok(RunCall {
        caller,
        run,
        action,
    })
}

fn lease_call(caller: Caller, run: RunId, action: RunAction) -> RunCall {
    RunCall {
        caller,
        run,
        action,
    }
}

fn machine_id(lease: &str) -> Result<MachineId, ApiError> {
    lease
        .parse()
        .map_err(|_| ApiError::new(ErrorCode::LeaseNotFound, "no such lease"))
}

async fn body<T: DeserializeOwned>(request: &mut Request) -> Result<T, ApiError> {
    read_json(request)
        .await
        .map_err(|error| ApiError::new(ErrorCode::InvalidRequest, error.message))
}

async fn forward(env: &Env, call: &RunCall) -> Result<Response, ApiError> {
    let stub = env
        .durable_object(RUNS)
        .and_then(|namespace| namespace.get_by_name(call.run.as_str()))
        .map_err(internal)?;
    let headers = Headers::new();
    headers
        .set("content-type", "application/json")
        .map_err(internal)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(serde_json::to_string(call).map_err(internal)?.into()));
    let inner =
        Request::new_with_init(&format!("https://run{CALL_PATH}"), &init).map_err(internal)?;

    stub.fetch_with_request(inner).await.map_err(internal)
}

fn health(env: &Env) -> Result<Response, ApiError> {
    // the route needs no credential, so it says only whether each setting works and logs why
    // one does not
    let check = |name: &str, result: Result<(), String>| {
        if let Err(error) = &result {
            worker::console_error!("health: {name}: {error}");
        }
        result.is_ok()
    };
    let parsed = config(env);
    let needs = |provider: ProviderName| {
        parsed
            .as_ref()
            .is_ok_and(|config| config.providers().contains(&provider))
    };
    // null when no setting is present and no class needs the provider
    let optional =
        |name: &str, built: Result<bool, String>, needed: bool, missing: &str| match built {
            Ok(true) => Some(true),
            Ok(false) if !needed => None,
            Ok(false) => Some(check(name, Err(missing.to_owned()))),
            Err(error) => Some(check(name, Err(error))),
        };
    let phoenixnap = optional(
        "phoenixnap",
        phoenixnap(env)
            .map(|built| built.is_some())
            .map_err(|e| e.to_string()),
        needs(ProviderName::PhoenixNap),
        "a class uses phoenixNAP, but PNAP_CLIENT_ID and PNAP_CLIENT_SECRET are not set",
    );
    let latitude = optional(
        "latitude",
        latitude(env)
            .map(|built| built.is_some())
            .map_err(|e| e.to_string()),
        needs(ProviderName::Latitude),
        "a class uses Latitude.sh, but LATITUDE_API_KEY and LATITUDE_PROJECT are not set",
    );
    let config = check("config", parsed.map(drop).map_err(|e| e.message));
    let tokens = check(
        "client_tokens",
        client_tokens(env).map(drop).map_err(|e| e.message),
    );
    let digitalocean = check(
        "digitalocean",
        digitalocean(env).map(drop).map_err(|e| e.to_string()),
    );
    let ntfy = secret(env, NTFY_URL).is_some();
    // ntfy is reported but not required: without it alerts still reach the log
    let ok =
        config && tokens && digitalocean && phoenixnap != Some(false) && latitude != Some(false);
    let checks = serde_json::json!({
        "ok": ok,
        "config": config,
        "client_tokens": tokens,
        "digitalocean": digitalocean,
        "phoenixnap": phoenixnap,
        "latitude": latitude,
        "ntfy": ntfy,
    });

    json_response(if ok { 200 } else { 503 }, &checks).map_err(internal)
}
