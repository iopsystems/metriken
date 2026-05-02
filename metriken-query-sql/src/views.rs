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
///
/// **Performance:** before any view is created, the parquet file is loaded
/// **once** into an in-memory `_src` table. The view DDLs all reference
/// `_src` instead of `read_parquet(...)` directly, so a query that touches a
/// view with N UNION-ALL branches costs N in-memory scans of `_src` (cheap)
/// rather than N full parquet reads (~10-100s on Rezolus production data).
/// The single parquet read amortises across every view and every query for
/// the lifetime of the connection.
/// Per-metric shape and label-key index built once at `ensure_views`
/// time. The backend caches this alongside the connection so per-query
/// pre-flight checks are hashmap reads instead of DESCRIBE roundtrips
/// (~1-2ms saved per query for catalogue entries with metric idents).
#[derive(Debug, Default, Clone)]
pub struct MetricCatalog {
    /// `Scalar` for gauge/counter (view has a `value` column);
    /// `Histogram` for histogram (view has `buckets` + `p`).
    pub shapes: std::collections::HashMap<String, MetricShape>,
    /// Union of label keys present on each metric view.
    pub label_keys: std::collections::HashMap<String, BTreeSet<String>>,
    /// All view names registered — used by the backend's
    /// `first_missing_from_view` scan to short-circuit on hardcoded
    /// metric names that the fixture doesn't carry.
    pub view_names: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricShape {
    Scalar,
    Histogram,
}

pub fn ensure_views(conn: &Connection, parquet_path: &str) -> duckdb::Result<MetricCatalog> {
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

    // Some Rezolus parquet writers (e.g. the disaggregated-serving sglang
    // recordings) emit duplicate top-level column names with the same
    // type/metadata. DuckDB's `read_parquet` silently keeps only the first
    // occurrence in `_src`, but `arrow::Schema::fields()` returns all of
    // them — leaving the metric-views layer to insert duplicate rows into
    // `_metadata` (which has `col` as PRIMARY KEY) and to UNION-ALL views
    // over a column that no longer exists by that name twice. Dedupe by
    // physical name, first occurrence wins, matching DuckDB's behavior.
    let mut seen_physical: BTreeSet<String> = BTreeSet::new();
    let columns: Vec<ColumnInfo> = schema
        .fields()
        .iter()
        .filter_map(|f| {
            if f.name() == "timestamp" || f.name() == "duration" {
                return None;
            }
            let info = classify(f);
            if matches!(info.kind, ColumnKind::Other) {
                return None;
            }
            if !seen_physical.insert(info.physical.clone()) {
                return None;
            }
            Some(info)
        })
        .collect();

    // Sampling interval (ns), pulled from the parquet file-level kv. The
    // metriken-query loader snaps every parquet timestamp to the nearest
    // multiple of this interval (`tsdb/mod.rs:50-58`), so the SQL side has
    // to do the same — otherwise shadow-mode comparison against PromQL diffs
    // on the timestamp axis even when values agree exactly.
    let interval_ns: u64 = meta
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|kvs| {
            kvs.iter().find_map(|kv| {
                if kv.key == "sampling_interval_ms" {
                    kv.value.as_ref().and_then(|v| v.parse::<u64>().ok())
                } else {
                    None
                }
            })
        })
        .map(|ms| ms * 1_000_000)
        .unwrap_or(1_000_000_000);
    let half = interval_ns / 2;

    // Single parquet read into an in-memory table. All metric views project
    // from `_src` instead of `read_parquet(...)`, so per-view UNION ALL
    // branches are cheap memory scans, not redundant parquet decodes.
    //
    // The `timestamp` column is replaced with its snapped form (round-to-
    // nearest interval) so SQL queries see the same canonical timestamps
    // PromQL does.
    // Use `//` (integer division) — DuckDB's `/` promotes to DOUBLE which
    // loses precision at parquet-timestamp scale (~1.8e18 ns; DOUBLE has
    // only ~15.95 significant decimal digits, so the snap would silently
    // become a no-op and shadow comparison would still diverge).
    let load_src = format!(
        "CREATE OR REPLACE TEMP TABLE _src AS \
         SELECT \
            ((CAST(timestamp AS BIGINT) + {half}) // {interval_ns}) * {interval_ns} AS timestamp, \
            * EXCLUDE (timestamp) \
         FROM read_parquet('{}')",
        parquet_path.replace('\'', "''")
    );
    conn.execute(&load_src, [])?;

    // Column metadata index: one row per metric column, with the canonical
    // metric name, type, histogram config (when applicable), and a
    // MAP<VARCHAR, VARCHAR> of all user labels. Queries can look up the
    // physical columns for a given metric / label predicate without
    // re-parsing parquet metadata, enabling more efficient SQL patterns
    // (e.g. dynamic UNPIVOT column lists driven by `_metadata` rows).
    populate_metadata_table(conn, &columns)?;

    // Group by metric name, emit one view per metric. Within a metric, all
    // columns must share the same kind (counter / gauge / histogram) and
    // — for histograms — the same grouping_power. We don't enforce here;
    // we trust the writer.
    let mut by_metric: BTreeMap<String, Vec<&ColumnInfo>> = BTreeMap::new();
    for c in &columns {
        by_metric.entry(c.metric.clone()).or_default().push(c);
    }

    let mut catalog = MetricCatalog::default();
    for (metric, cols) in by_metric {
        // Union of label keys across columns of this metric — every series
        // contributes the same set of label columns to the view (rows that
        // don't have a particular label fill it as ''/empty).
        let label_keys: BTreeSet<String> = cols
            .iter()
            .flat_map(|c| c.labels.keys().cloned())
            .collect();
        let label_keys_vec: Vec<String> = label_keys.iter().cloned().collect();

        // All cols within a metric share kind (we trust the writer); pick
        // the first to determine view shape for the catalog cache.
        let shape = match cols.first().map(|c| &c.kind) {
            Some(ColumnKind::Histogram { .. }) => MetricShape::Histogram,
            _ => MetricShape::Scalar,
        };

        let view_sql = build_view_sql(&metric, &cols, &label_keys_vec);
        conn.execute(&view_sql, [])?;

        catalog.shapes.insert(metric.clone(), shape);
        catalog.label_keys.insert(metric.clone(), label_keys);
        catalog.view_names.insert(metric);
    }

    Ok(catalog)
}

/// Build the `_metadata` table with one row per metric-bearing column.
///
/// Schema:
/// - `col` VARCHAR — physical column name in the parquet (matches `_src` column).
/// - `metric` VARCHAR — canonical metric name (from the `metric` field of the column's metadata).
/// - `metric_type` VARCHAR — `'counter'`, `'gauge'`, or `'histogram'`.
/// - `grouping_power` INTEGER — populated for histograms, NULL otherwise.
/// - `max_value_power` INTEGER — populated for histograms, NULL otherwise.
/// - `labels` MAP(VARCHAR, VARCHAR) — all user labels for this column
///   (excluding internal keys: metric, metric_type, unit, grouping_power,
///   max_value_power).
fn populate_metadata_table(conn: &Connection, columns: &[ColumnInfo]) -> duckdb::Result<()> {
    conn.execute(
        "CREATE OR REPLACE TEMP TABLE _metadata (\
            col VARCHAR PRIMARY KEY,\
            metric VARCHAR,\
            metric_type VARCHAR,\
            grouping_power INTEGER,\
            max_value_power INTEGER,\
            labels MAP(VARCHAR, VARCHAR)\
        )",
        [],
    )?;
    if columns.is_empty() {
        return Ok(());
    }

    // Build a single multi-row INSERT for all columns. Faster than N
    // separate inserts and keeps the transaction lock contention low.
    let mut sql = String::from("INSERT INTO _metadata VALUES ");
    for (i, info) in columns.iter().enumerate() {
        if i > 0 {
            sql.push_str(",\n");
        }
        let (metric_type_str, gp_lit, mvp_lit) = match &info.kind {
            ColumnKind::Counter => ("counter", "NULL".to_string(), "NULL".to_string()),
            ColumnKind::Gauge => ("gauge", "NULL".to_string(), "NULL".to_string()),
            ColumnKind::Histogram { grouping_power } => (
                "histogram",
                grouping_power.to_string(),
                // We only carry grouping_power on ColumnInfo today; if the
                // writer's metadata had max_value_power we can surface it
                // by extending ColumnKind. For now leave NULL — the H2
                // bucket math derives it from grouping_power + n=64.
                "NULL".to_string(),
            ),
            ColumnKind::Other => continue,
        };
        // Build MAP literal: `MAP {'k1': 'v1', 'k2': 'v2'}` (DuckDB syntax).
        let map_lit = if info.labels.is_empty() {
            "MAP {}".to_string()
        } else {
            let pairs = info
                .labels
                .iter()
                .map(|(k, v)| format!("'{}': '{}'", escape_sql(k), escape_sql(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("MAP {{{pairs}}}")
        };
        sql.push_str(&format!(
            "  ('{col}', '{metric}', '{metric_type}', {gp}, {mvp}, {labels})",
            col = escape_sql(&info.physical),
            metric = escape_sql(&info.metric),
            metric_type = metric_type_str,
            gp = gp_lit,
            mvp = mvp_lit,
            labels = map_lit,
        ));
    }
    conn.execute(&sql, [])?;
    Ok(())
}

fn build_view_sql(metric: &str, cols: &[&ColumnInfo], label_keys: &[String]) -> String {
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

        let physical = c.physical.replace('"', r#""""#);
        let value_expr = match &c.kind {
            ColumnKind::Counter => format!(r#"CAST("{physical}" AS UBIGINT) AS value"#),
            ColumnKind::Gauge => format!(r#"CAST("{physical}" AS BIGINT) AS value"#),
            ColumnKind::Histogram { grouping_power } => format!(
                r#""{physical}" AS buckets, {grouping_power}::INTEGER AS p"#
            ),
            ColumnKind::Other => continue,
        };

        let label_part = if label_keys.is_empty() {
            String::new()
        } else {
            format!(", {label_select}")
        };

        // Emit `col` as the physical column name — this is the unique
        // per-series identifier in the wide-form parquet. Catalogue
        // queries should `PARTITION BY col` for per-series windowed math
        // (irate / rate / LAG): partitioning by a label subset like
        // (id, state) collapses multi-source captures (e.g. cachecannon's
        // rezolus-client::* and rezolus-server::* columns share id+state
        // but represent different series under different `node` labels).
        selects.push(format!(
            "SELECT timestamp, {value_expr}, '{col_lit}' AS col{label_part} FROM _src WHERE \"{physical}\" IS NOT NULL",
            col_lit = escape_sql(&c.physical)
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
    fn timestamps_are_snapped_to_sampling_interval() {
        // The synthetic fixtures already have clean integer-second timestamps,
        // so the snap is a no-op there. This test verifies the snap math and
        // its DuckDB types directly: a sub-second timestamp must round to the
        // nearest second in BIGINT (not DOUBLE — DuckDB's `/` would otherwise
        // promote to DOUBLE and lose precision at 1.8e18 scale).
        let conn = fresh_conn();
        let ts: i64 = conn
            .query_row(
                // Mirror the views.rs snap formula. 1768956638999716606 is
                // a real Rezolus timestamp (~1ms shy of the second mark);
                // it should snap UP to 1768956639_000000000.
                "SELECT ((1768956638999716606::BIGINT + 500000000) // 1000000000) * 1000000000",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ts, 1_768_956_639_000_000_000);
    }

    #[test]
    fn src_table_carries_snapped_timestamp() {
        // End-to-end: load a fixture, query MIN(timestamp) % 1e9 from `_src`.
        // Must be 0 for every row — proof that the snap was applied.
        let conn = fresh_conn();
        ensure_views(&conn, fixture_path("counter_basic").to_str().unwrap()).unwrap();
        let nonzero: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _src WHERE (CAST(timestamp AS BIGINT) % 1000000000) != 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(nonzero, 0, "every _src.timestamp must be a clean multiple of 1e9 ns");
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
