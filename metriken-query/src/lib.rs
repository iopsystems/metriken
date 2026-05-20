//! DuckDB-side query engine for metriken Parquet files.
//!
//! Two layers, both registered onto a `duckdb::Connection`:
//!
//! - `udf` — H2 histogram scalar UDFs (`h2_lower`, `h2_upper`, `h2_midpoint`,
//!   `h2_total`, `h2_delta`, `h2_quantile`, `h2_quantiles`, `h2_count_in_range`,
//!   `h2_combine`). Pure functions over Rezolus's H2 bucket layout, with
//!   bucket math verified to match the rezolus `histogram` crate.
//! - `macros` — SQL macro layer mirroring the Rezolus dashboard's recurring
//!   PromQL idioms (`irate_1s`, `rate_5m`, `cpu_busy_pct`, `ipc`, `ipns`,
//!   `hist_p99`, …). Composes the UDF layer.
//!
//! `register_all(&conn)` registers everything in the right order.
//!
//! The crate's public API is two methods on [`DuckDbBackend`]:
//! [`DuckDbBackend::run_sql`] (SQL string + parquet path → Arrow) and
//! [`DuckDbBackend::describe_parquet`] (parquet path → per-metric
//! catalog). Callers own SQL generation and any projection of the Arrow
//! output (Rezolus uses `crates/prom-matrix/` for the native + WASM
//! Prometheus-matrix envelope).

use duckdb::Connection;
use thiserror::Error;

// `backend`, `macros`, and `observability` are implementation
// details — their public types are re-exported below. `udf` and
// `views` stay `pub`: `udf::h2_lower` / `udf::h2_upper` are called
// across the crate boundary by external projectors, and
// `views::ensure_views` is used by perf-investigation scripts.
pub(crate) mod backend;
pub mod live;
pub(crate) mod macros;
pub(crate) mod observability;
pub mod udf;
pub mod views;

pub use backend::DuckDbBackend;
pub use live::{canonical_column_name, LiveColumn, LiveColumnKind, LiveSource, LiveValue};
pub use macros::SHARED_MACROS;
pub use observability::{BackendStats, PhaseSnapshot, StatsSnapshot};
pub use views::{view_name_for_node, view_name_for_source, MetricCatalog, MetricSeries};

/// Errors returned by the engine. The single `Backend` variant covers
/// all DuckDB-side failures with a free-form message; callers compose
/// their own context. Kept simple: callers don't dispatch on variants
/// today, so a richer enum would be extra surface for no benefit.
#[derive(Debug, Error)]
pub enum SqlError {
    #[error("SQL backend error: {0}")]
    Backend(String),
}

/// Register all UDFs and macros on `conn`. Idempotent within a single
/// connection (registrations use `CREATE OR REPLACE`).
pub fn register_all(conn: &Connection) -> duckdb::Result<()> {
    udf::register_all(conn)?;
    macros::register_all(conn)?;
    Ok(())
}
