//! The metriken observation model: what a producer reads and what a recording
//! stores, as plain types.
//!
//! Nothing here depends on the `metriken` registry, so the code that reads
//! recordings (storage, the query engine, the viewer, in the browser too)
//! can use these types without linking it. The registry's side, turning
//! registered metrics into these types, is `metriken-exposition`'s.
//!
//! - [`snapshot`]: one pass over a producer's metrics, in its V1, V2 and V3
//!   forms, and their msgpack and JSON encodings.
//! - [`schema`]: an acquisition group's membership.
//! - [`wal`]: the rows a writer stages before sealing a segment, which are
//!   also the rows a producer sends on dendro's replication stream.
//! - [`occupants`]: the occupant stream's rows, naming the occupants of a
//!   long table.
//! - [`convert`]: a group snapshot as a row.

pub mod convert;
pub mod occupants;
pub mod schema;
pub mod snapshot;
pub mod wal;

pub use metriken_types::{Window, UID_LABEL};
pub use schema::{GroupSchema, MetricDesc};
pub use snapshot::{
    Counter, Gauge, GroupSnapshot, GroupValidationError, Histogram, Snapshot, SnapshotV1,
    SnapshotV2, SnapshotV3,
};
