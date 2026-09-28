//! The parquet segment format for metriken metrics.
//!
//! What a segment's columns mean, written and read in one place, so a
//! producer other than rezolus can write segments that `metriken-query`
//! reads. No storage and no metrics registry: this builds for wasm32.
//! See `docs/journal/2026-09-28-high-cardinality-stack.md`.
//!
//! - [`table`]: the wide layout, a column per metric (or per metric and
//!   slot); its parquet encoding and decoding.
//! - [`builder`]: growing a wide table row by row.
//! - [`schema`] and [`window`]: a group's membership and a reading's
//!   acquisition window, as segments store them.
//! - [`long`]: the long layout, one row per (timestamp, occupant).
//! - [`occupants`]: the stream that says which labels each occupant number
//!   of a long table stands for.

pub mod builder;
pub mod long;
pub mod occupants;
pub mod schema;
pub mod table;
pub mod window;
