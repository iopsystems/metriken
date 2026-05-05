//! Query frontend for metriken parquet files.
//!
//! Two backends, gated by Cargo features:
//!
//! - `sql` (default for desktop): translates incoming PromQL queries
//!   against a catalogue of templates and routes execution through
//!   the DuckDB engine in `metriken-query-sql`. Returns Arrow-projected
//!   `QueryResult`s. This is the production path.
//!
//! - `legacy` (default off; enabled by the Rezolus WASM viewer): the
//!   original in-memory PromQL evaluator (`promql/streaming/`) over an
//!   in-process `Tsdb`. Kept for WASM consumers because DuckDB doesn't
//!   compile to wasm32 today.
//!
//! Shared by both backends: `result::{QueryResult, Sample, MatrixSample,
//! HistogramHeatmapResult, QueryError}`.

pub mod result;

#[cfg(feature = "sql")]
pub mod catalogue;
#[cfg(feature = "sql")]
pub mod engine;
#[cfg(feature = "sql")]
pub mod interp;
#[cfg(feature = "sql")]
pub mod project;
#[cfg(feature = "sql")]
pub mod template;
#[cfg(feature = "sql")]
pub mod translate;

#[cfg(feature = "legacy")]
pub mod promql;
#[cfg(feature = "legacy")]
pub mod tsdb;

pub use bytes::Bytes;
pub use result::{HistogramHeatmapResult, MatrixSample, QueryError, QueryResult, Sample};

#[cfg(feature = "sql")]
pub use catalogue::{Catalogue, CatalogueEntry, CatalogueError, GoldenExample, OutputShape};
#[cfg(feature = "sql")]
pub use engine::{Engine, EngineError, ParquetMetadata};
#[cfg(feature = "sql")]
pub use metriken_query_sql::SqlError;
#[cfg(feature = "sql")]
pub use template::{
    CaptureKind, CaptureValue, Captures, CompiledTemplate, LabelMatcher, LabelOp, TemplateError,
    TemplatePart,
};

#[cfg(feature = "legacy")]
pub use promql::QueryEngine;
#[cfg(feature = "legacy")]
pub use tsdb::Tsdb;
