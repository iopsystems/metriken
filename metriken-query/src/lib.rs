pub(crate) mod labels;
pub(crate) mod memory;
pub mod parquet;
pub(crate) mod promql;
pub(crate) mod types;
pub(crate) mod histogram_stream;
pub(crate) mod memory_store;

pub use parquet::{ParquetBuilder, ParquetReader};
pub use memory_store::{MemoryStore, MemoryStoreBuilder};
pub use promql::{HistogramHeatmapResult, MatrixSample, QueryError, QueryResult, Sample};

use histogram_stream::HistogramStream;
use labels::Labels;
use types::{Counters, Gauges};

pub(crate) trait DataSource: Send + Sync {
    fn counters(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Counters>;
    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges>;
    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream>;
    /// Sampling interval in seconds.
    fn interval(&self) -> f64;
    /// Full time extent of the stored data in nanoseconds, or `None` if empty.
    fn time_range(&self) -> Option<(u64, u64)>;
    /// Names of all counter metrics (sorted, deduplicated).
    fn counter_names(&self) -> Vec<String>;
    /// Names of all gauge metrics (sorted, deduplicated).
    fn gauge_names(&self) -> Vec<String>;
    /// Names of all histogram metrics (sorted, deduplicated).
    fn histogram_names(&self) -> Vec<String>;
    /// All label combinations for the named counter metric. Empty if unknown.
    fn counter_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;
    /// All label combinations for the named gauge metric. Empty if unknown.
    fn gauge_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;
    /// All label combinations for the named histogram metric. Empty if unknown.
    fn histogram_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;
    /// Key-value metadata from the file footer. Default returns empty.
    fn file_metadata(&self) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }
    /// Look up a single metadata value by key without cloning the full map.
    fn metadata_get(&self, key: &str) -> Option<String> {
        self.file_metadata().get(key).cloned()
    }
    /// Parquet column name for every `(metric_name, labels)` pair.
    fn column_map(&self) -> std::collections::HashMap<String, std::collections::HashMap<Labels, String>>;
}

/// Public trait expressing the full read-only capability of a metrics source.
///
/// Implement this trait to expose a uniform interface for PromQL queries,
/// schema introspection, and metadata access over any metrics backend.
///
/// # Example
///
/// ```rust,ignore
/// fn describe<S: MetricsSource>(src: &S) {
///     println!("source: {}", src.source());
///     println!("counters: {:?}", src.counter_names());
/// }
/// ```
pub trait MetricsSource: Send + Sync {
    /// Execute a PromQL range query.
    fn query_range(&self, expr: &str, start_s: f64, end_s: f64, step_s: f64) -> Result<QueryResult, QueryError>;

    /// Execute an instant PromQL query at a single timestamp (uses the latest
    /// available timestamp when `time` is `None`).
    fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError>;

    /// Resolve a PromQL query to the set of physical parquet column names it
    /// touches, without reading any values.
    fn columns(&self, query: &str) -> Result<std::collections::HashSet<String>, QueryError>;

    /// Full time extent of the stored data in seconds, or `None` if empty.
    fn time_range(&self) -> Option<(f64, f64)>;

    /// Sampling interval in seconds.
    fn interval(&self) -> f64;

    /// Convenience: the `source` key from file metadata (e.g. `"rezolus"`).
    /// Returns an empty string if absent.
    fn source(&self) -> String;

    /// Convenience: the `version` key from file metadata.
    /// Returns an empty string if absent.
    fn version(&self) -> String;

    /// Optional display name. For `ParquetReader::open(path)` this defaults to
    /// the path's basename. For bytes-backed or builder-constructed readers it's
    /// `None` unless explicitly set.
    fn filename(&self) -> Option<String>;

    /// Look up a single metadata value by key. Avoids cloning the full
    /// metadata map when callers only need one entry.
    fn metadata_get(&self, key: &str) -> Option<String>;

    /// Key-value metadata from the file footer.
    fn file_metadata(&self) -> std::collections::HashMap<String, String>;

    /// Names of all counter metrics (sorted, deduplicated).
    fn counter_names(&self) -> Vec<String>;

    /// Names of all gauge metrics (sorted, deduplicated).
    fn gauge_names(&self) -> Vec<String>;

    /// Names of all histogram metrics (sorted, deduplicated).
    fn histogram_names(&self) -> Vec<String>;

    /// All label combinations for the named counter metric. Empty if unknown.
    fn counter_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;

    /// All label combinations for the named gauge metric. Empty if unknown.
    fn gauge_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;

    /// All label combinations for the named histogram metric. Empty if unknown.
    fn histogram_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;

    /// Distinct values of `key` across all series for `metric`. Walks counter,
    /// gauge, and histogram series and returns the union. Returns empty if the
    /// metric is unknown or no series carry the key.
    fn label_values(&self, metric: &str, key: &str) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        for labels in self.counter_labels(metric) {
            if let Some(v) = labels.get(key) { out.insert(v.clone()); }
        }
        for labels in self.gauge_labels(metric) {
            if let Some(v) = labels.get(key) { out.insert(v.clone()); }
        }
        for labels in self.histogram_labels(metric) {
            if let Some(v) = labels.get(key) { out.insert(v.clone()); }
        }
        out
    }

    /// Sorted union of all counter, gauge, and histogram metric names.
    fn all_names(&self) -> Vec<String> {
        let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        out.extend(self.counter_names());
        out.extend(self.gauge_names());
        out.extend(self.histogram_names());
        out.into_iter().collect()
    }

    /// Total number of distinct time series across all metric types.
    ///
    /// Sums the number of label-set combinations for every counter, gauge, and
    /// histogram metric. Useful for capacity estimates and UI badges.
    fn total_series_count(&self) -> usize {
        let mut count = 0;
        for name in self.counter_names() { count += self.counter_labels(&name).len(); }
        for name in self.gauge_names() { count += self.gauge_labels(&name).len(); }
        for name in self.histogram_names() { count += self.histogram_labels(&name).len(); }
        count
    }

    /// Returns `true` if a counter metric with the given name exists.
    fn has_counter(&self, name: &str) -> bool {
        self.counter_names().iter().any(|n| n == name)
    }

    /// Returns `true` if a gauge metric with the given name exists.
    fn has_gauge(&self, name: &str) -> bool {
        self.gauge_names().iter().any(|n| n == name)
    }

    /// Returns `true` if a histogram metric with the given name exists.
    fn has_histogram(&self, name: &str) -> bool {
        self.histogram_names().iter().any(|n| n == name)
    }

    /// Union of all label keys across every series for every metric type.
    /// Useful for answering "what dimensions does this data have?"
    fn all_label_keys(&self) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let mut collect = |labels_list: Vec<std::collections::BTreeMap<String, String>>| {
            for labels in labels_list {
                for k in labels.into_keys() {
                    out.insert(k);
                }
            }
        };
        for name in self.counter_names() { collect(self.counter_labels(&name)); }
        for name in self.gauge_names() { collect(self.gauge_labels(&name)); }
        for name in self.histogram_names() { collect(self.histogram_labels(&name)); }
        out
    }

    /// For a specific metric name, return every label key mapped to the set of
    /// distinct values seen across all series of that metric (counter, gauge,
    /// and histogram types are all included).
    fn label_values_by_key(&self, metric: &str) -> std::collections::HashMap<String, std::collections::HashSet<String>> {
        let mut out: std::collections::HashMap<String, std::collections::HashSet<String>> = std::collections::HashMap::new();
        let mut collect = |labels_list: Vec<std::collections::BTreeMap<String, String>>| {
            for labels in labels_list {
                for (k, v) in labels {
                    out.entry(k).or_default().insert(v);
                }
            }
        };
        collect(self.counter_labels(metric));
        collect(self.gauge_labels(metric));
        collect(self.histogram_labels(metric));
        out
    }

    /// Returns `self.filename()` or an empty string if no name is set.
    fn filename_or_default(&self) -> String {
        self.filename().unwrap_or_default()
    }

    /// Time range of the data in nanoseconds, or `None` if empty.
    ///
    /// Use this instead of [`time_range()`](Self::time_range) when you need
    /// exact nanosecond timestamps without floating-point precision loss.
    fn time_range_ns(&self) -> Option<(u64, u64)>;
}

#[cfg(test)]
mod trait_impls {
    use super::*;

    /// Compile-time assertion that `ParquetReader` implements `MetricsSource`.
    fn _assert_parquet_reader_is_metrics_source<T: MetricsSource>(_: &T) {}
    fn _check(_reader: &ParquetReader) {
        _assert_parquet_reader_is_metrics_source(_reader);
    }

    /// Compile-time check: `Arc<dyn MetricsSource>` is Send + Sync via the supertrait.
    #[test]
    fn test_metrics_source_dyn_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>(_: T) {}
        fn _check(s: std::sync::Arc<dyn MetricsSource>) {
            assert_send_sync(s);
        }
    }

    /// Compile-time check: `ParquetReader::open_file` accepts a `std::fs::File`.
    #[test]
    fn test_open_file_compiles() {
        fn _check(f: std::fs::File) {
            let _ = ParquetReader::open_file(f);
        }
    }
}
