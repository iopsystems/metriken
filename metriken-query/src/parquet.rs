use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Int64Array, ListArray, UInt64Array};
use arrow::datatypes::DataType;
use bytes::Bytes;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder};
use parquet::arrow::ProjectionMask;
use parquet::file::metadata::RowGroupMetaData;
use parquet::file::statistics::Statistics;

use crate::histogram_stream::{HistogramRow, HistogramStream, HistogramStreamMeta};
use crate::labels::Labels;
use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::types::{Counter, Counters, Gauge, Gauges, HistogramSnapshot};
use crate::{DataSource, MetricsSource};

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
    pub fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        let filename = path.file_name().and_then(|n| n.to_str()).map(String::from);
        let source = ParquetSource::open(path)?;
        let inner = Arc::new(MultiParquetSource { files: vec![(source, Labels::default())] });
        let ds: Arc<dyn DataSource> = inner.clone();
        Ok(Self { engine: QueryEngine::new(ds), inner, filename })
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
        let inner = Arc::new(MultiParquetSource { files: vec![(source, Labels::default())] });
        let ds: Arc<dyn DataSource> = inner.clone();
        Ok(Self { engine: QueryEngine::new(ds), inner, filename: None })
    }

    /// Return the underlying `(source, labels)` pairs for use by
    /// [`ParquetBuilder::reader`] and [`ParquetBuilder::reader_labeled`].
    pub(crate) fn sources_for_composition(&self) -> Vec<(Arc<ParquetSource>, Labels)> {
        self.inner.files.clone()
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

    pub fn query_range(&self, expr: &str, start_s: f64, end_s: f64, step_s: f64) -> Result<QueryResult, QueryError> {
        self.engine.query_range(expr, start_s, end_s, step_s)
    }

    /// Time range of data across all files in seconds, or `None` if empty.
    pub fn time_range(&self) -> Option<(f64, f64)> {
        self.engine.time_range().map(|(lo, hi)| (lo as f64 / 1e9, hi as f64 / 1e9))
    }

    /// Time range of data across all files in nanoseconds, or `None` if empty.
    ///
    /// Prefer this over [`time_range()`](Self::time_range) when you need exact
    /// nanosecond timestamps without floating-point precision loss.
    pub fn time_range_ns(&self) -> Option<(u64, u64)> {
        self.engine.time_range()
    }

    /// Names of all counter metrics across all files (sorted, deduplicated).
    pub fn counter_names(&self) -> Vec<String> { self.engine.counter_names() }

    /// Names of all gauge metrics across all files (sorted, deduplicated).
    pub fn gauge_names(&self) -> Vec<String> { self.engine.gauge_names() }

    /// Names of all histogram metrics across all files (sorted, deduplicated).
    pub fn histogram_names(&self) -> Vec<String> { self.engine.histogram_names() }

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
    pub fn interval(&self) -> f64 { self.engine.interval() }

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
}

impl MetricsSource for ParquetReader {
    fn query_range(&self, expr: &str, start_s: f64, end_s: f64, step_s: f64) -> Result<QueryResult, QueryError> {
        self.query_range(expr, start_s, end_s, step_s)
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
}

impl Default for ParquetBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ParquetBuilder {
    pub fn new() -> Self {
        Self { entries: Vec::new(), filename: None }
    }

    /// Add a file with no extra labels.
    pub fn file(mut self, path: impl AsRef<Path>) -> Self {
        self.entries.push(BuilderEntry::Path(path.as_ref().to_path_buf(), Labels::default()));
        self
    }

    /// Add a file whose series will carry `labels` as additional metadata.
    /// The labels are injected into every series from this file at query time.
    ///
    /// # Precondition
    /// Injected label keys must not conflict with column labels already present
    /// in the parquet file's schema; if they do, the native value is overwritten.
    pub fn file_labeled(mut self, path: impl AsRef<Path>, labels: impl Into<Labels>) -> Self {
        self.entries.push(BuilderEntry::Path(path.as_ref().to_path_buf(), labels.into()));
        self
    }

    /// Add an in-memory parquet source with no extra labels.
    /// Accepts any type that converts to `bytes::Bytes` (e.g. `Vec<u8>`, `&[u8]`, `Bytes`).
    /// `Bytes::clone()` is a refcount bump — cloning this source is cheap.
    pub fn bytes(mut self, bytes: impl Into<Bytes>) -> Self {
        self.entries.push(BuilderEntry::Bytes(bytes.into(), Labels::default()));
        self
    }

    /// Add an in-memory parquet source whose series will carry `labels` as additional metadata.
    pub fn bytes_labeled(mut self, bytes: impl Into<Bytes>, labels: impl Into<Labels>) -> Self {
        self.entries.push(BuilderEntry::Bytes(bytes.into(), labels.into()));
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
        self.entries.push(BuilderEntry::OwnedFile(file, Labels::default()));
        self
    }

    /// Add an already-open file handle whose series will carry `labels` as
    /// additional metadata.
    pub fn file_owned_labeled(mut self, file: File, labels: impl Into<Labels>) -> Self {
        self.entries.push(BuilderEntry::OwnedFile(file, labels.into()));
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
        let files: Result<Vec<(Arc<ParquetSource>, Labels)>, Box<dyn Error>> = self
            .entries
            .into_iter()
            .map(|entry| match entry {
                BuilderEntry::Path(path, labels) => Ok((ParquetSource::open(&path)?, labels)),
                BuilderEntry::Bytes(bytes, labels) => Ok((ParquetSource::open_bytes(bytes)?, labels)),
                BuilderEntry::OwnedFile(file, labels) => Ok((ParquetSource::open_file(file)?, labels)),
                BuilderEntry::Source(source, labels) => Ok((source, labels)),
            })
            .collect();
        let inner = Arc::new(MultiParquetSource { files: files? });
        let ds: Arc<dyn DataSource> = inner.clone();
        Ok(ParquetReader { engine: QueryEngine::new(ds), inner, filename })
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
        inner: filter.inner.iter()
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
        inner: filter.inner.iter()
            .filter(|(k, _)| !extra.inner.contains_key(k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    })
}

impl DataSource for MultiParquetSource {
    // NOTE: Counter series are naively concatenated across files.
    // Same (metric, label) pairs in multiple files produce duplicate series.
    // Label injection via file_labeled() enables filtering on injected keys.
    fn counters(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Counters> {
        let series: Vec<Counter> = self.files.iter()
            .filter_map(|(pf, extra)| {
                let pq_filter = resolve_filter(extra, filter)?;
                let counters = read_counters(pf, name, &pq_filter, start_ns, end_ns).ok()?;
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
        if series.is_empty() { None } else { Some(Counters { series }) }
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        let series: Vec<Gauge> = self.files.iter()
            .filter_map(|(pf, extra)| {
                let pq_filter = resolve_filter(extra, filter)?;
                let gauges = read_gauges(pf, name, &pq_filter, start_ns, end_ns).ok()?;
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
        if series.is_empty() { None } else { Some(Gauges { series }) }
    }

    fn histogram_stream(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<HistogramStream> {
        let streams: Vec<HistogramStream> = self.files.iter()
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
        self.files.iter()
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

    fn column_map(&self) -> std::collections::HashMap<String, std::collections::HashMap<Labels, String>> {
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

// ─── Private file reader ──────────────────────────────────────────────────────

enum ParquetBacking {
    File(File),
    Bytes(Bytes),
}

pub(crate) struct ParquetSource {
    backing: ParquetBacking,
    meta: ArrowReaderMetadata,
    sampling_interval_ms: u64,
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
        Ok(Arc::new(Self { backing: ParquetBacking::File(file), meta, sampling_interval_ms }))
    }

    fn open_bytes(bytes: Bytes) -> Result<Arc<Self>, Box<dyn Error>> {
        let meta = ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::default())?;
        let sampling_interval_ms = parse_sampling_interval(&meta);
        Ok(Arc::new(Self { backing: ParquetBacking::Bytes(bytes), meta, sampling_interval_ms }))
    }

    fn open_file(file: File) -> Result<Arc<Self>, Box<dyn Error>> {
        let meta = ArrowReaderMetadata::load(&file, ArrowReaderOptions::default())?;
        let sampling_interval_ms = parse_sampling_interval(&meta);
        Ok(Arc::new(Self { backing: ParquetBacking::File(file), meta, sampling_interval_ms }))
    }

    fn build_batch_reader(
        &self,
        rg_idx: usize,
        projection: ProjectionMask,
    ) -> Result<parquet::arrow::arrow_reader::ParquetRecordBatchReader, Box<dyn Error>> {
        let reader = match &self.backing {
            ParquetBacking::File(f) => {
                ParquetRecordBatchReaderBuilder::new_with_metadata(f.try_clone()?, self.meta.clone())
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
        self.meta.metadata().file_metadata().key_value_metadata()?.iter()
            .find(|e| e.key == key)
            .map(|e| e.value.clone().unwrap_or_default())
    }

    fn time_range_from_stats(&self) -> Option<(u64, u64)> {
        let ts_col_idx = self.meta.schema().index_of("timestamp").ok()?;
        let pq_metadata = self.meta.metadata();
        let mut min_ns: Option<u64> = None;
        let mut max_ns: Option<u64> = None;
        for rg_idx in 0..pq_metadata.num_row_groups() {
            let Some(stats) = pq_metadata.row_group(rg_idx).column(ts_col_idx).statistics() else {
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
            ColKind::Histogram { grouping_power: gp, max_value_power: mvp } => {
                ::histogram::Config::new(gp, mvp).ok()
            }
            _ => None,
        })?;

        let series: Vec<Labels> = col_descs.iter().map(|c| c.labels.clone()).collect();

        let rg_queue: std::collections::VecDeque<usize> = (0..num_rgs)
            .filter(|&rg_idx| !matches!(
                rg_classify(
                    self.meta.metadata().row_group(rg_idx),
                    ts_col_idx,
                    start_ns,
                    end_ns,
                ),
                RgClass::Before | RgClass::After,
            ))
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
        let ts_col_idx = self.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
        let mut out: HashMap<String, HashMap<Labels, String>> = HashMap::new();
        for c in parse_schema(self, ts_col_idx) {
            out.entry(c.name).or_default().insert(c.labels, c.column_name);
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
    let Some(stats) = rg.column(ts_col_idx).statistics() else { return RgClass::Unknown; };
    let Statistics::Int64(s) = stats else { return RgClass::Unknown; };
    let (Some(rg_min), Some(rg_max)) = (s.min_opt(), s.max_opt()) else { return RgClass::Unknown; };
    let (rg_min, rg_max) = (*rg_min as u64, *rg_max as u64);
    if rg_max < start_ns { RgClass::Before }
    else if rg_min > end_ns { RgClass::After }
    else { RgClass::Overlaps }
}

// ─── Schema parsing ───────────────────────────────────────────────────────────

enum ColKind {
    Counter,
    Gauge,
    Histogram { grouping_power: u8, max_value_power: u8 },
}

struct ColDesc {
    col_idx: usize,
    name: String,
    labels: Labels,
    column_name: String,
    kind: ColKind,
}

fn parse_schema(pf: &ParquetSource, ts_col_idx: usize) -> Vec<ColDesc> {
    pf.meta.schema().fields().iter().enumerate().filter_map(|(col_idx, field)| {
        if col_idx == ts_col_idx { return None; }
        let mut meta = field.metadata().clone();
        let column_name = field.name().to_string();
        let name = meta.get("metric").cloned().unwrap_or_else(|| {
            column_name.strip_suffix(":buckets").unwrap_or(&column_name).to_string()
        });
        let grouping_power: Option<u8> = meta.remove("grouping_power").and_then(|v| v.parse().ok());
        let max_value_power: Option<u8> = meta.remove("max_value_power").and_then(|v| v.parse().ok());
        let mut labels = Labels::default();
        for (k, v) in meta.iter() {
            match k.as_str() {
                "metric" | "metric_type" | "unit" => continue,
                _ => { labels.inner.insert(k.clone(), v.clone()); }
            }
        }
        let kind = match field.data_type() {
            DataType::UInt64 => ColKind::Counter,
            DataType::Int64 => ColKind::Gauge,
            DataType::List(inner) if inner.data_type() == &DataType::UInt64 => {
                let (Some(gp), Some(mvp)) = (grouping_power, max_value_power) else { return None; };
                ColKind::Histogram { grouping_power: gp, max_value_power: mvp }
            }
            _ => return None,
        };
        Some(ColDesc { col_idx, name, labels, column_name, kind })
    }).collect()
}

// ─── Timestamp reader ─────────────────────────────────────────────────────────

fn snap_timestamp(ts: u64, interval_ns: u64) -> u64 {
    (ts + interval_ns / 2).checked_div(interval_ns).map_or(ts, |q| q * interval_ns)
}

fn read_timestamps(pf: &ParquetSource, rg_idx: usize, ts_col_idx: usize, interval_ns: u64) -> Result<Vec<Option<u64>>, Box<dyn Error>> {
    let parquet_schema = pf.meta.metadata().file_metadata().schema_descr_ptr();
    let reader = pf.build_batch_reader(rg_idx, ProjectionMask::roots(&parquet_schema, [ts_col_idx]))?;
    let mut out = Vec::new();
    for batch in reader.flatten() {
        let arr = batch.column(0).as_any().downcast_ref::<UInt64Array>().ok_or("timestamp column is not UInt64")?;
        out.reserve(arr.len());
        for v in arr.iter() { out.push(v.map(|raw| snap_timestamp(raw, interval_ns))); }
    }
    Ok(out)
}

// ─── Counter reader ───────────────────────────────────────────────────────────

fn read_counters(pf: &ParquetSource, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Result<Counters, Box<dyn Error>> {
    let ts_col_idx = pf.meta.schema().index_of("timestamp").map_err(|_| "missing timestamp")?;
    let interval_ns = pf.sampling_interval_ms * 1_000_000;
    let parquet_schema = pf.meta.metadata().file_metadata().schema_descr_ptr();
    let num_rgs = pf.meta.metadata().num_row_groups();

    let cols: Vec<ColDesc> = parse_schema(pf, ts_col_idx).into_iter()
        .filter(|c| matches!(c.kind, ColKind::Counter) && c.name == name && (filter.inner.is_empty() || c.labels.matches(filter)))
        .collect();
    if cols.is_empty() { return Ok(Counters { series: vec![] }); }

    let mut ts_acc: Vec<Vec<u64>> = vec![Vec::new(); cols.len()];
    let mut val_acc: Vec<Vec<u64>> = vec![Vec::new(); cols.len()];

    for rg_idx in 0..num_rgs {
        match rg_classify(pf.meta.metadata().row_group(rg_idx), ts_col_idx, start_ns, end_ns) {
            RgClass::Before | RgClass::After => continue,
            _ => {}
        }
        let timestamps = read_timestamps(pf, rg_idx, ts_col_idx, interval_ns)?;
        for (i, col) in cols.iter().enumerate() {
            let reader = pf.build_batch_reader(rg_idx, ProjectionMask::roots(&parquet_schema, [col.col_idx]))?;
            let mut row = 0usize;
            for batch in reader.flatten() {
                let arr = batch.column(0).as_any().downcast_ref::<UInt64Array>().expect("counter column is not UInt64");
                for v in arr.iter() {
                    if let (Some(v), Some(Some(ts))) = (v, timestamps.get(row)) {
                        if *ts >= start_ns && *ts <= end_ns { ts_acc[i].push(*ts); val_acc[i].push(v); }
                    }
                    row += 1;
                }
            }
        }
    }

    Ok(Counters { series: cols.into_iter().zip(ts_acc).zip(val_acc)
        .filter(|((_, ts), _)| !ts.is_empty())
        .map(|((col, timestamps), values)| Counter { labels: col.labels, timestamps, values })
        .collect() })
}

// ─── Gauge reader ─────────────────────────────────────────────────────────────

fn read_gauges(pf: &ParquetSource, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Result<Gauges, Box<dyn Error>> {
    let ts_col_idx = pf.meta.schema().index_of("timestamp").map_err(|_| "missing timestamp")?;
    let interval_ns = pf.sampling_interval_ms * 1_000_000;
    let parquet_schema = pf.meta.metadata().file_metadata().schema_descr_ptr();
    let num_rgs = pf.meta.metadata().num_row_groups();

    let cols: Vec<ColDesc> = parse_schema(pf, ts_col_idx).into_iter()
        .filter(|c| matches!(c.kind, ColKind::Gauge) && c.name == name && (filter.inner.is_empty() || c.labels.matches(filter)))
        .collect();
    if cols.is_empty() { return Ok(Gauges { series: vec![] }); }

    let mut ts_acc: Vec<Vec<u64>> = vec![Vec::new(); cols.len()];
    let mut val_acc: Vec<Vec<i64>> = vec![Vec::new(); cols.len()];

    for rg_idx in 0..num_rgs {
        match rg_classify(pf.meta.metadata().row_group(rg_idx), ts_col_idx, start_ns, end_ns) {
            RgClass::Before | RgClass::After => continue,
            _ => {}
        }
        let timestamps = read_timestamps(pf, rg_idx, ts_col_idx, interval_ns)?;
        for (i, col) in cols.iter().enumerate() {
            let reader = pf.build_batch_reader(rg_idx, ProjectionMask::roots(&parquet_schema, [col.col_idx]))?;
            let mut row = 0usize;
            for batch in reader.flatten() {
                let arr = batch.column(0).as_any().downcast_ref::<Int64Array>().expect("gauge column is not Int64");
                for v in arr.iter() {
                    if let (Some(v), Some(Some(ts))) = (v, timestamps.get(row)) {
                        if *ts >= start_ns && *ts <= end_ns { ts_acc[i].push(*ts); val_acc[i].push(v); }
                    }
                    row += 1;
                }
            }
        }
    }

    Ok(Gauges { series: cols.into_iter().zip(ts_acc).zip(val_acc)
        .filter(|((_, ts), _)| !ts.is_empty())
        .map(|((col, timestamps), values)| Gauge { labels: col.labels, timestamps, values })
        .collect() })
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
            let Ok(timestamps) = read_timestamps(
                &self.pf, rg_idx, self.ts_col_idx, self.interval_ns
            ) else { continue; };

            let parquet_schema = self.pf.meta.metadata().file_metadata().schema_descr_ptr();
            let mut rg_rows: Vec<HistogramRow> = Vec::new();

            for (si, col) in self.col_descs.iter().enumerate() {
                let ColKind::Histogram { .. } = col.kind else { continue; };
                // Iterator::next cannot propagate errors; skip on failure.
                let Ok(reader) = self.pf.build_batch_reader(
                    rg_idx,
                    ProjectionMask::roots(&parquet_schema, [col.col_idx]),
                ) else { continue; };

                let mut row = 0usize;
                for batch in reader.flatten() {
                    let list = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<ListArray>()
                        .expect("histogram column is not List");
                    for value in list.iter() {
                        let ts = timestamps.get(row).copied().flatten();
                        row += 1;
                        let Some(ts) = ts else { continue; };
                        if ts < self.start_ns || ts > self.end_ns { continue; }
                        let snap = value
                            .and_then(|lv| lv.as_any().downcast_ref::<UInt64Array>()
                                .map(raw_to_sparse_cumulative))
                            .unwrap_or_else(|| HistogramSnapshot { index: vec![], count: vec![] });
                        rg_rows.push(HistogramRow {
                            series_idx: si,
                            timestamp: ts,
                            snapshot: snap,
                        });
                    }
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
