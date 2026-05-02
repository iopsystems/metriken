//! PromQL query engine and time-series database for metriken parquet files.
//!
//! This crate provides:
//! - A time-series database (TSDB) that loads metrics from parquet files
//! - A PromQL query engine for querying the loaded metrics
//! - Optional HTTP API routes for serving PromQL queries (with `http` feature)
//!
//! # Example
//!
//! ```ignore
//! use metriken_query::{Tsdb, QueryEngine};
//! use std::sync::Arc;
//! use std::path::Path;
//!
//! // Load a parquet file
//! let tsdb = Arc::new(Tsdb::load(Path::new("metrics.parquet")).unwrap());
//!
//! // Create a query engine
//! let engine = QueryEngine::new(tsdb);
//!
//! // Execute a range query
//! let result = engine.query_range("rate(http_requests[5m])", start, end, step);
//! ```

pub mod dispatch;
pub mod promql;
pub mod template;
pub mod tsdb;

pub use bytes::Bytes;
pub use dispatch::{
    canonicalise, Catalogue, CatalogueEntry, CatalogueError, Diff, DispatchObserver,
    GoldenExample, Mode, NoopObserver, OutputShape, SqlBackend, SqlError,
};
pub use template::{
    CaptureKind, CaptureValue, Captures, CompiledTemplate, LabelMatcher, LabelOp, TemplateError,
    TemplatePart,
};
pub use promql::{
    DispatchConfig, HistogramHeatmapResult, MatrixSample, QueryEngine, QueryError, QueryResult,
    Sample,
};
pub use tsdb::Tsdb;
