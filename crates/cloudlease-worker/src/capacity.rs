//! The capacity gate Durable Object logic and a client for it
//!
//! One gate object per app holds every permit. An owner acquires a permit before it creates
//! resources and releases it only with an [`AbsenceProof`](cloudlease_core::capacity::AbsenceProof).
//! The gate's alarm turns expired permits suspect; suspect permits keep counting until the
//! owner or the sweep proves absence.
//!
//! The app declares the Durable Object class and forwards to [`CapacityGate`]:
//!
//! ```ignore
//! #[durable_object(alarm)]
//! pub struct Capacity(CapacityGate);
//!
//! impl DurableObject for Capacity {
//!     fn new(state: State, env: Env) -> Self {
//!         Self(CapacityGate::new(state, env, CONFIG))
//!     }
//!     async fn fetch(&self, request: Request) -> Result<Response> {
//!         self.0.fetch(request).await
//!     }
//!     async fn alarm(&self) -> Result<Response> {
//!         self.0.alarm().await
//!     }
//! }
//! ```

use std::time::Duration;

use cloudlease_core::{
    alarm::Wake,
    capacity::{
        AcquireDecision, AcquireRequest, CapacityLimit, Permit, PermitStatus, PermitTtl,
        PermitUnits, ReleaseRequest, decide_acquire,
    },
    ids::{Generation, OwnerId, PermitId},
    time::UnixMillis,
};
use serde::{Deserialize, Serialize};
use worker::{Env, Method, Request, Response, SqlStorage, SqlStorageValue, State, Stub};

use crate::{
    alarm::rearm,
    error::{CloudleaseError, CloudleaseResult},
    http::{HttpError, call_json, json_response, read_json},
    runtime::now,
};

/// Takes a permit: `POST` an [`AcquireRequest`]
pub const ACQUIRE_PATH: &str = "/cloudlease/capacity/acquire";
/// Returns a permit: `POST` a [`ReleaseRequest`]
pub const RELEASE_PATH: &str = "/cloudlease/capacity/release";
/// Reads every permit: `GET`
pub const SNAPSHOT_PATH: &str = "/cloudlease/capacity/snapshot";
/// Overrides the limit: `PUT { "limit": n }`, or `{ "limit": null }` to clear the override
pub const LIMIT_PATH: &str = "/cloudlease/capacity/limit";

const CREATE_PERMITS: &str = "
    CREATE TABLE IF NOT EXISTS cloudlease_permits (
        permit_id TEXT PRIMARY KEY,
        owner_id TEXT NOT NULL,
        generation INTEGER NOT NULL,
        units INTEGER NOT NULL DEFAULT 1 CHECK (units > 0),
        expires_at INTEGER NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('active', 'suspect')),
        UNIQUE(owner_id, generation)
    )
";

const CREATE_SETTINGS: &str = "
    CREATE TABLE IF NOT EXISTS cloudlease_capacity_settings (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        limit_value INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    )
";

/// Settings for one app's gate
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityConfig {
    /// The Worker variable holding the configured limit
    pub limit_var: &'static str,
    /// The limit when the variable is absent
    pub default_limit: u16,
    /// The largest limit the variable or an override may set
    pub max_limit: u16,
    /// The longest permit lifetime a caller may request
    pub max_ttl: Duration,
}

/// Where the effective limit comes from
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LimitSource {
    /// The Worker variable or the default
    Configured,
    /// A stored override
    Override,
}

/// Every permit and the limit
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// The effective limit
    pub limit: u16,
    /// The limit from configuration alone
    pub configured_limit: u16,
    /// Where the effective limit comes from
    pub limit_source: LimitSource,
    /// Every held permit, earliest expiry first
    pub permits: Vec<Permit>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitRequest {
    limit: Option<u16>,
}

/// The gate logic behind an app's capacity Durable Object
pub struct CapacityGate {
    state: State,
    env: Env,
    config: CapacityConfig,
}

impl CapacityGate {
    /// Creates the gate for one object
    #[must_use]
    pub const fn new(state: State, env: Env, config: CapacityConfig) -> Self {
        Self { state, env, config }
    }

    /// Serves the gate routes
    pub async fn fetch(&self, mut request: Request) -> worker::Result<Response> {
        match self.route(&mut request).await {
            Ok(response) => Ok(response),
            Err(error) => error.into_response(),
        }
    }

    /// Turns expired permits suspect and re-arms for the next expiry
    pub async fn alarm(&self) -> worker::Result<Response> {
        let store = PermitStore::open(self.state.storage().sql())?;
        let now = now();
        store.mark_expired_suspect(now)?;
        self.rearm(&store, now).await?;

        Response::empty()
    }

    async fn route(&self, request: &mut Request) -> Result<Response, HttpError> {
        let store = PermitStore::open(self.state.storage().sql())?;

        match (request.method(), request.path().as_str()) {
            (Method::Post, ACQUIRE_PATH) => self.acquire(&store, read_json(request).await?).await,
            (Method::Post, RELEASE_PATH) => self.release(&store, read_json(request).await?).await,
            (Method::Get, SNAPSHOT_PATH) => Ok(json_response(200, &self.snapshot(&store)?)?),
            (Method::Put, LIMIT_PATH) => {
                let body: LimitRequest = read_json(request).await?;
                match body.limit {
                    Some(limit) => {
                        let limit = CapacityLimit::new(limit, self.config.max_limit)
                            .map_err(|error| HttpError::invalid_request(error.to_string()))?;
                        store.write_override(limit, now())?;
                    }
                    None => store.clear_override()?,
                }

                Ok(json_response(200, &self.snapshot(&store)?)?)
            }
            _ => Err(HttpError::not_found()),
        }
    }

    async fn acquire(
        &self,
        store: &PermitStore,
        request: AcquireRequest,
    ) -> Result<Response, HttpError> {
        let ttl = PermitTtl::new(Duration::from_millis(request.ttl_ms), self.config.max_ttl)
            .map_err(|error| HttpError::invalid_request(error.to_string()))?;
        let existing = store.find(&request.owner_id, request.generation)?;
        let limit = self.limit(store)?;

        match decide_acquire(existing.as_ref(), store.held_units()?, request.units, limit) {
            AcquireDecision::Existing(permit) => Ok(json_response(200, permit)?),
            AcquireDecision::Exhausted => Err(HttpError::new(
                409,
                "capacity_exhausted",
                "every unit of capacity is held",
                true,
            )),
            AcquireDecision::Grant => {
                let now = now();
                let permit_id = PermitId::parse(format!("cap_{}", uuid::Uuid::new_v4()))
                    .map_err(CloudleaseError::from)?;
                let permit = Permit::grant(permit_id, request.key(), request.units, now, ttl)
                    .map_err(CloudleaseError::from)?;
                store.insert(&permit)?;
                self.rearm(store, now).await?;

                Ok(json_response(201, &permit)?)
            }
        }
    }

    async fn release(
        &self,
        store: &PermitStore,
        request: ReleaseRequest,
    ) -> Result<Response, HttpError> {
        // the full identity must match, so a stale caller cannot release a newer permit
        store.delete(&request.permit_id, &request.owner_id, request.generation)?;
        self.rearm(store, now()).await?;

        Ok(json_response(
            200,
            &serde_json::json!({ "released": true }),
        )?)
    }

    fn snapshot(&self, store: &PermitStore) -> CloudleaseResult<Snapshot> {
        let configured = self.configured_limit()?;
        let stored = store.read_override(self.config.max_limit)?;

        Ok(Snapshot {
            limit: stored.unwrap_or(configured).get(),
            configured_limit: configured.get(),
            limit_source: if stored.is_some() {
                LimitSource::Override
            } else {
                LimitSource::Configured
            },
            permits: store.list()?,
        })
    }

    fn limit(&self, store: &PermitStore) -> CloudleaseResult<CapacityLimit> {
        match store.read_override(self.config.max_limit)? {
            Some(limit) => Ok(limit),
            None => self.configured_limit(),
        }
    }

    fn configured_limit(&self) -> CloudleaseResult<CapacityLimit> {
        let default = CapacityLimit::new(self.config.default_limit, self.config.max_limit)?;
        let configured = self
            .env
            .var(self.config.limit_var)
            .ok()
            .map(|value| value.to_string());

        Ok(CapacityLimit::parse_configured(
            configured.as_deref(),
            default,
            self.config.max_limit,
        )?)
    }

    async fn rearm(&self, store: &PermitStore, now: UnixMillis) -> CloudleaseResult<()> {
        let next = store.next_active_expiry()?;

        rearm(&self.state.storage(), next.map(Wake::At), now).await
    }
}

struct PermitStore(SqlStorage);

#[derive(Deserialize)]
struct PermitRow {
    permit_id: String,
    owner_id: String,
    generation: i64,
    units: i64,
    expires_at: i64,
    status: PermitStatus,
}

impl TryFrom<PermitRow> for Permit {
    type Error = CloudleaseError;

    fn try_from(row: PermitRow) -> Result<Self, Self::Error> {
        Ok(Self {
            permit_id: PermitId::parse(row.permit_id)?,
            owner_id: OwnerId::parse(row.owner_id)?,
            generation: Generation::new(from_sql(row.generation)?)?,
            units: u16::try_from(row.units)
                .map_err(|_| CloudleaseError::Storage("invalid permit units".to_owned()))
                .and_then(|units| Ok(PermitUnits::new(units)?))?,
            expires_at: UnixMillis::new(from_sql(row.expires_at)?)
                .ok_or_else(|| CloudleaseError::Storage("invalid expiry".to_owned()))?,
            status: row.status,
        })
    }
}

/// Creates the gate's tables if they do not exist yet
///
/// An app moving permits from its own earlier tables calls this before copying rows into
/// `cloudlease_permits` and `cloudlease_capacity_settings`, so it never repeats the schema.
pub fn ensure_schema(sql: &SqlStorage) -> CloudleaseResult<()> {
    sql.exec(CREATE_PERMITS, None)?;
    sql.exec(CREATE_SETTINGS, None)?;

    Ok(())
}

impl PermitStore {
    fn open(sql: SqlStorage) -> CloudleaseResult<Self> {
        ensure_schema(&sql)?;

        Ok(Self(sql))
    }

    fn find(&self, owner: &OwnerId, generation: Generation) -> CloudleaseResult<Option<Permit>> {
        self.0
            .exec(
                "SELECT permit_id, owner_id, generation, units, expires_at, status \
                 FROM cloudlease_permits WHERE owner_id = ? AND generation = ?",
                vec![
                    SqlStorageValue::from(owner.as_str()),
                    to_sql(generation.get())?,
                ],
            )?
            .to_array::<PermitRow>()?
            .into_iter()
            .next()
            .map(Permit::try_from)
            .transpose()
    }

    fn held_units(&self) -> CloudleaseResult<u64> {
        #[derive(Deserialize)]
        struct Row {
            units: i64,
        }

        // suspect permits count, because their resources may still exist
        let row = self
            .0
            .exec(
                "SELECT COALESCE(SUM(units), 0) AS units FROM cloudlease_permits",
                None,
            )?
            .one::<Row>()?;

        from_sql(row.units)
    }

    fn insert(&self, permit: &Permit) -> CloudleaseResult<()> {
        self.0.exec(
            "INSERT INTO cloudlease_permits \
             (permit_id, owner_id, generation, units, expires_at, status) \
             VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                SqlStorageValue::from(permit.permit_id.as_str()),
                SqlStorageValue::from(permit.owner_id.as_str()),
                to_sql(permit.generation.get())?,
                to_sql(u64::from(permit.units.get()))?,
                to_sql(permit.expires_at.as_millis())?,
                SqlStorageValue::from(permit.status.as_str()),
            ],
        )?;

        Ok(())
    }

    fn delete(
        &self,
        permit_id: &PermitId,
        owner: &OwnerId,
        generation: Generation,
    ) -> CloudleaseResult<()> {
        self.0.exec(
            "DELETE FROM cloudlease_permits \
             WHERE permit_id = ? AND owner_id = ? AND generation = ?",
            vec![
                SqlStorageValue::from(permit_id.as_str()),
                SqlStorageValue::from(owner.as_str()),
                to_sql(generation.get())?,
            ],
        )?;

        Ok(())
    }

    fn mark_expired_suspect(&self, now: UnixMillis) -> CloudleaseResult<()> {
        self.0.exec(
            "UPDATE cloudlease_permits SET status = 'suspect' \
             WHERE status = 'active' AND expires_at <= ?",
            vec![to_sql(now.as_millis())?],
        )?;

        Ok(())
    }

    fn next_active_expiry(&self) -> CloudleaseResult<Option<UnixMillis>> {
        #[derive(Deserialize)]
        struct Row {
            expires_at: Option<i64>,
        }

        let row = self
            .0
            .exec(
                "SELECT MIN(expires_at) AS expires_at FROM cloudlease_permits \
                 WHERE status = 'active'",
                None,
            )?
            .one::<Row>()?;

        row.expires_at
            .map(|value| {
                UnixMillis::new(from_sql(value)?)
                    .ok_or_else(|| CloudleaseError::Storage("invalid expiry".to_owned()))
            })
            .transpose()
    }

    fn list(&self) -> CloudleaseResult<Vec<Permit>> {
        self.0
            .exec(
                "SELECT permit_id, owner_id, generation, units, expires_at, status \
                 FROM cloudlease_permits ORDER BY expires_at",
                None,
            )?
            .to_array::<PermitRow>()?
            .into_iter()
            .map(Permit::try_from)
            .collect()
    }

    fn read_override(&self, max: u16) -> CloudleaseResult<Option<CapacityLimit>> {
        #[derive(Deserialize)]
        struct Row {
            limit_value: i64,
        }

        let row = self
            .0
            .exec(
                "SELECT limit_value FROM cloudlease_capacity_settings WHERE id = 1",
                None,
            )?
            .to_array::<Row>()?
            .into_iter()
            .next();

        // a stored value passes the same ceiling as configuration, so an edited row cannot
        // widen it
        row.map(|row| {
            let value = u16::try_from(row.limit_value)
                .map_err(|_| CloudleaseError::Storage("invalid stored limit".to_owned()))?;

            Ok(CapacityLimit::new(value, max)?)
        })
        .transpose()
    }

    fn write_override(&self, limit: CapacityLimit, now: UnixMillis) -> CloudleaseResult<()> {
        self.0.exec(
            "INSERT INTO cloudlease_capacity_settings (id, limit_value, updated_at) \
             VALUES (1, ?, ?) ON CONFLICT(id) DO UPDATE SET \
             limit_value = excluded.limit_value, updated_at = excluded.updated_at",
            vec![to_sql(u64::from(limit.get()))?, to_sql(now.as_millis())?],
        )?;

        Ok(())
    }

    fn clear_override(&self) -> CloudleaseResult<()> {
        self.0.exec(
            "DELETE FROM cloudlease_capacity_settings WHERE id = 1",
            None,
        )?;

        Ok(())
    }
}

fn to_sql(value: u64) -> CloudleaseResult<SqlStorageValue> {
    let value = i64::try_from(value)
        .map_err(|_| CloudleaseError::Storage("an integer exceeds the SQLite range".to_owned()))?;

    Ok(SqlStorageValue::try_from_i64(value)?)
}

fn from_sql(value: i64) -> CloudleaseResult<u64> {
    u64::try_from(value).map_err(|_| CloudleaseError::Storage("a negative integer".to_owned()))
}

/// The answer to an acquire request
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Acquired {
    /// The permit, new or already held for this owner and generation
    Granted(Permit),
    /// Every unit of capacity is held
    Exhausted,
}

/// Calls an app's capacity gate object
pub struct CapacityClient(Stub);

impl CapacityClient {
    /// Connects to the gate object `name` in the namespace bound as `binding`
    pub fn connect(env: &Env, binding: &str, name: &str) -> CloudleaseResult<Self> {
        Ok(Self(env.durable_object(binding)?.get_by_name(name)?))
    }

    /// Takes a permit
    pub async fn acquire(&self, request: &AcquireRequest) -> CloudleaseResult<Acquired> {
        match call_json(
            &self.0,
            "capacity gate",
            Method::Post,
            &url(ACQUIRE_PATH),
            Some(request),
        )
        .await
        {
            Ok(permit) => Ok(Acquired::Granted(permit)),
            Err(CloudleaseError::Remote { status: 409, .. }) => Ok(Acquired::Exhausted),
            Err(error) => Err(error),
        }
    }

    /// Returns a permit whose resources are confirmed gone
    pub async fn release(&self, request: &ReleaseRequest) -> CloudleaseResult<()> {
        let _: serde_json::Value = call_json(
            &self.0,
            "capacity gate",
            Method::Post,
            &url(RELEASE_PATH),
            Some(request),
        )
        .await?;

        Ok(())
    }

    /// Sets the stored limit override, or clears it with `None`, and returns the new snapshot
    ///
    /// The gate refuses a limit outside `1..=max_limit` with a 400 remote error.
    pub async fn set_limit(&self, limit: Option<u16>) -> CloudleaseResult<Snapshot> {
        call_json(
            &self.0,
            "capacity gate",
            Method::Put,
            &url(LIMIT_PATH),
            Some(&serde_json::json!({ "limit": limit })),
        )
        .await
    }

    /// Reads every permit
    pub async fn snapshot(&self) -> CloudleaseResult<Snapshot> {
        call_json::<(), _>(
            &self.0,
            "capacity gate",
            Method::Get,
            &url(SNAPSHOT_PATH),
            None,
        )
        .await
    }
}

fn url(path: &str) -> String {
    format!("https://capacity{path}")
}
