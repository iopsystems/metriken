//! Embedded-DuckDB implementation of `metriken_query::SqlBackend`.
//!
//! Each `run` call opens a fresh in-memory connection, registers the UDFs
//! and macros, executes the SQL twin (with `{fixture_path}` substituted
//! with the configured data source), and projects the rows into a
//! `QueryResult::Matrix`.
//!
//! **Positional SQL convention:**
//! - Column 0 — `t`, DOUBLE seconds.
//! - Columns 1..1+`value_columns.len()` — DOUBLE values (column 1 is used
//!   today; multi-value support arrives with multi-quantile queries).
//! - Remaining columns — series-defining label values, one per
//!   `label_columns` entry, in the order declared.
//!
//! Positional rather than name-based access avoids duckdb-rs's habit of
//! panicking if column metadata is read before the statement executes.

use std::collections::{BTreeMap, HashMap};

use duckdb::Connection;
use metriken_query::{
    CatalogueEntry, MatrixSample, QueryResult, SqlBackend as TraitSqlBackend, SqlError,
};

/// Default DuckDB-backed implementation of `SqlBackend`. Stateless — every
/// query opens its own in-memory connection. This is fine because reading
/// from a Parquet file via `read_parquet(...)` is the same regardless of
/// connection state.
pub struct DuckDbBackend;

impl DuckDbBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DuckDbBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TraitSqlBackend for DuckDbBackend {
    fn run(
        &self,
        entry: &CatalogueEntry,
        data_source: &str,
        _start: f64,
        _end: f64,
        _step: f64,
    ) -> Result<QueryResult, SqlError> {
        let template = entry
            .sql
            .as_ref()
            .ok_or_else(|| SqlError::Backend(format!("entry {} has no SQL twin", entry.id)))?;
        let sql = template.replace("{fixture_path}", data_source);

        let conn = Connection::open_in_memory()
            .map_err(|e| SqlError::Backend(format!("open duckdb: {e}")))?;
        crate::register_all(&conn)
            .map_err(|e| SqlError::Backend(format!("register UDFs/macros: {e}")))?;
        crate::views::ensure_views(&conn, data_source)
            .map_err(|e| SqlError::Backend(format!("create metric views: {e}")))?;

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| SqlError::Backend(format!("prepare {}: {e}", entry.id)))?;

        let n_values = entry.value_columns.len().max(1);
        let label_names = entry.label_columns.clone();
        let label_offset = 1 + n_values;

        let rows = stmt
            .query_map([], |row| {
                let t: Option<f64> = row.get(0)?;
                let v: Option<f64> = row.get(1)?;
                let labels: Vec<(String, String)> = label_names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        let val: Option<String> = row.get(label_offset + i).ok().flatten();
                        (name.clone(), val.unwrap_or_default())
                    })
                    .collect();
                Ok((labels, t, v))
            })
            .map_err(|e| SqlError::Backend(format!("query {}: {e}", entry.id)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| SqlError::Backend(format!("collect rows for {}: {e}", entry.id)))?;

        let mut series: BTreeMap<Vec<(String, String)>, Vec<(f64, f64)>> = BTreeMap::new();
        for (labels, t, v) in rows {
            let (Some(t), Some(v)) = (t, v) else { continue };
            series.entry(labels).or_default().push((t, v));
        }

        let result: Vec<MatrixSample> = series
            .into_iter()
            .map(|(labels, values)| {
                let mut metric: HashMap<String, String> = HashMap::new();
                for (k, v) in &entry.output_metric {
                    metric.insert(k.clone(), v.clone());
                }
                for (k, v) in labels {
                    metric.insert(k, v);
                }
                MatrixSample { metric, values }
            })
            .collect();

        Ok(QueryResult::Matrix { result })
    }
}
