// Several nested `Result<Result<…>, _>` types arise naturally from
// `catch_unwind` wrapping fallible decode functions; flattening them through
// type aliases is more confusing than the inline form.
#![allow(clippy::type_complexity)]

use std::error::Error;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;

use crate::buffer_pool::BufferPool;
use crate::labels::Labels;
use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::{DataSource, MetricsSource, QueryOptions};

pub use metriken_storage::parquet::*;

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
            files: vec![(
                Arc::new(FileSource(source)) as Arc<dyn DataSource>,
                Labels::default(),
            )],
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
            files: vec![(
                Arc::new(FileSource(source)) as Arc<dyn DataSource>,
                Labels::default(),
            )],
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
            files: vec![(
                Arc::new(FileSource(source)) as Arc<dyn DataSource>,
                Labels::default(),
            )],
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
            files: vec![(
                Arc::new(FileSource(source)) as Arc<dyn DataSource>,
                Labels::default(),
            )],
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
    pub(crate) fn sources_for_composition(&self) -> Vec<(Arc<dyn DataSource>, Labels)> {
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
            .query_range_opts(expr, start_s, end_s, step_s, opts)
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
            let counters = pf.counters(name, &filter, start, end)?;
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

    /// Per-sample collection timestamps (ns since epoch), ascending, in row
    /// order — the `timestamp` column, concatenated across all files.
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

    fn query_range_display_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &crate::DisplayOptions,
        qopts: &QueryOptions,
    ) -> Result<crate::DisplayResult, QueryError> {
        self.engine
            .query_range_display_opts(expr, start_s, end_s, step_s, opts, qopts)
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

    /// Asks each composed child rather than walking the merged labels, so a
    /// lazy child can answer without loading.
    fn total_series_count(&self) -> usize {
        self.inner.series_count()
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

impl From<&ParquetReader> for CompositionSource {
    fn from(reader: &ParquetReader) -> Self {
        CompositionSource::from_source(reader.data_source())
    }
}

impl From<&crate::SegmentedParquetReader> for CompositionSource {
    fn from(reader: &crate::SegmentedParquetReader) -> Self {
        CompositionSource::from_source(reader.data_source())
    }
}

/// A store assembled in memory composes like a reader — the same reason
/// [`crate::UnionChild`] takes one.
impl From<&crate::MemoryStore> for CompositionSource {
    fn from(store: &crate::MemoryStore) -> Self {
        CompositionSource::from_source(store.data_source())
    }
}

enum BuilderEntry {
    Path(std::path::PathBuf, Labels),
    Bytes(Bytes, Labels),
    OwnedFile(File, Labels),
    Source(Arc<dyn DataSource>, Labels),
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

    /// Compose with an already-open source -- a [`ParquetReader`] or a
    /// [`SegmentedParquetReader`](crate::SegmentedParquetReader) -- merging `extra` labels into every series
    /// it contributes.
    ///
    /// This is the heterogeneous counterpart to
    /// [`reader_labeled`](Self::reader_labeled), which can only take a
    /// `ParquetReader`. A `.rez` archive table is a segmented source whenever
    /// its writer sealed more than once, so composing N such tables under
    /// per-artifact labels needs this entry point.
    ///
    /// No I/O occurs -- the already-open handle is reused.
    pub fn source_labeled(
        mut self,
        source: impl Into<CompositionSource>,
        extra: impl Into<Labels>,
    ) -> Self {
        self.entries
            .push(BuilderEntry::Source(source.into().0, extra.into()));
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
        let files: Result<Vec<(Arc<dyn DataSource>, Labels)>, Box<dyn Error>> = self
            .entries
            .into_iter()
            .map(|entry| match entry {
                BuilderEntry::Path(path, labels) => {
                    let src = match &pool {
                        Some(p) => ParquetSource::open_with_pool(&path, Arc::clone(p))?,
                        None => ParquetSource::open(&path)?,
                    };
                    Ok((Arc::new(FileSource(src)) as Arc<dyn DataSource>, labels))
                }
                BuilderEntry::Bytes(bytes, labels) => {
                    let src = match &pool {
                        Some(p) => ParquetSource::open_bytes_with_pool(bytes, Arc::clone(p))?,
                        None => ParquetSource::open_bytes(bytes)?,
                    };
                    Ok((Arc::new(FileSource(src)) as Arc<dyn DataSource>, labels))
                }
                BuilderEntry::OwnedFile(file, labels) => {
                    let src = match &pool {
                        Some(p) => ParquetSource::open_file_with_pool(file, Arc::clone(p))?,
                        None => ParquetSource::open_file(file)?,
                    };
                    Ok((Arc::new(FileSource(src)) as Arc<dyn DataSource>, labels))
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

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::UInt64Array;
    use arrow::array::{ArrayRef, Int64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;
    use std::collections::HashMap;

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

    /// A segment from a newer format, or with a layout this reader does not
    /// know, is refused at open rather than read as a wide table.
    #[test]
    fn a_newer_segment_format_or_unknown_layout_is_refused() {
        let file = |kv: Vec<(&str, &str)>| {
            let schema = Arc::new(Schema::new(vec![Field::new(
                "timestamp",
                DataType::UInt64,
                false,
            )]));
            let batch = RecordBatch::try_new(
                Arc::clone(&schema),
                vec![Arc::new(UInt64Array::from(vec![1u64, 2])) as ArrayRef],
            )
            .unwrap();
            let kv = kv
                .into_iter()
                .map(|(k, v)| KeyValue::new(k.to_string(), v.to_string()))
                .collect();
            let props = WriterProperties::builder()
                .set_key_value_metadata(Some(kv))
                .build();
            let mut buf = Vec::new();
            let mut w = ArrowWriter::try_new(&mut buf, schema, Some(props)).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
            buf
        };
        use crate::long::LAYOUT_KEY;
        use metriken_storage::format::FORMAT_KEY;
        assert!(ParquetReader::open_bytes(file(vec![(FORMAT_KEY, "1")])).is_ok());
        let err = ParquetReader::open_bytes(file(vec![(FORMAT_KEY, "2")]))
            .err()
            .unwrap();
        assert!(err.to_string().contains("segment format 2"), "{err}");
        let err = ParquetReader::open_bytes(file(vec![(LAYOUT_KEY, "long2")]))
            .err()
            .unwrap();
        assert!(err.to_string().contains("\"long2\""), "{err}");
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
    fn sample_timestamps_are_the_recorded_values() {
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

    /// One row per gauge value, so a collapsed row is visible as a repeated
    /// value rather than only as a missing timestamp.
    fn build_parquet_counting_rows(raw: &[u64], sampling_interval_ms: u64) -> Vec<u8> {
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
        let values: Vec<i64> = (0..raw.len() as i64).collect();
        let gauge_array = Arc::new(Int64Array::from(values)) as ArrayRef;
        let batch = RecordBatch::try_new(schema, vec![ts_array, gauge_array]).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        buf
    }

    /// A file's declared `sampling_interval_ms` does not move its rows.
    ///
    /// The read path used to round every timestamp to that declared grid. A
    /// file whose declaration was wrong — or absent, which the reader treats
    /// as 1000 ms — therefore lost data rather than merely being described
    /// oddly: at a 100 ms cadence all ten rows of a second rounded onto the
    /// same instant, and only one of the ten values survived to be read back.
    /// This file declares exactly that wrong interval.
    ///
    /// The assertion is on VALUES, not on how many points come back. A range
    /// query emits one point per `start + k·step` whatever the data does, so a
    /// point count is fixed by the query and would pass either way; it is the
    /// values that reveal nine rows in ten having been thrown away and the
    /// survivor held forward in their place.
    #[test]
    fn a_wrong_declared_interval_does_not_move_the_rows() {
        // 100 ms cadence, declared as 1 s. Row i carries the value i.
        let raw: Vec<u64> = (0..25).map(|i| 1_000_000_000 + i * 100_000_000).collect();
        let bytes = build_parquet_counting_rows(&raw, 1000);
        let reader = ParquetReader::open_bytes(bytes).unwrap();
        assert_eq!(reader.sample_timestamps(), raw);

        let result = reader
            .query_range("dummy_gauge", 1.0, 3.4, 0.1)
            .expect("a sub-second range query must answer");
        let QueryResult::Matrix { result } = result else {
            panic!("expected a matrix, got {result:?}");
        };
        let values: Vec<f64> = result
            .first()
            .expect("one series")
            .values
            .iter()
            .map(|(_, v)| *v)
            .collect();
        let expected: Vec<f64> = (0..25).map(|i| i as f64).collect();
        assert_eq!(
            values, expected,
            "every row read back at its own instant; a rounded grid would repeat \
             each second's surviving value ten times"
        );
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
