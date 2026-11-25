//! A run object's records in its SQLite storage, next to the core's holdings table

use cloudlease_core::ids::Generation;
use cloudlease_machines::{
    ids::RequestId,
    run::{MachineRecord, RunRecord},
};
use cloudlease_worker::{
    error::{CloudleaseError, CloudleaseResult},
    store::add_missing_column,
};
use serde::{Deserialize, de::DeserializeOwned};
use worker::{SqlStorage, SqlStorageValue};

const CREATE_RUN: &str = "
    CREATE TABLE IF NOT EXISTS machines_run (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        record TEXT NOT NULL,
        last_generation INTEGER NOT NULL DEFAULT 0
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
        // a run stored before the counter took its next generation after the highest
        // recorded one, so the counter starts there
        if add_missing_column(
            &sql,
            "machines_run",
            "last_generation",
            "INTEGER NOT NULL DEFAULT 0",
        )? {
            sql.exec(
                "UPDATE machines_run SET last_generation = \
                 (SELECT COALESCE(MAX(generation), 0) FROM machines_requests)",
                None,
            )?;
        }

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

    /// Takes the next generation from the run's counter, which only goes up
    ///
    /// A pending record that expires or is refused is deleted, so the highest recorded
    /// generation can go down; the counter never hands its generation out again, so a gate
    /// reply still in flight for it cannot land on a later request.
    pub fn next_generation(&self) -> CloudleaseResult<Generation> {
        #[derive(Deserialize)]
        struct Next {
            last_generation: i64,
        }

        let next = self
            .0
            .exec(
                "UPDATE machines_run SET last_generation = last_generation + 1 WHERE id = 1 \
                 RETURNING last_generation",
                None,
            )?
            .one::<Next>()?;

        u64::try_from(next.last_generation)
            .ok()
            .and_then(|value| Generation::new(value).ok())
            .ok_or_else(|| CloudleaseError::Storage("the generation counter is invalid".into()))
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
