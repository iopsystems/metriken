use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::io::Write;
use std::ops::*;
use std::path::Path;
use std::sync::{Arc, Mutex};

use arrow::datatypes::DataType;
use bytes::Bytes;
use duckdb::types::Value as DuckValue;
use histogram::{CumulativeROHistogram, Histogram};
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};
use serde::Serialize;
use tokio::task::JoinSet;

/// How many DuckDB connections (and therefore concurrent column fetches) the
/// load path runs in parallel. DuckDB connections are single-threaded, so this
/// caps how many SELECTs can be in flight at once. 4 is a balance between
/// parallelism and not over-subscribing DuckDB's internal parquet-reader
/// threads.
const DEFAULT_POOL_SIZE: usize = 4;

mod collection;
mod heatmap;
mod labels;
mod series;

pub use collection::*;
pub use heatmap::Heatmap;
pub use labels::Labels;
pub use series::*;
use series::{delta_to_32_or_empty, empty_delta_32};

/// Per-column dispatch target precomputed from the parquet schema.
/// `column_name` is the parquet field name, preserved so callers can map
/// a resolved series back to its on-disk column without re-parsing the
/// schema.
enum ColumnTarget {
    Skip,
    Counter {
        name: String,
        labels: Labels,
        column_name: String,
    },
    Gauge {
        name: String,
        labels: Labels,
        column_name: String,
    },
    Histogram {
        name: String,
        labels: Labels,
        column_name: String,
        grouping_power: u8,
        max_value_power: u8,
        config: Option<::histogram::Config>,
    },
}

/// Snap a nanosecond timestamp to the nearest multiple of `interval_ns`.
/// Returns the timestamp unchanged when `interval_ns` is zero (i.e. unknown).
fn snap_timestamp(ts: u64, interval_ns: u64) -> u64 {
    if interval_ns > 0 {
        ((ts + interval_ns / 2) / interval_ns) * interval_ns
    } else {
        ts
    }
}

/// Quote a parquet column name for inclusion in a DuckDB SQL statement.
/// SQL identifiers can't be bound as parameters, so callers building
/// queries over schema-derived column names must route every name
/// through here rather than interpolating raw. NUL bytes are rejected
/// because some SQL parsers treat them as string terminators; every
/// other character is permitted inside ANSI/DuckDB quoted identifiers as
/// long as embedded double quotes are doubled.
fn quote_ident(name: &str) -> Result<String, FetchError> {
    if name.as_bytes().contains(&0) {
        return Err(FetchError::BadIdentifier(format!(
            "column name contains NUL byte: {name:?}"
        )));
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

/// Error type for the per-column fetch path. Concrete (rather than
/// `Box<dyn Error>`) so it satisfies `Send + Sync + 'static` for
/// `JoinSet::spawn_blocking`.
#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error("duckdb error: {0}")]
    Duckdb(#[from] duckdb::Error),
    #[error("bad column identifier: {0}")]
    BadIdentifier(String),
    #[error("histogram bucket element was not UBIGINT")]
    BadHistogramBucket,
}

/// Small fixed-size pool of in-memory DuckDB connections. Connections are
/// stateless wrt parquet (every query is `read_parquet(?)`), so any
/// available connection can serve any column fetch. Acquire/release runs
/// inside `spawn_blocking`, so a `std::sync::Mutex` is correct — no async
/// awaiting while the mutex is held.
struct ConnPool {
    inner: Mutex<Vec<duckdb::Connection>>,
}

impl ConnPool {
    fn new(size: usize) -> Result<Self, duckdb::Error> {
        let conns: Result<Vec<_>, _> = (0..size)
            .map(|_| duckdb::Connection::open_in_memory())
            .collect();
        Ok(Self {
            inner: Mutex::new(conns?),
        })
    }

    /// Pop a connection from the pool. Callers must `release` it back
    /// when done. Pool exhaustion is impossible by construction —
    /// `run_pool_load` caps in-flight tasks at the pool size.
    fn acquire(&self) -> duckdb::Connection {
        self.inner
            .lock()
            .expect("conn pool mutex poisoned")
            .pop()
            .expect("conn pool exhausted — should be bounded by JoinSet size")
    }

    fn release(&self, conn: duckdb::Connection) {
        self.inner
            .lock()
            .expect("conn pool mutex poisoned")
            .push(conn);
    }
}

/// Untyped per-column rows returned from a worker task. Pairs with the
/// matching `ColumnTarget` so the post-fetch sync pass knows how to
/// fold each batch into the in-memory series.
enum RawRows {
    Counter(Vec<(u64, u64)>),
    Gauge(Vec<(u64, i64)>),
    Histogram(Vec<(u64, Option<Vec<u64>>)>),
}

fn fetch_counter_column(
    conn: &mut duckdb::Connection,
    parquet_path: &str,
    column_name: &str,
) -> Result<Vec<(u64, u64)>, FetchError> {
    let col = quote_ident(column_name)?;
    let sql = format!(
        "SELECT timestamp, {col} FROM read_parquet(?) \
         WHERE timestamp IS NOT NULL AND {col} IS NOT NULL \
         ORDER BY timestamp"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([parquet_path], |row| {
        Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?))
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn fetch_gauge_column(
    conn: &mut duckdb::Connection,
    parquet_path: &str,
    column_name: &str,
) -> Result<Vec<(u64, i64)>, FetchError> {
    let col = quote_ident(column_name)?;
    let sql = format!(
        "SELECT timestamp, {col} FROM read_parquet(?) \
         WHERE timestamp IS NOT NULL AND {col} IS NOT NULL \
         ORDER BY timestamp"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([parquet_path], |row| {
        Ok((row.get::<_, u64>(0)?, row.get::<_, i64>(1)?))
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn fetch_histogram_column(
    conn: &mut duckdb::Connection,
    parquet_path: &str,
    column_name: &str,
) -> Result<Vec<(u64, Option<Vec<u64>>)>, FetchError> {
    let col = quote_ident(column_name)?;
    // Null bucket lists are kept (not WHERE-filtered) so the post-fetch
    // pass still emits an explicit empty delta against `prev` at those
    // timestamps — matches the prior loader and keeps offset-based
    // timestamp axes aligned.
    let sql = format!(
        "SELECT timestamp, {col} FROM read_parquet(?) \
         WHERE timestamp IS NOT NULL \
         ORDER BY timestamp"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([parquet_path], |row| {
        Ok((row.get::<_, u64>(0)?, row.get::<_, DuckValue>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (ts, val) = r?;
        let buckets = match val {
            DuckValue::List(elems) => {
                let mut buckets = Vec::with_capacity(elems.len());
                for v in elems {
                    match v {
                        DuckValue::UBigInt(x) => buckets.push(x),
                        DuckValue::Null => buckets.push(0),
                        _ => return Err(FetchError::BadHistogramBucket),
                    }
                }
                Some(buckets)
            }
            _ => None,
        };
        out.push((ts, buckets));
    }
    Ok(out)
}

fn fetch_dispatch(
    conn: &mut duckdb::Connection,
    parquet_path: &str,
    target: &ColumnTarget,
) -> Result<RawRows, FetchError> {
    match target {
        ColumnTarget::Counter { column_name, .. } => {
            fetch_counter_column(conn, parquet_path, column_name).map(RawRows::Counter)
        }
        ColumnTarget::Gauge { column_name, .. } => {
            fetch_gauge_column(conn, parquet_path, column_name).map(RawRows::Gauge)
        }
        ColumnTarget::Histogram { column_name, .. } => {
            fetch_histogram_column(conn, parquet_path, column_name).map(RawRows::Histogram)
        }
        ColumnTarget::Skip => panic!("Skip targets must be filtered out before dispatch"),
    }
}

/// Fold one fetched column's raw rows into the in-memory `Tsdb`.
/// Mirrors the per-arm logic of the old synchronous loader exactly —
/// only the data source is different.
fn populate_target(data: &mut Tsdb, target: ColumnTarget, rows: RawRows, interval_ns: u64) {
    match (target, rows) {
        (
            ColumnTarget::Counter {
                name,
                labels,
                column_name,
            },
            RawRows::Counter(rows),
        ) => {
            data.columns
                .entry(name.clone())
                .or_default()
                .insert(labels.clone(), column_name);
            let series = data
                .counters
                .entry(name)
                .or_default()
                .entry(labels)
                .or_default();
            for (ts_raw, v) in rows {
                series.insert(snap_timestamp(ts_raw, interval_ns), v);
            }
        }
        (
            ColumnTarget::Gauge {
                name,
                labels,
                column_name,
            },
            RawRows::Gauge(rows),
        ) => {
            data.columns
                .entry(name.clone())
                .or_default()
                .insert(labels.clone(), column_name);
            let series = data
                .gauges
                .entry(name)
                .or_default()
                .entry(labels)
                .or_default();
            for (ts_raw, v) in rows {
                series.insert(snap_timestamp(ts_raw, interval_ns), v);
            }
        }
        (
            ColumnTarget::Histogram {
                name,
                labels,
                column_name,
                grouping_power,
                max_value_power,
                config,
            },
            RawRows::Histogram(rows),
        ) => {
            data.columns
                .entry(name.clone())
                .or_default()
                .insert(labels.clone(), column_name);
            let series = data
                .histograms
                .entry(name)
                .or_default()
                .entry(labels)
                .or_default();

            let mut prev: Option<CumulativeROHistogram> = None;
            for (ts_raw, buckets) in rows {
                let ts = snap_timestamp(ts_raw, interval_ns);
                let curr = buckets.and_then(|b| {
                    Histogram::from_buckets(grouping_power, max_value_power, b)
                        .ok()
                        .map(|h| CumulativeROHistogram::from(&h))
                });
                match (prev.as_ref(), curr.as_ref()) {
                    (Some(p), Some(c)) => series.insert(ts, delta_to_32_or_empty(p, c)),
                    (Some(_), None) => {
                        if let Some(cfg) = config {
                            series.insert(ts, empty_delta_32(cfg));
                        }
                    }
                    _ => {}
                }
                if curr.is_some() {
                    prev = curr;
                }
            }
        }
        _ => panic!("fetch_dispatch must return RawRows matching the ColumnTarget kind"),
    }
}

/// Extract file-level kv metadata and populate `data`'s metadata fields.
fn populate_metadata(data: &mut Tsdb, meta: &ArrowReaderMetadata) {
    let mut kv = HashMap::new();
    if let Some(entries) = meta.metadata().file_metadata().key_value_metadata() {
        for entry in entries {
            kv.insert(entry.key.clone(), entry.value.clone().unwrap_or_default());
        }
    }
    data.sampling_interval_ms = kv
        .get("sampling_interval_ms")
        .map(|v| v.parse::<u64>().expect("bad interval"))
        .unwrap_or(1000);
    data.source = kv
        .get("source")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    data.version = kv
        .get("version")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    data.file_metadata = kv;
}

/// Classify each parquet column into a `ColumnTarget` based on its
/// Arrow field metadata. DuckDB doesn't surface per-field metadata
/// through SQL, so this stays on the `parquet` crate path.
fn classify_columns(meta: &ArrowReaderMetadata) -> Result<Vec<ColumnTarget>, Box<dyn Error>> {
    let schema = meta.schema();
    let ts_col_idx = schema
        .index_of("timestamp")
        .map_err(|_| "missing 'timestamp' column")?;

    Ok(schema
        .fields()
        .iter()
        .enumerate()
        .map(|(col_idx, field)| {
            if col_idx == ts_col_idx {
                return ColumnTarget::Skip;
            }
            let mut field_meta = field.metadata().clone();
            let column_name = field.name().to_string();
            let name = if let Some(n) = field_meta.get("metric").cloned() {
                n
            } else {
                column_name
                    .strip_suffix(":buckets")
                    .unwrap_or(&column_name)
                    .to_string()
            };
            let grouping_power: Option<u8> = field_meta
                .remove("grouping_power")
                .and_then(|v| v.parse().ok());
            let max_value_power: Option<u8> = field_meta
                .remove("max_value_power")
                .and_then(|v| v.parse().ok());

            let mut labels = Labels::default();
            for (k, v) in field_meta.iter() {
                match k.as_str() {
                    // Internal metadata — not user-facing labels
                    "metric" | "metric_type" | "unit" => continue,
                    _ => {
                        labels.inner.insert(k.to_string(), v.to_string());
                    }
                }
            }

            match field.data_type() {
                DataType::UInt64 => ColumnTarget::Counter {
                    name,
                    labels,
                    column_name,
                },
                DataType::Int64 => ColumnTarget::Gauge {
                    name,
                    labels,
                    column_name,
                },
                DataType::List(inner) if inner.data_type() == &DataType::UInt64 => {
                    let (Some(gp), Some(mvp)) = (grouping_power, max_value_power) else {
                        return ColumnTarget::Skip;
                    };
                    let config = ::histogram::Config::new(gp, mvp).ok();
                    ColumnTarget::Histogram {
                        name,
                        labels,
                        column_name,
                        grouping_power: gp,
                        max_value_power: mvp,
                        config,
                    }
                }
                _ => ColumnTarget::Skip,
            }
        })
        .collect())
}

/// Spawn one column-fetch task onto the JoinSet. Factored out so both
/// the priming and the replenish sites in `run_pool_load` use the
/// exact same closure shape.
fn spawn_fetch(
    set: &mut JoinSet<Result<(ColumnTarget, RawRows), FetchError>>,
    target: ColumnTarget,
    pool: Arc<ConnPool>,
    parquet_path: Arc<str>,
    tempfile_keepalive: Option<Arc<tempfile::NamedTempFile>>,
) {
    set.spawn_blocking(move || {
        let _keepalive = tempfile_keepalive;
        let mut conn = pool.acquire();
        let result = fetch_dispatch(&mut conn, &parquet_path, &target);
        pool.release(conn);
        result.map(|rows| (target, rows))
    });
}

#[derive(Default, Clone)]
pub struct Tsdb {
    sampling_interval_ms: u64,
    source: String,
    version: String,
    filename: String,
    file_metadata: HashMap<String, String>,
    counters: HashMap<String, CounterCollection>,
    gauges: HashMap<String, GaugeCollection>,
    histograms: HashMap<String, HistogramCollection>,
    /// Parquet column name for each loaded `(metric_name, labels)`
    /// pair: populated from `field.name()` on the parquet load path,
    /// synthesized on the ingest path. Consumed by
    /// `QueryEngine::columns()`.
    columns: HashMap<String, HashMap<Labels, String>>,
    /// Most-recent cumulative per series; `ingest` differences against the
    /// next snapshot to produce the per-period delta. Unused on the parquet
    /// load path (which differences in-place).
    #[cfg(feature = "ingest")]
    prev_histograms: HashMap<String, HashMap<Labels, CumulativeROHistogram>>,
}

impl Tsdb {
    /// Load a parquet file into an in-memory TSDB. DuckDB reads the file
    /// directly from `path` (no extra in-memory copy) and per-column
    /// SELECTs run on a small connection pool concurrently. Requires a
    /// tokio runtime to await.
    pub async fn load(path: &Path) -> Result<Self, Box<dyn Error>> {
        let filename = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("unknown")
            .to_string();
        let path_str: Arc<str> =
            Arc::from(path.to_str().ok_or("path is not valid UTF-8")?);

        // Footer read is small (KB) but still blocking I/O; off the
        // async runtime. `parquet::errors::ParquetError` has a From
        // impl for io::Error, so both possible failure modes
        // collapse into one concrete error type that `?` can box.
        let path_owned = path.to_owned();
        let meta = tokio::task::spawn_blocking(
            move || -> Result<_, parquet::errors::ParquetError> {
                let file = std::fs::File::open(&path_owned)?;
                let m = ArrowReaderMetadata::load(&file, ArrowReaderOptions::default())?;
                Ok(m)
            },
        )
        .await??;

        let mut data = Tsdb::default();
        populate_metadata(&mut data, &meta);
        let targets = classify_columns(&meta)?;

        Self::run_pool_load(&mut data, path_str, targets, None).await?;
        data.filename = filename;
        Ok(data)
    }

    /// Load from an in-memory parquet buffer. DuckDB requires a
    /// filesystem path, so the bytes are materialized to a tempfile
    /// that's held alive until every column fetch finishes. Requires a
    /// tokio runtime to await.
    pub async fn load_from_bytes(bytes: Bytes) -> Result<Self, Box<dyn Error>> {
        // Schema discovery is cheap and can happen on `bytes` directly
        // before we move them into the tempfile-writing task.
        let meta = ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::default())?;
        let mut data = Tsdb::default();
        populate_metadata(&mut data, &meta);
        let targets = classify_columns(&meta)?;

        // Tempfile creation involves blocking syscalls; do them on the
        // blocking pool.
        let tempfile = tokio::task::spawn_blocking(move || -> Result<_, std::io::Error> {
            let mut tmp = tempfile::NamedTempFile::new()?;
            tmp.as_file_mut().write_all(&bytes)?;
            tmp.as_file_mut().sync_all()?;
            Ok(tmp)
        })
        .await??;

        let parquet_path: Arc<str> = Arc::from(
            tempfile
                .path()
                .to_str()
                .ok_or("tempfile path is not valid UTF-8")?
                .to_string()
                .as_str(),
        );
        let tempfile = Arc::new(tempfile);

        Self::run_pool_load(&mut data, parquet_path, targets, Some(tempfile)).await?;
        Ok(data)
    }

    /// Drive the per-column fetch loop. A small pool of DuckDB
    /// connections is created and reused across columns; a JoinSet
    /// keeps at most `DEFAULT_POOL_SIZE` tasks in flight at any time so
    /// the pool can never be exhausted. Each completed task's rows are
    /// folded into `data` synchronously before the next task is
    /// scheduled.
    async fn run_pool_load(
        data: &mut Tsdb,
        parquet_path: Arc<str>,
        targets: Vec<ColumnTarget>,
        tempfile_keepalive: Option<Arc<tempfile::NamedTempFile>>,
    ) -> Result<(), Box<dyn Error>> {
        let interval_ns = data.sampling_interval_ms * 1_000_000;
        let pool = Arc::new(ConnPool::new(DEFAULT_POOL_SIZE)?);

        // Drop Skip targets up front; reverse so `pop()` yields the
        // remaining targets in their original (schema) order. Order
        // doesn't affect correctness — fetches are independent across
        // columns and histogram `prev` lives inside one column — but
        // keeping schema order makes any debug output predictable.
        let mut targets: Vec<ColumnTarget> = targets
            .into_iter()
            .filter(|t| !matches!(t, ColumnTarget::Skip))
            .collect();
        targets.reverse();

        let mut set: JoinSet<Result<(ColumnTarget, RawRows), FetchError>> = JoinSet::new();

        // Prime: spawn up to POOL_SIZE tasks immediately.
        for _ in 0..DEFAULT_POOL_SIZE.min(targets.len()) {
            let target = targets.pop().expect("min ensures non-empty");
            spawn_fetch(
                &mut set,
                target,
                pool.clone(),
                parquet_path.clone(),
                tempfile_keepalive.clone(),
            );
        }

        // Drain: as each task finishes, fold its rows in and replenish
        // from the remaining targets. JoinSet hands back results in
        // completion order, not submission order — fine, populate_target
        // is independent across columns.
        while let Some(join_result) = set.join_next().await {
            let (target, rows) = join_result??;
            populate_target(data, target, rows, interval_ns);

            if let Some(next) = targets.pop() {
                spawn_fetch(
                    &mut set,
                    next,
                    pool.clone(),
                    parquet_path.clone(),
                    tempfile_keepalive.clone(),
                );
            }
        }

        Ok(())
    }

    pub fn set_sampling_interval_ms(&mut self, ms: u64) {
        self.sampling_interval_ms = ms;
    }

    pub fn set_source(&mut self, source: String) {
        self.source = source;
    }

    pub fn set_version(&mut self, version: String) {
        self.version = version;
    }

    pub fn set_filename(&mut self, filename: String) {
        self.filename = filename;
    }

    /// Ingest a snapshot from a running agent, inserting all metrics into the
    /// TSDB.
    #[cfg(feature = "ingest")]
    pub fn ingest(&mut self, mut snapshot: metriken_exposition::Snapshot) {
        let raw_ts = snapshot
            .systemtime()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("system clock is earlier than 1970")
            .as_nanos() as u64;

        // Snap to the nearest sampling interval boundary so that metrics
        // from different samplers within the same collection cycle share
        // an identical timestamp.
        let interval_ns = self.sampling_interval_ms * 1_000_000;
        let ts = snap_timestamp(raw_ts, interval_ns);

        for counter in snapshot.counters() {
            let (name, labels) = Self::extract_name_labels(&counter.metadata);
            self.columns
                .entry(name.clone())
                .or_default()
                .entry(labels.clone())
                .or_insert_with(|| name.clone());
            self.counters
                .entry(name)
                .or_default()
                .entry(labels)
                .or_default()
                .insert(ts, counter.value);
        }

        for gauge in snapshot.gauges() {
            let (name, labels) = Self::extract_name_labels(&gauge.metadata);
            self.columns
                .entry(name.clone())
                .or_default()
                .entry(labels.clone())
                .or_insert_with(|| name.clone());
            self.gauges
                .entry(name)
                .or_default()
                .entry(labels)
                .or_default()
                .insert(ts, gauge.value);
        }

        for histogram in snapshot.histograms() {
            let (name, labels) = Self::extract_name_labels(&histogram.metadata);
            let curr = CumulativeROHistogram::from(&histogram.value);

            let prev_for_metric = self.prev_histograms.entry(name.clone()).or_default();

            if let Some(prev) = prev_for_metric.get(&labels) {
                let d = delta_to_32_or_empty(prev, &curr);
                self.columns
                    .entry(name.clone())
                    .or_default()
                    .entry(labels.clone())
                    .or_insert_with(|| format!("{name}:buckets"));
                self.histograms
                    .entry(name.clone())
                    .or_default()
                    .entry(labels.clone())
                    .or_default()
                    .insert(ts, d);
            }

            prev_for_metric.insert(labels, curr);
        }
    }

    /// Extract the metric name and labels from snapshot metric metadata.
    #[cfg(feature = "ingest")]
    fn extract_name_labels(metadata: &HashMap<String, String>) -> (String, Labels) {
        let name = metadata.get("metric").cloned().unwrap_or_default();

        let mut labels = Labels::default();
        for (k, v) in metadata {
            match k.as_str() {
                "metric" | "unit" | "grouping_power" | "max_value_power" => continue,
                _ => {
                    labels.inner.insert(k.clone(), v.clone());
                }
            }
        }

        (name, labels)
    }

    /// Borrow the raw counter collection without cloning, so streaming
    /// iterator chains can reference TSDB storage directly.
    pub fn counters_ref(&self, name: &str) -> Option<&CounterCollection> {
        self.counters.get(name)
    }

    /// See [`Tsdb::counters_ref`].
    pub fn gauges_ref(&self, name: &str) -> Option<&GaugeCollection> {
        self.gauges.get(name)
    }

    /// See [`Tsdb::counters_ref`].
    pub fn histograms_ref(&self, name: &str) -> Option<&HistogramCollection> {
        self.histograms.get(name)
    }

    /// Parquet column name for the `(metric_name, labels)` pair, or
    /// `None` if no such series was loaded.
    pub fn column(&self, name: &str, labels: &Labels) -> Option<&str> {
        self.columns.get(name)?.get(labels).map(String::as_str)
    }

    /// Iteration-friendly view of the column map, for resolvers that
    /// need to scan every series (e.g. regex / negated `__name__`).
    pub(crate) fn columns_ref(&self) -> &HashMap<String, HashMap<Labels, String>> {
        &self.columns
    }

    // sampling interval in seconds
    pub fn interval(&self) -> f64 {
        self.sampling_interval_ms as f64 / 1000.0
    }

    /// Returns the time range (min, max) in nanoseconds across all data, or
    /// None if empty.
    pub fn time_range(&self) -> Option<(u64, u64)> {
        let mut min_time: Option<u64> = None;
        let mut max_time: Option<u64> = None;

        for collection in self.counters.values() {
            if let Some((coll_min, coll_max)) = collection.time_bounds() {
                min_time = Some(min_time.map_or(coll_min, |m| m.min(coll_min)));
                max_time = Some(max_time.map_or(coll_max, |m| m.max(coll_max)));
            }
        }

        for collection in self.gauges.values() {
            if let Some((coll_min, coll_max)) = collection.time_bounds() {
                min_time = Some(min_time.map_or(coll_min, |m| m.min(coll_min)));
                max_time = Some(max_time.map_or(coll_max, |m| m.max(coll_max)));
            }
        }

        for collection in self.histograms.values() {
            if let Some((coll_min, coll_max)) = collection.time_bounds() {
                min_time = Some(min_time.map_or(coll_min, |m| m.min(coll_min)));
                max_time = Some(max_time.map_or(coll_max, |m| m.max(coll_max)));
            }
        }

        min_time.zip(max_time)
    }

    // data source
    pub fn source(&self) -> &str {
        &self.source
    }

    // data source version
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn filename(&self) -> &str {
        &self.filename
    }

    pub fn file_metadata(&self) -> &HashMap<String, String> {
        &self.file_metadata
    }

    // Get all counter metric names
    pub fn counter_names(&self) -> Vec<&str> {
        self.counters.keys().map(|s| s.as_str()).collect()
    }

    // Get all gauge metric names
    pub fn gauge_names(&self) -> Vec<&str> {
        self.gauges.keys().map(|s| s.as_str()).collect()
    }

    // Get all histogram metric names
    pub fn histogram_names(&self) -> Vec<&str> {
        self.histograms.keys().map(|s| s.as_str()).collect()
    }

    // Get labels for a specific counter metric
    pub fn counter_labels(&self, name: &str) -> Option<Vec<Labels>> {
        self.counters.get(name).map(|collection| {
            collection
                .iter()
                .map(|(labels, _)| labels.clone())
                .collect()
        })
    }

    // Get labels for a specific gauge metric
    pub fn gauge_labels(&self, name: &str) -> Option<Vec<Labels>> {
        self.gauges.get(name).map(|collection| {
            collection
                .iter()
                .map(|(labels, _)| labels.clone())
                .collect()
        })
    }

    // Get labels for a specific histogram metric
    pub fn histogram_labels(&self, name: &str) -> Option<Vec<Labels>> {
        self.histograms.get(name).map(|collection| {
            collection
                .iter()
                .map(|(labels, _)| labels.clone())
                .collect()
        })
    }
}

#[cfg(all(test, feature = "ingest"))]
mod ingest_tests {
    use std::time::{Duration, SystemTime};

    use histogram::Histogram;
    use metriken_exposition::{Histogram as SnapHistogram, Snapshot, SnapshotV2};

    use super::*;

    fn snapshot_at(ts_secs: u64, hist: Histogram, name: &str) -> Snapshot {
        let systemtime = SystemTime::UNIX_EPOCH + Duration::from_secs(ts_secs);
        let mut metadata = HashMap::new();
        metadata.insert("metric".to_string(), name.to_string());
        Snapshot::V2(SnapshotV2 {
            systemtime,
            duration: Duration::from_secs(1),
            metadata: HashMap::new(),
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: vec![SnapHistogram {
                name: name.to_string(),
                value: hist,
                metadata,
            }],
        })
    }

    /// End-to-end check that the ingest path preserves every observed
    /// snapshot's timestamp, even when the per-period delta is empty (no
    /// events) or unrepresentable (counter reset).  Without explicit empties
    /// the offset-aligned lookup pattern would silently shift entries onto
    /// the wrong timestamps.
    #[test]
    fn ingest_preserves_timestamps_for_empty_and_reset_deltas() {
        let mut tsdb = Tsdb {
            sampling_interval_ms: 1000,
            ..Tsdb::default()
        };

        // Construct four snapshots of one histogram metric.  Bucket index 10
        // grows: 0 → 5 → 5 (no events) → 8 → 1 (reset).
        let snapshots: Vec<(u64, &[u32])> = vec![
            (1, &[]),                               // first cumulative — no delta produced
            (2, &[10, 10, 10, 10, 10]),             // 5 events bucket 10 (vs s1)
            (3, &[10, 10, 10, 10, 10]),             // identical → empty delta
            (4, &[10, 10, 10, 10, 10, 10, 10, 10]), // +3 events vs s3
            (5, &[10]),                             // reset (cumu went down) → empty delta
        ];

        for (ts, samples) in &snapshots {
            let mut h = Histogram::new(4, 16).unwrap();
            for v in *samples {
                h.increment(*v as u64).unwrap();
            }
            tsdb.ingest(snapshot_at(*ts, h, "lat"));
        }

        let collection = tsdb.histograms_ref("lat").expect("histogram series exists");
        let (_, series) = collection.iter().next().expect("one labelset");

        // s1 produces no delta; s2..s5 each produce one.  4 entries expected.
        let times: Vec<u64> = series.iter().map(|(t, _)| t).collect();
        let expected: Vec<u64> = vec![2_000_000_000, 3_000_000_000, 4_000_000_000, 5_000_000_000];
        assert_eq!(
            times, expected,
            "every observed snapshot timestamp must be present"
        );

        // Spot-check empties: s2->s3 had no new events, s4->s5 reset.
        let entry = |ts: u64| {
            series
                .iter()
                .find(|(t, _)| *t == ts)
                .map(|(_, h)| h)
                .unwrap()
        };
        assert!(entry(3_000_000_000).is_empty(), "no-event delta is empty");
        assert!(entry(5_000_000_000).is_empty(), "reset delta is empty");
        assert!(!entry(2_000_000_000).is_empty());
        assert!(!entry(4_000_000_000).is_empty());
    }
}
