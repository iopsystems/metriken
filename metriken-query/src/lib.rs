//! Query frontend for metriken parquet files.
//!
//! Two backends, gated by Cargo features:
//!
//! - `sql` (default): translates incoming PromQL queries against a
//!   catalogue of templates and routes execution through the DuckDB
//!   engine in `metriken-query-sql`. Returns Arrow-projected
//!   `QueryResult`s. This is the long-term path; the catalogue +
//!   translator layer is migration scaffolding that goes away once
//!   callers emit SQL directly.
//!
//! - `legacy`: the original in-memory PromQL evaluator
//!   (`promql/streaming/`) over an in-process `Tsdb`. Still linked
//!   today by the Rezolus dashboard crate (for `Tsdb`) and the
//!   Rezolus binary's server-backed `/api/v1/query_range` endpoint +
//!   live-agent ingest path. Goes away when those callers migrate to
//!   the SQL path.
//!
//! There was previously a third "shadow" / dispatcher mode that ran
//! both backends side-by-side for verification. It was removed in
//! commit a25e285 ("collapse PromQL evaluator to streaming-only");
//! the `sql` and `legacy` features are now mutually-exclusive runtime
//! paths.
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
