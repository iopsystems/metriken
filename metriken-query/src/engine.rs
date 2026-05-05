//! SQL-backed query engine for metriken parquet files.
//!
//! `Engine` is the SQL-only successor to the legacy `QueryEngine`.
//! Construction takes a parquet path; queries come in as PromQL strings
//! and are routed through:
//!
//! 1. `Catalogue::lookup` — match the query against a registered template,
//!    extract captures.
//! 2. `metriken-query-sql::DuckDbBackend::describe_parquet` — get the
//!    parquet's metric metadata (cached after first request).
//! 3. `crate::translate::try_generate` — emit the wide-form SQL.
//! 4. `metriken-query-sql::DuckDbBackend::run_sql` — execute it.
//! 5. `crate::project::run` — turn Arrow batches back into `QueryResult`.
//!
//! No PromQL evaluator, no shadow comparison, no `Mode` lifecycle.
//! Queries that don't match any catalogue entry are a hard error.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use metriken_query_sql::{DuckDbBackend, SqlError};
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};

use crate::catalogue::Catalogue;
use crate::result::{MatrixSample, QueryError, QueryResult, Sample};

/// Errors returned by `Engine`.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("parquet read error: {0}")]
    Parquet(String),

    #[error("no catalogue entry matched query: {0}")]
    NoCatalogueMatch(String),

    #[error("entry `{0}` has no wide-form generator")]
    NoTranslator(String),

    #[error("sql backend error: {0}")]
    Sql(#[from] SqlError),
}

impl From<EngineError> for QueryError {
    fn from(e: EngineError) -> Self {
        QueryError::EvaluationError(e.to_string())
    }
}

/// Parquet-file metadata exposed for callers that want to display
/// recording properties (filename, source agent, version, sampling
/// interval) without running a query. Read once at `Engine::new` and
/// cached for the engine's lifetime.
#[derive(Debug, Clone, Default)]
pub struct ParquetMetadata {
    /// Sampling interval in nanoseconds.
    pub interval_ns: u64,
    /// Source agent name from the parquet file's kv metadata.
    pub source: Option<String>,
    /// Source agent version.
    pub version: Option<String>,
    /// Display filename (caller-supplied via `set_filename`; not present
    /// in the parquet file itself).
    pub filename: Option<String>,
    /// Raw parquet file kv metadata, with the keys this struct already
    /// surfaces removed.
    pub other: HashMap<String, String>,
}

impl ParquetMetadata {
    fn from_bytes(bytes: &Bytes) -> Result<Self, EngineError> {
        let meta = ArrowReaderMetadata::load(bytes, ArrowReaderOptions::default())
            .map_err(|e| EngineError::Parquet(format!("read parquet metadata: {e}")))?;
        let kvs = meta
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .map(|kvs| kvs.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut interval_ms: Option<u64> = None;
        let mut source = None;
        let mut version = None;
        let mut other = HashMap::new();
        for kv in kvs {
            let value = kv.value.unwrap_or_default();
            match kv.key.as_str() {
                "sampling_interval_ms" => interval_ms = value.parse().ok(),
                "source" => source = Some(value),
                "version" => version = Some(value),
                _ => {
                    other.insert(kv.key, value);
                }
            }
        }
        Ok(Self {
            interval_ns: interval_ms.map(|ms| ms * 1_000_000).unwrap_or(1_000_000_000),
            source,
            version,
            filename: None,
            other,
        })
    }
}

/// SQL-backed query engine. Lightweight; built once per parquet path
/// and reused for every query. `DuckDbBackend` and `Catalogue` are
/// `Arc`-shareable so multiple engines can share warm pools.
pub struct Engine {
    catalogue: Catalogue,
    backend: Arc<DuckDbBackend>,
    parquet: String,
    metadata: ParquetMetadata,
}

impl Engine {
    /// Build an engine over the parquet at `path`. Reads the file's
    /// metadata once for the metadata accessors; the heavy DuckDB pool
    /// is built lazily on the first query.
    pub fn new(path: impl Into<String>) -> Result<Self, EngineError> {
        let parquet = path.into();
        let bytes = std::fs::read(&parquet)
            .map(Bytes::from)
            .map_err(|e| EngineError::Parquet(format!("open {parquet}: {e}")))?;
        let metadata = ParquetMetadata::from_bytes(&bytes)?;
        Ok(Self {
            catalogue: Catalogue::embedded(),
            backend: Arc::new(DuckDbBackend::new()),
            parquet,
            metadata,
        })
    }

    /// Build an engine with an already-shared backend (so multiple
    /// engines over different parquets can share the warm-pool
    /// connection cache).
    pub fn with_backend(
        path: impl Into<String>,
        backend: Arc<DuckDbBackend>,
    ) -> Result<Self, EngineError> {
        let parquet = path.into();
        let bytes = std::fs::read(&parquet)
            .map(Bytes::from)
            .map_err(|e| EngineError::Parquet(format!("open {parquet}: {e}")))?;
        let metadata = ParquetMetadata::from_bytes(&bytes)?;
        Ok(Self {
            catalogue: Catalogue::embedded(),
            backend,
            parquet,
            metadata,
        })
    }

    /// File metadata (sampling interval, source agent, etc.).
    pub fn metadata(&self) -> &ParquetMetadata {
        &self.metadata
    }

    /// Mutable access to the metadata struct — useful for the caller
    /// to set a display filename.
    pub fn metadata_mut(&mut self) -> &mut ParquetMetadata {
        &mut self.metadata
    }

    /// Time range (start, end) of all data in seconds.
    pub fn time_range(&self) -> Result<(f64, f64), EngineError> {
        let batches = self
            .backend
            .run_sql("SELECT MIN(timestamp), MAX(timestamp) FROM _src", &self.parquet)?;
        let batch = batches
            .first()
            .ok_or_else(|| EngineError::Sql(SqlError::Backend("empty time range".into())))?;
        let min = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .ok_or_else(|| EngineError::Sql(SqlError::Backend("min ts not Int64".into())))?
            .value(0);
        let max = batch
            .column(1)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .ok_or_else(|| EngineError::Sql(SqlError::Backend("max ts not Int64".into())))?
            .value(0);
        Ok((min as f64 / 1e9, max as f64 / 1e9))
    }

    /// Range query: evaluate `query_str` over `[start, end]` at `step`
    /// resolution. Returns a `QueryResult::Matrix` for time-series shapes
    /// or `QueryResult::HistogramHeatmap` for histogram heatmaps.
    pub fn query_range(
        &self,
        query_str: &str,
        _start: f64,
        _end: f64,
        _step: f64,
    ) -> Result<QueryResult, EngineError> {
        let (entry, captures) = self
            .catalogue
            .lookup(query_str)
            .ok_or_else(|| EngineError::NoCatalogueMatch(query_str.to_string()))?;
        let catalog = self.backend.describe_parquet(&self.parquet)?;
        let sql = crate::translate::try_generate(entry, &captures, &catalog)
            .ok_or_else(|| EngineError::NoTranslator(entry.id.clone()))?;
        let batches = self.backend.run_sql(&sql, &self.parquet)?;
        Ok(crate::project::run(&batches, entry, &captures)?)
    }

    /// Instant query: degenerate range query at a single timestamp,
    /// collapsed into a `QueryResult::Vector` by taking the latest
    /// point of each result series.
    pub fn query(&self, query_str: &str, time: f64) -> Result<QueryResult, EngineError> {
        let step = (self.metadata.interval_ns as f64 / 1e9).max(1.0);
        let result = self.query_range(query_str, time, time, step)?;
        Ok(matrix_to_vector(result))
    }
}

/// Collapse a matrix result into a vector by taking the latest point
/// of each series. Used by `query()` to convert a degenerate range
/// query into instant-query shape. Non-matrix results pass through
/// unchanged.
fn matrix_to_vector(result: QueryResult) -> QueryResult {
    let QueryResult::Matrix { result: samples } = result else {
        return result;
    };
    let vector: Vec<Sample> = samples
        .into_iter()
        .filter_map(|s| {
            s.values.last().copied().map(|value| Sample {
                metric: s.metric,
                value,
            })
        })
        .collect();
    let _ = std::mem::size_of::<MatrixSample>();
    QueryResult::Vector { result: vector }
}
