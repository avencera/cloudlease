//! Reading the configuration and secrets, and answering errors

use std::fmt::Display;

use cloudlease_machines::{
    config::MachinesConfig,
    provider::Providers,
    token::ClientTokens,
    wire::{ApiError, ErrorCode},
};
use cloudlease_worker::{
    digitalocean::{self, WorkersDigitalOcean},
    error::CloudleaseResult,
    http::{HttpError, json_response},
    latitude::{self, WorkersLatitude},
    phoenixnap::{self, WorkersPhoenixNap},
};
use serde::Serialize;
use worker::{Env, Response};

use crate::{CLIENT_TOKENS, CONFIG_VAR, DIGITALOCEAN, LATITUDE, PHOENIXNAP};

/// Every provider the Worker has credentials for
pub type MachineProviders = Providers<WorkersDigitalOcean, WorkersPhoenixNap, WorkersLatitude>;

/// Turns any failure into a retryable `500 internal`
pub fn internal(error: impl Display) -> ApiError {
    ApiError::new(ErrorCode::Internal, error.to_string())
}

/// Reads a secret, or a plain variable of the same name as local tests bind them
pub fn secret(env: &Env, name: &str) -> Option<String> {
    env.secret(name)
        .map(|value| value.to_string())
        .or_else(|_| env.var(name).map(|value| value.to_string()))
        .ok()
}

/// Reads and checks `MACHINES_CONFIG`, a JSON object or a string holding one
pub fn config(env: &Env) -> Result<MachinesConfig, ApiError> {
    let value: serde_json::Value = env
        .object_var(CONFIG_VAR)
        .map_err(|_| internal(format!("{CONFIG_VAR} is not set")))?;
    let json = match value {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    };

    MachinesConfig::parse(&json).map_err(internal)
}

/// Reads the client token hashes
pub fn client_tokens(env: &Env) -> Result<ClientTokens, ApiError> {
    let json = secret(env, CLIENT_TOKENS)
        .ok_or_else(|| internal(format!("{CLIENT_TOKENS} is not set")))?;

    ClientTokens::parse(&json).map_err(internal)
}

/// Builds the DigitalOcean adapter
pub fn digitalocean(env: &Env) -> CloudleaseResult<WorkersDigitalOcean> {
    digitalocean::from_env(env, DIGITALOCEAN)
}

/// Builds the phoenixNAP adapter, or `None` when the Worker has no phoenixNAP credentials
pub fn phoenixnap(env: &Env) -> CloudleaseResult<Option<WorkersPhoenixNap>> {
    phoenixnap::from_env(env, PHOENIXNAP)
}

/// Builds the Latitude.sh adapter, or `None` when the Worker has no Latitude.sh key and
/// project
pub fn latitude(env: &Env) -> CloudleaseResult<Option<WorkersLatitude>> {
    latitude::from_env(env, LATITUDE)
}

/// Builds the provider of every class: DigitalOcean always, phoenixNAP and Latitude.sh when
/// configured
///
/// Broken optional settings, such as one secret set without the other, leave that provider
/// out rather than failing, so they never stop the other providers' deletes; health reports
/// them.
pub fn provider(env: &Env) -> CloudleaseResult<MachineProviders> {
    let phoenixnap = phoenixnap(env).unwrap_or_else(|error| {
        worker::console_error!("cloudlease machines: phoenixNAP left out: {error}");
        None
    });
    let latitude = latitude(env).unwrap_or_else(|error| {
        worker::console_error!("cloudlease machines: Latitude.sh left out: {error}");
        None
    });

    Ok(Providers::new(digitalocean(env)?, phoenixnap, latitude))
}

/// Answers with a JSON body
pub fn answer<T: Serialize>(status: u16, body: &T) -> Result<Response, ApiError> {
    json_response(status, body).map_err(internal)
}

/// Answers with the contract's error body
pub fn error_response(error: &ApiError) -> worker::Result<Response> {
    if error.code == ErrorCode::Internal {
        worker::console_error!("cloudlease machines: {}", error.message);
    }

    HttpError::new(
        error.code.status(),
        error.code.as_str(),
        error.message.clone(),
        error.code.retryable(),
    )
    .into_response()
}
