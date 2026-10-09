//! Exposition of Metriken metrics
//!
//! Produces snapshots of the registered metrics and exposes them: as
//! `metriken-model`'s snapshot types (re-exported here), as Prometheus text,
//! and as parquet.

#[cfg(all(feature = "serde", feature = "msgpack", feature = "parquet"))]
mod convert;
#[cfg(feature = "msgpack")]
pub mod group_builder;
#[cfg(feature = "parquet")]
mod parquet;
mod prometheus;
#[cfg(feature = "segment")]
mod segment;
#[cfg(feature = "segment")]
pub use segment::{group_approx_bytes, wal_group_row};
#[cfg(feature = "parquet")]
mod hashed;
mod snapshotter;

#[cfg(all(feature = "serde", feature = "msgpack", feature = "parquet"))]
pub use convert::MsgpackToParquet;
pub use metriken_model::{
    Counter, Gauge, GroupSchema, GroupSnapshot, GroupValidationError, Histogram, MetricDesc,
    Snapshot, SnapshotV1, SnapshotV2, SnapshotV3,
};
#[cfg(feature = "parquet")]
pub use parquet::{
    ParquetCompression, ParquetHistogramType, ParquetOptions, ParquetSchema, ParquetWriter,
};
pub use prometheus::{prometheus_text, PrometheusOptions};
pub use snapshotter::{Snapshotter, SnapshotterBuilder};
