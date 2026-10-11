//! Exposition of Metriken metrics
//!
//! Produces snapshots of the registered metrics and exposes them: as
//! `metriken-model`'s snapshot types (re-exported here), as Prometheus text,
//! and with feature `stream` as dendro replication frames served on
//! `/metrics/stream`. Converting msgpack snapshots to parquet is
//! `metriken-storage`'s (`metriken_storage::convert`).

#[cfg(feature = "msgpack")]
pub mod group_builder;
mod prometheus;
#[cfg(feature = "segment")]
mod segment;
#[cfg(feature = "segment")]
pub use segment::{group_approx_bytes, wal_group_row};
mod snapshotter;
#[cfg(feature = "stream")]
pub mod stream;

pub use metriken_model::{
    Counter, Gauge, GroupSchema, GroupSnapshot, GroupValidationError, Histogram, MetricDesc,
    Snapshot, SnapshotV1, SnapshotV2, SnapshotV3,
};
pub use prometheus::{prometheus_text, PrometheusOptions};
pub use snapshotter::{Snapshotter, SnapshotterBuilder};
