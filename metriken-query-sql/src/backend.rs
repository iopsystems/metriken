//! Embedded-DuckDB implementation of `metriken_query::SqlBackend`.
//!
//! Each `run` call opens a fresh in-memory connection, registers the UDFs
//! and macros, executes the SQL twin (with `{fixture_path}` substituted
//! with the configured data source), and projects the rows.
//!
//! Two output shapes, dispatched on `entry.output_shape`:
//!
//! **Matrix** (default) — positional columns:
//! - Column 0 — `t`, DOUBLE seconds.
//! - Columns 1..1+`value_columns.len()` — DOUBLE values (column 1 is used
//!   today; multi-value support arrives with multi-quantile queries).
//! - Remaining columns — series-defining label values, one per
//!   `label_columns` entry, in the order declared.
//!
//! **Heatmap** — positional columns:
//! - Column 0 — `t`, DOUBLE seconds.
//! - Column 1 — `bucket_idx`, INTEGER (H2 bucket index, NOT remapped).
//! - Column 2 — `count`, DOUBLE (non-zero count for this `(t, bucket)`).
//! - Column 3 — `p`, INTEGER (grouping_power; expected constant within a query).
//!
//! Positional rather than name-based access avoids duckdb-rs's habit of
//! panicking if column metadata is read before the statement executes.

use std::collections::{BTreeMap, HashMap};

use duckdb::Connection;
use metriken_query::{
    CatalogueEntry, HistogramHeatmapResult, MatrixSample, OutputShape, QueryResult,
    SqlBackend as TraitSqlBackend, SqlError,
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

        match entry.output_shape {
            OutputShape::Matrix => run_matrix(&conn, entry, &sql),
            OutputShape::Heatmap => run_heatmap(&conn, entry, &sql),
        }
    }
}

fn run_matrix(
    conn: &Connection,
    entry: &CatalogueEntry,
    sql: &str,
) -> Result<QueryResult, SqlError> {
    let mut stmt = conn
        .prepare(sql)
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

/// Project rows shaped `(t DOUBLE, bucket_idx INTEGER, count DOUBLE, p INTEGER)`
/// into a `HistogramHeatmapResult` matching `streaming/histogram.rs:357-560`:
/// timestamps + axis-trimmed bucket_bounds + remapped non-zero data triples.
fn run_heatmap(
    conn: &Connection,
    entry: &CatalogueEntry,
    sql: &str,
) -> Result<QueryResult, SqlError> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| SqlError::Backend(format!("prepare {}: {e}", entry.id)))?;

    let rows = stmt
        .query_map([], |row| {
            let t: f64 = row.get(0)?;
            let bucket_idx: i32 = row.get(1)?;
            let count: f64 = row.get(2)?;
            let p: i32 = row.get(3)?;
            Ok((t, bucket_idx, count, p))
        })
        .map_err(|e| SqlError::Backend(format!("query {}: {e}", entry.id)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| SqlError::Backend(format!("collect rows for {}: {e}", entry.id)))?;

    if rows.is_empty() {
        return Err(SqlError::Backend(format!(
            "no histogram data for {}",
            entry.id
        )));
    }

    // Timestamps: sorted unique values, preserving the order in which rows
    // arrive (the SQL ORDER BY t guarantees ascending).
    let mut timestamps: Vec<f64> = Vec::new();
    let mut t_to_idx: HashMap<u64, usize> = HashMap::new();
    for (t, _, _, _) in &rows {
        let key = t.to_bits();
        if !t_to_idx.contains_key(&key) {
            t_to_idx.insert(key, timestamps.len());
            timestamps.push(*t);
        }
    }

    // H2 bucket index range observed in the data.
    let mut min_bucket_idx: i32 = i32::MAX;
    let mut max_bucket_idx: i32 = i32::MIN;
    for (_, b, _, _) in &rows {
        if *b < min_bucket_idx {
            min_bucket_idx = *b;
        }
        if *b > max_bucket_idx {
            max_bucket_idx = *b;
        }
    }

    // grouping_power should be constant across rows; take it from the first.
    let p = rows[0].3 as u32;

    // Trimmed bucket bounds: contiguous H2 upper bounds for buckets in
    // [min_bucket_idx, max_bucket_idx]. Includes zero-count buckets in the
    // interior so the visualisation has a continuous Y axis (matches
    // `streaming/histogram.rs:554-560`).
    let bucket_bounds: Vec<u64> = (min_bucket_idx as u32..=max_bucket_idx as u32)
        .map(|i| crate::udf::h2_upper(i, p))
        .collect();

    // Data triples: time index, *remapped* bucket index (relative to
    // min_bucket_idx), count.
    let mut data: Vec<(usize, usize, f64)> = Vec::with_capacity(rows.len());
    let mut min_value = f64::MAX;
    let mut max_value = f64::MIN;
    for (t, b, c, _) in rows {
        let time_idx = *t_to_idx
            .get(&t.to_bits())
            .expect("every row's t was inserted above");
        let bucket_idx = (b - min_bucket_idx) as usize;
        data.push((time_idx, bucket_idx, c));
        if c < min_value {
            min_value = c;
        }
        if c > max_value {
            max_value = c;
        }
    }

    // Same fallback semantics as `streaming/histogram.rs:547-552`.
    if min_value == f64::MAX {
        min_value = 0.0;
    }
    if max_value == f64::MIN {
        max_value = 0.0;
    }

    Ok(QueryResult::HistogramHeatmap {
        result: HistogramHeatmapResult {
            timestamps,
            bucket_bounds,
            data,
            min_value,
            max_value,
        },
    })
}
