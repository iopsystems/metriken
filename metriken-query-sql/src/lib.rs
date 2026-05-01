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

use duckdb::Connection;

pub mod backend;
pub mod macros;
pub mod udf;
pub mod views;

pub use backend::DuckDbBackend;

/// Register all UDFs and macros on `conn`. Idempotent within a single
/// connection (registrations use `CREATE OR REPLACE`).
pub fn register_all(conn: &Connection) -> duckdb::Result<()> {
    udf::register_all(conn)?;
    macros::register_all(conn)?;
    Ok(())
}
