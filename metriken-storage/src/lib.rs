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
//! - [`format`](mod@format): the version every segment carries, and the check a reader
//!   makes before interpreting one.
//! - [`schema`] and [`window`]: a group's membership and a reading's
//!   acquisition window, as segments store them.
//! - [`wal`]: the write-ahead log's row format, and materializing a WAL
//!   tail into a segment.
//! - [`long`]: the long layout, one row per (timestamp, occupant).
//! - [`long_table`]: building a long segment.
//! - [`occupants`]: the stream that says which labels each occupant number
//!   of a long table stands for.

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
