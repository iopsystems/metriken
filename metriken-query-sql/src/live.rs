//! In-memory DuckDB-backed live data source.
//!
//! Lives next to the parquet-backed connection pool in
//! [`crate::backend::DuckDbBackend`]. Whereas a parquet source's `_src`
//! is a TEMP TABLE materialised from `read_parquet(...)` once per pool
//! slot, a live source's `_src` is a real DuckDB table that **grows**
//! as snapshots arrive: each call to [`LiveSource::append`] runs
//! `ALTER TABLE _src ADD COLUMN` for any new metric and then `INSERT`s
//! a row with the current snapshot's values. Existing rows correctly
//! get NULL for newly-added columns — that metric didn't exist at
//! those timestamps.
//!
//! Concurrency: `duckdb::Connection` is `!Sync`, so the single live
//! `Connection` is wrapped in `Mutex` and both writer (`append`) and
//! reader (`run_sql`) acquire it. At the agent's ~1 Hz poll cadence
//! and the viewer's human-rate query refresh, contention is not a
//! concern. A future optimisation is `Database::connect()` to share
//! the DB across multiple connections (DuckDB has MVCC) — deferred
//! until measurement shows it matters.
//!
//! Why a single shared connection rather than the parquet path's
//! N-slot pool: every pool slot in `backend::ConnState` is an
//! **independent** in-memory database (each slot does its own
//! `CREATE TEMP TABLE _src AS SELECT * FROM read_parquet(...)`),
//! which is fine for parquet because the slots are stateless and
//! identical. For live mode, `_src` must be the *same* mutable table
//! across all reads, so the pool model can't apply.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use arrow::record_batch::RecordBatch;
use duckdb::Connection;

use crate::views::{
    build_catalog, quote_ident, render_cgroup_index_sql, view_name_for_source, ColumnInfo,
    ColumnKind, MetricCatalog,
};
use crate::SqlError;

/// Public column descriptor for live ingest. Mirrors the crate-private
/// `views::ColumnInfo`; external callers (rezolus's snapshot loop)
/// build these from `metriken_exposition::Snapshot` metadata.
#[derive(Debug, Clone)]
pub struct LiveColumn {
    /// Column name in `_src` (must be unique per source — same role as
    /// the parquet column name). For histograms this is the agent's
    /// metric name with a `:buckets` suffix, matching parquet
    /// convention.
    pub physical: String,
    /// Canonical metric name (e.g. `cpu_usage` for a series
    /// `cpu_usage/user/0`). Used to populate `_cgroup_index` and
    /// `MetricCatalog`.
    pub metric: String,
    /// Type info — drives the DuckDB column type at `ALTER` time.
    pub kind: LiveColumnKind,
    /// All key-value pairs apart from the infrastructure keys
    /// (`metric`, `metric_type`, `unit`, `grouping_power`,
    /// `max_value_power`). Used to populate `_cgroup_index` rows for
    /// cgroup_* metrics.
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveColumnKind {
    Counter,
    Gauge,
    Histogram { grouping_power: u8 },
}

/// Per-column value for a single snapshot row.
pub enum LiveValue<'a> {
    Counter(u64),
    Gauge(i64),
    /// H2 bucket counts. The slice length is determined by the
    /// histogram's `grouping_power` and `max_value_power`; this side
    /// doesn't validate it — the bucket math lives in the h2_* UDFs.
    Histogram(&'a [u64]),
}

/// Derive the canonical `_src` column name for a metric, matching the
/// parquet path's `views::canonical_alias`. Public so external bridges
/// (rezolus's snapshot loop) compute the same name for the same input
/// metric — a live `_src` ends up shaped identically to a parquet
/// `_src` for the agent's emit format.
///
/// The agent's emit shape:
///   - `raw_physical` is typically a numeric ID (`"49"`, `"24x0"`) or
///     an already-canonical name (`"cpu_usage/user/0"`).
///   - `metric` is the canonical metric name (`"cpu_usage"`,
///     `"rezolus_cpu_usage"`).
///   - `labels` are key→value pairs after shape keys are stripped
///     (`{"state": "user", "id": "0"}`).
///
/// Returns:
///   - `raw_physical` (verbatim, sans `<src>::` prefix) when it's
///     already in canonical form — `metric`, `metric/...`, or
///     `metric:buckets`.
///   - Otherwise `metric/v1/v2/...:buckets?` rebuilt from sorted
///     value labels (non-numeric values first, numeric values last).
///
/// This mirrors `views::canonical_alias` precisely so the same logic
/// runs on both paths.
pub fn canonical_column_name(
    raw_physical: &str,
    metric: &str,
    labels: &BTreeMap<String, String>,
    kind: LiveColumnKind,
) -> String {
    // Strip any `<src>::` prefix. Live single-source agents won't
    // typically emit one, but be defensive.
    let rest = match raw_physical.split_once("::") {
        Some((_, r)) => r,
        None => raw_physical,
    };

    // Already-canonical short-circuit.
    if rest == metric
        || rest == format!("{metric}:buckets")
        || rest.starts_with(&format!("{metric}/"))
    {
        return rest.to_string();
    }

    // Rebuild from value labels. Infrastructure keys (`node`,
    // `source`, `endpoint`, `instance`) and shape keys are excluded
    // by the caller before we get here, so all entries in `labels`
    // are real value labels.
    const NON_VALUE_KEYS: &[&str] = &["endpoint", "instance", "node", "source"];
    let mut value_labels: Vec<(&str, &str)> = labels
        .iter()
        .filter(|(k, _)| !NON_VALUE_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    value_labels.sort_by(|a, b| {
        let na = a.1.chars().all(|c| c.is_ascii_digit()) && !a.1.is_empty();
        let nb = b.1.chars().all(|c| c.is_ascii_digit()) && !b.1.is_empty();
        (na as u8).cmp(&(nb as u8)).then_with(|| a.0.cmp(b.0))
    });

    let mut name = metric.to_string();
    for (_, v) in &value_labels {
        name.push('/');
        name.push_str(v);
    }
    if matches!(kind, LiveColumnKind::Histogram { .. }) {
        name.push_str(":buckets");
    }
    name
}

/// An in-memory DuckDB database whose `_src` table grows as snapshots
/// are appended. Cloning the `Arc<LiveSource>` is cheap; all real
/// state sits behind a `Mutex`.
///
/// Construction order on the contained connection:
///   1. Open in-memory DB.
///   2. Register UDFs + macros (so dashboard SQL — `irate_1s`,
///      `h2_quantile`, etc. — binds the moment a query arrives).
///   3. Create the bootstrap `_src(timestamp, duration)` table.
///   4. Create an empty `_cgroup_index` (dashboard SQL JOINs against
///      it unconditionally).
///   5. Create the bootstrap `_src_<source>` view.
///
/// Subsequent `append` calls grow the schema in-place.
pub struct LiveSource {
    inner: Mutex<Inner>,
    /// Display source name (e.g. "rezolus"). Drives the `_src_<source>`
    /// view name via [`view_name_for_source`].
    source_name: String,
    /// Polling interval in nanoseconds. Used to snap incoming
    /// timestamps to a fixed grid, mirroring the parquet path's
    /// `((ts + half) // interval) * interval` projection.
    interval_ns: u64,
}

struct Inner {
    conn: Connection,
    /// Tracked schema state. Key = column physical name. Values are
    /// the same `ColumnInfo` the renderers consume so we can hand a
    /// `Vec<ColumnInfo>` to [`render_cgroup_index_sql`] etc.
    schema: BTreeMap<String, ColumnInfo>,
}

impl LiveSource {
    /// Create a new live source. `source_name` becomes the suffix on
    /// the `_src_<source>` view (sanitised via
    /// [`view_name_for_source`]); `sampling_interval_ms` is the
    /// agent's poll interval and drives the timestamp snap.
    pub fn new(source_name: &str, sampling_interval_ms: u64) -> Result<Arc<Self>, SqlError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| SqlError::Backend(format!("open duckdb: {e}")))?;
        // Match the parquet path's prepared-statement cache size so
        // dashboard SQL gets the same warm-cache benefit on a live
        // source. Each cached statement is small (planning artifacts
        // only).
        conn.set_prepared_statement_cache_capacity(1024);
        crate::register_all(&conn)
            .map_err(|e| SqlError::Backend(format!("register UDFs/macros: {e}")))?;

        // Bootstrap _src with the two universal columns. Non-TEMP so
        // it survives across all interactions with this connection
        // (`CREATE TEMP TABLE` would be session-scoped — which is fine
        // for a single connection but no different and clearer to
        // make it plain).
        //
        // Type alignment with the parquet path: `views::render_src_sql`
        // produces `_src.timestamp` as `BIGINT` (the parquet-side
        // `CAST(timestamp AS BIGINT)` for the snap math) and
        // `_src.duration` as `UBIGINT` (parquet's UInt64 passthrough).
        // We mirror those types exactly so dashboard SQL written
        // against `_src.timestamp` (BIGINT arithmetic, RANGE windows,
        // etc.) binds identically on both sources.
        conn.execute(
            "CREATE TABLE _src (\
                 timestamp BIGINT NOT NULL, \
                 duration  UBIGINT\
             )",
            [],
        )
        .map_err(|e| SqlError::Backend(format!("create _src: {e}")))?;

        // _cgroup_index always exists so cgroup dashboard SQL JOINs
        // bind even on parquets/sources without cgroup metrics.
        // Initial render uses an empty column list — empty INSERT body.
        conn.execute_batch(&render_cgroup_index_sql(&[]))
            .map_err(|e| SqlError::Backend(format!("create _cgroup_index: {e}")))?;

        let view_name = view_name_for_source(source_name);
        // Live mode is single-source by construction (a live rezolus
        // agent emits its own metrics under its own canonical names),
        // so `_src_<source>` is just a pass-through of `_src` — no
        // re-projection, no aliasing, no read_parquet. When schema
        // grows the view picks up new columns automatically via
        // `SELECT *`.
        conn.execute(
            &format!("CREATE OR REPLACE TEMP VIEW {view_name} AS SELECT * FROM _src"),
            [],
        )
        .map_err(|e| SqlError::Backend(format!("create {view_name}: {e}")))?;

        Ok(Arc::new(Self {
            inner: Mutex::new(Inner {
                conn,
                schema: BTreeMap::new(),
            }),
            source_name: source_name.to_string(),
            interval_ns: sampling_interval_ms.saturating_mul(1_000_000).max(1),
        }))
    }

    /// Apply a single snapshot row.
    ///
    /// Steps under a single `Mutex` acquisition:
    ///   1. Diff `columns` against the tracked schema.
    ///   2. `ALTER TABLE _src ADD COLUMN` for each new physical name.
    ///   3. If new columns appeared with `cgroup_*` metrics, rebuild
    ///      `_cgroup_index` from the updated schema.
    ///   4. INSERT one row. Columns in the schema but absent from
    ///      `columns` get NULL — that's correct in both directions
    ///      (metric was sampled previously but not this poll;
    ///      metric was just added and didn't exist in earlier rows).
    pub fn append(
        &self,
        timestamp_ns: u64,
        duration_ns: Option<u64>,
        columns: &[(LiveColumn, LiveValue<'_>)],
    ) -> Result<(), SqlError> {
        let mut inner = self.inner.lock().expect("LiveSource mutex poisoned");

        // Phase 1 — schema diff + ALTER.
        let mut added_cgroup_column = false;
        for (col, _) in columns {
            if inner.schema.contains_key(&col.physical) {
                continue;
            }
            let type_sql = duckdb_column_type(col.kind);
            let alter = format!(
                "ALTER TABLE _src ADD COLUMN {} {}",
                quote_ident(&col.physical),
                type_sql,
            );
            inner
                .conn
                .execute(&alter, [])
                .map_err(|e| SqlError::Backend(format!("alter _src ({}): {e}", col.physical)))?;
            if col.metric.starts_with("cgroup_") {
                added_cgroup_column = true;
            }
            inner
                .schema
                .insert(col.physical.clone(), to_internal_column_info(col));
        }

        // Phase 2 — rebuild dependent objects when the schema grew.
        // The `_src_<source>` view uses `SELECT *` so it auto-picks
        // up new columns; only `_cgroup_index` needs an explicit
        // rebuild (its rows are enumerated from cgroup_* columns).
        if added_cgroup_column {
            let cols: Vec<ColumnInfo> = inner.schema.values().cloned().collect();
            let sql = render_cgroup_index_sql(&cols);
            inner
                .conn
                .execute_batch(&sql)
                .map_err(|e| SqlError::Backend(format!("rebuild _cgroup_index: {e}")))?;
        }

        // Phase 3 — INSERT one row.
        let snapped_ts = snap_timestamp(timestamp_ns, self.interval_ns);

        // Build an ordered (column_name, sql_value_literal) list,
        // starting with the two universal columns and appending one
        // entry per schema column. Columns absent from `columns`
        // contribute `NULL`. Iterating schema (BTreeMap → sorted)
        // means INSERT order is deterministic — useful for tests and
        // for diagnostic logging.
        let value_by_name: BTreeMap<&str, &LiveValue<'_>> = columns
            .iter()
            .map(|(c, v)| (c.physical.as_str(), v))
            .collect();

        let mut col_idents: Vec<String> = Vec::with_capacity(2 + inner.schema.len());
        let mut value_lits: Vec<String> = Vec::with_capacity(2 + inner.schema.len());

        col_idents.push(quote_ident("timestamp"));
        // BIGINT to match the parquet path's _src.timestamp type.
        value_lits.push(format!("{}::BIGINT", snapped_ts));

        col_idents.push(quote_ident("duration"));
        value_lits.push(match duration_ns {
            Some(d) => format!("{}::UBIGINT", d),
            None => "NULL".to_string(),
        });

        for col_name in inner.schema.keys() {
            col_idents.push(quote_ident(col_name));
            match value_by_name.get(col_name.as_str()) {
                Some(v) => value_lits.push(format_value(v)),
                None => value_lits.push("NULL".to_string()),
            }
        }

        let sql = format!(
            "INSERT INTO _src ({}) VALUES ({})",
            col_idents.join(", "),
            value_lits.join(", "),
        );
        inner
            .conn
            .execute(&sql, [])
            .map_err(|e| SqlError::Backend(format!("insert: {e}")))?;

        Ok(())
    }

    /// Execute `sql` against the live source. Serialized with `append`
    /// via the connection mutex — see the module-level docs for why.
    /// Returns the same Arrow `RecordBatch` shape as the parquet path
    /// produces, so callers (rezolus's `prom-matrix` projection) can
    /// treat the two sources uniformly.
    pub fn run_sql(&self, sql: &str) -> Result<Vec<RecordBatch>, SqlError> {
        let inner = self.inner.lock().expect("LiveSource mutex poisoned");
        let mut stmt = inner
            .conn
            .prepare_cached(sql)
            .map_err(|e| SqlError::Backend(format!("prepare: {e}")))?;
        let arrow = stmt
            .query_arrow([])
            .map_err(|e| SqlError::Backend(format!("query_arrow: {e}")))?;
        Ok(arrow.collect())
    }

    /// Min/max timestamp currently in `_src` (nanoseconds). Returns
    /// `None` when no snapshots have been appended yet.
    ///
    /// `_src.timestamp` is stored as `BIGINT` (i64) to match the
    /// parquet path's projection — see `views::render_src_sql`'s
    /// `CAST(timestamp AS BIGINT)`. We read i64 then cast to u64 for
    /// the public API (timestamps are non-negative nanoseconds since
    /// epoch; the cast is lossless for any realistic value).
    pub fn time_range_ns(&self) -> Result<Option<(u64, u64)>, SqlError> {
        let inner = self.inner.lock().expect("LiveSource mutex poisoned");
        let row: (Option<i64>, Option<i64>) = inner
            .conn
            .query_row("SELECT MIN(timestamp), MAX(timestamp) FROM _src", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|e| SqlError::Backend(format!("time_range_ns: {e}")))?;
        Ok(match row {
            (Some(min), Some(max)) => Some((min as u64, max as u64)),
            _ => None,
        })
    }

    /// Per-metric catalog built from the current schema. Mirrors
    /// `MetricCatalog` produced by the parquet path so the rest of
    /// the system (Save-as-Report column trim, MCP `describe`) gets
    /// the same shape.
    pub fn catalog(&self) -> MetricCatalog {
        let inner = self.inner.lock().expect("LiveSource mutex poisoned");
        let cols: Vec<ColumnInfo> = inner.schema.values().cloned().collect();
        build_catalog(&cols)
    }

    /// Source name as configured at construction.
    pub fn source_name(&self) -> &str {
        &self.source_name
    }
}

/// DuckDB column type for a given live column kind.
fn duckdb_column_type(kind: LiveColumnKind) -> &'static str {
    match kind {
        // Counters are non-negative cumulative — u64.
        LiveColumnKind::Counter => "UBIGINT",
        // Gauges can go negative — i64.
        LiveColumnKind::Gauge => "BIGINT",
        // H2 bucket arrays — list of u64. DuckDB's `LIST<UBIGINT>`
        // spelled `UBIGINT[]`.
        LiveColumnKind::Histogram { .. } => "UBIGINT[]",
    }
}

/// Project a public `LiveColumn` into the crate-internal `ColumnInfo`
/// shape consumed by the renderers and catalog builder.
fn to_internal_column_info(col: &LiveColumn) -> ColumnInfo {
    let kind = match col.kind {
        LiveColumnKind::Counter => ColumnKind::Counter,
        LiveColumnKind::Gauge => ColumnKind::Gauge,
        LiveColumnKind::Histogram { grouping_power } => ColumnKind::Histogram { grouping_power },
    };
    ColumnInfo {
        physical: col.physical.clone(),
        metric: col.metric.clone(),
        kind,
        labels: col.labels.clone(),
    }
}

/// Snap `ts` to the nearest multiple of `interval`. Mirrors the
/// parquet path's snap math in `views::render_src_sql` so live and
/// parquet timestamps land on the same grid (downstream window
/// functions like `RANGE 5m PRECEDING` are sensitive to this).
fn snap_timestamp(ts: u64, interval: u64) -> u64 {
    let half = interval / 2;
    // Saturating arithmetic to avoid overflow on extreme inputs
    // (real timestamps are ~1.7e18 ns; the headroom is huge but
    // saturation is the cheap defensive choice).
    let snapped = ts.saturating_add(half) / interval;
    snapped.saturating_mul(interval)
}

/// Format a `LiveValue` as a DuckDB literal. Counters/gauges become
/// `N::TYPE`; histograms become `[b1, b2, ...]::UBIGINT[]`.
fn format_value(v: &LiveValue<'_>) -> String {
    match v {
        LiveValue::Counter(n) => format!("{}::UBIGINT", n),
        LiveValue::Gauge(n) => format!("{}::BIGINT", n),
        LiveValue::Histogram(buckets) => {
            // Histograms can have hundreds of buckets; preallocate.
            let mut s = String::with_capacity(buckets.len() * 6);
            s.push('[');
            for (i, b) in buckets.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                // u64 → decimal is injection-safe; no quoting needed.
                use std::fmt::Write;
                let _ = write!(s, "{}", b);
            }
            s.push_str("]::UBIGINT[]");
            s
        }
    }
}

#[cfg(test)]
mod tests {
    //! Cross-engine parity tests. These pin the core correctness
    //! property of the live path: given identical input data, the live
    //! `_src` (grown via `ALTER + INSERT`) and the parquet `_src`
    //! (materialised via `read_parquet`) must produce **identical**
    //! results for the same SQL. If this ever drifts, the dashboard
    //! will silently render different numbers in live mode vs file
    //! mode — exactly the silent regression the SQL migration sprint
    //! showed is possible.
    //!
    //! The mechanism: for each fixture parquet, read its rows back via
    //! `DuckDbBackend::run_sql("SELECT * FROM _src ...")`, project
    //! each row into the `(LiveColumn, LiveValue)` shape `LiveSource`
    //! accepts, replay through `append`, then run several SQL queries
    //! against both sources and assert canonical equality on the
    //! Arrow RecordBatches.

    use super::*;
    use crate::DuckDbBackend;
    use arrow::array::{Array, Int64Array, ListArray, RecordBatch, UInt64Array};
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("metriken-query-fixtures")
            .join("fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    /// Convert the crate-private `ColumnInfo` (read from parquet
    /// metadata) into the public `LiveColumn` descriptor the live
    /// source consumes. Skip `ColumnKind::Other` (no DuckDB
    /// representation).
    fn live_column_from_info(info: &crate::views::ColumnInfo) -> Option<LiveColumn> {
        let kind = match info.kind {
            crate::views::ColumnKind::Counter => LiveColumnKind::Counter,
            crate::views::ColumnKind::Gauge => LiveColumnKind::Gauge,
            crate::views::ColumnKind::Histogram { grouping_power } => {
                LiveColumnKind::Histogram { grouping_power }
            }
            crate::views::ColumnKind::Other => return None,
        };
        Some(LiveColumn {
            physical: info.physical.clone(),
            metric: info.metric.clone(),
            kind,
            labels: info.labels.clone(),
        })
    }

    /// Replay every row of a parquet fixture into a fresh `LiveSource`.
    /// The fixture must use the single-source convention (no `<src>::`
    /// prefixes); rezolus live agents emit single-source data so this
    /// matches the production shape.
    fn replay_parquet_into_live(parquet_path: &str) -> Arc<LiveSource> {
        let (col_infos, interval_ns) =
            crate::views::read_introspection(parquet_path).expect("introspect parquet");
        let live_columns: BTreeMap<String, LiveColumn> = col_infos
            .iter()
            .filter_map(|info| live_column_from_info(info).map(|lc| (lc.physical.clone(), lc)))
            .collect();
        let interval_ms = interval_ns / 1_000_000;
        let live = LiveSource::new("rezolus", interval_ms).expect("LiveSource::new");

        // Use a fresh backend to read the parquet's rows.
        let backend = DuckDbBackend::with_pool_size(1);
        let batches = backend
            .run_sql("SELECT * FROM _src ORDER BY timestamp", parquet_path)
            .expect("read parquet rows");

        for batch in &batches {
            replay_batch(&live, batch, &live_columns);
        }
        live
    }

    fn replay_batch(
        live: &LiveSource,
        batch: &RecordBatch,
        live_columns: &BTreeMap<String, LiveColumn>,
    ) {
        let schema = batch.schema();
        for row_idx in 0..batch.num_rows() {
            // `_src.timestamp` is BIGINT (i64) after the parquet
            // path's snap CAST; downcast accordingly and re-widen to
            // u64 for `append`. Safe because timestamps are
            // non-negative ns-since-epoch and well below i64::MAX.
            let ts = batch
                .column_by_name("timestamp")
                .expect("timestamp column")
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("timestamp BIGINT")
                .value(row_idx) as u64;
            let duration = batch.column_by_name("duration").and_then(|c| {
                let arr = c.as_any().downcast_ref::<UInt64Array>()?;
                if arr.is_null(row_idx) {
                    None
                } else {
                    Some(arr.value(row_idx))
                }
            });

            // Histograms are borrowed by `LiveValue::Histogram(&[u64])`,
            // so the underlying Vec<u64>s must outlive the append call.
            // Stage them here, build the row_columns list with borrows
            // pointing into this Vec.
            let mut histograms: Vec<(String, Vec<u64>)> = Vec::new();
            for (field, col) in schema.fields().iter().zip(batch.columns()) {
                let name = field.name();
                if name == "timestamp" || name == "duration" {
                    continue;
                }
                if col.is_null(row_idx) {
                    continue;
                }
                let Some(live_col) = live_columns.get(name) else {
                    continue;
                };
                if matches!(live_col.kind, LiveColumnKind::Histogram { .. }) {
                    let list = col
                        .as_any()
                        .downcast_ref::<ListArray>()
                        .expect("histogram ListArray");
                    let inner = list.value(row_idx);
                    let inner_arr = inner
                        .as_any()
                        .downcast_ref::<UInt64Array>()
                        .expect("histogram inner UBIGINT");
                    let buckets: Vec<u64> = (0..inner_arr.len())
                        .map(|i| {
                            if inner_arr.is_null(i) {
                                0
                            } else {
                                inner_arr.value(i)
                            }
                        })
                        .collect();
                    histograms.push((name.clone(), buckets));
                }
            }
            let hist_map: BTreeMap<&str, &[u64]> = histograms
                .iter()
                .map(|(n, b)| (n.as_str(), b.as_slice()))
                .collect();

            let mut row_columns: Vec<(LiveColumn, LiveValue)> = Vec::new();
            for (field, col) in schema.fields().iter().zip(batch.columns()) {
                let name = field.name();
                if name == "timestamp" || name == "duration" {
                    continue;
                }
                if col.is_null(row_idx) {
                    continue;
                }
                let Some(live_col) = live_columns.get(name) else {
                    continue;
                };
                let value = match live_col.kind {
                    LiveColumnKind::Counter => {
                        let v = col
                            .as_any()
                            .downcast_ref::<UInt64Array>()
                            .expect("counter UBIGINT")
                            .value(row_idx);
                        LiveValue::Counter(v)
                    }
                    LiveColumnKind::Gauge => {
                        let v = col
                            .as_any()
                            .downcast_ref::<Int64Array>()
                            .expect("gauge BIGINT")
                            .value(row_idx);
                        LiveValue::Gauge(v)
                    }
                    LiveColumnKind::Histogram { .. } => {
                        LiveValue::Histogram(hist_map[name.as_str()])
                    }
                };
                row_columns.push((live_col.clone(), value));
            }

            live.append(ts, duration, &row_columns).expect("append");
        }
    }

    /// Assert two batch sets are equivalent by their pretty-printed
    /// arrow representation. `arrow::util::pretty::pretty_format_batches`
    /// gives canonical text — same numeric values, same nullability,
    /// same column order produce byte-identical strings. Robust under
    /// column-ordering differences only if both sides project columns
    /// in the same order, which our SELECT statements ensure.
    fn assert_batches_eq(parquet: &[RecordBatch], live: &[RecordBatch], context: &str) {
        let p = arrow::util::pretty::pretty_format_batches(parquet)
            .expect("pretty parquet")
            .to_string();
        let l = arrow::util::pretty::pretty_format_batches(live)
            .expect("pretty live")
            .to_string();
        assert_eq!(
            p, l,
            "results diverge for [{context}]\n\n--- parquet ---\n{p}\n\n--- live ---\n{l}\n"
        );
    }

    /// Helper: run the same SQL on parquet path + live path, assert
    /// identical results.
    fn parity_query(parquet_path: &str, live: &LiveSource, sql: &str) {
        let backend = DuckDbBackend::with_pool_size(1);
        let parquet = backend.run_sql(sql, parquet_path).expect("parquet run_sql");
        let live = live.run_sql(sql).expect("live run_sql");
        assert_batches_eq(&parquet, &live, sql);
    }

    #[test]
    fn parity_counter_basic_row_level_and_aggregates() {
        // Smallest fixture: one counter, 11 rows. Pin parity on
        // straight value reads, COUNT, MIN/MAX, SUM.
        let path = fixture_path("counter_basic.parquet");
        let live = replay_parquet_into_live(&path);

        parity_query(&path, &live, "SELECT * FROM _src ORDER BY timestamp");
        parity_query(&path, &live, "SELECT COUNT(*) AS n FROM _src");
        parity_query(
            &path,
            &live,
            "SELECT MIN(timestamp) AS mn, MAX(timestamp) AS mx FROM _src",
        );
        parity_query(&path, &live, "SELECT SUM(requests) AS s FROM _src");
    }

    #[test]
    fn parity_counter_multi_label_irate() {
        // Tests dashboard-style SQL with the `irate_1s` macro across
        // four labeled series. Catches any divergence in the
        // window-function path (which is sensitive to NULL handling
        // and timestamp ordering).
        let path = fixture_path("counter_multi_label.parquet");
        let live = replay_parquet_into_live(&path);

        parity_query(&path, &live, "SELECT * FROM _src ORDER BY timestamp");
        // Per-column irate. The fixture uses numeric column names
        // (cpu_usage__0..__3); pick one to exercise the window
        // function. irate_1s takes (column, timestamp).
        parity_query(
            &path,
            &live,
            "SELECT timestamp, irate_1s(cpu_usage__0, timestamp) \
             FROM _src ORDER BY timestamp",
        );
        // Sum across all four labeled series.
        parity_query(
            &path,
            &live,
            "SELECT timestamp, \
                    cpu_usage__0 + cpu_usage__1 + cpu_usage__2 + cpu_usage__3 AS total \
             FROM _src ORDER BY timestamp",
        );
    }

    #[test]
    fn parity_histogram_basic_h2_aggregates() {
        // Histogram fixture: pin parity on h2_total + h2_quantile.
        // These are the most arithmetic-heavy UDFs; any difference in
        // bucket-storage round-trip would show up here.
        let path = fixture_path("histogram_basic.parquet");
        let live = replay_parquet_into_live(&path);

        parity_query(&path, &live, "SELECT COUNT(*) FROM _src");
        // Identify the histogram column dynamically — fixture uses
        // `request_latency:buckets`. h2_quantile signature is
        // `(buckets, quantile_double)`.
        parity_query(
            &path,
            &live,
            "SELECT timestamp, h2_total(\"request_latency:buckets\") AS total \
             FROM _src ORDER BY timestamp",
        );
        parity_query(
            &path,
            &live,
            "SELECT timestamp, h2_quantile(\"request_latency:buckets\", 0.5::DOUBLE) AS p50 \
             FROM _src ORDER BY timestamp",
        );
        parity_query(
            &path,
            &live,
            "SELECT timestamp, h2_quantile(\"request_latency:buckets\", 0.99::DOUBLE) AS p99 \
             FROM _src ORDER BY timestamp",
        );
    }

    #[test]
    fn parity_gauge_basic_passthrough() {
        let path = fixture_path("gauge_basic.parquet");
        let live = replay_parquet_into_live(&path);

        parity_query(&path, &live, "SELECT * FROM _src ORDER BY timestamp");
        parity_query(&path, &live, "SELECT COUNT(*) FROM _src");
    }

    #[test]
    fn parity_rezolus_minimal_realistic_workload() {
        // The most realistic single-source fixture — actual Rezolus
        // sampler output. If anything is going to diverge, it's here.
        let path = fixture_path("rezolus_minimal.parquet");
        let live = replay_parquet_into_live(&path);

        parity_query(&path, &live, "SELECT COUNT(*) FROM _src");
        parity_query(
            &path,
            &live,
            "SELECT MIN(timestamp), MAX(timestamp) FROM _src",
        );
        // Use a star query so any silent column-set discrepancy
        // surfaces (`_src` order matches between paths because both
        // are SELECT *; on the live path, columns are BTreeMap-ordered
        // which equals lex order; on the parquet path, columns are
        // parquet-schema order. If they differ, we'd want to know).
        parity_query(
            &path,
            &live,
            "SELECT * FROM _src ORDER BY timestamp LIMIT 5",
        );
    }
}
