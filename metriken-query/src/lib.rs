//! Arrow-native PromQL query engine for metriken parquet files.
//!
//! Two metric sources implement the [`MetricsSource`] trait:
//!
//! * [`ParquetReader`] — streams row groups on demand from a parquet
//!   file (open with [`ParquetReader::open`]) or in-memory bytes
//!   ([`ParquetReader::open_bytes`], used by the WASM viewer).
//!   Resident memory is `O(active row group + parquet metadata)`,
//!   not `O(file size)`.
//! * [`MemoryStore`] — in-memory source for live agent polling.
//!   Available with the `ingest` feature flag; ingests
//!   `metriken_exposition::Snapshot` values.
//!
//! Queries go through the source's [`MetricsSource::query_range`]
//! method, which parses a PromQL expression, dispatches recognised
//! shapes to a streaming iterator pipeline, and returns
//! Prometheus-compatible `MatrixSample` JSON.
//!
//! For multi-file (k-way merge) or per-file label injection, see
//! [`ParquetBuilder`].
//!
//! An optional shared [`BufferPool`] caches decoded blocks across
//! queries; attach it to any source with `.with_pool(...)`. The pool
//! is process-wide and LRU-evicted by byte budget.
//!
//! # Example
//!
//! ```no_run
//! use metriken_query::{MetricsSource, ParquetReader};
//!
//! let reader = ParquetReader::open("metrics.parquet").unwrap();
//! let (start, end) = reader.time_range().unwrap();
//! let _matrix = reader
//!     .query_range("rate(cpu_cycles[1m])", start, end, 1.0)
//!     .unwrap();
//! ```

pub(crate) mod buffer_pool;
pub mod display;
pub(crate) mod histogram_stream;
pub(crate) mod labels;
pub(crate) mod memory;
pub(crate) mod memory_store;
pub mod parquet;
pub(crate) mod promql;
pub(crate) mod types;

#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;

pub use buffer_pool::{BufferPool, BufferPoolStats};
pub use display::{DisplayOptions, DisplayResult, DisplaySeries, EnvPoint, Reducer};
pub use memory_store::{MemoryStore, MemoryStoreBuilder};
pub use parquet::{ParquetBuilder, ParquetReader};
pub use promql::{HistogramHeatmapResult, MatrixSample, QueryError, QueryResult, Sample};

use histogram_stream::HistogramStream;
use labels::Labels;
use types::{Counters, Gauges};

pub(crate) trait DataSource: Send + Sync {
    fn counters(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64)
        -> Option<Counters>;
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
    fn column_map(
        &self,
    ) -> std::collections::HashMap<String, std::collections::HashMap<Labels, String>>;
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
    fn query_range(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
    ) -> Result<QueryResult, QueryError>;

    /// Execute a PromQL range query in *display* mode: evaluate at native
    /// resolution (`step_s`), then decimate each result series per `opts`
    /// (see [`DisplayOptions`]) into per-bucket boxplots — a robust median
    /// line, a configurable inner band, and a hard min/max envelope so
    /// short-lived spikes survive the downsample.
    ///
    /// Only `Matrix` results are decimated (into `Series`); heatmap, scalar,
    /// and vector results pass through unchanged. `opts.budget == 0` disables
    /// decimation (full resolution). Analysis consumers that recompute on the
    /// data should call [`query_range`](Self::query_range) instead — the
    /// envelope is lossy for anything but display.
    ///
    /// The default implementation post-processes [`query_range`](Self::query_range),
    /// so it works for every backend without per-impl code. (A future
    /// engine-level optimization could decimate before materializing the full
    /// matrix, saving memory on long recordings.)
    fn query_range_display(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &DisplayOptions,
    ) -> Result<DisplayResult, QueryError> {
        match self.query_range(expr, start_s, end_s, step_s)? {
            QueryResult::Matrix { result } => {
                let series = result
                    .into_iter()
                    .map(|s| {
                        let raw_points = s.values.len() as u64;
                        let points = opts.reducer.reduce(&s.values, opts.budget, opts.band);
                        DisplaySeries {
                            decimated: (points.len() as u64) < raw_points,
                            metric: s.metric,
                            points,
                            native_interval: step_s,
                            raw_points,
                            reducer: opts.reducer,
                            band: opts.band,
                        }
                    })
                    .collect();
                Ok(DisplayResult::Series {
                    result: series,
                    budget: opts.budget as u32,
                })
            }
            QueryResult::HistogramHeatmap { result } => {
                Ok(DisplayResult::HistogramHeatmap { result })
            }
            QueryResult::Scalar { result } => Ok(DisplayResult::Scalar { result }),
            QueryResult::Vector { result } => Ok(DisplayResult::Vector { result }),
        }
    }

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
            if let Some(v) = labels.get(key) {
                out.insert(v.clone());
            }
        }
        for labels in self.gauge_labels(metric) {
            if let Some(v) = labels.get(key) {
                out.insert(v.clone());
            }
        }
        for labels in self.histogram_labels(metric) {
            if let Some(v) = labels.get(key) {
                out.insert(v.clone());
            }
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
        for name in self.counter_names() {
            count += self.counter_labels(&name).len();
        }
        for name in self.gauge_names() {
            count += self.gauge_labels(&name).len();
        }
        for name in self.histogram_names() {
            count += self.histogram_labels(&name).len();
        }
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
        for name in self.counter_names() {
            collect(self.counter_labels(&name));
        }
        for name in self.gauge_names() {
            collect(self.gauge_labels(&name));
        }
        for name in self.histogram_names() {
            collect(self.histogram_labels(&name));
        }
        out
    }

    /// For a specific metric name, return every label key mapped to the set of
    /// distinct values seen across all series of that metric (counter, gauge,
    /// and histogram types are all included).
    fn label_values_by_key(
        &self,
        metric: &str,
    ) -> std::collections::HashMap<String, std::collections::HashSet<String>> {
        let mut out: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::new();
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

    /// Raw per-sample collection timestamps (ns since epoch), ascending, in
    /// row order — the un-snapped `timestamp` column. Default empty for
    /// sources that don't track it (e.g. live `MemoryStore`).
    fn sample_timestamps(&self) -> Vec<u64> {
        Vec::new()
    }
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
        fn _assert_send_sync<T: Send + Sync>(_: T) {}
        fn _check(s: std::sync::Arc<dyn MetricsSource>) {
            _assert_send_sync(s);
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
