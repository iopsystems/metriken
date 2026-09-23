use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::memory::Memory;
use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::types::{Counter, Counters, Gauge, Gauges, Histogram, HistogramSnapshot};
use crate::{DataSource, MetricsSource, QueryOptions};

// ─── Public entry point ───────────────────────────────────────────────────────

/// In-memory queryable metric store. Use this for live ingestion of snapshots
/// (e.g. polling a running rezolus agent).
///
/// Cheaply cloneable via `Arc`. All methods are thread-safe: multiple readers
/// may query concurrently and a single writer may ingest new snapshots.
#[derive(Clone)]
pub struct MemoryStore {
    state: Arc<MemoryStoreInner>,
}

pub(crate) struct MemoryStoreInner {
    pub(crate) memory: RwLock<Memory>,
    pub(crate) metadata: RwLock<HashMap<String, String>>,
    pub(crate) filename: RwLock<Option<String>>,
}

impl MemoryStore {
    /// Create a builder for configuring and constructing a `MemoryStore`.
    pub fn builder() -> MemoryStoreBuilder {
        MemoryStoreBuilder::default()
    }

    /// Test helper: wrap a pre-built `MemoryStoreInner`.
    #[cfg(test)]
    pub(crate) fn from_inner(inner: Arc<MemoryStoreInner>) -> Self {
        Self { state: inner }
    }

    /// This store as a union child's data source.
    pub(crate) fn data_source(&self) -> Arc<dyn DataSource> {
        Arc::clone(&self.state) as Arc<dyn DataSource>
    }

    /// Add a whole counter series.
    ///
    /// For a caller that assembled series itself — a reader that split a
    /// table's columns by occupant, say — rather than one feeding snapshots
    /// through `ingest_snapshot`. `windows` are the per-sample acquisition
    /// windows `(begin_ns, end_ns)` that `rate()`/`irate()` turn into
    /// uncertainty bounds; a source that has them should pass them, since
    /// the engine cannot reconstruct them. `timestamps` must be ascending.
    /// A second series with the same name and labels is a second series;
    /// nothing is merged.
    pub fn insert_counter_series(
        &self,
        name: &str,
        labels: impl Into<Labels>,
        timestamps: Vec<u64>,
        values: Vec<u64>,
        windows: Option<Vec<(u64, u64)>>,
    ) -> Result<(), String> {
        check_series(
            name,
            timestamps.len(),
            values.len(),
            windows.as_ref().map(Vec::len),
        )?;
        self.state.memory.write().unwrap().push_counter_series(
            name,
            Counter {
                labels: labels.into(),
                timestamps,
                values,
                windows,
            },
        );
        Ok(())
    }

    /// Add a whole gauge series. See [`insert_counter_series`](Self::insert_counter_series).
    pub fn insert_gauge_series(
        &self,
        name: &str,
        labels: impl Into<Labels>,
        timestamps: Vec<u64>,
        values: Vec<i64>,
        windows: Option<Vec<(u64, u64)>>,
    ) -> Result<(), String> {
        check_series(
            name,
            timestamps.len(),
            values.len(),
            windows.as_ref().map(Vec::len),
        )?;
        self.state.memory.write().unwrap().push_gauge_series(
            name,
            Gauge {
                labels: labels.into(),
                timestamps,
                values,
                windows,
            },
        );
        Ok(())
    }

    /// Add a whole histogram series: one cumulative sparse snapshot per
    /// sample, all under one bucket configuration. See
    /// [`insert_counter_series`](Self::insert_counter_series).
    pub fn insert_histogram_series(
        &self,
        name: &str,
        labels: impl Into<Labels>,
        config: ::histogram::Config,
        timestamps: Vec<u64>,
        snapshots: Vec<HistogramSnapshot>,
    ) -> Result<(), String> {
        check_series(name, timestamps.len(), snapshots.len(), None)?;
        self.state.memory.write().unwrap().push_histogram_series(
            name,
            Histogram {
                labels: labels.into(),
                config,
                timestamps,
                snapshots,
            },
        );
        Ok(())
    }

    /// Declare the row timestamps this store stands for, for a caller that
    /// assembled it from a table and knows its rows. Without this a store
    /// reports the union of its series' timestamps, which cannot include a
    /// row every series skipped.
    pub fn set_sample_timestamps(&self, timestamps: Vec<u64>) {
        self.state
            .memory
            .write()
            .unwrap()
            .set_sample_timestamps(timestamps);
    }

    /// Set or replace the display name.
    pub fn set_filename(&self, name: impl Into<String>) {
        *self.state.filename.write().unwrap() = Some(name.into());
    }

    /// Return the display name, if set.
    pub fn filename(&self) -> Option<String> {
        self.state.filename.read().unwrap().clone()
    }

    /// Set or replace the `source` metadata key.
    pub fn set_source(&self, source: impl Into<String>) {
        self.state
            .metadata
            .write()
            .unwrap()
            .insert("source".to_string(), source.into());
    }

    /// Set or replace the `version` metadata key.
    pub fn set_version(&self, version: impl Into<String>) {
        self.state
            .metadata
            .write()
            .unwrap()
            .insert("version".to_string(), version.into());
    }

    /// Set an arbitrary metadata key.
    pub fn set_metadata(&self, key: impl Into<String>, value: impl Into<String>) {
        self.state
            .metadata
            .write()
            .unwrap()
            .insert(key.into(), value.into());
    }

    /// Update the sampling interval (in milliseconds). Useful when the interval
    /// is discovered after construction (e.g. from an agent banner).
    pub fn set_sampling_interval_ms(&self, ms: u64) {
        self.state.memory.write().unwrap().set_interval_ms(ms);
    }

    fn engine(&self) -> QueryEngine {
        let source: Arc<dyn DataSource> = self.state.clone();
        QueryEngine::new(source)
    }

    // ─── Public query API ─────────────────────────────────────────────────────

    /// Execute a PromQL range query.
    pub fn query_range(
        &self,
        expr: &str,
        start: f64,
        end: f64,
        step: f64,
    ) -> Result<QueryResult, QueryError> {
        self.engine().query_range(expr, start, end, step)
    }

    /// Range query with explicit [`QueryOptions`] (e.g. a non-default
    /// [`crate::RateMode`]).
    pub fn query_range_opts(
        &self,
        expr: &str,
        start: f64,
        end: f64,
        step: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        self.engine().query_range_opts(expr, start, end, step, opts)
    }

    /// Execute an instant PromQL query at a single timestamp.
    /// Uses the latest available timestamp when `time` is `None`.
    pub fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        self.engine().query(expr, time)
    }

    /// Resolve a PromQL query to the set of column names it touches,
    /// without reading any values.
    pub fn columns(&self, query: &str) -> Result<HashSet<String>, QueryError> {
        self.engine().columns(query)
    }

    /// Full time extent of the stored data in seconds, or `None` if empty.
    pub fn time_range(&self) -> Option<(f64, f64)> {
        self.state
            .memory
            .read()
            .unwrap()
            .time_range()
            .map(|(lo, hi)| (lo as f64 / 1e9, hi as f64 / 1e9))
    }

    /// Full time extent of the stored data in nanoseconds, or `None` if empty.
    ///
    /// Prefer this over [`time_range()`](Self::time_range) when you need exact
    /// nanosecond timestamps without floating-point precision loss.
    pub fn time_range_ns(&self) -> Option<(u64, u64)> {
        self.state.memory.read().unwrap().time_range()
    }

    /// Sampling interval in seconds.
    pub fn interval(&self) -> f64 {
        self.state.memory.read().unwrap().interval()
    }

    /// Look up a single metadata value by key.
    pub fn metadata_get(&self, key: &str) -> Option<String> {
        self.state.metadata.read().unwrap().get(key).cloned()
    }

    /// Convenience: the `source` key from metadata. Returns an empty string if absent.
    pub fn source(&self) -> String {
        self.metadata_get("source").unwrap_or_default()
    }

    /// Convenience: the `version` key from metadata. Returns an empty string if absent.
    pub fn version(&self) -> String {
        self.metadata_get("version").unwrap_or_default()
    }

    /// Key-value metadata for this store.
    pub fn file_metadata(&self) -> HashMap<String, String> {
        self.state.metadata.read().unwrap().clone()
    }

    /// Names of all counter metrics (sorted, deduplicated).
    pub fn counter_names(&self) -> Vec<String> {
        self.state.memory.read().unwrap().counter_names()
    }

    /// Names of all gauge metrics (sorted, deduplicated).
    pub fn gauge_names(&self) -> Vec<String> {
        self.state.memory.read().unwrap().gauge_names()
    }

    /// Names of all histogram metrics (sorted, deduplicated).
    pub fn histogram_names(&self) -> Vec<String> {
        self.state.memory.read().unwrap().histogram_names()
    }

    /// All label combinations for the named counter metric. Empty if unknown.
    pub fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.state.memory.read().unwrap().counter_labels(name)
    }

    /// All label combinations for the named gauge metric. Empty if unknown.
    pub fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.state.memory.read().unwrap().gauge_labels(name)
    }

    /// All label combinations for the named histogram metric. Empty if unknown.
    pub fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.state.memory.read().unwrap().histogram_labels(name)
    }
}

// ─── Builder ──────────────────────────────────────────────────────────────────

/// Builder for [`MemoryStore`]. Sets metadata that's known at construction time.
#[derive(Default)]
pub struct MemoryStoreBuilder {
    source: Option<String>,
    version: Option<String>,
    sampling_interval_ms: Option<u64>,
    filename: Option<String>,
}

impl MemoryStoreBuilder {
    /// Set the `source` metadata key (e.g. `"rezolus"`).
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Set the `version` metadata key.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Set the sampling interval in milliseconds. Defaults to 1000 ms.
    pub fn sampling_interval_ms(mut self, ms: u64) -> Self {
        self.sampling_interval_ms = Some(ms);
        self
    }

    /// Set the display name.
    pub fn filename(mut self, name: impl Into<String>) -> Self {
        self.filename = Some(name.into());
        self
    }

    /// Construct the [`MemoryStore`].
    pub fn build(self) -> MemoryStore {
        let interval_ms = self.sampling_interval_ms.unwrap_or(1000);
        let memory = Memory::new(interval_ms);
        let mut metadata: HashMap<String, String> = HashMap::new();
        if let Some(s) = self.source {
            metadata.insert("source".to_string(), s);
        }
        if let Some(v) = self.version {
            metadata.insert("version".to_string(), v);
        }
        MemoryStore {
            state: Arc::new(MemoryStoreInner {
                memory: RwLock::new(memory),
                metadata: RwLock::new(metadata),
                filename: RwLock::new(self.filename),
            }),
        }
    }
}

// ─── DataSource on inner ──────────────────────────────────────────────────────

/// Implement `DataSource` on the inner so `QueryEngine` can hold
/// `Arc<MemoryStoreInner>` directly, without an extra allocation.
/// A series' parallel vectors must agree in length, or a sample would carry
/// another sample's value. Said at insert, where the caller can act on it,
/// rather than as an index panic inside a query.
fn check_series(
    name: &str,
    timestamps: usize,
    values: usize,
    windows: Option<usize>,
) -> Result<(), String> {
    if timestamps != values {
        return Err(format!(
            "series {name}: {timestamps} timestamps but {values} values"
        ));
    }
    if let Some(w) = windows {
        if w != timestamps {
            return Err(format!(
                "series {name}: {timestamps} timestamps but {w} windows"
            ));
        }
    }
    Ok(())
}

impl DataSource for MemoryStoreInner {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Counters> {
        // Nothing rounds a sample's timestamp, so `raw` is a no-op here.
        self.memory
            .read()
            .unwrap()
            .counters(name, filter, start_ns, end_ns)
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        self.memory
            .read()
            .unwrap()
            .gauges(name, filter, start_ns, end_ns)
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        self.memory
            .read()
            .unwrap()
            .histogram_stream(name, filter, start_ns, end_ns)
    }

    fn interval(&self) -> f64 {
        self.memory.read().unwrap().interval()
    }

    fn time_range(&self) -> Option<(u64, u64)> {
        self.memory.read().unwrap().time_range()
    }

    fn counter_names(&self) -> Vec<String> {
        self.memory.read().unwrap().counter_names()
    }

    fn gauge_names(&self) -> Vec<String> {
        self.memory.read().unwrap().gauge_names()
    }

    fn histogram_names(&self) -> Vec<String> {
        self.memory.read().unwrap().histogram_names()
    }

    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.memory.read().unwrap().counter_labels(name)
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.memory.read().unwrap().gauge_labels(name)
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.memory.read().unwrap().histogram_labels(name)
    }

    fn file_metadata(&self) -> HashMap<String, String> {
        self.metadata.read().unwrap().clone()
    }

    fn metadata_get(&self, key: &str) -> Option<String> {
        self.metadata.read().unwrap().get(key).cloned()
    }

    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        self.memory.read().unwrap().column_map()
    }

    fn sample_timestamps(&self) -> Vec<u64> {
        self.memory.read().unwrap().sample_timestamps()
    }
}

// ─── MetricsSource on MemoryStore ─────────────────────────────────────────────

impl MetricsSource for MemoryStore {
    fn query_range_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        MemoryStore::query_range_opts(self, expr, start_s, end_s, step_s, opts)
    }

    fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        MemoryStore::query(self, expr, time)
    }

    fn columns(&self, query: &str) -> Result<HashSet<String>, QueryError> {
        MemoryStore::columns(self, query)
    }

    fn time_range(&self) -> Option<(f64, f64)> {
        MemoryStore::time_range(self)
    }

    fn time_range_ns(&self) -> Option<(u64, u64)> {
        MemoryStore::time_range_ns(self)
    }

    fn interval(&self) -> f64 {
        MemoryStore::interval(self)
    }

    fn source(&self) -> String {
        MemoryStore::source(self)
    }

    fn version(&self) -> String {
        MemoryStore::version(self)
    }

    fn filename(&self) -> Option<String> {
        MemoryStore::filename(self)
    }

    fn metadata_get(&self, key: &str) -> Option<String> {
        MemoryStore::metadata_get(self, key)
    }

    fn file_metadata(&self) -> HashMap<String, String> {
        MemoryStore::file_metadata(self)
    }

    fn sample_timestamps(&self) -> Vec<u64> {
        DataSource::sample_timestamps(&*self.state)
    }

    fn counter_names(&self) -> Vec<String> {
        MemoryStore::counter_names(self)
    }

    fn gauge_names(&self) -> Vec<String> {
        MemoryStore::gauge_names(self)
    }

    fn histogram_names(&self) -> Vec<String> {
        MemoryStore::histogram_names(self)
    }

    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        MemoryStore::counter_labels(self, name)
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        MemoryStore::gauge_labels(self, name)
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        MemoryStore::histogram_labels(self, name)
    }
}

// ─── Ingest feature ──────────────────────────────────────────────────────────

#[cfg(feature = "ingest")]
impl MemoryStore {
    /// Ingest a single snapshot into the store, at the timestamp the snapshot
    /// carries. Every metric in one snapshot shares that timestamp, so they
    /// align by construction; this used to additionally round it to the
    /// store's nominal interval, which moved readings the producer had already
    /// timed exactly.
    ///
    /// For histograms: a `HistogramSnapshot` is stored representing the
    /// cumulative (running) bucket counts. Quantile/rate computations are
    /// performed at query time against pairs of consecutive snapshots.
    pub fn ingest_snapshot(&self, mut snapshot: metriken_exposition::Snapshot) {
        use crate::memory::extract_name_labels;

        let raw_ts = snapshot
            .systemtime()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is earlier than 1970")
            .as_nanos() as u64;

        let mut memory = self.state.memory.write().unwrap();
        let ts = raw_ts;

        for counter in snapshot.counters() {
            let (name, labels) = extract_name_labels(&counter.metadata, &counter.name);
            memory.upsert_counter_sample(&name, labels, ts, counter.value);
        }

        for gauge in snapshot.gauges() {
            let (name, labels) = extract_name_labels(&gauge.metadata, &gauge.name);
            memory.upsert_gauge_sample(&name, labels, ts, gauge.value);
        }

        for histogram in snapshot.histograms() {
            let (name, labels) = extract_name_labels(&histogram.metadata, &histogram.name);
            let config = histogram.value.config();
            let snap = histogram_to_snapshot(&histogram.value);
            memory.upsert_histogram_sample(&name, labels, config, ts, snap);
        }
    }
}

#[cfg(feature = "ingest")]
fn histogram_to_snapshot(h: &::histogram::Histogram) -> crate::types::HistogramSnapshot {
    let mut index = Vec::new();
    let mut count = Vec::new();
    let mut running: u64 = 0;
    for (i, bucket) in h.iter().enumerate() {
        let c = bucket.count();
        if c > 0 {
            running = running.saturating_add(c);
            index.push(i as u32);
            count.push(running);
        }
    }
    crate::types::HistogramSnapshot { index, count }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_store_builder_sets_metadata() {
        let store = MemoryStore::builder()
            .source("test-source")
            .version("1.0")
            .sampling_interval_ms(500)
            .build();
        assert_eq!(store.source(), "test-source");
        assert_eq!(store.version(), "1.0");
        assert_eq!(store.interval(), 0.5);
    }

    #[test]
    fn test_memory_store_set_metadata_after_build() {
        let store = MemoryStore::builder().build();
        store.set_source("rezolus");
        store.set_version("2.0");
        store.set_metadata("hostname", "host1");
        assert_eq!(store.source(), "rezolus");
        assert_eq!(store.version(), "2.0");
        let fm = store.file_metadata();
        assert_eq!(fm.get("hostname").map(String::as_str), Some("host1"));
    }

    #[test]
    fn test_memory_store_empty_returns_metric_not_found() {
        let store = MemoryStore::builder().build();
        let result = store.query_range("rate(does_not_exist[5m])", 0.0, 100.0, 1.0);
        assert!(result.is_err());
    }

    #[test]
    fn test_memory_store_implements_metrics_source() {
        fn _assert_metrics_source<T: MetricsSource>(_: &T) {}
        fn _check(s: &MemoryStore) {
            _assert_metrics_source(s);
        }
    }

    #[test]
    fn test_memory_store_clone_shares_state() {
        let store1 = MemoryStore::builder().source("original").build();
        let store2 = store1.clone();
        store1.set_source("updated");
        // Both clones share the same Arc, so store2 sees the update.
        assert_eq!(store2.source(), "updated");
    }

    #[test]
    fn test_memory_store_set_sampling_interval_ms() {
        let store = MemoryStore::builder().sampling_interval_ms(1000).build();
        assert_eq!(store.interval(), 1.0);
        store.set_sampling_interval_ms(250);
        assert_eq!(store.interval(), 0.25);
    }

    #[test]
    fn test_memory_store_empty_time_range_is_none() {
        let store = MemoryStore::builder().build();
        assert!(store.time_range().is_none());
    }

    #[test]
    fn test_memory_store_default_interval_is_one_second() {
        let store = MemoryStore::builder().build();
        assert_eq!(store.interval(), 1.0);
    }
}

#[cfg(test)]
#[cfg(feature = "ingest")]
mod ingest_tests {
    use std::collections::HashMap;
    use std::time::{Duration, SystemTime};

    fn make_counter_snap(
        ts: SystemTime,
        counter_name: &str,
        value: u64,
        labels: &[(&str, &str)],
    ) -> metriken_exposition::Snapshot {
        let mut metadata: HashMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        metadata.insert("metric".to_string(), counter_name.to_string());
        metriken_exposition::Snapshot::V2(metriken_exposition::SnapshotV2 {
            systemtime: ts,
            duration: Duration::from_secs(0),
            metadata: HashMap::new(),
            counters: vec![metriken_exposition::Counter::new(
                counter_name.to_string(),
                value,
                metadata,
            )],
            gauges: vec![],
            histograms: vec![],
        })
    }

    /// The live path and the recorded path must agree on which keys are
    /// labels. They did not: a histogram ingested from a snapshot kept
    /// `grouping_power` and `max_value_power` as labels, while the same
    /// histogram read from parquet had them stripped, so a recording carried
    /// two extra labels per histogram series when viewed live. Both loaders
    /// now go through `Labels::from_metadata`; this pins the live side.
    #[test]
    fn ingest_does_not_turn_histogram_configuration_into_labels() {
        let store = crate::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        let mut metadata: HashMap<String, String> = HashMap::new();
        metadata.insert("metric".to_string(), "latency".to_string());
        metadata.insert("metric_type".to_string(), "histogram".to_string());
        metadata.insert("unit".to_string(), "nanoseconds".to_string());
        metadata.insert("grouping_power".to_string(), "7".to_string());
        metadata.insert("max_value_power".to_string(), "64".to_string());
        metadata.insert("op".to_string(), "read".to_string());
        let mut h = ::histogram::Histogram::new(7, 64).unwrap();
        h.increment(1_000).unwrap();
        let snap = metriken_exposition::Snapshot::V2(metriken_exposition::SnapshotV2 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
            duration: Duration::from_secs(0),
            metadata: HashMap::new(),
            counters: vec![],
            gauges: vec![],
            histograms: vec![metriken_exposition::Histogram::new(
                "latency".to_string(),
                h,
                metadata,
            )],
        });
        store.ingest_snapshot(snap);

        let labels = store.histogram_labels("latency");
        assert_eq!(labels.len(), 1);
        let only: Vec<(&str, &str)> = labels[0]
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            only,
            vec![("op", "read")],
            "storage keys must not survive as labels: {:?}",
            labels[0]
        );
    }

    #[test]
    fn test_ingest_counter_basic() {
        let store = crate::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1001);
        let s0 = make_counter_snap(t0, "cpu_cycles", 100, &[("cpu", "0")]);
        let s1 = make_counter_snap(t1, "cpu_cycles", 200, &[("cpu", "0")]);
        store.ingest_snapshot(s0);
        store.ingest_snapshot(s1);

        assert!(store.counter_names().contains(&"cpu_cycles".to_string()));
        let labels = store.counter_labels("cpu_cycles");
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].get("cpu").map(String::as_str), Some("0"));

        // Time range covers both samples in seconds
        let (lo, hi) = store.time_range().unwrap();
        assert!((lo - 1000.0).abs() < 0.01);
        assert!((hi - 1001.0).abs() < 0.01);
    }

    #[test]
    fn test_ingest_two_series_different_labels() {
        let store = crate::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let s0 = make_counter_snap(t0, "cpu_cycles", 100, &[("cpu", "0")]);
        let s1 = make_counter_snap(t0, "cpu_cycles", 200, &[("cpu", "1")]);
        store.ingest_snapshot(s0);
        store.ingest_snapshot(s1);

        let labels = store.counter_labels("cpu_cycles");
        assert_eq!(labels.len(), 2);
    }

    fn make_gauge_snap(ts: SystemTime, name: &str, value: i64) -> metriken_exposition::Snapshot {
        let mut metadata: HashMap<String, String> = HashMap::new();
        metadata.insert("metric".to_string(), name.to_string());
        metriken_exposition::Snapshot::V2(metriken_exposition::SnapshotV2 {
            systemtime: ts,
            duration: Duration::from_secs(0),
            metadata: HashMap::new(),
            counters: vec![],
            gauges: vec![metriken_exposition::Gauge::new(
                name.to_string(),
                value,
                metadata,
            )],
            histograms: vec![],
        })
    }

    // Ingest a gauge with `n` 1s samples that are flat at `base` except for a
    // single spike to `spike` in the middle.
    fn store_with_spiky_gauge(n: u64, base: i64, spike: i64) -> (crate::MemoryStore, u64, u64) {
        let store = crate::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        let start = 1000;
        for i in 0..n {
            let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(start + i);
            let v = if i == n / 2 { spike } else { base };
            store.ingest_snapshot(make_gauge_snap(ts, "load", v));
        }
        let _ = store.interval();
        (store, start, start + n - 1)
    }

    #[test]
    fn query_range_display_decimates_matrix_and_preserves_spike() {
        use crate::{DisplayOptions, DisplayResult, MetricsSource, Reducer};
        let (store, lo, hi) = store_with_spiky_gauge(200, 10, 1000);

        let opts = DisplayOptions {
            budget: 10,
            ..Default::default()
        };
        let result = store
            .query_range_display("load", lo as f64, hi as f64, 1.0, &opts)
            .unwrap();

        match result {
            DisplayResult::Series { result, budget } => {
                assert_eq!(budget, 10);
                assert_eq!(result.len(), 1, "one series");
                let s = &result[0];
                assert!(
                    s.points.len() <= 10,
                    "bounded by budget: {}",
                    s.points.len()
                );
                assert_eq!(s.raw_points, 200, "raw sample count recorded");
                assert!(s.decimated, "200 samples -> 10 budget is a downsample");
                assert_eq!(s.native_interval, 1.0);
                assert_eq!(s.reducer, Reducer::Boxplot);
                assert_eq!(s.band, [0.25, 0.75], "default inner band is IQR");
                let max_max = s.points.iter().map(|p| p.max).fold(f64::MIN, f64::max);
                assert_eq!(max_max, 1000.0, "spike survives in max");
                // The spike (1 in 200) never moves a bucket median off baseline.
                for p in &s.points {
                    assert_eq!(p.median, 10.0, "median robust to the spike");
                    assert!(p.min <= p.lo && p.lo <= p.median);
                    assert!(p.median <= p.hi && p.hi <= p.max);
                }
            }
            other => panic!("expected Series, got {other:?}"),
        }
    }

    #[test]
    fn query_range_display_is_identity_when_under_budget() {
        use crate::{DisplayOptions, DisplayResult, MetricsSource};
        let (store, lo, hi) = store_with_spiky_gauge(50, 10, 1000);

        let opts = DisplayOptions {
            budget: 5000,
            ..Default::default()
        };
        let result = store
            .query_range_display("load", lo as f64, hi as f64, 1.0, &opts)
            .unwrap();

        match result {
            DisplayResult::Series { result, .. } => {
                let s = &result[0];
                assert!(
                    !s.decimated,
                    "50 samples under a 5000 budget is not decimated"
                );
                assert_eq!(s.points.len() as u64, s.raw_points);
                for p in &s.points {
                    // Identity: the boxplot of a single sample collapses.
                    assert_eq!((p.min, p.lo, p.median, p.hi, p.max), {
                        let v = p.median;
                        (v, v, v, v, v)
                    });
                }
            }
            other => panic!("expected Series, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod series_tests {
    use crate::{MetricsSource, QueryResult};

    const S: u64 = 1_000_000_000;

    fn matrix(r: Result<QueryResult, crate::QueryError>) -> Vec<crate::MatrixSample> {
        match r.expect("query resolves") {
            QueryResult::Matrix { result } => result,
            other => panic!("expected a matrix, got {other:?}"),
        }
    }

    /// The point of the series API: windows reach the engine. A counter
    /// series inserted with acquisition windows yields `rate()` bounds; the
    /// same series without them yields none. The ingest path could never do
    /// this, and a reader that assembles series itself must not lose it.
    #[test]
    fn windows_on_an_inserted_series_become_rate_bounds() {
        let with = crate::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        let without = crate::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        let ts: Vec<u64> = (0..6).map(|i| 1000 * S + i * S).collect();
        let values: Vec<u64> = (0..6).map(|i| i * 10).collect();
        // Each read took 10 ms, ending at its timestamp.
        let windows: Vec<(u64, u64)> = ts.iter().map(|t| (t - 10_000_000, *t)).collect();
        with.insert_counter_series(
            "ops",
            [("cpu", "0")],
            ts.clone(),
            values.clone(),
            Some(windows),
        )
        .unwrap();
        without
            .insert_counter_series("ops", [("cpu", "0")], ts, values, None)
            .unwrap();

        let q =
            |s: &crate::MemoryStore| matrix(s.query_range("rate(ops[2s])", 1001.0, 1005.0, 1.0));
        let banded = q(&with);
        let bare = q(&without);
        assert_eq!(banded.len(), 1);
        assert!(!banded[0].values.is_empty());
        assert!(
            banded[0].intervals.as_ref().is_some_and(|b| !b.is_empty()),
            "windows must surface as bounds: {:?}",
            banded[0].intervals
        );
        assert!(bare[0].intervals.is_none(), "no windows, no bounds");
        // And the values themselves agree: 10/s either way.
        for (_, v) in &banded[0].values {
            assert!((v - 10.0).abs() < 1e-6, "{v}");
        }
    }

    /// Two series with identical visible labels and different `__uid__`s are
    /// two series — the reader's PID-reuse split — and `without` folds them.
    #[test]
    fn series_that_differ_only_by_an_internal_label_stay_distinct() {
        let store = crate::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        let ts: Vec<u64> = (0..4).map(|i| 1000 * S + i * S).collect();
        let a: std::collections::BTreeMap<String, String> = [("comm", "redis"), ("__uid__", "a")]
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        let b: std::collections::BTreeMap<String, String> = [("comm", "redis"), ("__uid__", "b")]
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        store
            .insert_gauge_series("rss", a, ts.clone(), vec![1, 1, 1, 1], None)
            .unwrap();
        store
            .insert_gauge_series("rss", b, ts, vec![2, 2, 2, 2], None)
            .unwrap();

        assert_eq!(store.gauge_labels("rss").len(), 2);
        let both = matrix(store.query_range("rss", 1000.0, 1003.0, 1.0));
        assert_eq!(both.len(), 2, "two occupants, two series");
        let summed = matrix(store.query_range("sum without (__uid__) (rss)", 1000.0, 1003.0, 1.0));
        assert_eq!(summed.len(), 1);
        assert!(summed[0]
            .values
            .iter()
            .all(|(_, v)| (*v - 3.0).abs() < 1e-9));
    }

    /// A store assembled from a table declares its rows; one fed sample by
    /// sample reports the union of what it holds. A row every series skipped
    /// exists only in the declaration.
    #[test]
    fn sample_timestamps_are_declared_or_derived() {
        let store = crate::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        store
            .insert_counter_series("a", [("k", "1")], vec![S, 3 * S], vec![0, 1], None)
            .unwrap();
        store
            .insert_counter_series("b", [("k", "1")], vec![2 * S, 3 * S], vec![0, 1], None)
            .unwrap();
        assert_eq!(
            store.sample_timestamps(),
            vec![S, 2 * S, 3 * S],
            "derived: the union"
        );
        store.set_sample_timestamps(vec![S, 2 * S, 3 * S, 4 * S]);
        assert_eq!(
            store.sample_timestamps().len(),
            4,
            "declared: the table's rows"
        );
    }

    /// Parallel vectors of unequal length are refused at insert, where the
    /// caller can act on it, not as an index panic inside a query.
    #[test]
    fn a_ragged_series_is_refused() {
        let store = crate::MemoryStore::builder().build();
        let err = store
            .insert_counter_series("x", [("k", "1")], vec![1, 2, 3], vec![1, 2], None)
            .expect_err("values short");
        assert!(
            err.contains("x") && err.contains("3") && err.contains("2"),
            "{err}"
        );
        let err = store
            .insert_gauge_series("y", (), vec![1, 2], vec![1, 2], Some(vec![(0, 1)]))
            .expect_err("windows short");
        assert!(err.contains("windows"), "{err}");
        assert!(
            store.counter_names().is_empty() && store.gauge_names().is_empty(),
            "nothing landed"
        );
    }

    /// An assembled store sits beside other children in a union and answers
    /// for its names.
    #[test]
    fn a_memory_store_composes_as_a_union_child() {
        let a = crate::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        let b = crate::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        let ts: Vec<u64> = (0..3).map(|i| 1000 * S + i * S).collect();
        a.insert_counter_series("left", [("k", "1")], ts.clone(), vec![0, 1, 2], None)
            .unwrap();
        b.insert_gauge_series("right", [("k", "1")], ts, vec![5, 5, 5], None)
            .unwrap();
        let union = crate::UnionMetricsSource::try_new(vec![
            crate::UnionChild::from(&a),
            crate::UnionChild::from(&b),
        ])
        .expect("disjoint names");
        assert_eq!(union.counter_names(), vec!["left".to_string()]);
        assert_eq!(union.gauge_names(), vec!["right".to_string()]);
        let m = matrix(union.query_range("right", 1000.0, 1002.0, 1.0));
        assert_eq!(m.len(), 1);
    }

    /// A histogram series inserted whole is queryable like an ingested one.
    #[test]
    fn a_histogram_series_inserted_whole_is_queryable() {
        let store = crate::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        let config = ::histogram::Config::new(7, 64).unwrap();
        let mut h = ::histogram::Histogram::with_config(&config);
        let mut snapshots = Vec::new();
        let mut ts = Vec::new();
        for i in 0..4u64 {
            h.increment(1_000 * (i + 1)).unwrap();
            snapshots.push(super::histogram_to_snapshot(&h));
            ts.push(1000 * S + i * S);
        }
        store
            .insert_histogram_series("latency", [("op", "read")], config, ts, snapshots)
            .unwrap();
        assert_eq!(store.histogram_names(), vec!["latency".to_string()]);
        assert_eq!(store.histogram_labels("latency").len(), 1);
        let m = matrix(store.query_range("histogram_mean(latency)", 1001.0, 1003.0, 1.0));
        assert!(!m.is_empty() && !m[0].values.is_empty(), "{m:?}");
    }
}
