//! Storage for metriken recordings: the segment format, the column readers,
//! and the archive containers, read and written. Queries over these are
//! `metriken-query`'s, which implements `MetricsSource` for the readers here.
//! Without `write`, `stream` or `rez` this builds for wasm32. See
//! `docs/journal/2026-10-09-storage-scan.md`.
//!
//! The segment format:
//! - [`table`]: the wide layout, a column per metric (or per metric and
//!   slot); its parquet encoding and decoding. [`builder`]: growing one row
//!   by row.
//! - [`long`]: the long layout, one row per (timestamp, occupant), and
//!   [`long::OccupantLabels`], the relabel that puts an occupant's labels on
//!   its series. [`long_table`]: building a long segment. [`occupants`]: the
//!   stream that names each occupant.
//! - [`format`](mod@format): the version every segment carries, and the check
//!   a reader makes before interpreting one.
//! - [`schema`] and [`window`]: a group's membership and a reading's
//!   acquisition window, as segments store them.
//! - [`wal`]: the write-ahead log's row format, and materializing a WAL tail
//!   into a segment.
//!
//! Reading:
//! - [`source`]: [`DataSource`], the trait the query engine reads through,
//!   and [`scan`], the decoded columns its batched rate path consumes.
//! - [`parquet`]: one or more parquet files as a source. [`segmented`]: a
//!   table of many segments as one source, with the segment store and
//!   relabel. [`union`]: sources with disjoint metric names as one.
//!   [`lazy`]: a composition child opened on first use. [`memory_store`] and
//!   [`memory`]: an in-memory source.
//! - [`buffer_pool`]: the cache of decoded blocks readers share.
//!
//! Archives:
//! - [`catalog`]: what a reader needs from a container, and dendro's
//!   implementation. [`reader`]: [`ArchiveReader`], one source per table of a
//!   recording. `writer` (feature `write`): [`ArchiveWriter`]. `stream`
//!   (feature `stream`): a producer's groups as dendro replication frames.
//! - `rez` (feature `rez`): the `.rez` format.

mod archive;
pub mod buffer_pool;
pub mod builder;
pub mod catalog;
pub mod encoder;
pub mod filter;
pub mod format;
pub mod histogram_stream;
pub mod labels;
pub mod lazy;
pub mod long;
pub mod long_table;
pub mod memory;
pub mod memory_store;
mod names;
pub mod occupants;
pub mod parquet;
pub mod reader;
#[cfg(feature = "rez")]
pub mod rez;
pub mod scan;
pub mod schema;
pub mod segmented;
pub mod source;
#[cfg(feature = "stream")]
pub mod stream;
pub mod table;
pub mod types;
pub mod union;
pub mod util;
pub mod wal;
pub mod window;
#[cfg(feature = "write")]
pub mod writer;

pub use archive::*;
pub use catalog::{Catalog, DendroCatalog, SegmentIndex};
pub use encoder::{Encoder, ENCODER_VERSION, READABLE_ENCODERS};
pub use filter::KeepMetrics;
pub use reader::{ArchiveReader, LabeledRecordings};
#[cfg(feature = "stream")]
pub use stream::FrameProducer;
#[cfg(feature = "write")]
pub use writer::{ArchiveWriter, SourceRecorder, StreamDecoder, StreamedGroup, WriterConfig};

pub use buffer_pool::{BufferPool, BufferPoolStats};
pub use labels::{is_internal_label, is_storage_key, Labels, STORAGE_KEYS};
pub use lazy::CompositionCatalog;
pub use parquet::CompositionSource;
pub use segmented::{ColumnRelabel, Handover, InMemorySegments, Run, SegmentBytes, SegmentStore};
pub use source::{label_walk_series_count, ColumnPosition, CounterColumnRef, DataSource};
pub use types::{ColumnChunk, CounterSample, CounterStream, HistogramSnapshot};
pub use union::{UnionChild, UnionError};
