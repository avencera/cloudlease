//! Holdings in a Durable Object's SQLite storage

use cloudlease_core::{holding::Holding, ids::Generation, tags::ResourceKind, time::UnixMillis};
use serde::{Deserialize, de::DeserializeOwned};
use worker::{SqlStorage, SqlStorageValue};

use crate::error::{CloudleaseError, CloudleaseResult};

const CREATE_TABLE: &str = "
    CREATE TABLE IF NOT EXISTS cloudlease_holdings (
        generation INTEGER PRIMARY KEY,
        holding TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    )
";

#[derive(Deserialize)]
struct HoldingRow {
    holding: String,
}

/// One owner's holdings, keyed by generation
pub struct HoldingStore(SqlStorage);

impl HoldingStore {
    /// Opens the store and creates its table if needed
    pub fn open(sql: SqlStorage) -> CloudleaseResult<Self> {
        sql.exec(CREATE_TABLE, None)?;

        Ok(Self(sql))
    }

    /// Reads one holding
    pub fn get<K>(&self, generation: Generation) -> CloudleaseResult<Option<Holding<K>>>
    where
        K: ResourceKind + DeserializeOwned,
    {
        let rows = self
            .0
            .exec(
                "SELECT holding FROM cloudlease_holdings WHERE generation = ?",
                vec![sql_integer(generation.get())?],
            )?
            .to_array::<HoldingRow>()?;

        rows.into_iter().next().map(|row| decode(&row)).transpose()
    }

    /// Reads every holding, oldest generation first
    pub fn list<K>(&self) -> CloudleaseResult<Vec<Holding<K>>>
    where
        K: ResourceKind + DeserializeOwned,
    {
        self.0
            .exec(
                "SELECT holding FROM cloudlease_holdings ORDER BY generation",
                None,
            )?
            .to_array::<HoldingRow>()?
            .iter()
            .map(decode)
            .collect()
    }

    /// Writes a holding, replacing the one with the same generation
    pub fn put<K: ResourceKind>(
        &self,
        holding: &Holding<K>,
        now: UnixMillis,
    ) -> CloudleaseResult<()> {
        self.0.exec(
            "INSERT INTO cloudlease_holdings (generation, holding, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(generation) DO UPDATE SET holding = excluded.holding, \
             updated_at = excluded.updated_at",
            vec![
                sql_integer(holding.generation().get())?,
                SqlStorageValue::from(serde_json::to_string(holding)?),
                sql_integer(now.as_millis())?,
            ],
        )?;

        Ok(())
    }

    /// Returns the highest stored generation
    pub fn last_generation(&self) -> CloudleaseResult<Option<Generation>> {
        #[derive(Deserialize)]
        struct Row {
            generation: Option<i64>,
        }

        let row = self
            .0
            .exec(
                "SELECT MAX(generation) AS generation FROM cloudlease_holdings",
                None,
            )?
            .one::<Row>()?;

        row.generation
            .map(|value| {
                u64::try_from(value)
                    .ok()
                    .and_then(|value| Generation::new(value).ok())
                    .ok_or_else(|| CloudleaseError::Storage("invalid stored generation".to_owned()))
            })
            .transpose()
    }
}

fn decode<K>(row: &HoldingRow) -> CloudleaseResult<Holding<K>>
where
    K: ResourceKind + DeserializeOwned,
{
    Ok(serde_json::from_str(&row.holding)?)
}

fn sql_integer(value: u64) -> CloudleaseResult<SqlStorageValue> {
    let value = i64::try_from(value)
        .map_err(|_| CloudleaseError::Storage("an integer exceeds the SQLite range".to_owned()))?;

    Ok(SqlStorageValue::try_from_i64(value)?)
}
