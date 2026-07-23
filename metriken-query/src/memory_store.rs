use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::memory::Memory;
use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::types::{Counters, Gauges};
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
        self.engine()
            .query_range_opts(expr, start, end, step, opts.rate_mode)
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
impl DataSource for MemoryStoreInner {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        raw: bool,
    ) -> Option<Counters> {
        // Live in-memory samples aren't grid-snapped, so `raw` is a no-op here.
        self.memory
            .read()
            .unwrap()
            .counters(name, filter, start_ns, end_ns, raw)
    }

    fn gauges(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        raw: bool,
    ) -> Option<Gauges> {
        self.memory
            .read()
            .unwrap()
            .gauges(name, filter, start_ns, end_ns, raw)
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
    /// Ingest a single snapshot into the store. Snapshot timestamp is snapped
    /// to the nearest sampling-interval boundary so multiple samplers within
    /// one collection cycle align.
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
        let interval_ns = memory.interval_ms() * 1_000_000;
        let ts = snap_timestamp(raw_ts, interval_ns);

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
fn snap_timestamp(raw_ts: u64, interval_ns: u64) -> u64 {
    if interval_ns == 0 {
        return raw_ts;
    }
    ((raw_ts + interval_ns / 2) / interval_ns) * interval_ns
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
