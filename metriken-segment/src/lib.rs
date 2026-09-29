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
//! - [`format`]: the version every segment carries, and the check a reader
//!   makes before interpreting one.
//! - [`schema`] and [`window`]: a group's membership and a reading's
//!   acquisition window, as segments store them.
//! - [`wal`]: the write-ahead log's row format, and materializing a WAL
//!   tail into a segment.
//! - [`long`]: the long layout, one row per (timestamp, occupant).
//! - [`long_table`]: building a long segment.
//! - [`occupants`]: the stream that says which labels each occupant number
//!   of a long table stands for.

pub mod builder;
pub mod format;
pub mod long;
pub mod long_table;
pub mod occupants;
pub mod schema;
pub mod table;
pub mod wal;
pub mod window;
