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
