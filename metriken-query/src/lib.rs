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
//!   `metriken_model::Snapshot` values.
//!
//! Queries go through the source's [`MetricsSource::query_range`]
//! method, which parses a PromQL expression, dispatches recognised
//! shapes to a streaming iterator pipeline, and returns
//! Prometheus-compatible `MatrixSample` JSON.
//!
//! For multi-file (k-way merge) or per-file label injection, see
//! [`ParquetBuilder`].
//!
//! An optional shared [`BufferPool`](metriken_storage::BufferPool) caches decoded blocks across
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

#[cfg(feature = "archive")]
mod archive;
pub(crate) mod batch_rate;
#[cfg(all(test, feature = "archive"))]
mod conformance;
pub mod display;
#[cfg(test)]
mod long;
pub(crate) mod memory_store;
pub mod parquet;
pub(crate) mod promql;
pub mod segmented;
pub mod union;

#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
#[cfg(test)]
mod lazy_tests;

// The column readers are `metriken-storage`'s.
pub(crate) use metriken_storage::DataSource;
pub(crate) use metriken_storage::{buffer_pool, histogram_stream, labels, memory, scan, types};

#[cfg(feature = "archive")]
pub use archive::eval_timestamps_for;
pub use display::{DisplayOptions, DisplayResult, DisplaySeries, EnvPoint, Reducer};
pub use memory_store::{MemoryStore, MemoryStoreBuilder};
pub use parquet::{ParquetBuilder, ParquetReader};
pub use promql::{
    referenced_metrics, HistogramHeatmapResult, MatrixSample, QueryError, QueryResult, Sample,
};
pub use segmented::SegmentedParquetReader;
pub use union::UnionMetricsSource;

/// Where `rate()` / `irate()` points are placed in time.
///
/// `rate(x[r])` at `t` is the increase of the reset-adjusted cumulative
/// counter across `[t − r, t]`, interpolated at both ends, divided by the
/// window's length. The value at `t − r` is interpolated from a sample up
/// to two of the series' interval before it: its table's, where the source
/// sets it on the series (`LabeledSource::with_interval`, as an archive
/// does for each table), else the source's `DataSource::counter_interval`.
/// Where the series has no sample that close before `t − r` (it starts
/// later, or has a gap there), the window starts at the first sample after
/// `t − r` and is shorter than `r`, and with no sample in `(t − r, t)` such
/// a window gives no point. `irate(x[r])` ignores `r`: on the grid its
/// window is one step, and in raw mode it is the gap to the previous
/// sample.
///
/// * [`Grid`](RateMode::Grid) (default): points at each grid tick
///   `t = floor(start/step)·step + k·step`. The grid phase is fixed to the
///   step boundary, so two recordings on the same step share a grid and are
///   directly comparable (A/B). `irate`'s value is attributable to the grid
///   interval `[t − step, t]`.
/// * [`Raw`](RateMode::Raw): points at the series' sample timestamps. `irate`
///   is the rate between each sample and the one before it, with no value
///   when they are further apart than four of the counter's interval. The
///   points of two series are not aligned with one another; for
///   jitter/cadence and single-recording analysis. An aggregation or a
///   binary operator over raw series has a point at every time one of them
///   has one, each series contributing its latest point while it is no
///   older than the gap before it, and at most the hold bound: two steps,
///   or two of the longest interval (`DataSource::counter_interval` and its
///   gauge and histogram counterparts) of a metric the query reads,
///   whichever is longer. A series' first point is held for the bound, and
///   so the hold depends on the step. Every raw read starts twice the bound
///   for each join the query nests, and the function's window, before the
///   range, so which points a result has and their values do not depend on
///   where the range starts, up to floating-point rounding. A rate's band
///   and its interpolated flag treat a gap longer than twice the median
///   spacing of the first nine samples read as a hole, so on a series whose
///   spacing changes they can depend on where the range starts.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum RateMode {
    /// Fixed-phase grid. Default.
    #[default]
    Grid,
    /// The series' sample timestamps. Not grid-aligned.
    Raw,
}

/// Query-evaluation options threaded into the streaming engine. Additive and
/// `#[non_exhaustive]`: new knobs can be added without breaking callers, and
/// `QueryOptions::default()` reproduces today's default behavior.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct QueryOptions {
    /// Rate/irate time-alignment mode. See [`RateMode`].
    pub rate_mode: RateMode,
    /// Evaluate at these timestamps instead of on a uniform grid.
    ///
    /// The grid is uniform by default: `start`, `start + step`, … Every value
    /// is therefore produced wherever the grid happens to fall, which is
    /// rarely where a slow source actually took a reading — so its value gets
    /// held or interpolated between real observations, and anything combined
    /// with it inherits that as uncertainty.
    ///
    /// A slow source's readings are not evenly spaced either. Measured on a
    /// real recording, one sampler's rows fell 30 s apart and then 60 s apart,
    /// so no uniform grid can sit on them at any step or phase. Supplying the
    /// timestamps explicitly is the only way to evaluate where the data
    /// is.
    ///
    /// When set, an `irate`'s window is the gap to the preceding timestamp,
    /// so the uniform grid is the case where those gaps are all equal; a
    /// `rate`'s window is its `[range]`.
    pub eval_timestamps: Option<std::sync::Arc<[u64]>>,
    /// Compute `rate`/`irate` from one sample stream per series rather than
    /// in one pass over a segmented reader's columns. For samples in
    /// increasing time order the two give the same series and timestamps; an
    /// aggregate's values and bands can differ in the last bits, from
    /// summation order. For comparing the two and for diagnosis.
    pub per_series_rates: bool,
}

impl QueryOptions {
    /// Construct options selecting a specific [`RateMode`].
    pub fn with_rate_mode(rate_mode: RateMode) -> Self {
        Self {
            rate_mode,
            eval_timestamps: None,
            per_series_rates: false,
        }
    }

    /// Compute rates from one stream per series. See
    /// [`QueryOptions::per_series_rates`].
    pub fn with_per_series_rates(mut self, per_series: bool) -> Self {
        self.per_series_rates = per_series;
        self
    }

    /// Evaluate at an explicit timestamp list. See
    /// [`QueryOptions::eval_timestamps`].
    pub fn with_eval_timestamps(mut self, ts: Option<std::sync::Arc<[u64]>>) -> Self {
        self.eval_timestamps = ts;
        self
    }
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
    /// Execute a PromQL range query with the default [`QueryOptions`]
    /// (i.e. [`RateMode::Grid`]). Delegates to
    /// [`query_range_opts`](Self::query_range_opts); implementors override that
    /// one, not this.
    fn query_range(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
    ) -> Result<QueryResult, QueryError> {
        self.query_range_opts(expr, start_s, end_s, step_s, &QueryOptions::default())
    }

    /// Execute a PromQL range query with explicit [`QueryOptions`] (e.g. a
    /// non-default [`RateMode`]). This is the method concrete sources implement;
    /// the non-`_opts` and display variants delegate to it.
    fn query_range_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError>;

    /// Execute a PromQL range query in display mode: evaluate at `step_s`,
    /// then reduce each result series per `opts` (see [`DisplayOptions`])
    /// into per-bucket boxplots: a median line, a configurable inner band,
    /// and a min/max envelope that keeps short spikes visible.
    ///
    /// Every series is cut into buckets of one width, aligned to multiples
    /// of it: the smallest of 1, 2, 5, 10, 15, 20 or 30 s, 1, 2, 5, 10, 15 or
    /// 30 min, 1, 2, 3, 6 or 12 h, or a whole number of days, at least
    /// `(end_s - start_s) / opts.budget`. When `(end_s - start_s) /
    /// opts.budget` is at most `step_s`, the width is at most `step_s`, so a
    /// series on the step grid is returned point by point. A bucket holding
    /// one point gives that point at its own time. `opts.budget == 0`, or
    /// `end_s <= start_s`, returns full resolution.
    ///
    /// Only `Matrix` results are reduced (into `Series`); heatmap, scalar
    /// and vector results pass through unchanged. Analysis consumers that
    /// recompute on the data should call [`query_range`](Self::query_range)
    /// instead.
    ///
    /// The default implementation reduces the result of
    /// [`query_range_opts`](Self::query_range_opts). The readers in this
    /// crate reduce each series of a streamed expression as it is collected;
    /// histogram functions are evaluated in full and then reduced. A source
    /// that wraps one of them should forward
    /// [`query_range_display_opts`](Self::query_range_display_opts) to it.
    fn query_range_display(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &DisplayOptions,
    ) -> Result<DisplayResult, QueryError> {
        self.query_range_display_opts(expr, start_s, end_s, step_s, opts, &QueryOptions::default())
    }

    /// [`query_range_display`](Self::query_range_display) with explicit
    /// [`QueryOptions`].
    fn query_range_display_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &DisplayOptions,
        qopts: &QueryOptions,
    ) -> Result<DisplayResult, QueryError> {
        let result = self.query_range_opts(expr, start_s, end_s, step_s, qopts)?;
        Ok(display::display_from_result(
            result, start_s, end_s, step_s, opts,
        ))
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

    /// Per-sample collection timestamps (ns since epoch), ascending, in row
    /// order — the `timestamp` column, as recorded. Default empty for sources
    /// that don't track it (e.g. live `MemoryStore`).
    ///
    /// These are the instants the query path reads, so this is also the form
    /// to use when deciding WHERE a series has data — for instance to build
    /// [`QueryOptions::eval_timestamps`]. A parquet source used to round them
    /// to a nominal grid, which is why a `snapped_sample_timestamps` companion
    /// existed; nothing rounds them now.
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
