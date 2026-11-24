//! Reading the configuration and secrets, and answering errors

use std::fmt::Display;

use cloudlease_machines::{
    config::MachinesConfig,
    token::ClientTokens,
    wire::{ApiError, ErrorCode},
};
use cloudlease_worker::{
    digitalocean::{self, WorkersDigitalOcean},
    http::{HttpError, json_response},
};
use serde::Serialize;
use worker::{Env, Response};

use crate::{CLIENT_TOKENS, CONFIG_VAR, DIGITALOCEAN};

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
pub fn provider(env: &Env) -> cloudlease_worker::error::CloudleaseResult<WorkersDigitalOcean> {
    digitalocean::from_env(env, DIGITALOCEAN)
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
