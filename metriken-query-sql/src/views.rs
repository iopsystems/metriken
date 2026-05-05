//! Parquet load + per-metric catalog.
//!
//! Metriken parquet files store metrics in *wide* form: one column per
//! labeled series, with the canonical metric name and label values
//! living in Arrow field metadata. The wide-form SQL generator
//! projects directly off these columns, so this module's job is only:
//!
//! 1. Load the parquet into an in-memory `_src` table (one read,
//!    snapped timestamps).
//! 2. Build a `MetricCatalog` indexing each canonical metric name to
//!    its physical columns + label maps + (for histograms)
//!    `grouping_power`. The wide-form generator consumes this index
//!    to know which columns to project for any `M{labels}` selector.
//!
//! No per-metric VIEWs are created — the long-form
//! `metric(timestamp, value, ...labels)` layer was removed once
//! every catalogue entry had a wide-form path.

use std::collections::{BTreeMap, BTreeSet};

use arrow::datatypes::DataType;
use duckdb::Connection;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};

/// Internal: classify a column from its data type + metadata.
#[derive(Debug)]
pub(crate) enum ColumnKind {
    Counter,
    Gauge,
    Histogram { grouping_power: u8 },
    Other,
}

#[derive(Debug)]
pub(crate) struct ColumnInfo {
    /// The actual column name in the parquet file.
    pub(crate) physical: String,
    /// Canonical metric name (from `metric` metadata, or the column name
    /// minus a `:buckets` suffix).
    pub(crate) metric: String,
    pub(crate) kind: ColumnKind,
    /// All label key-value pairs (excluding the internal keys `metric`,
    /// `metric_type`, `unit`, `grouping_power`, `max_value_power`).
    pub(crate) labels: BTreeMap<String, String>,
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

/// Read the parquet schema, load the file into a single in-memory
/// `_src` TEMP TABLE, and build the per-metric catalog the wide-form
/// SQL generator consumes.
///
/// **Performance:** the parquet is loaded **once** into the `_src`
/// table. Wide-form SQL queries reference `_src` instead of
/// `read_parquet(...)` directly, so each query is an in-memory scan
/// (cheap) rather than a fresh parquet read (~10-100s on Rezolus
/// production data). The single parquet read amortises across every
/// query for the lifetime of the connection.
/// Per-metric metadata built once at `ensure_views` time. The backend
/// caches this alongside the connection so per-query pre-flight
/// checks (which physical columns belong to a metric, what the
/// histogram's grouping_power is) are hashmap reads.
///
/// `series_by_metric` is the per-(metric, physical-column) label map.
/// Used by the wide-form SQL generator to project values/rates
/// directly off `_src` columns; the long-form metric VIEW is gone.
#[derive(Debug, Default, Clone)]
pub struct MetricCatalog {
    /// Per-metric ordered list of physical columns + their label maps.
    /// Drives the wide-form SQL generator. Order is parquet-schema order.
    pub series_by_metric: std::collections::HashMap<String, Vec<MetricSeries>>,
    /// Per-histogram-metric `grouping_power` (`p`). The wide-form
    /// histogram path needs `p` to call `h2_quantile` / `h2_heatmap`.
    /// Counters/gauges are absent from this map.
    pub histogram_p_by_metric: std::collections::HashMap<String, u8>,
}

#[derive(Debug, Clone)]
pub struct MetricSeries {
    pub physical: String,
    pub labels: BTreeMap<String, String>,
}

/// Read parquet metadata: classify columns, dedupe duplicates, extract
/// the sampling interval. Pure introspection — no DuckDB side effects.
pub(crate) fn read_introspection(
    parquet_path: &str,
) -> duckdb::Result<(Vec<ColumnInfo>, u64)> {
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
    // them — without dedup the catalog would index physical column names
    // that don't exist in `_src`. Dedupe by physical name, first
    // occurrence wins, matching DuckDB's behavior.
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

    Ok((columns, interval_ns))
}

/// Group classified columns by canonical metric name and record
/// histogram grouping_power. Pure data transformation; no I/O.
pub(crate) fn build_catalog(columns: &[ColumnInfo]) -> MetricCatalog {
    let mut by_metric: BTreeMap<String, Vec<&ColumnInfo>> = BTreeMap::new();
    for c in columns {
        by_metric.entry(c.metric.clone()).or_default().push(c);
    }

    let mut catalog = MetricCatalog::default();
    for (metric, cols) in by_metric {
        // Record `grouping_power` for histogram metrics so wide-form
        // can pass it to h2_quantile / h2_heatmap without recomputing.
        if let Some(ColumnKind::Histogram { grouping_power }) =
            cols.first().map(|c| &c.kind)
        {
            catalog
                .histogram_p_by_metric
                .insert(metric.clone(), *grouping_power);
        }
        let series: Vec<MetricSeries> = cols
            .iter()
            .map(|c| MetricSeries {
                physical: c.physical.clone(),
                labels: c.labels.clone(),
            })
            .collect();
        catalog.series_by_metric.insert(metric, series);
    }
    catalog
}

/// Read the parquet schema and return the per-metric catalog without
/// any DuckDB side effects. Used by callers that need parquet metadata
/// (e.g. to generate SQL for a wide-form selector) but don't yet have
/// — or don't want to commit to — a `_src` table. Cheap to call from
/// any thread; reads the file once.
pub fn describe_parquet(parquet_path: &str) -> duckdb::Result<MetricCatalog> {
    let (columns, _interval_ns) = read_introspection(parquet_path)?;
    Ok(build_catalog(&columns))
}

/// Create the `_src` TEMP TABLE on `conn` from `parquet_path`,
/// snapping timestamps to the nearest multiple of `interval_ns`.
///
/// The snap matches PromQL's `Tsdb` loader so SQL queries see the
/// same canonical timestamps PromQL does. `//` is integer division —
/// DuckDB's `/` promotes to DOUBLE which loses precision at parquet-
/// timestamp scale (~1.8e18 ns; DOUBLE has only ~15.95 significant
/// decimal digits, so the snap would silently become a no-op).
pub(crate) fn create_src_table(
    conn: &Connection,
    parquet_path: &str,
    interval_ns: u64,
) -> duckdb::Result<()> {
    let half = interval_ns / 2;
    let load_src = format!(
        "CREATE OR REPLACE TEMP TABLE _src AS \
         SELECT \
            ((CAST(timestamp AS BIGINT) + {half}) // {interval_ns}) * {interval_ns} AS timestamp, \
            * EXCLUDE (timestamp) \
         FROM read_parquet('{}')",
        parquet_path.replace('\'', "''")
    );
    conn.execute(&load_src, [])?;
    Ok(())
}

/// Read the parquet schema, load the file into a single in-memory
/// `_src` TEMP TABLE on `conn`, and return the per-metric catalog the
/// wide-form SQL generator consumes.
///
/// Convenience: combines `read_introspection` + `create_src_table` +
/// `build_catalog` for callers that don't want to pool slot setup.
/// Used by tests and one-off perf scripts.
pub fn ensure_views(conn: &Connection, parquet_path: &str) -> duckdb::Result<MetricCatalog> {
    let (columns, interval_ns) = read_introspection(parquet_path)?;
    create_src_table(conn, parquet_path, interval_ns)?;
    Ok(build_catalog(&columns))
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
    fn counter_basic_catalog_indexes_the_one_metric() {
        let conn = fresh_conn();
        let catalog =
            ensure_views(&conn, fixture_path("counter_basic").to_str().unwrap()).unwrap();
        let series = catalog.series_by_metric.get("requests").expect("requests metric");
        assert_eq!(series.len(), 1);
    }

    #[test]
    fn counter_multi_label_catalog_indexes_four_series() {
        let conn = fresh_conn();
        let catalog = ensure_views(&conn, fixture_path("counter_multi_label").to_str().unwrap())
            .unwrap();
        let series = catalog
            .series_by_metric
            .get("cpu_usage")
            .expect("cpu_usage metric");
        // 4 labeled series (id × state).
        assert_eq!(series.len(), 4);
        // All four label permutations are present.
        let mut combos: std::collections::BTreeSet<(String, String)> = Default::default();
        for s in series {
            combos.insert((
                s.labels.get("id").cloned().unwrap_or_default(),
                s.labels.get("state").cloned().unwrap_or_default(),
            ));
        }
        assert_eq!(combos.len(), 4);
    }

    #[test]
    fn histogram_grouping_power_is_recorded() {
        let conn = fresh_conn();
        let catalog =
            ensure_views(&conn, fixture_path("histogram_basic").to_str().unwrap()).unwrap();
        let p = catalog
            .histogram_p_by_metric
            .get("request_latency")
            .copied()
            .expect("request_latency p");
        // metriken-query-fixtures uses gp=4 for the rezolus histogram config.
        assert_eq!(p, 4);
    }

    #[test]
    fn describe_parquet_returns_same_catalog_without_creating_src_table() {
        // describe_parquet does pure introspection — no DuckDB side
        // effects. Two assertions: (1) the catalog matches what
        // ensure_views builds; (2) `_src` does not exist on a fresh
        // connection that only saw describe_parquet.
        let path = fixture_path("counter_multi_label");
        let path = path.to_str().unwrap();

        let described = describe_parquet(path).unwrap();
        assert_eq!(described.series_by_metric.get("cpu_usage").unwrap().len(), 4);

        let conn = fresh_conn();
        // No call to ensure_views — only describe_parquet was used.
        let src_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM duckdb_tables() WHERE table_name = '_src')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!src_exists, "describe_parquet must not create _src");

        // ensure_views still produces an equivalent catalog.
        let ensured = ensure_views(&conn, path).unwrap();
        assert_eq!(
            described.series_by_metric.keys().collect::<Vec<_>>(),
            ensured.series_by_metric.keys().collect::<Vec<_>>()
        );
        assert_eq!(described.histogram_p_by_metric, ensured.histogram_p_by_metric);
    }
}
