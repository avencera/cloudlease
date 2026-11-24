//! Request and response bodies of the HTTP contract and its error codes
//!
//! The contract is the "Temporary machines contract" section of the cloudlease design note;
//! homebased is its only client. Every body is JSON with snake_case fields, times are
//! RFC 3339 in UTC, and every error is `{ "error": { code, message, retryable } }`.
//!
//! [`RunCall`] is internal: the public Worker authenticates a request, parses its body into a
//! typed action and sends it to the run's Durable Object, which trusts it.

use std::num::NonZeroU64;

use cloudlease_core::ids::Generation;
use serde::{Deserialize, Serialize};

use crate::{
    boot::BootPayload,
    ids::{AppName, ClassName, Hostname, MachineId, RequestId, RunId, RunName},
    time::Timestamp,
    token::TokenHash,
};

/// `POST /v1/runs`
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRunRequest {
    /// The client's idempotency key
    pub request_id: RequestId,
    /// A display name, such as the pool's
    pub name: RunName,
    /// The machine class every lease of the run uses
    pub class: ClassName,
    /// The most leases of the run that may be live at once
    pub max_machines: u16,
    /// No lease of the run outlives this
    pub deadline_at: Timestamp,
    /// The most the run may spend, in US cents; at most the config's `run_spend_cap_usd`,
    /// which applies when it is absent
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_cap_cents: Option<NonZeroU64>,
}

/// `POST /v1/runs/{run_id}/machines`
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMachineRequest {
    /// The client's idempotency key, homebased's slot ID
    pub request_id: RequestId,
    /// The machine's hostname and droplet name
    pub hostname: Hostname,
    /// Files to write and commands to run at first boot
    pub boot: BootPayload,
}

/// Why a client releases a run
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunReleaseReason {
    /// The client asked
    Requested,
}

/// `POST /v1/runs/{run_id}/release`
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRunRequest {
    /// Why
    pub reason: RunReleaseReason,
}

/// Why a client or a machine releases a lease
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseReleaseReason {
    /// The client or the machine asked
    Requested,
    /// The machine had no work for its idle limit
    Idle,
    /// The machine's work is done
    Done,
}

/// `POST /v1/leases/{lease_id}/release`
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseLeaseRequest {
    /// Why
    pub reason: LeaseReleaseReason,
}

/// `POST /v1/leases/{lease_id}/renew`, whose body is `{}`
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RenewRequest {}

/// Where a run is in its life
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// It takes new machines
    Open,
    /// It takes no new machines and some of its machines may still exist
    Releasing,
    /// It takes no new machines and every machine of it is confirmed gone
    Released,
}

/// Where a lease is in its life
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// The droplet is being created or has not been recorded, and the machine has not renewed
    Provisioning,
    /// The droplet exists or the machine has renewed
    Running,
    /// The lease ended and its resources are being deleted
    Releasing,
    /// Every resource of the lease is confirmed gone
    Released,
}

/// Why a lease ended
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    /// The client or the machine asked
    Requested,
    /// The machine had no work for its idle limit
    Idle,
    /// The machine's work is done
    Done,
    /// The machine stopped renewing
    HeartbeatLapsed,
    /// The lease reached its hard deadline
    HardDeadline,
    /// The client released the run
    RunReleased,
    /// The run reached its spend cap
    SpendCap,
    /// The provider refused to create the machine
    CreateFailed,
    /// The provider took the machine back while its lease was live, such as an AWS Spot
    /// interruption, or it disappeared some other way
    Interrupted,
}

impl From<LeaseReleaseReason> for ReleaseReason {
    fn from(reason: LeaseReleaseReason) -> Self {
        match reason {
            LeaseReleaseReason::Requested => Self::Requested,
            LeaseReleaseReason::Idle => Self::Idle,
            LeaseReleaseReason::Done => Self::Done,
        }
    }
}

/// Who ended a lease
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleasedBy {
    /// The client app, with its client token
    Client,
    /// The machine, with its lease token
    Machine,
    /// cloudlease: a deadline, a lapsed heartbeat, a cap or a failed create
    Cloudlease,
}

/// A started release
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReleaseView {
    /// Why
    pub reason: ReleaseReason,
    /// Who
    pub by: ReleasedBy,
    /// When
    pub at: Timestamp,
}

/// A lease as the API returns it
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LeaseView {
    /// The lease
    pub lease_id: MachineId,
    /// Its run
    pub run_id: RunId,
    /// The create's idempotency key
    pub request_id: RequestId,
    /// The machine's hostname
    pub hostname: Hostname,
    /// Where it is in its life
    pub state: LeaseState,
    /// When cloudlease created it
    pub created_at: Timestamp,
    /// When it ends whatever happens
    pub hard_deadline_at: Timestamp,
    /// When it ends unless renewed; its last value once ended
    pub heartbeat_deadline_at: Timestamp,
    /// Its release, once started
    pub release: Option<ReleaseView>,
    /// When every resource was confirmed gone
    pub gone_at: Option<Timestamp>,
}

/// A run as the API returns it
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunView {
    /// The run
    pub run_id: RunId,
    /// Its display name
    pub name: RunName,
    /// Its machine class
    pub class: ClassName,
    /// The most live leases at once
    pub max_machines: u16,
    /// No lease outlives this
    pub deadline_at: Timestamp,
    /// Where it is in its life
    pub state: RunState,
    /// When it was created
    pub created_at: Timestamp,
    /// Every lease of the run, oldest first
    pub machines: Vec<LeaseView>,
}

/// The answer to a renew
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct RenewView {
    /// The lease ends at this time unless renewed again
    pub heartbeat_deadline_at: Timestamp,
    /// The lease ends at this time whatever happens
    pub hard_deadline_at: Timestamp,
}

/// A stable error code, with its status and whether a retry may succeed
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The body or a path parameter is malformed
    InvalidRequest,
    /// The boot data breaks the contract's limits
    InvalidBoot,
    /// The run deadline is past the app's maximum lifetime
    DeadlineTooFar,
    /// The app may not use the class
    UnknownClass,
    /// No valid credential
    Unauthorized,
    /// A valid credential that may not make this call
    Forbidden,
    /// No such run for this app
    RunNotFound,
    /// No such lease for this caller
    LeaseNotFound,
    /// No such route
    NotFound,
    /// The run already has `max_machines` live leases
    RunFull,
    /// The request ID was used with a different body
    RequestConflict,
    /// The run takes no new machines
    RunEnded,
    /// The lease is releasing or released
    LeaseEnded,
    /// A create would break the run's or the hourly spend cap
    SpendCapReached,
    /// The same request is still being handled
    Busy,
    /// cloudlease failed
    Internal,
}

impl ErrorCode {
    /// Returns the HTTP status
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::InvalidRequest
            | Self::InvalidBoot
            | Self::DeadlineTooFar
            | Self::UnknownClass => 400,
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::RunNotFound | Self::LeaseNotFound | Self::NotFound => 404,
            Self::RunFull | Self::RequestConflict | Self::RunEnded => 409,
            Self::LeaseEnded => 410,
            Self::SpendCapReached => 429,
            Self::Internal => 500,
            Self::Busy => 503,
        }
    }

    /// Returns the code as it appears on the wire
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidBoot => "invalid_boot",
            Self::DeadlineTooFar => "deadline_too_far",
            Self::UnknownClass => "unknown_class",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::RunNotFound => "run_not_found",
            Self::LeaseNotFound => "lease_not_found",
            Self::NotFound => "not_found",
            Self::RunFull => "run_full",
            Self::RequestConflict => "request_conflict",
            Self::RunEnded => "run_ended",
            Self::LeaseEnded => "lease_ended",
            Self::SpendCapReached => "spend_cap_reached",
            Self::Busy => "busy",
            Self::Internal => "internal",
        }
    }

    /// Reports whether the same call may succeed later
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::Busy | Self::Internal)
    }
}

/// An error answer
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiError {
    /// The code
    pub code: ErrorCode,
    /// A message for operators
    pub message: String,
}

impl ApiError {
    /// Creates an error
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Who is calling, as the public Worker established it
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Caller {
    /// An app's coordinator, by its client token
    Client(AppName),
    /// A bearer that is no client token; the run object checks it against the lease's hash
    Lease(TokenHash),
}

/// What a caller asks a run object to do
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunAction {
    /// Create the run, or read it back on a same-body retry
    Create(CreateRunRequest),
    /// Read the run with every lease
    Read,
    /// Stop taking machines and release every lease
    Release(ReleaseRunRequest),
    /// Create a machine, or read it back on a same-body retry
    CreateMachine(CreateMachineRequest),
    /// Read one lease
    ReadLease(Generation),
    /// Release one lease
    ReleaseLease(Generation, ReleaseLeaseRequest),
    /// Renew one lease
    RenewLease(Generation),
}

/// One authenticated call to a run object
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunCall {
    /// Who is calling
    pub caller: Caller,
    /// The run object being called
    pub run: RunId,
    /// What to do
    pub action: RunAction,
}

#[cfg(test)]
mod tests {
    use super::{CreateRunRequest, ErrorCode, ReleaseLeaseRequest, RenewRequest};

    #[test]
    fn requests_parse_the_contract_bodies_and_nothing_else() {
        let run: CreateRunRequest = serde_json::from_value(serde_json::json!({
            "request_id": "0193a2b4-1c2d-7e3f-8a9b-0c1d2e3f4a5b",
            "name": "afv",
            "class": "do-c16",
            "max_machines": 16,
            "deadline_at": "2026-11-25T02:00:00Z"
        }))
        .unwrap();
        assert_eq!(run.max_machines, 16);
        assert!(serde_json::from_str::<RenewRequest>("{}").is_ok());
        assert!(serde_json::from_str::<RenewRequest>(r#"{"until":"later"}"#).is_err());
        assert!(serde_json::from_str::<ReleaseLeaseRequest>(r#"{"reason":"idle"}"#).is_ok());
        assert!(serde_json::from_str::<ReleaseLeaseRequest>(r#"{"reason":"bored"}"#).is_err());
    }

    #[test]
    fn error_codes_match_the_contract_table() {
        let table = [
            (ErrorCode::InvalidBoot, 400, "invalid_boot"),
            (ErrorCode::Unauthorized, 401, "unauthorized"),
            (ErrorCode::LeaseNotFound, 404, "lease_not_found"),
            (ErrorCode::RunFull, 409, "run_full"),
            (ErrorCode::RunEnded, 409, "run_ended"),
            (ErrorCode::LeaseEnded, 410, "lease_ended"),
            (ErrorCode::SpendCapReached, 429, "spend_cap_reached"),
        ];
        for (code, status, text) in table {
            assert_eq!((code.status(), code.as_str()), (status, text));
            assert_eq!(serde_json::to_value(code).unwrap(), text);
            assert!(!code.retryable());
        }
        assert!(ErrorCode::Busy.retryable());
    }
}
