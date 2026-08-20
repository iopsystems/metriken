// Several nested `Result<Result<…>, _>` types arise naturally from
// `catch_unwind` wrapping fallible decode functions; flattening them through
// type aliases is more confusing than the inline form.
#![allow(clippy::type_complexity)]

use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Int64Array, ListArray, UInt64Array};
use arrow::datatypes::DataType;
use bytes::Bytes;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::ProjectionMask;
use parquet::file::metadata::RowGroupMetaData;
use parquet::file::statistics::Statistics;

use crate::buffer_pool::{next_source_id, BufferPool, CacheKey};
use crate::histogram_stream::{HistogramRow, HistogramStream, HistogramStreamMeta};
use crate::labels::Labels;
use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::types::{Counter, Counters, Gauge, Gauges, HistogramSnapshot};
use crate::{DataSource, MetricsSource, QueryOptions};

// ─── Public entry point ───────────────────────────────────────────────────────

pub struct ParquetReader {
    engine: QueryEngine,
    /// Concrete handle retained for composition via `ParquetBuilder::reader()`.
    inner: Arc<MultiParquetSource>,
    filename: Option<String>,
}

impl ParquetReader {
    pub fn builder() -> ParquetBuilder {
        ParquetBuilder::new()
    }

    /// Convenience: open a single file with no extra labels.
    /// The `filename()` defaults to the path's basename.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Box<dyn Error>> {
        let path = path.as_ref();
        let filename = path.file_name().and_then(|n| n.to_str()).map(String::from);
        let source = ParquetSource::open(path)?;
        let inner = Arc::new(MultiParquetSource {
            files: vec![(source, Labels::default())],
        });
        let ds: Arc<dyn DataSource> = inner.clone();
        Ok(Self {
            engine: QueryEngine::new(ds),
            inner,
            filename,
        })
    }

    /// Convenience: open a parquet file from raw bytes (e.g. from a browser file upload).
    pub fn open_bytes(bytes: impl Into<Bytes>) -> Result<Self, Box<dyn Error>> {
        Self::builder().bytes(bytes).build()
    }

    /// Open a parquet from an already-open file handle. Useful for the
    /// "open-then-unlink" temp-file pattern: `NamedTempFile::into_file()`
    /// hands you an owned `File` whose disk path has been removed, and the
    /// data persists as long as the reader (and its `File`) is alive.
    pub fn open_file(file: File) -> Result<Self, Box<dyn Error>> {
        let source = ParquetSource::open_file(file)?;
        let inner = Arc::new(MultiParquetSource {
            files: vec![(source, Labels::default())],
        });
        let ds: Arc<dyn DataSource> = inner.clone();
        Ok(Self {
            engine: QueryEngine::new(ds),
            inner,
            filename: None,
        })
    }

    /// Open a single file wired to `pool` for caching decoded row groups.
    ///
    /// Subsequent queries against this reader will populate the pool on first
    /// access and serve cached decoded blocks on repeated access.  Multiple
    /// readers sharing the same `Arc<BufferPool>` share the budget and LRU.
    pub fn open_with_pool(
        path: impl AsRef<Path>,
        pool: Arc<BufferPool>,
    ) -> Result<Self, Box<dyn Error>> {
        let path = path.as_ref();
        let filename = path.file_name().and_then(|n| n.to_str()).map(String::from);
        let source = ParquetSource::open_with_pool(path, pool)?;
        let inner = Arc::new(MultiParquetSource {
            files: vec![(source, Labels::default())],
        });
        let ds: Arc<dyn DataSource> = inner.clone();
        Ok(Self {
            engine: QueryEngine::new(ds),
            inner,
            filename,
        })
    }

    /// Open in-memory bytes wired to `pool`.
    pub fn open_bytes_with_pool(
        bytes: impl Into<Bytes>,
        pool: Arc<BufferPool>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::builder().pool(pool).bytes(bytes).build()
    }

    /// Open an already-open file handle wired to `pool`.
    pub fn open_file_with_pool(file: File, pool: Arc<BufferPool>) -> Result<Self, Box<dyn Error>> {
        let source = ParquetSource::open_file_with_pool(file, pool)?;
        let inner = Arc::new(MultiParquetSource {
            files: vec![(source, Labels::default())],
        });
        let ds: Arc<dyn DataSource> = inner.clone();
        Ok(Self {
            engine: QueryEngine::new(ds),
            inner,
            filename: None,
        })
    }

    /// Return the underlying `(source, labels)` pairs for use by
    /// [`ParquetBuilder::reader`] and [`ParquetBuilder::reader_labeled`].
    pub(crate) fn sources_for_composition(&self) -> Vec<(Arc<ParquetSource>, Labels)> {
        self.inner.files.clone()
    }

    /// The reader's raw-sample [`DataSource`], for composition by
    /// [`crate::SegmentedParquetReader`] (which splices per-segment samples
    /// below PromQL evaluation) or [`crate::UnionMetricsSource`] (which
    /// dispatches by metric name across readers with disjoint identity
    /// sets).
    pub(crate) fn data_source(&self) -> Arc<dyn DataSource> {
        self.inner.clone()
    }

    /// Histogram `(grouping_power, max_value_power)` per metric name, read from
    /// parquet **field metadata only** — no row group is touched.
    ///
    /// First column wins for a given name, mirroring how
    /// `ParquetSource::histogram_stream` picks the config it decodes with.
    /// Used by [`crate::SegmentedParquetReader`] to reject segments whose
    /// histogram configs disagree, which would otherwise splice into silently
    /// wrong bucket boundaries.
    pub(crate) fn histogram_configs(&self) -> std::collections::BTreeMap<String, (u8, u8)> {
        let mut out = std::collections::BTreeMap::new();
        for (pf, _) in &self.inner.files {
            let ts = pf.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
            for col in parse_schema(pf, ts) {
                if let ColKind::Histogram {
                    grouping_power,
                    max_value_power,
                } = col.kind
                {
                    out.entry(col.name)
                        .or_insert((grouping_power, max_value_power));
                }
            }
        }
        out
    }

    /// All distinct histogram `(grouping_power, max_value_power)` configs
    /// observed per metric name in this reader, footer-only (no row-group
    /// decode). Unlike [`histogram_configs`](Self::histogram_configs)
    /// (first column wins), this surfaces EVERY distinct config so a caller
    /// can detect a same-file conflict: `ParquetSource::histogram_stream`
    /// groups columns purely by name and decodes every matching column
    /// under the first one's config, so two differently-configured columns
    /// for the same name within one file can never be decoded separately.
    /// Used by [`crate::SegmentedParquetReader`] to reject that case at
    /// open (a cross-*segment* difference is fine — that's handled by
    /// splitting into distinct runs, not rejected).
    pub(crate) fn histogram_config_variants(
        &self,
    ) -> std::collections::BTreeMap<String, Vec<(u8, u8)>> {
        let mut out: std::collections::BTreeMap<String, Vec<(u8, u8)>> =
            std::collections::BTreeMap::new();
        for (pf, _) in &self.inner.files {
            let ts = pf.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
            for col in parse_schema(pf, ts) {
                if let ColKind::Histogram {
                    grouping_power,
                    max_value_power,
                } = col.kind
                {
                    let cfg = (grouping_power, max_value_power);
                    let list = out.entry(col.name).or_default();
                    if !list.contains(&cfg) {
                        list.push(cfg);
                    }
                }
            }
        }
        out
    }

    /// `(name, labels)` for every counter column, in RAW SCHEMA order —
    /// unlike [`counter_labels`](Self::counter_labels), this does not sort
    /// or dedupe. Footer-only: one pass over the schema, no row-group
    /// decode. Used by [`crate::SegmentedParquetReader`] to build an
    /// open-time identity index that preserves "first appearance across
    /// segments" splicing order without re-scanning the schema per metric
    /// name.
    pub(crate) fn counter_columns(&self) -> Vec<(String, Labels)> {
        let mut out = Vec::new();
        for (pf, extra) in &self.inner.files {
            let ts = pf.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
            for col in parse_schema(pf, ts) {
                if matches!(col.kind, ColKind::Counter) {
                    let mut labels = col.labels;
                    for (k, v) in &extra.inner {
                        labels.inner.insert(k.clone(), v.clone());
                    }
                    out.push((col.name, labels));
                }
            }
        }
        out
    }

    /// Gauge twin of [`counter_columns`](Self::counter_columns).
    pub(crate) fn gauge_columns(&self) -> Vec<(String, Labels)> {
        let mut out = Vec::new();
        for (pf, extra) in &self.inner.files {
            let ts = pf.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
            for col in parse_schema(pf, ts) {
                if matches!(col.kind, ColKind::Gauge) {
                    let mut labels = col.labels;
                    for (k, v) in &extra.inner {
                        labels.inner.insert(k.clone(), v.clone());
                    }
                    out.push((col.name, labels));
                }
            }
        }
        out
    }

    /// Histogram twin of [`counter_columns`](Self::counter_columns).
    pub(crate) fn histogram_columns(&self) -> Vec<(String, Labels)> {
        let mut out = Vec::new();
        for (pf, extra) in &self.inner.files {
            let ts = pf.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
            for col in parse_schema(pf, ts) {
                if matches!(col.kind, ColKind::Histogram { .. }) {
                    let mut labels = col.labels;
                    for (k, v) in &extra.inner {
                        labels.inner.insert(k.clone(), v.clone());
                    }
                    out.push((col.name, labels));
                }
            }
        }
        out
    }

    /// Set the display name. Useful when constructing from bytes or
    /// after the fact (e.g. a WASM viewer setting the original upload name).
    pub fn with_filename(mut self, name: impl Into<String>) -> Self {
        self.filename = Some(name.into());
        self
    }

    /// Return the display name, if set.
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    pub fn query_range(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
    ) -> Result<QueryResult, QueryError> {
        self.engine.query_range(expr, start_s, end_s, step_s)
    }

    /// Range query with explicit [`QueryOptions`] (e.g. a non-default
    /// [`crate::RateMode`]). The no-arg [`query_range`](Self::query_range)
    /// forwards here with defaults.
    pub fn query_range_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        self.engine
            .query_range_opts(expr, start_s, end_s, step_s, opts.rate_mode)
    }

    /// Time range of data across all files in seconds, or `None` if empty.
    pub fn time_range(&self) -> Option<(f64, f64)> {
        self.engine
            .time_range()
            .map(|(lo, hi)| (lo as f64 / 1e9, hi as f64 / 1e9))
    }

    /// Time range of data across all files in nanoseconds, or `None` if empty.
    ///
    /// Prefer this over [`time_range()`](Self::time_range) when you need exact
    /// nanosecond timestamps without floating-point precision loss.
    pub fn time_range_ns(&self) -> Option<(u64, u64)> {
        self.engine.time_range()
    }

    /// Test-only accessor: read the counter series named `name` over the full
    /// time range and return the first series' reconstructed acquisition windows.
    /// Routes through the same `read_counters` path production queries use.
    #[cfg(feature = "fixtures")]
    pub fn counter_windows_for_test(&self, name: &str) -> Option<Vec<(u64, u64)>> {
        let (start, end) = self.time_range_ns()?;
        let filter = Labels::default();
        for (pf, _extra) in &self.inner.files {
            let counters = read_counters(pf, name, &filter, start, end, false).ok()?;
            if let Some(c) = counters.series.into_iter().next() {
                return c.windows;
            }
        }
        None
    }

    /// Names of all counter metrics across all files (sorted, deduplicated).
    pub fn counter_names(&self) -> Vec<String> {
        self.engine.counter_names()
    }

    /// Names of all gauge metrics across all files (sorted, deduplicated).
    pub fn gauge_names(&self) -> Vec<String> {
        self.engine.gauge_names()
    }

    /// Names of all histogram metrics across all files (sorted, deduplicated).
    pub fn histogram_names(&self) -> Vec<String> {
        self.engine.histogram_names()
    }

    /// All label combinations for the named counter metric across all files.
    /// Includes any per-file labels injected via `file_labeled`. Empty if unknown.
    pub fn counter_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        self.engine.counter_labels(name)
    }

    /// All label combinations for the named gauge metric across all files.
    /// Includes any per-file labels injected via `file_labeled`. Empty if unknown.
    pub fn gauge_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        self.engine.gauge_labels(name)
    }

    /// All label combinations for the named histogram metric across all files.
    /// Includes any per-file labels injected via `file_labeled`. Empty if unknown.
    pub fn histogram_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        self.engine.histogram_labels(name)
    }

    /// Sampling interval in seconds. For multi-file readers, returns the finest interval.
    pub fn interval(&self) -> f64 {
        self.engine.interval()
    }

    /// Key-value metadata from the parquet file footer.
    /// For multi-file readers, merges across all files (last file wins on collision).
    pub fn file_metadata(&self) -> std::collections::HashMap<String, String> {
        self.engine.file_metadata()
    }

    /// Look up a single metadata value by key without cloning the full map.
    pub fn metadata_get(&self, key: &str) -> Option<String> {
        self.engine.metadata_get(key)
    }

    /// Convenience: the `source` key from file metadata (e.g. "rezolus").
    /// Returns an empty string if absent.
    pub fn source(&self) -> String {
        self.engine.metadata_get("source").unwrap_or_default()
    }

    /// Convenience: the `version` key from file metadata.
    /// Returns an empty string if absent.
    pub fn version(&self) -> String {
        self.engine.metadata_get("version").unwrap_or_default()
    }

    /// Execute an instant PromQL query at a single timestamp.
    /// Uses the latest available timestamp when `time` is `None`.
    pub fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        self.engine.query(expr, time)
    }

    /// Resolve a PromQL query to the set of physical parquet column names it
    /// touches, without reading any values.
    pub fn columns(&self, query: &str) -> Result<std::collections::HashSet<String>, QueryError> {
        self.engine.columns(query)
    }

    /// Raw per-sample collection timestamps (ns since epoch), ascending, in
    /// row order — the un-snapped `timestamp` column, concatenated across
    /// all files. Unlike query results, this is never gridded or rounded.
    pub fn sample_timestamps(&self) -> Vec<u64> {
        self.inner.sample_timestamps()
    }
}

impl MetricsSource for ParquetReader {
    fn query_range_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        self.query_range_opts(expr, start_s, end_s, step_s, opts)
    }

    fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        self.query(expr, time)
    }

    fn columns(&self, query: &str) -> Result<std::collections::HashSet<String>, QueryError> {
        self.columns(query)
    }

    fn time_range(&self) -> Option<(f64, f64)> {
        self.time_range()
    }

    fn time_range_ns(&self) -> Option<(u64, u64)> {
        self.time_range_ns()
    }

    fn interval(&self) -> f64 {
        self.interval()
    }

    fn source(&self) -> String {
        self.source()
    }

    fn version(&self) -> String {
        self.version()
    }

    fn filename(&self) -> Option<String> {
        self.filename.clone()
    }

    fn metadata_get(&self, key: &str) -> Option<String> {
        self.metadata_get(key)
    }

    fn file_metadata(&self) -> std::collections::HashMap<String, String> {
        self.file_metadata()
    }

    fn counter_names(&self) -> Vec<String> {
        self.counter_names()
    }

    fn gauge_names(&self) -> Vec<String> {
        self.gauge_names()
    }

    fn histogram_names(&self) -> Vec<String> {
        self.histogram_names()
    }

    fn counter_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        self.counter_labels(name)
    }

    fn gauge_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        self.gauge_labels(name)
    }

    fn histogram_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        self.histogram_labels(name)
    }

    fn sample_timestamps(&self) -> Vec<u64> {
        self.sample_timestamps()
    }
}

// ─── Builder ──────────────────────────────────────────────────────────────────

enum BuilderEntry {
    Path(std::path::PathBuf, Labels),
    Bytes(Bytes, Labels),
    OwnedFile(File, Labels),
    Source(Arc<ParquetSource>, Labels),
}

pub struct ParquetBuilder {
    entries: Vec<BuilderEntry>,
    filename: Option<String>,
    pool: Option<Arc<BufferPool>>,
}

impl Default for ParquetBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ParquetBuilder {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            filename: None,
            pool: None,
        }
    }

    /// Attach a `BufferPool` that all files opened by this builder will share.
    ///
    /// The pool is cloned into each `ParquetSource` at build time.  Multiple
    /// builders (and their resulting `ParquetReader`s) can share the same pool
    /// by passing the same `Arc`.
    pub fn pool(mut self, pool: Arc<BufferPool>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Add a file with no extra labels.
    pub fn file(mut self, path: impl AsRef<Path>) -> Self {
        self.entries.push(BuilderEntry::Path(
            path.as_ref().to_path_buf(),
            Labels::default(),
        ));
        self
    }

    /// Add a file whose series will carry `labels` as additional metadata.
    /// The labels are injected into every series from this file at query time.
    ///
    /// # Precondition
    /// Injected label keys must not conflict with column labels already present
    /// in the parquet file's schema; if they do, the native value is overwritten.
    pub fn file_labeled(mut self, path: impl AsRef<Path>, labels: impl Into<Labels>) -> Self {
        self.entries.push(BuilderEntry::Path(
            path.as_ref().to_path_buf(),
            labels.into(),
        ));
        self
    }

    /// Add an in-memory parquet source with no extra labels.
    /// Accepts any type that converts to `bytes::Bytes` (e.g. `Vec<u8>`, `&[u8]`, `Bytes`).
    /// `Bytes::clone()` is a refcount bump — cloning this source is cheap.
    pub fn bytes(mut self, bytes: impl Into<Bytes>) -> Self {
        self.entries
            .push(BuilderEntry::Bytes(bytes.into(), Labels::default()));
        self
    }

    /// Add an in-memory parquet source whose series will carry `labels` as additional metadata.
    pub fn bytes_labeled(mut self, bytes: impl Into<Bytes>, labels: impl Into<Labels>) -> Self {
        self.entries
            .push(BuilderEntry::Bytes(bytes.into(), labels.into()));
        self
    }

    /// Override the display name. Takes priority over basename auto-detection.
    pub fn filename(mut self, name: impl Into<String>) -> Self {
        self.filename = Some(name.into());
        self
    }

    /// Add an already-open file handle with no extra labels.
    ///
    /// Useful for the `NamedTempFile::into_file()` pattern where the path has
    /// already been unlinked but the data should remain accessible.
    pub fn file_owned(mut self, file: File) -> Self {
        self.entries
            .push(BuilderEntry::OwnedFile(file, Labels::default()));
        self
    }

    /// Add an already-open file handle whose series will carry `labels` as
    /// additional metadata.
    pub fn file_owned_labeled(mut self, file: File, labels: impl Into<Labels>) -> Self {
        self.entries
            .push(BuilderEntry::OwnedFile(file, labels.into()));
        self
    }

    /// Compose with all sources from an existing [`ParquetReader`].
    ///
    /// The per-file labels that were set when `reader` was built are preserved.
    /// No I/O occurs — the already-loaded `Arc<ParquetSource>` handles are
    /// reused directly.
    pub fn reader(mut self, reader: Arc<ParquetReader>) -> Self {
        for (source, labels) in reader.sources_for_composition() {
            self.entries.push(BuilderEntry::Source(source, labels));
        }
        self
    }

    /// Compose with an existing [`ParquetReader`], merging `extra` labels into
    /// every series the reader contributes.
    ///
    /// If the reader already has labels for a key that `extra` also contains,
    /// `extra` wins (overrides).
    pub fn reader_labeled(mut self, reader: Arc<ParquetReader>, extra: impl Into<Labels>) -> Self {
        let extra = extra.into();
        for (source, mut existing) in reader.sources_for_composition() {
            for (k, v) in &extra.inner {
                existing.inner.insert(k.clone(), v.clone());
            }
            self.entries.push(BuilderEntry::Source(source, existing));
        }
        self
    }

    pub fn build(self) -> Result<ParquetReader, Box<dyn Error>> {
        if self.entries.is_empty() {
            return Err("ParquetReader requires at least one file".into());
        }
        // Resolve filename: explicit > single-path basename > None
        let filename = self.filename.or_else(|| {
            if self.entries.len() == 1 {
                if let BuilderEntry::Path(ref p, _) = self.entries[0] {
                    return p.file_name().and_then(|n| n.to_str()).map(String::from);
                }
            }
            None
        });
        let pool = self.pool;
        let files: Result<Vec<(Arc<ParquetSource>, Labels)>, Box<dyn Error>> = self
            .entries
            .into_iter()
            .map(|entry| match entry {
                BuilderEntry::Path(path, labels) => {
                    let src = match &pool {
                        Some(p) => ParquetSource::open_with_pool(&path, Arc::clone(p))?,
                        None => ParquetSource::open(&path)?,
                    };
                    Ok((src, labels))
                }
                BuilderEntry::Bytes(bytes, labels) => {
                    let src = match &pool {
                        Some(p) => ParquetSource::open_bytes_with_pool(bytes, Arc::clone(p))?,
                        None => ParquetSource::open_bytes(bytes)?,
                    };
                    Ok((src, labels))
                }
                BuilderEntry::OwnedFile(file, labels) => {
                    let src = match &pool {
                        Some(p) => ParquetSource::open_file_with_pool(file, Arc::clone(p))?,
                        None => ParquetSource::open_file(file)?,
                    };
                    Ok((src, labels))
                }
                BuilderEntry::Source(source, labels) => Ok((source, labels)),
            })
            .collect();
        let inner = Arc::new(MultiParquetSource { files: files? });
        let ds: Arc<dyn DataSource> = inner.clone();
        Ok(ParquetReader {
            engine: QueryEngine::new(ds),
            inner,
            filename,
        })
    }
}

// ─── Multi-file source ────────────────────────────────────────────────────────

struct MultiParquetSource {
    files: Vec<(Arc<ParquetSource>, Labels)>,
}

/// Given a file's injected `extra` labels and the query `filter`:
/// - Returns `None` if `extra` contains a key from `filter` whose value
///   doesn't satisfy the filter constraint (skip this file entirely).
/// - Returns the filter with `extra`'s keys removed (since parquet columns
///   don't carry injected labels; the filter for those keys is already satisfied).
fn resolve_filter(extra: &Labels, filter: &Labels) -> Option<Labels> {
    if extra.inner.is_empty() {
        return Some(filter.clone());
    }
    // Build a sub-filter containing only keys present in extra.
    let extra_constrained = Labels {
        inner: filter
            .inner
            .iter()
            .filter(|(k, _)| extra.inner.contains_key(k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };
    // Check whether extra's values satisfy those constraints.
    if !extra.matches(&extra_constrained) {
        return None;
    }
    // Return filter with extra's keys stripped (parquet doesn't have them).
    Some(Labels {
        inner: filter
            .inner
            .iter()
            .filter(|(k, _)| !extra.inner.contains_key(k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    })
}

impl DataSource for MultiParquetSource {
    // NOTE: Counter series are naively concatenated across files.
    // Same (metric, label) pairs in multiple files produce duplicate series.
    // Label injection via file_labeled() enables filtering on injected keys.
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        raw: bool,
    ) -> Option<Counters> {
        let series: Vec<Counter> = self
            .files
            .iter()
            .filter_map(|(pf, extra)| {
                let pq_filter = resolve_filter(extra, filter)?;
                let counters = read_counters(pf, name, &pq_filter, start_ns, end_ns, raw).ok()?;
                Some((counters.series, extra.clone()))
            })
            .flat_map(|(series, extra)| {
                series.into_iter().map(move |mut c| {
                    for (k, v) in &extra.inner {
                        debug_assert!(
                            !c.labels.inner.contains_key(k),
                            "injected label key '{}' conflicts with native parquet label",
                            k
                        );
                        c.labels.inner.insert(k.clone(), v.clone());
                    }
                    c
                })
            })
            .collect();
        if series.is_empty() {
            None
        } else {
            Some(Counters { series })
        }
    }

    fn gauges(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        raw: bool,
    ) -> Option<Gauges> {
        let series: Vec<Gauge> = self
            .files
            .iter()
            .filter_map(|(pf, extra)| {
                let pq_filter = resolve_filter(extra, filter)?;
                let gauges = read_gauges(pf, name, &pq_filter, start_ns, end_ns, raw).ok()?;
                Some((gauges.series, extra.clone()))
            })
            .flat_map(|(series, extra)| {
                series.into_iter().map(move |mut g| {
                    for (k, v) in &extra.inner {
                        debug_assert!(
                            !g.labels.inner.contains_key(k),
                            "injected label key '{}' conflicts with native parquet label",
                            k
                        );
                        g.labels.inner.insert(k.clone(), v.clone());
                    }
                    g
                })
            })
            .collect();
        if series.is_empty() {
            None
        } else {
            Some(Gauges { series })
        }
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let streams: Vec<HistogramStream> = self
            .files
            .iter()
            .filter_map(|(pf, extra)| {
                let pq_filter = resolve_filter(extra, filter)?;
                let mut stream = pf.histogram_stream(name, &pq_filter, start_ns, end_ns)?;
                if !extra.inner.is_empty() {
                    for series_labels in &mut stream.meta.series {
                        for (k, v) in &extra.inner {
                            debug_assert!(
                                !series_labels.inner.contains_key(k),
                                "injected label key '{}' conflicts with native parquet label",
                                k
                            );
                            series_labels.inner.insert(k.clone(), v.clone());
                        }
                    }
                }
                Some(stream)
            })
            .collect();
        HistogramStream::merge(streams)
    }

    fn file_metadata(&self) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        for (pf, _) in &self.files {
            out.extend(pf.read_file_metadata());
        }
        out
    }

    fn metadata_get(&self, key: &str) -> Option<String> {
        // Walk files; last value wins (matches file_metadata() merge semantics).
        let mut last: Option<String> = None;
        for (pf, _) in &self.files {
            if let Some(v) = pf.read_file_metadata_value(key) {
                last = Some(v);
            }
        }
        last
    }

    fn interval(&self) -> f64 {
        self.files
            .iter()
            .map(|(pf, _)| pf.sampling_interval_ms as f64 / 1000.0)
            .fold(f64::MAX, f64::min)
    }

    fn time_range(&self) -> Option<(u64, u64)> {
        let (mut lo, mut hi): (Option<u64>, Option<u64>) = (None, None);
        for (pf, _) in &self.files {
            if let Some((a, b)) = pf.time_range_from_stats() {
                lo = Some(lo.map_or(a, |m: u64| m.min(a)));
                hi = Some(hi.map_or(b, |m: u64| m.max(b)));
            }
        }
        lo.zip(hi)
    }

    fn counter_names(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (pf, _) in &self.files {
            let ts = pf.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
            for col in parse_schema(pf, ts) {
                if matches!(col.kind, ColKind::Counter) {
                    names.insert(col.name);
                }
            }
        }
        names.into_iter().collect()
    }

    fn gauge_names(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (pf, _) in &self.files {
            let ts = pf.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
            for col in parse_schema(pf, ts) {
                if matches!(col.kind, ColKind::Gauge) {
                    names.insert(col.name);
                }
            }
        }
        names.into_iter().collect()
    }

    fn histogram_names(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (pf, _) in &self.files {
            let ts = pf.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
            for col in parse_schema(pf, ts) {
                if matches!(col.kind, ColKind::Histogram { .. }) {
                    names.insert(col.name);
                }
            }
        }
        names.into_iter().collect()
    }

    fn counter_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        let mut sets: Vec<std::collections::BTreeMap<String, String>> = Vec::new();
        for (pf, extra) in &self.files {
            let ts = pf.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
            for col in parse_schema(pf, ts) {
                if matches!(col.kind, ColKind::Counter) && col.name == name {
                    let mut labels = col.labels.inner.clone();
                    for (k, v) in &extra.inner {
                        labels.insert(k.clone(), v.clone());
                    }
                    sets.push(labels);
                }
            }
        }
        sets.sort();
        sets.dedup();
        sets
    }

    fn gauge_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        let mut sets: Vec<std::collections::BTreeMap<String, String>> = Vec::new();
        for (pf, extra) in &self.files {
            let ts = pf.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
            for col in parse_schema(pf, ts) {
                if matches!(col.kind, ColKind::Gauge) && col.name == name {
                    let mut labels = col.labels.inner.clone();
                    for (k, v) in &extra.inner {
                        labels.insert(k.clone(), v.clone());
                    }
                    sets.push(labels);
                }
            }
        }
        sets.sort();
        sets.dedup();
        sets
    }

    fn histogram_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        let mut sets: Vec<std::collections::BTreeMap<String, String>> = Vec::new();
        for (pf, extra) in &self.files {
            let ts = pf.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
            for col in parse_schema(pf, ts) {
                if matches!(col.kind, ColKind::Histogram { .. }) && col.name == name {
                    let mut labels = col.labels.inner.clone();
                    for (k, v) in &extra.inner {
                        labels.insert(k.clone(), v.clone());
                    }
                    sets.push(labels);
                }
            }
        }
        sets.sort();
        sets.dedup();
        sets
    }

    fn column_map(
        &self,
    ) -> std::collections::HashMap<String, std::collections::HashMap<Labels, String>> {
        let mut out = std::collections::HashMap::new();
        for (pf, _) in &self.files {
            for (metric, cols) in pf.column_map() {
                out.entry(metric)
                    .or_insert_with(std::collections::HashMap::new)
                    .extend(cols);
            }
        }
        out
    }
}

impl MultiParquetSource {
    /// Raw `timestamp` column values across every file, in file-list then
    /// row-group order — un-snapped, unlike the query path (`read_timestamps`)
    /// which rounds to the nominal sampling grid.
    fn sample_timestamps(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for (pf, _labels) in &self.files {
            let Ok(ts_col_idx) = pf.meta.schema().index_of("timestamp") else {
                continue;
            };
            let num_rgs = pf.meta.metadata().num_row_groups();
            for rg_idx in 0..num_rgs {
                match read_raw_u64_rg(pf, rg_idx, ts_col_idx) {
                    Ok(values) => out.extend(values),
                    Err(e) => {
                        tracing::warn!(
                            source_id = pf.id,
                            rg_idx,
                            error = %e,
                            "skipping row group in sample_timestamps",
                        );
                    }
                }
            }
        }
        out
    }
}

// ─── Private file reader ──────────────────────────────────────────────────────

enum ParquetBacking {
    /// File-backed. We hold the `File` in an `Arc` so that concurrent queries
    /// against the same source share the file descriptor without duplicating
    /// it. Reads go through `PositionalFile`'s `ChunkReader` impl, which uses
    /// positional reads — they do not touch the shared seek offset, so
    /// concurrent reads cannot interleave-corrupt each other.
    File(PositionalFile),
    Bytes(Bytes),
}

/// Positional read into `file` at byte `offset`. Does not touch the shared
/// seek offset on Unix (via `pread`) or Windows (via `seek_read`). On other
/// targets falls back to a non-atomic `seek + read` — those platforms must
/// avoid concurrent reads on the same file handle.
fn pread(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(buf, offset)
    }
    #[cfg(not(any(unix, windows)))]
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut f: &File = file;
        f.seek(SeekFrom::Start(offset))?;
        f.read(buf)
    }
}

/// Wrapper around `Arc<File>` that implements parquet's `ChunkReader` trait
/// using positional reads. Cloning is cheap (`Arc::clone`). On Unix and
/// Windows, two readers from different threads can read concurrently without
/// interfering because positional reads do not advance the file's current
/// offset.
#[derive(Clone)]
pub(crate) struct PositionalFile {
    inner: Arc<File>,
    len: u64,
}

impl PositionalFile {
    fn new(file: File) -> std::io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self {
            inner: Arc::new(file),
            len,
        })
    }
}

impl parquet::file::reader::Length for PositionalFile {
    fn len(&self) -> u64 {
        self.len
    }
}

/// `Read` adapter built on `pread` so it does not depend on or modify the
/// file's seek offset.
pub(crate) struct PositionalReader {
    file: Arc<File>,
    pos: u64,
    end: u64,
}

impl std::io::Read for PositionalReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.end {
            return Ok(0);
        }
        let remaining = (self.end - self.pos) as usize;
        let to_read = buf.len().min(remaining);
        let n = pread(&self.file, &mut buf[..to_read], self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl parquet::file::reader::ChunkReader for PositionalFile {
    type T = PositionalReader;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        Ok(PositionalReader {
            file: Arc::clone(&self.inner),
            pos: start,
            end: self.len,
        })
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        let mut buf = vec![0u8; length];
        let mut read_total = 0usize;
        // pread can be a partial read; loop until full or EOF.
        while read_total < length {
            let n = pread(
                &self.inner,
                &mut buf[read_total..],
                start + read_total as u64,
            )?;
            if n == 0 {
                break;
            }
            read_total += n;
        }
        buf.truncate(read_total);
        Ok(Bytes::from(buf))
    }
}

pub(crate) struct ParquetSource {
    /// Unique numeric identity assigned at construction; used as the `source_id`
    /// component of every `CacheKey` produced by this source.
    id: u64,
    backing: ParquetBacking,
    meta: ArrowReaderMetadata,
    sampling_interval_ms: u64,
    /// Optional shared buffer pool for decoded row-group blocks.
    pool: Option<Arc<BufferPool>>,
}

fn parse_sampling_interval(meta: &ArrowReaderMetadata) -> u64 {
    let mut file_metadata: HashMap<String, String> = HashMap::new();
    if let Some(kv) = meta.metadata().file_metadata().key_value_metadata() {
        for entry in kv {
            file_metadata.insert(entry.key.clone(), entry.value.clone().unwrap_or_default());
        }
    }
    file_metadata
        .get("sampling_interval_ms")
        .map(|v| v.parse::<u64>().expect("bad interval"))
        .unwrap_or(1000)
}

impl ParquetSource {
    fn open(path: &Path) -> Result<Arc<Self>, Box<dyn Error>> {
        let file = File::open(path)?;
        let meta = ArrowReaderMetadata::load(&file, ArrowReaderOptions::default())?;
        let sampling_interval_ms = parse_sampling_interval(&meta);
        Ok(Arc::new(Self {
            id: next_source_id(),
            backing: ParquetBacking::File(PositionalFile::new(file)?),
            meta,
            sampling_interval_ms,
            pool: None,
        }))
    }

    fn open_bytes(bytes: Bytes) -> Result<Arc<Self>, Box<dyn Error>> {
        let meta = ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::default())?;
        let sampling_interval_ms = parse_sampling_interval(&meta);
        Ok(Arc::new(Self {
            id: next_source_id(),
            backing: ParquetBacking::Bytes(bytes),
            meta,
            sampling_interval_ms,
            pool: None,
        }))
    }

    fn open_file(file: File) -> Result<Arc<Self>, Box<dyn Error>> {
        let meta = ArrowReaderMetadata::load(&file, ArrowReaderOptions::default())?;
        let sampling_interval_ms = parse_sampling_interval(&meta);
        Ok(Arc::new(Self {
            id: next_source_id(),
            backing: ParquetBacking::File(PositionalFile::new(file)?),
            meta,
            sampling_interval_ms,
            pool: None,
        }))
    }

    fn open_with_pool(path: &Path, pool: Arc<BufferPool>) -> Result<Arc<Self>, Box<dyn Error>> {
        let file = File::open(path)?;
        let meta = ArrowReaderMetadata::load(&file, ArrowReaderOptions::default())?;
        let sampling_interval_ms = parse_sampling_interval(&meta);
        Ok(Arc::new(Self {
            id: next_source_id(),
            backing: ParquetBacking::File(PositionalFile::new(file)?),
            meta,
            sampling_interval_ms,
            pool: Some(pool),
        }))
    }

    fn open_bytes_with_pool(
        bytes: Bytes,
        pool: Arc<BufferPool>,
    ) -> Result<Arc<Self>, Box<dyn Error>> {
        let meta = ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::default())?;
        let sampling_interval_ms = parse_sampling_interval(&meta);
        Ok(Arc::new(Self {
            id: next_source_id(),
            backing: ParquetBacking::Bytes(bytes),
            meta,
            sampling_interval_ms,
            pool: Some(pool),
        }))
    }

    fn open_file_with_pool(file: File, pool: Arc<BufferPool>) -> Result<Arc<Self>, Box<dyn Error>> {
        let meta = ArrowReaderMetadata::load(&file, ArrowReaderOptions::default())?;
        let sampling_interval_ms = parse_sampling_interval(&meta);
        Ok(Arc::new(Self {
            id: next_source_id(),
            backing: ParquetBacking::File(PositionalFile::new(file)?),
            meta,
            sampling_interval_ms,
            pool: Some(pool),
        }))
    }

    fn build_batch_reader(
        &self,
        rg_idx: usize,
        projection: ProjectionMask,
    ) -> Result<parquet::arrow::arrow_reader::ParquetRecordBatchReader, Box<dyn Error>> {
        let reader = match &self.backing {
            ParquetBacking::File(f) => {
                // PositionalFile is Clone (Arc), and ChunkReader, so we don't
                // need (and must not use) try_clone — its positional reads are
                // concurrency-safe across queries against the same source.
                ParquetRecordBatchReaderBuilder::new_with_metadata(f.clone(), self.meta.clone())
                    .with_row_groups(vec![rg_idx])
                    .with_projection(projection)
                    .build()?
            }
            ParquetBacking::Bytes(b) => {
                ParquetRecordBatchReaderBuilder::new_with_metadata(b.clone(), self.meta.clone())
                    .with_row_groups(vec![rg_idx])
                    .with_projection(projection)
                    .build()?
            }
        };
        Ok(reader)
    }

    fn read_file_metadata(&self) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        if let Some(kv) = self.meta.metadata().file_metadata().key_value_metadata() {
            for entry in kv {
                out.insert(entry.key.clone(), entry.value.clone().unwrap_or_default());
            }
        }
        out
    }

    /// Walk key-value metadata entries looking for `key` without building a HashMap.
    fn read_file_metadata_value(&self, key: &str) -> Option<String> {
        self.meta
            .metadata()
            .file_metadata()
            .key_value_metadata()?
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.value.clone().unwrap_or_default())
    }

    fn time_range_from_stats(&self) -> Option<(u64, u64)> {
        let ts_col_idx = self.meta.schema().index_of("timestamp").ok()?;
        let pq_metadata = self.meta.metadata();
        let mut min_ns: Option<u64> = None;
        let mut max_ns: Option<u64> = None;
        for rg_idx in 0..pq_metadata.num_row_groups() {
            let Some(stats) = pq_metadata
                .row_group(rg_idx)
                .column(ts_col_idx)
                .statistics()
            else {
                continue;
            };
            if let Statistics::Int64(s) = stats {
                if let Some(v) = s.min_opt() {
                    let ts = *v as u64;
                    min_ns = Some(min_ns.map_or(ts, |m: u64| m.min(ts)));
                }
                if let Some(v) = s.max_opt() {
                    let ts = *v as u64;
                    max_ns = Some(max_ns.map_or(ts, |m: u64| m.max(ts)));
                }
            }
        }
        min_ns.zip(max_ns)
    }

    fn histogram_stream(
        self: &Arc<Self>,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let ts_col_idx = self.meta.schema().index_of("timestamp").ok()?;
        let interval_ns = self.sampling_interval_ms * 1_000_000;
        let num_rgs = self.meta.metadata().num_row_groups();

        let col_descs: Vec<ColDesc> = parse_schema(self, ts_col_idx)
            .into_iter()
            .filter(|c| {
                matches!(c.kind, ColKind::Histogram { .. })
                    && c.name == name
                    && (filter.inner.is_empty() || c.labels.matches(filter))
            })
            .collect();

        if col_descs.is_empty() {
            return None;
        }

        let config = col_descs.iter().find_map(|c| match c.kind {
            ColKind::Histogram {
                grouping_power: gp,
                max_value_power: mvp,
            } => ::histogram::Config::new(gp, mvp).ok(),
            _ => None,
        })?;

        let series: Vec<Labels> = col_descs.iter().map(|c| c.labels.clone()).collect();

        let rg_queue: std::collections::VecDeque<usize> = (0..num_rgs)
            .filter(|&rg_idx| {
                !matches!(
                    rg_classify(
                        self.meta.metadata().row_group(rg_idx),
                        ts_col_idx,
                        start_ns,
                        end_ns,
                    ),
                    RgClass::Before | RgClass::After,
                )
            })
            .collect();

        let cursor = ParquetHistogramCursor {
            pf: Arc::clone(self),
            ts_col_idx,
            interval_ns,
            start_ns,
            end_ns,
            col_descs,
            rg_queue,
            pending: std::collections::VecDeque::new(),
        };

        Some(HistogramStream {
            meta: HistogramStreamMeta { config, series },
            rows: Box::new(cursor),
        })
    }

    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        let ts_col_idx = self
            .meta
            .schema()
            .index_of("timestamp")
            .unwrap_or(usize::MAX);
        let mut out: HashMap<String, HashMap<Labels, String>> = HashMap::new();
        for c in parse_schema(self, ts_col_idx) {
            out.entry(c.name)
                .or_default()
                .insert(c.labels, c.column_name);
        }
        out
    }
}

// ─── Row group classification ─────────────────────────────────────────────────

enum RgClass {
    Before,
    Overlaps,
    After,
    Unknown,
}

fn rg_classify(rg: &RowGroupMetaData, ts_col_idx: usize, start_ns: u64, end_ns: u64) -> RgClass {
    let Some(stats) = rg.column(ts_col_idx).statistics() else {
        return RgClass::Unknown;
    };
    let Statistics::Int64(s) = stats else {
        return RgClass::Unknown;
    };
    let (Some(rg_min), Some(rg_max)) = (s.min_opt(), s.max_opt()) else {
        return RgClass::Unknown;
    };
    let (rg_min, rg_max) = (*rg_min as u64, *rg_max as u64);
    if rg_max < start_ns {
        RgClass::Before
    } else if rg_min > end_ns {
        RgClass::After
    } else {
        RgClass::Overlaps
    }
}

// ─── Schema parsing ───────────────────────────────────────────────────────────

enum ColKind {
    Counter,
    Gauge,
    Histogram {
        grouping_power: u8,
        max_value_power: u8,
    },
}

struct ColDesc {
    col_idx: usize,
    name: String,
    labels: Labels,
    column_name: String,
    kind: ColKind,
    /// Column index of this metric's acquisition-window begin (Int64 offset
    /// from the raw row timestamp), if present. Resolved atomically with
    /// `width_col` by [`resolve_window_cols`]: the metric's own
    /// `<m>:window_begin` sidecar (only if its `<m>:window_width` twin is
    /// also present), else the table-level bare `:window_begin` column
    /// (only if `:window_width` is also present), else `None`.
    begin_col: Option<usize>,
    /// Column index of this metric's acquisition-window width (UInt64 ns),
    /// if present. Resolved as an atomic pair with `begin_col` — see there.
    width_col: Option<usize>,
}

/// Resolve one metric's acquisition-window sidecar columns as an ATOMIC
/// pair, never mixing one source's begin with another's width (that would
/// fabricate a window describing no real acquisition, and the band math
/// would be silently wrong rather than absent).
///
/// Precedence:
/// 1. The metric's own `<m>:window_begin`/`<m>:window_width` sidecar,
///    only if BOTH are present.
/// 2. Else the table-level bare `:window_begin`/`:window_width` pair,
///    only if BOTH are present.
/// 3. Else neither (`(None, None)`) — including when either source has
///    only one half of its pair (e.g. a bare `:window_begin` with no
///    matching `:window_width`).
fn resolve_window_cols(
    own_begin: Option<usize>,
    own_width: Option<usize>,
    table_begin: Option<usize>,
    table_width: Option<usize>,
) -> (Option<usize>, Option<usize>) {
    match (own_begin, own_width) {
        (Some(b), Some(w)) => (Some(b), Some(w)),
        _ => match (table_begin, table_width) {
            (Some(b), Some(w)) => (Some(b), Some(w)),
            _ => (None, None),
        },
    }
}

fn parse_schema(pf: &ParquetSource, ts_col_idx: usize) -> Vec<ColDesc> {
    let fields = pf.meta.schema().fields();
    // Pass 1: window sidecar column indices, keyed by base column name.
    // A column named exactly `:window_begin` / `:window_width` (no base
    // name — `strip_suffix` yields "") is the table-level pair: one shared
    // acquisition window for every metric in the table, used as the
    // fallback when a metric has no `<m>:window_begin`/`<m>:window_width`
    // sidecar of its own. Both bare names are reserved (see the metric-skip
    // check below) and never surface as metrics themselves.
    let mut win_begin: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut win_width: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (col_idx, field) in fields.iter().enumerate() {
        let n = field.name();
        if let Some(base) = n.strip_suffix(":window_begin") {
            win_begin.insert(base.to_string(), col_idx);
        } else if let Some(base) = n.strip_suffix(":window_width") {
            win_width.insert(base.to_string(), col_idx);
        }
    }
    let table_begin_col = win_begin.get("").copied();
    let table_width_col = win_width.get("").copied();
    // Pass 2: metric ColDescs, attaching window indices by column name.
    fields
        .iter()
        .enumerate()
        .filter_map(|(col_idx, field)| {
            if col_idx == ts_col_idx {
                return None;
            }
            let mut meta = field.metadata().clone();
            let column_name = field.name().to_string();
            // Acquisition-window sidecar columns (`<m>:window_begin` Int64,
            // `<m>:window_width` UInt64 — per-metric; or the bare
            // `:window_begin`/`:window_width` table-level pair) describe an
            // observation window — they are not metrics. Without this skip
            // they would classify by Arrow type as a phantom gauge / counter.
            // The window path reads them for rate/histogram error bars.
            //
            // NOTE: `:window_begin` / `:window_width`, both as a per-metric
            // suffix and as the bare table-level column name, are therefore
            // RESERVED — a real metric literally named `foo:window_begin` (or
            // just `window_begin`, colon-prefixed) would be silently treated
            // as a sidecar and dropped. Acceptable given the recorder never
            // emits such names, but the reservation is intentional.
            if column_name.ends_with(":window_begin") || column_name.ends_with(":window_width") {
                return None;
            }
            // `:wall_offset` is a bare, table-level sidecar (one per table, not
            // one per metric): the raw wall-clock reading at each row's tick,
            // vs. the monotonic-anchored `timestamp` column. It carries no
            // `metric` metadata and would otherwise classify by Arrow type as
            // a phantom gauge. Unlike `:window_begin`/`:window_width`, it is
            // not a per-metric suffix — it has no name prefix — so match it
            // exactly rather than by `ends_with`.
            //
            // NOTE: `:wall_offset` is therefore also a RESERVED column name,
            // alongside the `:window_begin` / `:window_width` suffixes above.
            if column_name == ":wall_offset" {
                return None;
            }
            let name = meta.get("metric").cloned().unwrap_or_else(|| {
                column_name
                    .strip_suffix(":buckets")
                    .unwrap_or(&column_name)
                    .to_string()
            });
            let grouping_power: Option<u8> =
                meta.remove("grouping_power").and_then(|v| v.parse().ok());
            let max_value_power: Option<u8> =
                meta.remove("max_value_power").and_then(|v| v.parse().ok());
            let mut labels = Labels::default();
            for (k, v) in meta.iter() {
                match k.as_str() {
                    "metric" | "metric_type" | "unit" => continue,
                    _ => {
                        labels.inner.insert(k.clone(), v.clone());
                    }
                }
            }
            let kind = match field.data_type() {
                DataType::UInt64 => ColKind::Counter,
                DataType::Int64 => ColKind::Gauge,
                DataType::List(inner) if inner.data_type() == &DataType::UInt64 => {
                    let (Some(gp), Some(mvp)) = (grouping_power, max_value_power) else {
                        return None;
                    };
                    ColKind::Histogram {
                        grouping_power: gp,
                        max_value_power: mvp,
                    }
                }
                _ => return None,
            };
            // Precedence, resolved as an atomic pair (see resolve_window_cols):
            // this metric's own sidecar pair, else the table-level bare pair,
            // else no window columns at all. Never mixes one source's begin
            // with another's width.
            let (begin_col, width_col) = resolve_window_cols(
                win_begin.get(&column_name).copied(),
                win_width.get(&column_name).copied(),
                table_begin_col,
                table_width_col,
            );
            Some(ColDesc {
                col_idx,
                name,
                labels,
                column_name,
                kind,
                begin_col,
                width_col,
            })
        })
        .collect()
}

// ─── Timestamp reader ─────────────────────────────────────────────────────────

fn snap_timestamp(ts: u64, interval_ns: u64) -> u64 {
    (ts + interval_ns / 2)
        .checked_div(interval_ns)
        .map_or(ts, |q| q * interval_ns)
}

/// Run a parquet decode block, catching any panic from the parquet crate so a
/// malformed file (e.g. dictionary pages out of order — apache/arrow-rs has
/// known panics like "Decoder for dict should have been set") doesn't crash
/// the server.
///
/// Also suppresses the global panic hook (e.g. sentry's panic_handler) for
/// the duration of the call so deliberate catches don't pollute the error
/// reporting pipeline. The first call to this function installs a chained
/// panic hook that defers to the previously-installed hook for all panics
/// EXCEPT those that occur inside `catch_decode_panic`.
fn catch_decode_panic<T>(op: impl FnOnce() -> T) -> Result<T, String> {
    ensure_panic_hook_installed();
    SUPPRESS_DECODE_PANIC.with(|f| f.set(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(op));
    SUPPRESS_DECODE_PANIC.with(|f| f.set(false));

    result.map_err(|payload| {
        if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "<unknown panic payload>".to_string()
        }
    })
}

std::thread_local! {
    static SUPPRESS_DECODE_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

static PANIC_HOOK_INSTALL: std::sync::Once = std::sync::Once::new();

/// Install a panic hook (once per process) that chains on top of whatever
/// hook is currently active. When a panic fires while a thread has
/// `SUPPRESS_DECODE_PANIC` set, we skip the upstream hook so the panic
/// doesn't reach reporters like Sentry. All other panics still propagate
/// normally.
fn ensure_panic_hook_installed() {
    PANIC_HOOK_INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if SUPPRESS_DECODE_PANIC.with(|f| f.get()) {
                // Suppressed: this panic is caught by catch_decode_panic.
                return;
            }
            previous(info);
        }));
    });
}

fn read_timestamps(
    pf: &ParquetSource,
    rg_idx: usize,
    ts_col_idx: usize,
    interval_ns: u64,
) -> Result<Arc<Vec<Option<u64>>>, Box<dyn Error>> {
    let key = CacheKey {
        source_id: pf.id,
        column_idx: ts_col_idx,
        row_group_idx: rg_idx,
    };

    if let Some(pool) = &pf.pool {
        if let Some(cached) = pool.get_timestamps(key) {
            return Ok(cached);
        }
    }

    let parquet_schema = pf.meta.metadata().file_metadata().schema_descr_ptr();
    let reader =
        pf.build_batch_reader(rg_idx, ProjectionMask::roots(&parquet_schema, [ts_col_idx]))?;
    let decode_result: Result<Result<Vec<Option<u64>>, Box<dyn Error>>, String> =
        catch_decode_panic(|| {
            let mut out = Vec::new();
            for batch_result in reader {
                let batch = match batch_result {
                    Ok(b) => b,
                    Err(e) => {
                        // BAIL on first error instead of silently retrying via .flatten().
                        // Returning Err repeatedly without advancing internal state has been
                        // observed to spin the reader in production. We log and stop reading
                        // this row group, accepting whatever batches we got so far.
                        tracing::warn!(
                            rg_idx,
                            col_idx = ts_col_idx,
                            source_id = pf.id,
                            error = %e,
                            "aborting timestamp read on parquet error",
                        );
                        break;
                    }
                };
                let arr = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or::<Box<dyn Error>>("timestamp column is not UInt64".into())?;
                out.reserve(arr.len());
                for v in arr.iter() {
                    out.push(v.map(|raw| snap_timestamp(raw, interval_ns)));
                }
            }
            Ok(out)
        });
    let out = match decode_result {
        Ok(inner) => inner?,
        Err(panic_msg) => {
            tracing::error!(
                rg_idx,
                col_idx = ts_col_idx,
                source_id = pf.id,
                panic = %panic_msg,
                "parquet decode panic reading timestamps",
            );
            return Err(format!(
                "parquet decode panic reading timestamps (rg={rg_idx}, col={ts_col_idx}): {panic_msg}"
            )
            .into());
        }
    };
    let result = Arc::new(out);

    if let Some(pool) = &pf.pool {
        pool.put_timestamps(key, Arc::clone(&result));
    }

    Ok(result)
}

/// Read raw UInt64 values from `col_idx` in a single row group, in row
/// order, dropping nulls. Unlike `read_timestamps`, this does NOT snap to
/// the sampling grid and does NOT go through the buffer pool (which caches
/// only the snapped form) — callers that need the true on-disk values
/// (e.g. jitter visualization) need the untouched column.
fn read_raw_u64_rg(
    pf: &ParquetSource,
    rg_idx: usize,
    col_idx: usize,
) -> Result<Vec<u64>, Box<dyn Error>> {
    let parquet_schema = pf.meta.metadata().file_metadata().schema_descr_ptr();
    let reader =
        pf.build_batch_reader(rg_idx, ProjectionMask::roots(&parquet_schema, [col_idx]))?;
    let decode_result: Result<Result<Vec<u64>, Box<dyn Error>>, String> =
        catch_decode_panic(|| {
            let mut out = Vec::new();
            for batch_result in reader {
                let batch = match batch_result {
                    Ok(b) => b,
                    Err(e) => {
                        // Same bail-on-first-error rationale as read_timestamps: stop this
                        // row group rather than risk spinning the reader on repeated errors.
                        tracing::warn!(
                            rg_idx,
                            col_idx,
                            source_id = pf.id,
                            error = %e,
                            "aborting raw column read on parquet error",
                        );
                        break;
                    }
                };
                let arr = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or::<Box<dyn Error>>("column is not UInt64".into())?;
                out.extend(arr.iter().flatten());
            }
            Ok(out)
        });
    match decode_result {
        Ok(inner) => inner,
        Err(panic_msg) => {
            tracing::error!(
                rg_idx,
                col_idx,
                source_id = pf.id,
                panic = %panic_msg,
                "parquet decode panic reading raw column",
            );
            Err(format!(
                "parquet decode panic reading raw column (rg={rg_idx}, col={col_idx}): {panic_msg}"
            )
            .into())
        }
    }
}

fn read_counter_values_per_rg(
    pf: &ParquetSource,
    rg_idx: usize,
    col_idx: usize,
) -> Result<Arc<Vec<Option<u64>>>, Box<dyn Error>> {
    let key = CacheKey {
        source_id: pf.id,
        column_idx: col_idx,
        row_group_idx: rg_idx,
    };

    if let Some(pool) = &pf.pool {
        if let Some(cached) = pool.get_counter_values(key) {
            return Ok(cached);
        }
    }

    let parquet_schema = pf.meta.metadata().file_metadata().schema_descr_ptr();
    let reader =
        pf.build_batch_reader(rg_idx, ProjectionMask::roots(&parquet_schema, [col_idx]))?;
    let decode_result: Result<Result<Vec<Option<u64>>, Box<dyn Error>>, String> =
        catch_decode_panic(|| {
            let mut out = Vec::new();
            for batch_result in reader {
                let batch = match batch_result {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            rg_idx,
                            col_idx,
                            source_id = pf.id,
                            error = %e,
                            "aborting counter read on parquet error",
                        );
                        break;
                    }
                };
                let arr = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or::<Box<dyn Error>>("counter column is not UInt64".into())?;
                out.reserve(arr.len());
                for v in arr.iter() {
                    out.push(v); // preserve None so row indexing stays aligned with timestamps
                }
            }
            Ok(out)
        });
    let out = match decode_result {
        Ok(inner) => inner?,
        Err(panic_msg) => {
            tracing::error!(
                rg_idx,
                col_idx,
                source_id = pf.id,
                panic = %panic_msg,
                "parquet decode panic reading counter",
            );
            return Err(format!(
                "parquet decode panic reading counter (rg={rg_idx}, col={col_idx}): {panic_msg}"
            )
            .into());
        }
    };
    let result = Arc::new(out);

    if let Some(pool) = &pf.pool {
        pool.put_counter_values(key, Arc::clone(&result));
    }

    Ok(result)
}

fn read_gauge_values_per_rg(
    pf: &ParquetSource,
    rg_idx: usize,
    col_idx: usize,
) -> Result<Arc<Vec<Option<i64>>>, Box<dyn Error>> {
    let key = CacheKey {
        source_id: pf.id,
        column_idx: col_idx,
        row_group_idx: rg_idx,
    };

    if let Some(pool) = &pf.pool {
        if let Some(cached) = pool.get_gauge_values(key) {
            return Ok(cached);
        }
    }

    let parquet_schema = pf.meta.metadata().file_metadata().schema_descr_ptr();
    let reader =
        pf.build_batch_reader(rg_idx, ProjectionMask::roots(&parquet_schema, [col_idx]))?;
    let decode_result: Result<Result<Vec<Option<i64>>, Box<dyn Error>>, String> =
        catch_decode_panic(|| {
            let mut out = Vec::new();
            for batch_result in reader {
                let batch = match batch_result {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            rg_idx,
                            col_idx,
                            source_id = pf.id,
                            error = %e,
                            "aborting gauge read on parquet error",
                        );
                        break;
                    }
                };
                let arr = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .ok_or::<Box<dyn Error>>("gauge column is not Int64".into())?;
                out.reserve(arr.len());
                for v in arr.iter() {
                    out.push(v); // preserve None for row alignment
                }
            }
            Ok(out)
        });
    let out = match decode_result {
        Ok(inner) => inner?,
        Err(panic_msg) => {
            tracing::error!(
                rg_idx,
                col_idx,
                source_id = pf.id,
                panic = %panic_msg,
                "parquet decode panic reading gauge",
            );
            return Err(format!(
                "parquet decode panic reading gauge (rg={rg_idx}, col={col_idx}): {panic_msg}"
            )
            .into());
        }
    };
    let result = Arc::new(out);

    if let Some(pool) = &pf.pool {
        pool.put_gauge_values(key, Arc::clone(&result));
    }

    Ok(result)
}

// ─── Counter reader ───────────────────────────────────────────────────────────

/// Resolve one row's acquisition window, `base` being the raw row timestamp.
///
/// Precedence:
/// 1. **Per-observation sidecar** `[base+begin, base+begin+width]` — the tight,
///    per-metric window (`.rez` / live). `begin` may be negative; the start is
///    saturated and clamped to 0 so a corrupt offset can't wrap or overflow.
/// 2. **Fleet fallback** `[base, base+duration]` — when there is no sidecar but a
///    snapshot `duration` is present, synthesize the coarse per-snapshot
///    collection window (same `[begin, begin+elapsed]` shape the agent records).
///    Lets windowless (older/plain-parquet) recordings still carry rate bands.
/// 3. **Degenerate** `[base, base]` — neither sidecar nor duration available.
fn resolve_window(
    base: u64,
    begin_off: Option<i64>,
    width: Option<u64>,
    duration: Option<u64>,
) -> (u64, u64) {
    match (begin_off, width) {
        (Some(bo), Some(wd)) => {
            let begin_ns = (base as i64).saturating_add(bo).max(0) as u64;
            (begin_ns, begin_ns.saturating_add(wd))
        }
        _ => match duration {
            Some(dur) => (base, base.saturating_add(dur)),
            None => (base, base),
        },
    }
}

fn read_counters(
    pf: &ParquetSource,
    name: &str,
    filter: &Labels,
    start_ns: u64,
    end_ns: u64,
    raw: bool,
) -> Result<Counters, Box<dyn Error>> {
    let ts_col_idx = pf
        .meta
        .schema()
        .index_of("timestamp")
        .map_err(|_| "missing timestamp")?;
    // PROTOTYPE (duration fallback): the snapshot-level collection window. When a
    // metric has no per-observation :window_* sidecar, we synthesize a coarse
    // fleet window [timestamp, timestamp + duration] from this column — the same
    // [begin, begin+elapsed] formula the agent uses, just per-snapshot. Lets old
    // (windowless) recordings carry rate() uncertainty.
    let dur_col_idx = pf.meta.schema().index_of("duration").ok();
    let interval_ns = pf.sampling_interval_ms * 1_000_000;
    let num_rgs = pf.meta.metadata().num_row_groups();

    let cols: Vec<ColDesc> = parse_schema(pf, ts_col_idx)
        .into_iter()
        .filter(|c| {
            matches!(c.kind, ColKind::Counter)
                && c.name == name
                && (filter.inner.is_empty() || c.labels.matches(filter))
        })
        .collect();
    if cols.is_empty() {
        return Ok(Counters { series: vec![] });
    }

    let mut ts_acc: Vec<Vec<u64>> = vec![Vec::new(); cols.len()];
    let mut val_acc: Vec<Vec<u64>> = vec![Vec::new(); cols.len()];
    // Reconstruct windows when a metric carries sidecars OR a duration column
    // exists to synthesize the fleet fallback.
    let mut win_acc: Vec<Option<Vec<(u64, u64)>>> = cols
        .iter()
        .map(|c| {
            if (c.begin_col.is_some() && c.width_col.is_some()) || dur_col_idx.is_some() {
                Some(Vec::new())
            } else {
                None
            }
        })
        .collect();

    for rg_idx in 0..num_rgs {
        match rg_classify(
            pf.meta.metadata().row_group(rg_idx),
            ts_col_idx,
            start_ns,
            end_ns,
        ) {
            RgClass::Before | RgClass::After => continue,
            _ => {}
        }
        let timestamps = read_timestamps(pf, rg_idx, ts_col_idx, interval_ns)?;
        // Window offsets were written by the recorder relative to the RAW
        // (un-snapped) timestamp, and `time_range`/the stats bounds are raw too,
        // so filter and reconstruct windows against the raw column; the emitted
        // point keeps the snapped grid for cross-series alignment. (The ts column
        // is non-nullable, so the raw Vec aligns 1:1 with the snapped one; guard
        // on length and fall back to snapped if that ever fails to hold.)
        let raw_ts = read_raw_u64_rg(pf, rg_idx, ts_col_idx)?;
        let raw_aligned = raw_ts.len() == timestamps.len();
        // Per-snapshot collection duration for the fleet fallback window.
        let durations = dur_col_idx
            .map(|c| read_counter_values_per_rg(pf, rg_idx, c))
            .transpose()?;
        for (i, col) in cols.iter().enumerate() {
            let values = read_counter_values_per_rg(pf, rg_idx, col.col_idx)?;
            debug_assert_eq!(
                values.len(),
                timestamps.len(),
                "row count mismatch in row group"
            );
            let begins = col
                .begin_col
                .map(|c| read_gauge_values_per_rg(pf, rg_idx, c))
                .transpose()?;
            let widths = col
                .width_col
                .map(|c| read_counter_values_per_rg(pf, rg_idx, c))
                .transpose()?;
            for (row, (ts_opt, val_opt)) in timestamps.iter().zip(values.iter()).enumerate() {
                if let (Some(ts), Some(v)) = (ts_opt, val_opt) {
                    let base = if raw_aligned { raw_ts[row] } else { *ts };
                    if base >= start_ns && base <= end_ns {
                        // Raw mode emits at the actual (un-snapped) acquisition
                        // time; the default grid path emits the snapped nominal
                        // timestamp for cross-series alignment.
                        ts_acc[i].push(if raw { base } else { *ts });
                        val_acc[i].push(*v);
                        if let Some(w) = win_acc[i].as_mut() {
                            let bo = begins.as_ref().and_then(|b| b.get(row).copied()).flatten();
                            let wd = widths.as_ref().and_then(|x| x.get(row).copied()).flatten();
                            let dur = durations
                                .as_ref()
                                .and_then(|d| d.get(row).copied())
                                .flatten();
                            w.push(resolve_window(base, bo, wd, dur));
                        }
                    }
                }
            }
        }
    }

    Ok(Counters {
        series: cols
            .into_iter()
            .zip(ts_acc)
            .zip(val_acc)
            .zip(win_acc)
            .filter(|(((_, ts), _), _)| !ts.is_empty())
            .map(|(((col, timestamps), values), windows)| Counter {
                labels: col.labels,
                timestamps,
                values,
                windows,
            })
            .collect(),
    })
}

// ─── Gauge reader ─────────────────────────────────────────────────────────────

fn read_gauges(
    pf: &ParquetSource,
    name: &str,
    filter: &Labels,
    start_ns: u64,
    end_ns: u64,
    raw: bool,
) -> Result<Gauges, Box<dyn Error>> {
    let ts_col_idx = pf
        .meta
        .schema()
        .index_of("timestamp")
        .map_err(|_| "missing timestamp")?;
    // Snapshot collection duration for the fleet fallback window (see read_counters).
    let dur_col_idx = pf.meta.schema().index_of("duration").ok();
    let interval_ns = pf.sampling_interval_ms * 1_000_000;
    let num_rgs = pf.meta.metadata().num_row_groups();

    let cols: Vec<ColDesc> = parse_schema(pf, ts_col_idx)
        .into_iter()
        .filter(|c| {
            matches!(c.kind, ColKind::Gauge)
                && c.name == name
                && (filter.inner.is_empty() || c.labels.matches(filter))
        })
        .collect();
    if cols.is_empty() {
        return Ok(Gauges { series: vec![] });
    }

    let mut ts_acc: Vec<Vec<u64>> = vec![Vec::new(); cols.len()];
    let mut val_acc: Vec<Vec<i64>> = vec![Vec::new(); cols.len()];
    let mut win_acc: Vec<Option<Vec<(u64, u64)>>> = cols
        .iter()
        .map(|c| {
            if (c.begin_col.is_some() && c.width_col.is_some()) || dur_col_idx.is_some() {
                Some(Vec::new())
            } else {
                None
            }
        })
        .collect();

    for rg_idx in 0..num_rgs {
        match rg_classify(
            pf.meta.metadata().row_group(rg_idx),
            ts_col_idx,
            start_ns,
            end_ns,
        ) {
            RgClass::Before | RgClass::After => continue,
            _ => {}
        }
        let timestamps = read_timestamps(pf, rg_idx, ts_col_idx, interval_ns)?;
        // See read_counters: windows and the stats time range live in RAW
        // timestamp space, so filter and anchor windows on the raw column while
        // emitting the snapped grid point for alignment.
        let raw_ts = read_raw_u64_rg(pf, rg_idx, ts_col_idx)?;
        let raw_aligned = raw_ts.len() == timestamps.len();
        let durations = dur_col_idx
            .map(|c| read_counter_values_per_rg(pf, rg_idx, c))
            .transpose()?;
        for (i, col) in cols.iter().enumerate() {
            let values = read_gauge_values_per_rg(pf, rg_idx, col.col_idx)?;
            debug_assert_eq!(
                values.len(),
                timestamps.len(),
                "row count mismatch in row group"
            );
            let begins = col
                .begin_col
                .map(|c| read_gauge_values_per_rg(pf, rg_idx, c))
                .transpose()?;
            let widths = col
                .width_col
                .map(|c| read_counter_values_per_rg(pf, rg_idx, c))
                .transpose()?;
            for (row, (ts_opt, val_opt)) in timestamps.iter().zip(values.iter()).enumerate() {
                if let (Some(ts), Some(v)) = (ts_opt, val_opt) {
                    let base = if raw_aligned { raw_ts[row] } else { *ts };
                    if base >= start_ns && base <= end_ns {
                        // Raw mode emits at the actual (un-snapped) acquisition
                        // time; the default grid path emits the snapped nominal
                        // timestamp for cross-series alignment.
                        ts_acc[i].push(if raw { base } else { *ts });
                        val_acc[i].push(*v);
                        if let Some(w) = win_acc[i].as_mut() {
                            let bo = begins.as_ref().and_then(|b| b.get(row).copied()).flatten();
                            let wd = widths.as_ref().and_then(|x| x.get(row).copied()).flatten();
                            let dur = durations
                                .as_ref()
                                .and_then(|d| d.get(row).copied())
                                .flatten();
                            w.push(resolve_window(base, bo, wd, dur));
                        }
                    }
                }
            }
        }
    }

    Ok(Gauges {
        series: cols
            .into_iter()
            .zip(ts_acc)
            .zip(val_acc)
            .zip(win_acc)
            .filter(|(((_, ts), _), _)| !ts.is_empty())
            .map(|(((col, timestamps), values), windows)| Gauge {
                labels: col.labels,
                timestamps,
                values,
                windows,
            })
            .collect(),
    })
}

// ─── Histogram cursor ─────────────────────────────────────────────────────────

/// Streams histogram rows from a parquet file one row group at a time.
/// Assumes row groups appear in chronological timestamp order, which
/// metriken-exposition guarantees. Rows within each row group are sorted
/// by (timestamp, series_idx) before being buffered.
struct ParquetHistogramCursor {
    pf: Arc<ParquetSource>,
    ts_col_idx: usize,
    interval_ns: u64,
    start_ns: u64,
    end_ns: u64,
    /// Pre-filtered histogram columns for this metric, in series-index order.
    col_descs: Vec<ColDesc>,
    /// Overlapping row group indices remaining to process.
    rg_queue: std::collections::VecDeque<usize>,
    /// Buffered rows from the current row group (sorted, ready to yield).
    pending: std::collections::VecDeque<HistogramRow>,
}

impl ParquetHistogramCursor {
    /// Load the next overlapping row group into `self.pending`.
    /// Returns false if no more row groups remain.
    fn fill_next_rg(&mut self) -> bool {
        while let Some(rg_idx) = self.rg_queue.pop_front() {
            let Ok(timestamps) =
                read_timestamps(&self.pf, rg_idx, self.ts_col_idx, self.interval_ns)
            else {
                continue;
            };

            let mut rg_rows: Vec<HistogramRow> = Vec::new();

            for (si, col) in self.col_descs.iter().enumerate() {
                let ColKind::Histogram { .. } = col.kind else {
                    continue;
                };

                let key = CacheKey {
                    source_id: self.pf.id,
                    column_idx: col.col_idx,
                    row_group_idx: rg_idx,
                };

                let snapshots: Arc<Vec<HistogramSnapshot>> = if let Some(pool) = &self.pf.pool {
                    if let Some(cached) = pool.get_histogram_snapshots(key) {
                        cached
                    } else {
                        let decoded = Arc::new(self.decode_histogram_column(rg_idx, col.col_idx));
                        pool.put_histogram_snapshots(key, Arc::clone(&decoded));
                        decoded
                    }
                } else {
                    Arc::new(self.decode_histogram_column(rg_idx, col.col_idx))
                };

                for (ts_opt, snap) in timestamps.iter().zip(snapshots.iter()) {
                    let Some(ts) = ts_opt else {
                        continue;
                    };
                    if *ts < self.start_ns || *ts > self.end_ns {
                        continue;
                    }
                    rg_rows.push(HistogramRow {
                        series_idx: si,
                        timestamp: *ts,
                        snapshot: snap.clone(),
                    });
                }
            }

            if !rg_rows.is_empty() {
                rg_rows.sort_unstable_by_key(|r| (r.timestamp, r.series_idx));
                self.pending.extend(rg_rows);
                return true;
            }
        }
        false
    }

    /// Decode all rows for one histogram column in one row group.
    /// Returns an empty Vec on panic or read error (the streaming operator
    /// will then see no data for that column in that row group).
    fn decode_histogram_column(&self, rg_idx: usize, col_idx: usize) -> Vec<HistogramSnapshot> {
        let parquet_schema = self.pf.meta.metadata().file_metadata().schema_descr_ptr();
        let Ok(reader) = self
            .pf
            .build_batch_reader(rg_idx, ProjectionMask::roots(&parquet_schema, [col_idx]))
        else {
            return Vec::new();
        };

        let rg_idx_captured = rg_idx;
        let col_idx_captured = col_idx;
        let source_id_captured = self.pf.id;
        let decode_result = catch_decode_panic(|| {
            let mut out = Vec::new();
            for batch_result in reader {
                let batch = match batch_result {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            rg_idx = rg_idx_captured,
                            col_idx = col_idx_captured,
                            source_id = source_id_captured,
                            error = %e,
                            "aborting histogram read on parquet error",
                        );
                        break;
                    }
                };
                let list = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<ListArray>()
                    .expect("histogram column is not List");
                for value in list.iter() {
                    let snap = value
                        .and_then(|lv| {
                            lv.as_any()
                                .downcast_ref::<UInt64Array>()
                                .map(raw_to_sparse_cumulative)
                        })
                        .unwrap_or_else(|| HistogramSnapshot {
                            index: vec![],
                            count: vec![],
                        });
                    out.push(snap);
                }
            }
            out
        });

        match decode_result {
            Ok(out) => out,
            Err(panic_msg) => {
                tracing::error!(
                    rg_idx,
                    col_idx,
                    source_id = self.pf.id,
                    panic = %panic_msg,
                    "parquet decode panic reading histogram; returning empty data for this row group",
                );
                Vec::new()
            }
        }
    }
}

impl Iterator for ParquetHistogramCursor {
    type Item = HistogramRow;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.pending.pop_front() {
                return Some(row);
            }
            if !self.fill_next_rg() {
                return None;
            }
        }
    }
}

// ─── Histogram snapshot helper ────────────────────────────────────────────────

/// Convert a raw bucket array (individual counts per bucket) into a
/// sparse cumulative prefix-sum snapshot. Only non-zero buckets are stored.
fn raw_to_sparse_cumulative(arr: &UInt64Array) -> HistogramSnapshot {
    let mut index = Vec::new();
    let mut count = Vec::new();
    let mut running = 0u64;
    for (i, v) in arr.iter().enumerate() {
        let v = v.unwrap_or(0);
        if v > 0 {
            running = running.saturating_add(v);
            index.push(i as u32);
            count.push(running);
        }
    }
    HistogramSnapshot { index, count }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{ArrayRef, Int64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;

    #[test]
    fn resolve_window_precedence() {
        // 1. Sidecar present → tight per-observation window (base + begin, + width).
        assert_eq!(
            resolve_window(1_000, Some(-50), Some(30), Some(999)),
            (950, 980),
            "sidecar wins over duration"
        );
        // Negative begin clamped to 0 (no wrap/overflow).
        assert_eq!(resolve_window(10, Some(-100), Some(5), None), (0, 5));
        // 2. No sidecar but duration present → fleet window [base, base + duration].
        assert_eq!(resolve_window(1_000, None, None, Some(30)), (1_000, 1_030));
        // Partial sidecar (begin without width) also falls back to fleet.
        assert_eq!(
            resolve_window(1_000, Some(-50), None, Some(30)),
            (1_000, 1_030)
        );
        // 3. Neither → degenerate point window.
        assert_eq!(resolve_window(1_000, None, None, None), (1_000, 1_000));
    }

    /// Minimal parquet writer for timestamp-jitter tests: a `timestamp`
    /// UInt64 column set to exactly `raw` (no grid alignment) plus one dummy
    /// gauge column, mirroring the schema shape `FixtureBuilder` produces but
    /// without its "timestamp = tick * interval" grid assumption.
    fn build_parquet_with_timestamps(raw: &[u64], sampling_interval_ms: u64) -> Vec<u8> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new("dummy_gauge", DataType::Int64, true).with_metadata(HashMap::from([
                ("metric".to_string(), "dummy_gauge".to_string()),
                ("metric_type".to_string(), "gauge".to_string()),
            ])),
        ]));

        let kv = vec![KeyValue {
            key: "sampling_interval_ms".to_string(),
            value: Some(sampling_interval_ms.to_string()),
        }];
        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_key_value_metadata(Some(kv))
            .build();

        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();

        let ts_array = Arc::new(UInt64Array::from(raw.to_vec())) as ArrayRef;
        let gauge_array = Arc::new(Int64Array::from(vec![0i64; raw.len()])) as ArrayRef;
        let batch = RecordBatch::try_new(schema, vec![ts_array, gauge_array]).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        buf
    }

    #[test]
    fn sample_timestamps_returns_raw_unsnapped_values() {
        // 1s nominal interval, but samples are jittered off the grid.
        let raw: Vec<u64> = vec![
            1_000_000_000, // t0
            2_003_000_000, // +1.003s (late)
            2_998_000_000, // +0.995s (early)
            4_001_000_000, // +1.003s
        ];
        let bytes = build_parquet_with_timestamps(&raw, 1000);
        let reader = ParquetReader::open_bytes(bytes).unwrap();
        assert_eq!(reader.sample_timestamps(), raw);
    }

    #[test]
    fn memory_store_sample_timestamps_is_empty_by_default() {
        let store = crate::MemoryStore::builder().build();
        assert!(store.sample_timestamps().is_empty());
    }

    // A `.rez` per-sampler table carries one bare, table-level `:wall_offset`
    // sidecar column (Int64) alongside the monotonic row `timestamp` — the raw
    // wall-clock reading at that tick, for clock-drift bookkeeping. Unlike the
    // per-metric `<m>:window_begin` / `<m>:window_width` suffixes, it has no
    // metric prefix: it's one column per table, not one per metric. It must
    // never surface as a metric and must not disturb its sibling column.
    #[test]
    fn wall_offset_sidecar_column_is_not_a_metric() {
        let ts = Field::new("timestamp", DataType::UInt64, false);
        let counter =
            Field::new("cpu_cycles", DataType::UInt64, true).with_metadata(HashMap::from([
                ("metric".to_string(), "cpu_cycles".to_string()),
                ("metric_type".to_string(), "counter".to_string()),
            ]));
        // Bare sidecar column, no metric metadata — exactly as the rezolus
        // writer emits it.
        let wall_offset = Field::new(":wall_offset", DataType::Int64, true);
        let schema = Arc::new(Schema::new_with_metadata(
            vec![ts, counter, wall_offset],
            HashMap::from([
                ("source".to_string(), "rezolus".to_string()),
                ("sampling_interval_ms".to_string(), "1000".to_string()),
            ]),
        ));

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(UInt64Array::from(vec![1_000_000_000u64, 2_000_000_000u64])) as ArrayRef,
                Arc::new(UInt64Array::from(vec![Some(10u64), Some(20u64)])) as ArrayRef,
                Arc::new(Int64Array::from(vec![
                    Some(5_000_000i64),
                    Some(-3_000_000i64),
                ])) as ArrayRef,
            ],
        )
        .unwrap();

        let mut bytes: Vec<u8> = Vec::new();
        {
            let mut w = ArrowWriter::try_new(&mut bytes, schema, None).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
        }

        let reader = ParquetReader::open_bytes(bytes).unwrap();

        assert_eq!(
            reader.counter_names(),
            vec!["cpu_cycles".to_string()],
            ":wall_offset must not appear as a phantom counter"
        );
        assert!(
            reader.gauge_names().is_empty(),
            ":wall_offset must not appear as a phantom gauge: {:?}",
            reader.gauge_names()
        );
        assert!(
            reader.histogram_names().is_empty(),
            ":wall_offset must not appear as a phantom histogram: {:?}",
            reader.histogram_names()
        );

        // The sibling metric column must still resolve normally — a skip that
        // also broke it would be worse than the bug it fixes. Counters are
        // only queryable via rate()/irate(), not a bare selector.
        let (start, end) = reader.time_range().unwrap();
        let result = reader
            .query_range("rate(cpu_cycles[2s])", start, end + 1.0, 1.0)
            .unwrap();
        let QueryResult::Matrix { result } = result else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1, "expected exactly one series");
        assert!(
            result[0].values.iter().any(|(_, v)| *v > 0.0),
            "expected a positive rate from the still-resolving cpu_cycles counter: {:?}",
            result[0].values
        );
    }

    // ─── Table-level acquisition-window columns ───────────────────────────

    fn counter_field(name: &str) -> Field {
        Field::new(name, DataType::UInt64, true).with_metadata(HashMap::from([
            ("metric".to_string(), name.to_string()),
            ("metric_type".to_string(), "counter".to_string()),
        ]))
    }

    /// Build a single-row-group parquet from explicit `(Field, ArrayRef)`
    /// pairs, in column order, with a fixed 1s sampling interval.
    fn build_table(field_specs: Vec<(Field, ArrayRef)>) -> Vec<u8> {
        let fields: Vec<Field> = field_specs.iter().map(|(f, _)| f.clone()).collect();
        let arrays: Vec<ArrayRef> = field_specs.into_iter().map(|(_, a)| a).collect();
        let schema = Arc::new(Schema::new(fields));
        let kv = vec![KeyValue {
            key: "sampling_interval_ms".to_string(),
            value: Some("1000".to_string()),
        }];
        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_key_value_metadata(Some(kv))
            .build();
        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();
        let batch = RecordBatch::try_new(schema, arrays).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        buf
    }

    const WIN_TS: [u64; 4] = [1_000_000_000, 2_000_000_000, 3_000_000_000, 4_000_000_000];
    const WIN_BEGINS: [i64; 4] = [-5_000_000, -4_000_000, -6_000_000, -5_000_000];
    const WIN_WIDTHS: [u64; 4] = [10_000_000, 8_000_000, 12_000_000, 9_000_000];

    fn ts_field_array() -> (Field, ArrayRef) {
        (
            Field::new("timestamp", DataType::UInt64, false),
            Arc::new(UInt64Array::from(WIN_TS.to_vec())) as ArrayRef,
        )
    }

    fn window_cols(begin_name: &str, width_name: &str) -> Vec<(Field, ArrayRef)> {
        vec![
            (
                Field::new(begin_name, DataType::Int64, true),
                Arc::new(Int64Array::from(WIN_BEGINS.to_vec())) as ArrayRef,
            ),
            (
                Field::new(width_name, DataType::UInt64, true),
                Arc::new(UInt64Array::from(WIN_WIDTHS.to_vec())) as ArrayRef,
            ),
        ]
    }

    fn query_rate_with_bounds(bytes: Vec<u8>, expr: &str) -> String {
        let reader = ParquetReader::open_bytes(bytes).unwrap();
        let (start, end) = reader.time_range().unwrap();
        let result = reader.query_range(expr, start, end + 1.0, 1.0).unwrap();
        format!("{result:?}")
    }

    /// A table with ONLY the bare table-level `:window_begin`/`:window_width`
    /// pair (no per-metric sidecars) must give every metric in the table a
    /// band, and that band must be byte-identical to an equivalent table
    /// where each metric instead carries its own `<m>:window_begin`/
    /// `<m>:window_width` sidecar with the same values.
    #[test]
    fn table_level_only_windows_match_per_metric_sidecar_equivalent() {
        let a_vals = [10u64, 20, 35, 50];
        let b_vals = [100u64, 200, 350, 500];

        let mut per_metric = vec![ts_field_array()];
        per_metric.push((
            counter_field("a"),
            Arc::new(UInt64Array::from(a_vals.to_vec())) as ArrayRef,
        ));
        per_metric.extend(window_cols("a:window_begin", "a:window_width"));
        per_metric.push((
            counter_field("b"),
            Arc::new(UInt64Array::from(b_vals.to_vec())) as ArrayRef,
        ));
        per_metric.extend(window_cols("b:window_begin", "b:window_width"));
        let per_metric_bytes = build_table(per_metric);

        let mut table_level = vec![ts_field_array()];
        table_level.push((
            counter_field("a"),
            Arc::new(UInt64Array::from(a_vals.to_vec())) as ArrayRef,
        ));
        table_level.push((
            counter_field("b"),
            Arc::new(UInt64Array::from(b_vals.to_vec())) as ArrayRef,
        ));
        table_level.extend(window_cols(":window_begin", ":window_width"));
        let table_level_bytes = build_table(table_level);

        for expr in ["rate(a[2s])", "rate(b[2s])"] {
            let ref_out = query_rate_with_bounds(per_metric_bytes.clone(), expr);
            let table_out = query_rate_with_bounds(table_level_bytes.clone(), expr);
            assert_eq!(
                ref_out, table_out,
                "table-level window must reproduce per-metric-sidecar output for {expr}"
            );
            assert!(
                table_out.contains("intervals: Some"),
                "expected an uncertainty band from the table-level window: {table_out}"
            );
        }
    }

    /// Mixed table: a metric with its own sidecar keeps using it even though
    /// a table-level pair is also present; a metric with no sidecar of its
    /// own falls back to the table-level pair. Verified by comparing each
    /// metric's query output against a reference fixture that isolates the
    /// window source it's supposed to be using.
    #[test]
    fn mixed_table_precedence_own_sidecar_wins_over_table_level() {
        let a_vals = [10u64, 20, 35, 50];
        let b_vals = [100u64, 200, 350, 500];
        // Distinct values so a mix-up is visible in the query output.
        let a_begins = [-1_000_000i64, -1_000_000, -1_000_000, -1_000_000];
        let a_widths = [2_000_000u64, 2_000_000, 2_000_000, 2_000_000];

        let mut mixed = vec![ts_field_array()];
        mixed.push((
            counter_field("a"),
            Arc::new(UInt64Array::from(a_vals.to_vec())) as ArrayRef,
        ));
        mixed.push((
            Field::new("a:window_begin", DataType::Int64, true),
            Arc::new(Int64Array::from(a_begins.to_vec())) as ArrayRef,
        ));
        mixed.push((
            Field::new("a:window_width", DataType::UInt64, true),
            Arc::new(UInt64Array::from(a_widths.to_vec())) as ArrayRef,
        ));
        mixed.push((
            counter_field("b"),
            Arc::new(UInt64Array::from(b_vals.to_vec())) as ArrayRef,
        ));
        mixed.extend(window_cols(":window_begin", ":window_width")); // table-level (WIN_BEGINS/WIN_WIDTHS)
        let mixed_bytes = build_table(mixed);

        // Reference: "a" alone, using only its own sidecar (no table-level pair).
        let mut ref_a = vec![ts_field_array()];
        ref_a.push((
            counter_field("a"),
            Arc::new(UInt64Array::from(a_vals.to_vec())) as ArrayRef,
        ));
        ref_a.push((
            Field::new("a:window_begin", DataType::Int64, true),
            Arc::new(Int64Array::from(a_begins.to_vec())) as ArrayRef,
        ));
        ref_a.push((
            Field::new("a:window_width", DataType::UInt64, true),
            Arc::new(UInt64Array::from(a_widths.to_vec())) as ArrayRef,
        ));
        let ref_a_bytes = build_table(ref_a);

        // Reference: "b" alone, using only the table-level pair.
        let mut ref_b = vec![ts_field_array()];
        ref_b.push((
            counter_field("b"),
            Arc::new(UInt64Array::from(b_vals.to_vec())) as ArrayRef,
        ));
        ref_b.extend(window_cols(":window_begin", ":window_width"));
        let ref_b_bytes = build_table(ref_b);

        assert_eq!(
            query_rate_with_bounds(mixed_bytes.clone(), "rate(a[2s])"),
            query_rate_with_bounds(ref_a_bytes, "rate(a[2s])"),
            "metric with its own sidecar must ignore the table-level pair"
        );
        assert_eq!(
            query_rate_with_bounds(mixed_bytes, "rate(b[2s])"),
            query_rate_with_bounds(ref_b_bytes, "rate(b[2s])"),
            "metric with no sidecar of its own must fall back to the table-level pair"
        );
    }

    /// No sidecars at all (neither per-metric nor table-level, and no
    /// `duration` column) — unchanged behavior: no uncertainty band.
    #[test]
    fn table_with_neither_window_kind_has_no_bands() {
        let a_vals = [10u64, 20, 35, 50];
        let table = vec![
            ts_field_array(),
            (
                counter_field("a"),
                Arc::new(UInt64Array::from(a_vals.to_vec())) as ArrayRef,
            ),
        ];
        let out = query_rate_with_bounds(build_table(table), "rate(a[2s])");
        assert!(
            !out.contains("intervals: Some"),
            "expected no uncertainty band without any window columns: {out}"
        );
    }

    /// A metric literally named `:window_begin` (or `:window_width`) must
    /// never surface as a queryable series — the bare name is reserved for
    /// the table-level window pair regardless of what metadata the column
    /// carries.
    #[test]
    fn bare_window_names_are_reserved_even_with_metric_metadata() {
        let table = vec![
            ts_field_array(),
            (
                counter_field("a"),
                Arc::new(UInt64Array::from(vec![1u64, 2, 3, 4])) as ArrayRef,
            ),
            (
                // Masquerading as a real counter metric named ":window_begin".
                Field::new(":window_begin", DataType::UInt64, true).with_metadata(HashMap::from([
                    ("metric".to_string(), ":window_begin".to_string()),
                    ("metric_type".to_string(), "counter".to_string()),
                ])),
                Arc::new(UInt64Array::from(vec![1u64, 1, 1, 1])) as ArrayRef,
            ),
        ];
        let reader = ParquetReader::open_bytes(build_table(table)).unwrap();
        assert_eq!(
            reader.counter_names(),
            vec!["a".to_string()],
            "a column physically named ':window_begin' must never appear as a metric, \
             even with metric metadata claiming otherwise: {:?}",
            reader.counter_names()
        );
    }

    /// A metric with only HALF of its own sidecar pair (`<m>:window_begin`
    /// but no `<m>:window_width`) plus a table-level pair present must fall
    /// back cleanly to the table-level pair — never mix its own begin with
    /// the table's width. Pinned by comparing against a reference fixture
    /// where "a" has no own sidecar columns at all (pure table-level), using
    /// own-begin values distinct from the table's so a mix-up would produce
    /// different query output.
    #[test]
    fn partial_own_sidecar_falls_back_to_table_level_pair_not_mixed() {
        let a_vals = [10u64, 20, 35, 50];
        // Deliberately distinct from WIN_BEGINS so a mixed (own-begin +
        // table-width) resolution would produce a different band than the
        // correct table-level-only resolution.
        let own_begin_only = [-9_000_000i64, -9_000_000, -9_000_000, -9_000_000];

        let mut table = vec![ts_field_array()];
        table.push((
            counter_field("a"),
            Arc::new(UInt64Array::from(a_vals.to_vec())) as ArrayRef,
        ));
        table.push((
            Field::new("a:window_begin", DataType::Int64, true),
            Arc::new(Int64Array::from(own_begin_only.to_vec())) as ArrayRef,
        ));
        table.extend(window_cols(":window_begin", ":window_width"));
        let table_bytes = build_table(table);

        let mut reference = vec![ts_field_array()];
        reference.push((
            counter_field("a"),
            Arc::new(UInt64Array::from(a_vals.to_vec())) as ArrayRef,
        ));
        reference.extend(window_cols(":window_begin", ":window_width"));
        let reference_bytes = build_table(reference);

        assert_eq!(
            query_rate_with_bounds(table_bytes, "rate(a[2s])"),
            query_rate_with_bounds(reference_bytes, "rate(a[2s])"),
            "a metric with only half its own sidecar must fall back to the \
             table-level pair, not mix its own begin with the table's width"
        );
    }

    /// A table with only HALF of the bare table-level pair (`:window_begin`
    /// but no `:window_width`) must produce no bands for any metric — and
    /// must not panic while resolving or reading windows.
    #[test]
    fn bare_partial_table_pair_produces_no_bands_and_does_not_panic() {
        let a_vals = [10u64, 20, 35, 50];
        let table = vec![
            ts_field_array(),
            (
                counter_field("a"),
                Arc::new(UInt64Array::from(a_vals.to_vec())) as ArrayRef,
            ),
            (
                Field::new(":window_begin", DataType::Int64, true),
                Arc::new(Int64Array::from(WIN_BEGINS.to_vec())) as ArrayRef,
            ),
            // No matching ":window_width" column.
        ];
        let out = query_rate_with_bounds(build_table(table), "rate(a[2s])");
        assert!(
            !out.contains("intervals: Some"),
            "a partial (begin-only) table-level pair must not produce a band: {out}"
        );
    }
}
