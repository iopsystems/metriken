//! Query frontend for metriken parquet files.
//!
//! Two feature-gated paths:
//!
//! - `legacy` (live): the streaming PromQL evaluator (`promql/`) over
//!   an in-process `Tsdb`. Linked today by the Rezolus dashboard crate
//!   (for `Tsdb`) and the Rezolus binary's server-backed
//!   `/api/v1/query_range` and live-agent ingest paths.
//!
//! - `harness` (off by default): the PromQL→SQL catalogue + translator
//!   + `Engine` (`harness::*`). **Migration scaffolding**: zero
//!   production callers. Only consumed by `tests/engine_pipeline.rs`,
//!   `tests/translate_snapshots.rs`, `tests/orphan_detector.rs`, and
//!   the `examples/sql_vs_promql.rs` correctness harness. The feature
//!   exists so the harness can co-exist with the legacy evaluator
//!   side-by-side; if a real consumer never materializes the whole
//!   `harness/` subdirectory + `queries.toml` is a clean delete.
//!
//! Shared by both paths: `result::{QueryResult, Sample, MatrixSample,
//! HistogramHeatmapResult, QueryError}`.

pub mod result;

#[cfg(feature = "harness")]
pub mod harness;

#[cfg(feature = "legacy")]
pub mod promql;
#[cfg(feature = "legacy")]
pub mod tsdb;

pub use bytes::Bytes;
pub use result::{HistogramHeatmapResult, MatrixSample, QueryError, QueryResult, Sample};

#[cfg(feature = "legacy")]
pub use promql::QueryEngine;
#[cfg(feature = "legacy")]
pub use tsdb::Tsdb;
