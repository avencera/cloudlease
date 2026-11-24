//! A run object's records in its SQLite storage, next to the core's holdings table

use cloudlease_core::ids::Generation;
use cloudlease_machines::{
    ids::RequestId,
    run::{MachineRecord, RunRecord},
};
use cloudlease_worker::error::{CloudleaseError, CloudleaseResult};
use serde::{Deserialize, de::DeserializeOwned};
use worker::{SqlStorage, SqlStorageValue};

const CREATE_RUN: &str = "
    CREATE TABLE IF NOT EXISTS machines_run (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        record TEXT NOT NULL
    )
";

const CREATE_REQUESTS: &str = "
    CREATE TABLE IF NOT EXISTS machines_requests (
        generation INTEGER PRIMARY KEY,
        request_id TEXT NOT NULL UNIQUE,
        record TEXT NOT NULL
    )
";

#[derive(Deserialize)]
struct Row {
    record: String,
}

/// The run record and its machine records
pub struct RunStore(SqlStorage);

impl RunStore {
    /// Opens the store and creates its tables if needed
    pub fn open(sql: SqlStorage) -> CloudleaseResult<Self> {
        sql.exec(CREATE_RUN, None)?;
        sql.exec(CREATE_REQUESTS, None)?;

        Ok(Self(sql))
    }

    /// Reads the run, which exists once it was created
    pub fn run(&self) -> CloudleaseResult<Option<RunRecord>> {
        self.first("SELECT record FROM machines_run WHERE id = 1", None)
    }

    /// Writes the run
    pub fn put_run(&self, run: &RunRecord) -> CloudleaseResult<()> {
        self.0.exec(
            "INSERT INTO machines_run (id, record) VALUES (1, ?) \
             ON CONFLICT(id) DO UPDATE SET record = excluded.record",
            vec![SqlStorageValue::from(serde_json::to_string(run)?)],
        )?;

        Ok(())
    }

    /// Reads every machine record, oldest generation first
    pub fn records(&self) -> CloudleaseResult<Vec<MachineRecord>> {
        self.0
            .exec(
                "SELECT record FROM machines_requests ORDER BY generation",
                None,
            )?
            .to_array::<Row>()?
            .iter()
            .map(|row| decode(&row.record))
            .collect()
    }

    /// Reads the record of one generation
    pub fn record(&self, generation: Generation) -> CloudleaseResult<Option<MachineRecord>> {
        self.first(
            "SELECT record FROM machines_requests WHERE generation = ?",
            Some(vec![integer(generation.get())?]),
        )
    }

    /// Reads the record a request ID created
    pub fn by_request(&self, request_id: RequestId) -> CloudleaseResult<Option<MachineRecord>> {
        self.first(
            "SELECT record FROM machines_requests WHERE request_id = ?",
            Some(vec![SqlStorageValue::from(request_id.to_string())]),
        )
    }

    /// Writes a machine record
    pub fn put_record(&self, record: &MachineRecord) -> CloudleaseResult<()> {
        self.0.exec(
            "INSERT INTO machines_requests (generation, request_id, record) VALUES (?, ?, ?) \
             ON CONFLICT(generation) DO UPDATE SET record = excluded.record",
            vec![
                integer(record.generation.get())?,
                SqlStorageValue::from(record.request_id.to_string()),
                SqlStorageValue::from(serde_json::to_string(record)?),
            ],
        )?;

        Ok(())
    }

    /// Deletes a record that was never admitted, freeing its request ID
    pub fn delete_record(&self, generation: Generation) -> CloudleaseResult<()> {
        self.0.exec(
            "DELETE FROM machines_requests WHERE generation = ?",
            vec![integer(generation.get())?],
        )?;

        Ok(())
    }

    /// Returns the generation after every recorded one
    ///
    /// Only a record that was never admitted is ever deleted, and nothing was created for
    /// it, so reusing its generation cannot be confused with an earlier machine.
    pub fn next_generation(&self) -> CloudleaseResult<Generation> {
        #[derive(Deserialize)]
        struct Last {
            generation: Option<i64>,
        }

        let last = self
            .0
            .exec(
                "SELECT MAX(generation) AS generation FROM machines_requests",
                None,
            )?
            .one::<Last>()?;
        let Some(last) = last.generation else {
            return Ok(Generation::FIRST);
        };

        u64::try_from(last)
            .ok()
            .and_then(|value| Generation::new(value).ok())
            .and_then(Generation::next)
            .ok_or_else(|| CloudleaseError::Storage("the generation counter is exhausted".into()))
    }

    fn first<T: DeserializeOwned>(
        &self,
        query: &str,
        bindings: Option<Vec<SqlStorageValue>>,
    ) -> CloudleaseResult<Option<T>> {
        self.0
            .exec(query, bindings)?
            .to_array::<Row>()?
            .first()
            .map(|row| decode(&row.record))
            .transpose()
    }
}

fn decode<T: DeserializeOwned>(json: &str) -> CloudleaseResult<T> {
    Ok(serde_json::from_str(json)?)
}

fn integer(value: u64) -> CloudleaseResult<SqlStorageValue> {
    let value = i64::try_from(value)
        .map_err(|_| CloudleaseError::Storage("an integer exceeds the SQLite range".into()))?;

    Ok(SqlStorageValue::try_from_i64(value)?)
}
