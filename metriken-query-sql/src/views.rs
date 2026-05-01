//! Wide-to-long metric view generation.
//!
//! Metriken parquet files store metrics in *wide* form: one column per
//! labeled series, with the canonical metric name and label values living
//! in Arrow field metadata. SQL queries naturally want a *long* view:
//! `cpu_usage(timestamp, value, id, state)` — one row per `(time, label
//! permutation)`. This module reads a parquet file's schema and emits a
//! `CREATE OR REPLACE VIEW <metric>` per unique metric name, UNION-ALLing
//! every column that shares the metric name and projecting its label
//! values as columns.
//!
//! Counters, gauges, and histograms are all supported. For histograms the
//! view exposes a `buckets` column (the cumulative `List<UBIGINT>`) plus a
//! `p` column carrying the column's `grouping_power` — so SQL queries can
//! pass the right `p` to `h2_quantile` without baking it in by hand.
//!
//! The view generator is invoked once per `DuckDbBackend::run` call. It's
//! cheap (a single parquet metadata read, no row-group decode) and idempotent
//! within a connection (`CREATE OR REPLACE`).

use std::collections::{BTreeMap, BTreeSet};

use arrow::datatypes::DataType;
use duckdb::Connection;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};

/// Internal: classify a column from its data type + metadata.
#[derive(Debug)]
enum ColumnKind {
    Counter,
    Gauge,
    Histogram { grouping_power: u8 },
    Other,
}

#[derive(Debug)]
struct ColumnInfo {
    /// The actual column name in the parquet file.
    physical: String,
    /// Canonical metric name (from `metric` metadata, or the column name
    /// minus a `:buckets` suffix).
    metric: String,
    kind: ColumnKind,
    /// All label key-value pairs (excluding the internal keys `metric`,
    /// `metric_type`, `unit`, `grouping_power`, `max_value_power`).
    labels: BTreeMap<String, String>,
}

fn classify(field: &arrow::datatypes::Field) -> ColumnInfo {
    let mut meta = field.metadata().clone();
    let metric = meta.remove("metric").unwrap_or_else(|| {
        field
            .name()
            .strip_suffix(":buckets")
            .unwrap_or(field.name())
            .to_string()
    });
    let _metric_type = meta.remove("metric_type");
    let _unit = meta.remove("unit");
    let grouping_power: Option<u8> = meta.remove("grouping_power").and_then(|v| v.parse().ok());
    let _max_value_power = meta.remove("max_value_power");
    let labels: BTreeMap<String, String> = meta.into_iter().collect();

    let kind = match field.data_type() {
        DataType::UInt64 => ColumnKind::Counter,
        DataType::Int64 => ColumnKind::Gauge,
        DataType::List(inner) if inner.data_type() == &DataType::UInt64 => {
            if let Some(gp) = grouping_power {
                ColumnKind::Histogram { grouping_power: gp }
            } else {
                ColumnKind::Other
            }
        }
        _ => ColumnKind::Other,
    };

    ColumnInfo {
        physical: field.name().clone(),
        metric,
        kind,
        labels,
    }
}

/// Read the parquet schema, group columns by canonical metric name, and emit
/// `CREATE OR REPLACE VIEW <metric>` for each one. Counters and gauges share
/// a `(timestamp, value, ...labels)` shape; histograms get
/// `(timestamp, buckets, p, ...labels)`.
pub fn ensure_views(conn: &Connection, parquet_path: &str) -> duckdb::Result<()> {
    let bytes = std::fs::read(parquet_path).map_err(|e| {
        duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error {
                code: duckdb::ErrorCode::InternalMalfunction,
                extended_code: 0,
            },
            Some(format!("read parquet {parquet_path}: {e}")),
        )
    })?;
    let bytes = bytes::Bytes::from(bytes);
    let meta = ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::default()).map_err(|e| {
        duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error {
                code: duckdb::ErrorCode::InternalMalfunction,
                extended_code: 0,
            },
            Some(format!("read parquet metadata: {e}")),
        )
    })?;
    let schema = meta.schema().clone();

    let columns: Vec<ColumnInfo> = schema
        .fields()
        .iter()
        .enumerate()
        .filter_map(|(_, f)| {
            // Skip the timestamp / duration columns themselves.
            if f.name() == "timestamp" || f.name() == "duration" {
                return None;
            }
            let info = classify(f);
            if matches!(info.kind, ColumnKind::Other) {
                None
            } else {
                Some(info)
            }
        })
        .collect();

    // Group by metric name, emit one view per metric. Within a metric, all
    // columns must share the same kind (counter / gauge / histogram) and
    // — for histograms — the same grouping_power. We don't enforce here;
    // we trust the writer.
    let mut by_metric: BTreeMap<String, Vec<&ColumnInfo>> = BTreeMap::new();
    for c in &columns {
        by_metric.entry(c.metric.clone()).or_default().push(c);
    }

    for (metric, cols) in by_metric {
        // Union of label keys across columns of this metric — every series
        // contributes the same set of label columns to the view (rows that
        // don't have a particular label fill it as ''/empty).
        let label_keys: BTreeSet<String> = cols
            .iter()
            .flat_map(|c| c.labels.keys().cloned())
            .collect();
        let label_keys_vec: Vec<String> = label_keys.into_iter().collect();

        let view_sql = build_view_sql(&metric, &cols, &label_keys_vec, parquet_path);
        conn.execute(&view_sql, [])?;
    }

    Ok(())
}

fn build_view_sql(
    metric: &str,
    cols: &[&ColumnInfo],
    label_keys: &[String],
    parquet_path: &str,
) -> String {
    let mut selects: Vec<String> = Vec::with_capacity(cols.len());
    for c in cols {
        // Build the SELECT for this column. Each label key gets either the
        // column's own value (string-quoted) or '' if absent.
        let label_select = label_keys
            .iter()
            .map(|k| {
                let v = c.labels.get(k).cloned().unwrap_or_default();
                format!("'{}' AS {}", escape_sql(&v), quote_ident(k))
            })
            .collect::<Vec<_>>()
            .join(", ");

        let value_expr = match &c.kind {
            ColumnKind::Counter => format!(
                r#"CAST("{c}" AS UBIGINT) AS value"#,
                c = c.physical.replace('"', r#""""#)
            ),
            ColumnKind::Gauge => format!(
                r#"CAST("{c}" AS BIGINT) AS value"#,
                c = c.physical.replace('"', r#""""#)
            ),
            ColumnKind::Histogram { grouping_power } => format!(
                r#""{c}" AS buckets, {p}::INTEGER AS p"#,
                c = c.physical.replace('"', r#""""#),
                p = grouping_power
            ),
            ColumnKind::Other => continue,
        };

        let label_part = if label_keys.is_empty() {
            String::new()
        } else {
            format!(", {}", label_select)
        };

        selects.push(format!(
            "SELECT timestamp, {value_expr}{label_part} FROM read_parquet('{path}') WHERE \"{c}\" IS NOT NULL",
            path = parquet_path.replace('\'', "''"),
            c = cols
                .iter()
                .find(|x| x.metric == c.metric)
                .map(|x| x.physical.clone())
                .unwrap_or_default()
                .replace('"', r#""""#),
        ));
    }

    let body = selects.join("\nUNION ALL\n");
    format!(
        "CREATE OR REPLACE VIEW {} AS\n{}",
        quote_ident(metric),
        body
    )
}

fn escape_sql(s: &str) -> String {
    s.replace('\'', "''")
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("metriken-query-fixtures")
            .join("fixtures")
            .join(format!("{name}.parquet"))
    }

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        crate::register_all(&conn).expect("register");
        conn
    }

    #[test]
    fn counter_basic_creates_a_one_column_long_view() {
        let conn = fresh_conn();
        ensure_views(&conn, fixture_path("counter_basic").to_str().unwrap()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 11, "11 timestamped rows in counter_basic");
    }

    #[test]
    fn counter_multi_label_unions_four_columns() {
        let conn = fresh_conn();
        ensure_views(&conn, fixture_path("counter_multi_label").to_str().unwrap()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cpu_usage", [], |r| r.get(0))
            .unwrap();
        // 11 timestamps × 4 (id × state) labeled series = 44 rows
        assert_eq!(count, 44);

        // All four label permutations are present
        let distinct: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT (id, state)) FROM cpu_usage",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(distinct, 4);
    }

    #[test]
    fn histogram_view_carries_grouping_power_as_p() {
        let conn = fresh_conn();
        ensure_views(&conn, fixture_path("histogram_basic").to_str().unwrap()).unwrap();
        let p: i32 = conn
            .query_row("SELECT MIN(p) FROM request_latency", [], |r| r.get(0))
            .unwrap();
        // metriken-query-fixtures uses gp=4 for the rezolus histogram config.
        assert_eq!(p, 4);
    }
}
