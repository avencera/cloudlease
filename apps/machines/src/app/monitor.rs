//! The monitor object: consecutive sweep failures, so a failure that keeps happening alerts
//! and a one-off does not
//!
//! The scheduled handler is stateless, so one object keeps the streaks between sweeps.

use std::collections::{BTreeMap, BTreeSet};

use cloudlease_machines::alert::Streaks;
use cloudlease_worker::{
    error::{CloudleaseError, CloudleaseResult},
    http::{HttpError, json_response, read_json},
};
use serde::{Deserialize, Serialize};
use worker::{
    DurableObject, Env, Method, Request, Response, SqlStorage, SqlStorageValue, State,
    durable_object,
};

/// The path a sweep posts its failing keys to
pub const RECORD_PATH: &str = "/machines/monitor/record";

const CREATE: &str = "
    CREATE TABLE IF NOT EXISTS monitor_streaks (
        key TEXT PRIMARY KEY,
        count INTEGER NOT NULL CHECK (count > 0)
    )
";

/// The keys that failed in one sweep
#[derive(Debug, Deserialize, Serialize)]
pub struct Record {
    /// Owner IDs that did not answer, or the sweep key when the sweep failed outright
    pub failing: BTreeSet<String>,
}

/// The keys due an alert after one sweep, with their consecutive failure counts
#[derive(Debug, Deserialize, Serialize)]
pub struct Due {
    /// Key and count pairs
    pub due: Vec<(String, u32)>,
}

#[derive(Deserialize)]
struct Row {
    key: String,
    count: i64,
}

/// The sweep failure streaks
#[durable_object(fetch)]
pub struct Monitor {
    state: State,
}

impl DurableObject for Monitor {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, mut request: Request) -> worker::Result<Response> {
        if request.method() != Method::Post || request.path() != RECORD_PATH {
            return HttpError::not_found().into_response();
        }
        let record: Record = match read_json(&mut request).await {
            Ok(record) => record,
            Err(error) => return error.into_response(),
        };

        let due = self.record(&record.failing)?;
        json_response(200, &Due { due })
    }
}

impl Monitor {
    fn record(&self, failing: &BTreeSet<String>) -> CloudleaseResult<Vec<(String, u32)>> {
        let sql = self.state.storage().sql();
        sql.exec(CREATE, None)?;

        let (next, due) = load(&sql)?.record(failing);
        sql.exec("DELETE FROM monitor_streaks", None)?;
        for (key, count) in next.counts() {
            sql.exec(
                "INSERT INTO monitor_streaks (key, count) VALUES (?, ?)",
                vec![
                    SqlStorageValue::from(key.clone()),
                    SqlStorageValue::from(i64::from(*count)),
                ],
            )?;
        }

        Ok(due)
    }
}

fn load(sql: &SqlStorage) -> CloudleaseResult<Streaks> {
    let counts = sql
        .exec("SELECT key, count FROM monitor_streaks", None)?
        .to_array::<Row>()?
        .into_iter()
        .map(|row| {
            let count = u32::try_from(row.count)
                .map_err(|_| CloudleaseError::Storage(format!("bad streak count {}", row.count)))?;
            Ok((row.key, count))
        })
        .collect::<CloudleaseResult<BTreeMap<_, _>>>()?;

    Ok(Streaks::from_counts(counts))
}
