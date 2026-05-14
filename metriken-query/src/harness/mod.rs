//! PromQL → SQL harness (migration scaffolding).
//!
//! Five-step pipeline (see `engine.rs:1-18`):
//!
//! ```text
//!   Catalogue::lookup           match query, extract captures
//! → describe_parquet            get metric metadata (cached)
//! → translate::try_generate     emit wide-form SQL
//! → DuckDbBackend::run_sql      execute
//! → project::run                Arrow batches → QueryResult
//! ```
//!
//! **Not a production code path.** The Rezolus binary, the rezolus
//! dashboard crate, and the rezolus static viewer (`viewer-sql`) all
//! bypass this layer. The only consumers are the parity harness
//! (`examples/sql_vs_promql.rs`) and its supporting tests. The whole
//! subdirectory + `queries.toml` is a clean delete if a real consumer
//! never materializes.

pub mod catalogue;
pub mod engine;
pub mod interp;
pub mod project;
pub mod template;
pub mod translate;

pub use catalogue::{Catalogue, CatalogueEntry, CatalogueError, GoldenExample, OutputShape};
pub use engine::{Engine, EngineError, ParquetMetadata};
pub use template::{
    CaptureKind, CaptureValue, Captures, CompiledTemplate, LabelMatcher, LabelOp, TemplateError,
    TemplatePart,
};

pub use metriken_query_sql::SqlError;
