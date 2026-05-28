use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::memory::Memory;
use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::types::{Counters, Gauges};
use crate::{DataSource, MetricsSource};

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
}

impl MemoryStore {
    /// Create a builder for configuring and constructing a `MemoryStore`.
    pub fn builder() -> MemoryStoreBuilder {
        MemoryStoreBuilder::default()
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

    /// Sampling interval in seconds.
    pub fn interval(&self) -> f64 {
        self.state.memory.read().unwrap().interval()
    }

    /// Convenience: the `source` key from metadata. Returns an empty string if absent.
    pub fn source(&self) -> String {
        self.state
            .metadata
            .read()
            .unwrap()
            .get("source")
            .cloned()
            .unwrap_or_default()
    }

    /// Convenience: the `version` key from metadata. Returns an empty string if absent.
    pub fn version(&self) -> String {
        self.state
            .metadata
            .read()
            .unwrap()
            .get("version")
            .cloned()
            .unwrap_or_default()
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
    ) -> Option<Counters> {
        self.memory.read().unwrap().counters(name, filter, start_ns, end_ns)
    }

    fn gauges(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Gauges> {
        self.memory.read().unwrap().gauges(name, filter, start_ns, end_ns)
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        self.memory.read().unwrap().histogram_stream(name, filter, start_ns, end_ns)
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

    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        self.memory.read().unwrap().column_map()
    }
}

// ─── MetricsSource on MemoryStore ─────────────────────────────────────────────

impl MetricsSource for MemoryStore {
    fn query_range(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
    ) -> Result<QueryResult, QueryError> {
        MemoryStore::query_range(self, expr, start_s, end_s, step_s)
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

    fn interval(&self) -> f64 {
        MemoryStore::interval(self)
    }

    fn source(&self) -> String {
        MemoryStore::source(self)
    }

    fn version(&self) -> String {
        MemoryStore::version(self)
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
    pub fn ingest_snapshot(&self, snapshot: &mut metriken_exposition::Snapshot) {
        use crate::memory::extract_name_labels;
        use crate::types::HistogramSnapshot;

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
        fn assert_metrics_source<T: MetricsSource>(_: &T) {}
        fn _check(s: &MemoryStore) {
            assert_metrics_source(s);
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
    use super::*;
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
            counters: vec![metriken_exposition::Counter {
                name: counter_name.to_string(),
                value,
                metadata,
            }],
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
        let mut s0 = make_counter_snap(t0, "cpu_cycles", 100, &[("cpu", "0")]);
        let mut s1 = make_counter_snap(t1, "cpu_cycles", 200, &[("cpu", "0")]);
        store.ingest_snapshot(&mut s0);
        store.ingest_snapshot(&mut s1);

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
        let mut s0 = make_counter_snap(t0, "cpu_cycles", 100, &[("cpu", "0")]);
        let mut s1 = make_counter_snap(t0, "cpu_cycles", 200, &[("cpu", "1")]);
        store.ingest_snapshot(&mut s0);
        store.ingest_snapshot(&mut s1);

        let labels = store.counter_labels("cpu_cycles");
        assert_eq!(labels.len(), 2);
    }
}
