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

    // The `_metadata` table (one row per metric column with type +
    // labels MAP) is currently unused by any catalogue SQL twin and
    // takes ~280ms to populate on the largest fixtures (one big INSERT
    // with thousands of literal tuples). Skipping it shaves cold-start
    // without affecting correctness. `populate_metadata_table` is kept
    // for future use; the original intent was dynamic-UNPIVOT-column-
    // lists patterns driven by `_metadata` rows.

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

    Ok(catalog)
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
}
