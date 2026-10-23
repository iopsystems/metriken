//! Legacy recordings rewritten as dendro archives (feature `write`).
//!
//! metriken writes only `.dendro` and reads every format written before it.
//! An edit of an older recording therefore writes a new archive, and these
//! are the conversions it starts from:
//!
//! - [`ParquetToArchive`]: a standalone parquet recording (one file, a
//!   column per metric, as `MsgpackToParquet` and rezolus's recorder wrote
//!   them, combined files included).
//! - `rez::to_dendro::convert_tar_to_dendro` (feature `rez`): a v1 or v2
//!   tar `.rez`. A v3 `.rez` is `rez::to_dendro::convert_v3_to_dendro`.
//!
//! The design, which both follow:
//!
//! - **One source per recording, as the readers model it.** A tar's
//!   recordings become sources with their manifest labels, metadata, clock
//!   anchor, clock offsets and `complete` flag. A parquet file is one
//!   source: metriken's readers take a file as one source and never split
//!   a combined file into its producers (its columns are told apart by
//!   their own names and labels), so the converter does not either. A
//!   combined file's `per_source_metadata` is carried as metadata.
//! - **Tables keep their columns.** Each input table is decoded and
//!   re-encoded column for column: counters, gauges, histograms, the
//!   `timestamp`, `duration` and `:wall_offset` columns, per-metric and
//!   table-level acquisition window sidecars, and every column's field
//!   metadata (its labels). Two things are added or renamed, neither
//!   changing an answer: a counting histogram column whose `metric_type` is
//!   `histogram` or absent is written as `counter_histogram`, which is what
//!   0.5 writes and what every reader takes as the same kind; and a parquet
//!   file's `duration` column is also written as the table-level window
//!   pair (`:window_begin` 0, `:window_width` the duration) when the file
//!   has none, because the parquet reader gives a metric without its own
//!   window sidecars the window `[timestamp, timestamp + duration]` and an
//!   archive's reader reads windows only from sidecars. A tar table keeps
//!   its name; a parquet file's columns are one table, [`PARQUET_TABLE`].
//! - **Segments are bounded.** An input file is split into segments at the
//!   writer's seal caps ([`WriterConfig::seal`]'s `max_rows`, and
//!   `max_bytes` measured on the decoded rows), never merged with another
//!   input file. `max_age` bounds what a crash loses while recording and
//!   does not apply. A long segment (rezolus wrote none into a tar) is
//!   copied byte for byte, since its occupant list indexes its rows.
//! - **Metadata is carried as-is.** A parquet file's key-value metadata
//!   (`systeminfo`, `descriptions`, `per_source_metadata`, `version`,
//!   `producer_epoch`, `sampling_interval_ms`, report and event keys)
//!   becomes the source's metadata, without parquet's own `ARROW:schema`
//!   and the segment format key. A tar recording's metadata is copied. A
//!   `version` is also written under dendro's
//!   [`PRODUCER_VERSION`](dendro::keys::PRODUCER_VERSION), as
//!   `convert_v3_to_dendro` does.
//! - **The output is finished.** Every row is in a sealed segment, nothing
//!   is in the WAL. A recording that was cleanly finalized is marked
//!   complete; one recovered from a truncated tar is not, so it still reads
//!   as recovered.
//! - **An existing output is refused** and left as it is. On an error the
//!   files the conversion created are removed: the archive, and its `-wal`,
//!   `-shm` and `-journal` sidecars that did not exist before it.
//!
//! The converted archive answers the queries the input answered:
//! `tests/legacy_to_dendro.rs` compares the two over every committed legacy
//! fixture.
//!
//! One thing an archive does not carry is a declared interval. A parquet
//! file's `sampling_interval_ms` is kept in the metadata, but an archive's
//! reader measures each table's interval from its rows, so on a recording
//! with dropped or jittered rows gauge staleness and instant `irate` can
//! differ from the parquet reader's where a grid point falls between the
//! two intervals.
//!
//! The functions taking an [`ArchiveMut`] write into an archive the caller
//! made, which may be [`ArchiveMut::create_in_memory`] where there is no
//! filesystem (the browser); none of them spawns a thread.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use dendro::archive::{ArchiveMut, SegmentMeta, SourceMeta};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::file::metadata::KeyValue;
use parquet::file::reader::ChunkReader;

use crate::WriterConfig;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// The table a parquet file's columns are written to.
pub const PARQUET_TABLE: &str = "parquet";

/// The key parquet's arrow writer keeps the arrow schema under. It is
/// rewritten from each segment's schema, never copied.
const ARROW_SCHEMA_KEY: &str = "ARROW:schema";

/// The metadata key naming the producer's version (dendro's `keys::VERSION`).
const VERSION: &str = "version";

/// What one conversion wrote.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Written {
    /// Sources: a tar's recordings, or 1 for a parquet file.
    pub sources: usize,
    /// Tables, summed over the sources.
    pub tables: usize,
    /// Segments, summed over the tables.
    pub segments: usize,
    /// Rows, summed over the segments.
    pub rows: u64,
    /// A tar manifest's clock offset observations.
    pub clock_offsets: usize,
}

/// Converts a standalone parquet recording into a `.dendro` archive of one
/// source; see the [module docs](self) for what is carried.
///
/// ```no_run
/// use metriken_storage::to_dendro::ParquetToArchive;
///
/// let written = ParquetToArchive::new()
///     .convert_file_path("rezolus.parquet", "rezolus.dendro")
///     .unwrap();
/// println!("{} rows in {} segments", written.rows, written.segments);
/// ```
#[derive(Default)]
pub struct ParquetToArchive {
    labels: BTreeMap<String, String>,
    metadata: BTreeMap<String, String>,
    config: WriterConfig,
}

impl ParquetToArchive {
    /// A converter writing a source with no labels, so its series carry the
    /// labels they carry read from the file, under the default
    /// [`WriterConfig`].
    pub fn new() -> Self {
        Self::default()
    }

    /// A label of the archive's source (`source`, `host`, ...). Every
    /// series read from the archive carries it.
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// A metadata entry of the archive's source, replacing the file's entry
    /// of the same key.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// The seal caps segments are split at, and their codec.
    pub fn writer_config(mut self, config: WriterConfig) -> Self {
        self.config = config;
        self
    }

    /// Convert the parquet file at `input` into a new archive at `output`,
    /// which must not exist. On an error the files this created are
    /// removed.
    pub fn convert_file_path(
        &self,
        input: impl AsRef<Path>,
        output: impl AsRef<Path>,
    ) -> Result<Written, Error> {
        let input = input.as_ref();
        let file =
            std::fs::File::open(input).map_err(|e| format!("opening {}: {e}", input.display()))?;
        with_new_archive(output.as_ref(), |out| self.convert_into(file, out))
    }

    /// Convert a parquet file read from `input` (a [`std::fs::File`], or
    /// [`bytes::Bytes`] of a file in memory) into `out`, as one new source.
    pub fn convert_into<R: ChunkReader + 'static>(
        &self,
        input: R,
        out: &mut ArchiveMut,
    ) -> Result<Written, Error> {
        let builder = ParquetRecordBatchReaderBuilder::try_new(input)?;
        let kv = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .cloned()
            .unwrap_or_default();
        let mut metadata: BTreeMap<String, String> = kv
            .iter()
            .filter(|e| is_recording_key(&e.key))
            .map(|e| (e.key.clone(), e.value.clone().unwrap_or_default()))
            .collect();
        metadata.extend(self.metadata.clone());
        with_producer_version(&mut metadata);
        // The one file key a segment reader interprets (as the table's
        // interval when it cannot measure one) stays on each segment.
        let segment_kv: Vec<KeyValue> = kv
            .into_iter()
            .filter(|e| e.key == "sampling_interval_ms")
            .collect();

        // Each segment is inserted as it is sealed, so a large file is not
        // held in memory. The source is started with the first, whose first
        // row is the recording's start (its clock anchor, as a recorder
        // takes its first tick's).
        let mut written = Written::default();
        let mut source = None;
        let mut sink = |segment: EncodedSegment| -> Result<(), Error> {
            let id = match source {
                Some(id) => id,
                None => {
                    let id = out.insert_source(&SourceMeta {
                        labels: self.labels.clone(),
                        metadata: metadata.clone(),
                        clock_anchor_wall_ns: segment.meta.first_ts,
                    })?;
                    written.sources = 1;
                    written.tables = 1;
                    *source.insert(id)
                }
            };
            out.insert_segment(
                id,
                PARQUET_TABLE,
                written.segments as u64,
                &segment.meta,
                &segment.bytes,
            )?;
            written.segments += 1;
            written.rows += segment.meta.rows;
            Ok(())
        };
        TableWriter::new(PARQUET_TABLE, &self.config, &mut sink).push_file(
            builder,
            segment_kv,
            DurationWindows::Write,
        )?;
        let Some(source) = source else {
            return Err("the file holds no rows".into());
        };
        out.mark_complete(source)?;
        Ok(written)
    }
}

/// Whether a parquet file's key is the recording's metadata rather than
/// the file format's.
fn is_recording_key(key: &str) -> bool {
    key != ARROW_SCHEMA_KEY && key != crate::format::FORMAT_KEY
}

/// `version`, also under dendro's `producer_version` unless that is set.
pub(crate) fn with_producer_version(metadata: &mut BTreeMap<String, String>) {
    if let Some(version) = metadata.get(VERSION).cloned() {
        metadata
            .entry(dendro::keys::PRODUCER_VERSION.to_string())
            .or_insert(version);
    }
}

/// Create the archive at `output`, run `f` on it, and on an error remove
/// what was created: the archive, and the `-wal`, `-shm` and `-journal`
/// sidecars that did not exist before it. An existing `output` is refused
/// and never touched.
pub(crate) fn with_new_archive<T, E: From<String>>(
    output: &Path,
    f: impl FnOnce(&mut ArchiveMut) -> Result<T, E>,
) -> Result<T, E> {
    if output.exists() {
        return Err(E::from(format!("{} already exists", output.display())));
    }
    let sidecars: Vec<PathBuf> = ["-wal", "-shm", "-journal"]
        .iter()
        .map(|suffix| {
            let mut path = output.as_os_str().to_owned();
            path.push(suffix);
            PathBuf::from(path)
        })
        .filter(|path| !path.exists())
        .collect();
    let mut out = ArchiveMut::create(output)
        .map_err(|e| E::from(format!("failed to create {}: {e}", output.display())))?;
    let result = f(&mut out);
    drop(out);
    if result.is_err() {
        for path in std::iter::once(output.to_path_buf()).chain(sidecars) {
            let _ = std::fs::remove_file(path);
        }
    }
    result
}

/// One encoded segment, ready to insert.
#[cfg_attr(not(feature = "rez"), allow(dead_code))]
pub(crate) struct EncodedSegment {
    pub(crate) meta: SegmentMeta,
    pub(crate) bytes: Vec<u8>,
}

/// Insert a table's segments, numbered from 0, into `source`.
#[cfg_attr(not(feature = "rez"), allow(dead_code))]
pub(crate) fn insert_table(
    out: &mut ArchiveMut,
    source: i64,
    table: &str,
    segments: Vec<EncodedSegment>,
    written: &mut Written,
) -> Result<(), String> {
    if segments.is_empty() {
        return Ok(());
    }
    written.tables += 1;
    let mut rows = 0;
    out.transaction(|tx| {
        for (seq, segment) in segments.iter().enumerate() {
            tx.insert_segment(source, table, seq as u64, &segment.meta, &segment.bytes)?;
            rows += segment.meta.rows;
        }
        Ok(())
    })
    .map_err(|e| format!("writing table {table}: {e}"))?;
    written.segments += segments.len();
    written.rows += rows;
    Ok(())
}

/// Where a [`TableWriter`] hands each segment it seals, in order.
pub(crate) type Sink<'s> = &'s mut dyn FnMut(EncodedSegment) -> Result<(), Error>;

/// Re-encodes one table's input files as bounded segments.
pub(crate) struct TableWriter<'a, 's> {
    table: &'a str,
    max_rows: usize,
    max_bytes: usize,
    props: parquet::file::properties::WriterProperties,
    sink: Sink<'s>,
}

impl<'a, 's> TableWriter<'a, 's> {
    pub(crate) fn new(table: &'a str, config: &WriterConfig, sink: Sink<'s>) -> Self {
        Self {
            table,
            max_rows: config.seal.max_rows.max(1),
            max_bytes: config.seal.max_bytes.max(1),
            props: crate::segment_props(config.compression),
            sink,
        }
    }

    /// An input segment from a container: re-encoded keeping its own
    /// key-value metadata, or copied byte for byte when it is long.
    #[cfg_attr(not(feature = "rez"), allow(dead_code))]
    pub(crate) fn push_segment(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        let bytes = bytes::Bytes::from(bytes);
        let builder = ParquetRecordBatchReaderBuilder::try_new(bytes.clone())
            .map_err(|e| format!("table {}: {e}", self.table))?;
        let kv: Vec<KeyValue> = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| is_recording_key(&e.key))
            .collect();
        if kv.iter().any(|e| e.key == crate::long::LAYOUT_KEY) {
            let (first_ts, last_ts, rows) =
                span_of_file(builder).map_err(|e| format!("table {}: {e}", self.table))?;
            if rows > 0 {
                (self.sink)(EncodedSegment {
                    meta: SegmentMeta {
                        rows,
                        first_ts,
                        last_ts,
                    },
                    bytes: bytes.to_vec(),
                })
                .map_err(|e| format!("table {}: {e}", self.table))?;
            }
            return Ok(());
        }
        self.push_file(builder, kv, DurationWindows::Leave)
            .map_err(|e| format!("table {}: {e}", self.table))
    }

    /// Every row of one input file, cut into segments carrying `kv`.
    pub(crate) fn push_file<R: ChunkReader + 'static>(
        &mut self,
        builder: ParquetRecordBatchReaderBuilder<R>,
        kv: Vec<KeyValue>,
        durations: DurationWindows,
    ) -> Result<(), Error> {
        let mut schema = counting_histograms_named(builder.schema().clone());
        let duration_col = match durations {
            DurationWindows::Write => duration_windows(&schema),
            DurationWindows::Leave => None,
        };
        if duration_col.is_some() {
            let mut fields: Vec<Field> =
                schema.fields().iter().map(|f| f.as_ref().clone()).collect();
            fields.push(Field::new(
                crate::table::WINDOW_BEGIN_COLUMN,
                DataType::Int64,
                true,
            ));
            fields.push(Field::new(
                crate::table::WINDOW_WIDTH_COLUMN,
                DataType::UInt64,
                true,
            ));
            schema = Arc::new(Schema::new(fields));
        }
        let ts_col = schema
            .fields()
            .iter()
            .position(|f| f.name() == "timestamp" && f.data_type() == &DataType::UInt64)
            .ok_or("the file has no UInt64 `timestamp` column")?;
        let props = crate::format::stamped_for(
            self.props
                .clone()
                .into_builder()
                .set_key_value_metadata((!kv.is_empty()).then_some(kv))
                .build(),
            &schema,
        );
        let reader = builder.with_batch_size(1024).build()?;
        let mut pending: Vec<RecordBatch> = Vec::new();
        let (mut rows, mut bytes) = (0usize, 0usize);
        for batch in reader {
            let batch = batch?;
            let mut columns = batch.columns().to_vec();
            if let Some(col) = duration_col {
                let duration = batch
                    .column(col)
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or("the `duration` column is not UInt64")?;
                let begin: arrow::array::Int64Array =
                    duration.iter().map(|d| d.map(|_| 0i64)).collect();
                columns.push(Arc::new(begin));
                columns.push(Arc::new(duration.clone()));
            }
            let batch = RecordBatch::try_new(Arc::clone(&schema), columns)?;
            let per_row = batch.get_array_memory_size() / batch.num_rows().max(1);
            let mut rest = batch;
            while rest.num_rows() > 0 {
                let take = (self.max_rows - rows).min(rest.num_rows());
                pending.push(rest.slice(0, take));
                rest = rest.slice(take, rest.num_rows() - take);
                rows += take;
                bytes += take * per_row;
                if rows >= self.max_rows || bytes >= self.max_bytes {
                    self.seal(&schema, ts_col, &props, std::mem::take(&mut pending))?;
                    (rows, bytes) = (0, 0);
                }
            }
        }
        if rows > 0 {
            self.seal(&schema, ts_col, &props, pending)?;
        }
        Ok(())
    }

    fn seal(
        &mut self,
        schema: &Arc<Schema>,
        ts_col: usize,
        props: &parquet::file::properties::WriterProperties,
        batches: Vec<RecordBatch>,
    ) -> Result<(), Error> {
        let batch = arrow::compute::concat_batches(schema, &batches)?;
        let (first_ts, last_ts) = span(batch.column(ts_col).as_ref())?;
        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, Arc::clone(schema), Some(props.clone()))?;
        writer.write(&batch)?;
        writer.close()?;
        (self.sink)(EncodedSegment {
            meta: SegmentMeta {
                rows: batch.num_rows() as u64,
                first_ts,
                last_ts,
            },
            bytes: buf,
        })
    }
}

/// Whether a file's `duration` column is written as its table-level window.
#[derive(Clone, Copy)]
pub(crate) enum DurationWindows {
    /// For a standalone parquet file, whose reader takes a metric without a
    /// window sidecar to have been read over `[timestamp, timestamp +
    /// duration]` (`parquet::resolve_window`'s fallback). An archive's
    /// reader reads windows only from sidecars, so the conversion writes
    /// that window as the table-level `:window_begin` (0) and
    /// `:window_width` (`duration`) pair, which a metric without its own
    /// pair takes, and the bands stay as they were.
    Write,
    /// For a container's segment, whose reader (`ArchiveReader`) already
    /// read it without the fallback.
    #[cfg_attr(not(feature = "rez"), allow(dead_code))]
    Leave,
}

/// The `duration` column's position, when the file has one as `UInt64` and
/// no table-level window pair of its own.
fn duration_windows(schema: &Schema) -> Option<usize> {
    let has = |name: &str| schema.fields().iter().any(|f| f.name() == name);
    if has(crate::table::WINDOW_BEGIN_COLUMN) || has(crate::table::WINDOW_WIDTH_COLUMN) {
        return None;
    }
    schema
        .fields()
        .iter()
        .position(|f| f.name() == "duration" && f.data_type() == &DataType::UInt64)
}

/// `schema` with each counting histogram column (a `List<UInt64>` with a
/// histogram configuration) whose `metric_type` is `histogram` or absent
/// named `counter_histogram`. Readers take the three alike; this is what
/// a segment written now says.
fn counting_histograms_named(schema: Arc<Schema>) -> Arc<Schema> {
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|f| {
            let list_of_u64 =
                matches!(f.data_type(), DataType::List(inner) if inner.data_type() == &DataType::UInt64);
            let md = f.metadata();
            let configured =
                md.contains_key("grouping_power") && md.contains_key("max_value_power");
            let legacy = match md.get(metriken_model::METRIC_TYPE) {
                None => true,
                Some(t) => t == metriken_model::LEGACY_HISTOGRAM,
            };
            if list_of_u64 && configured && legacy {
                let mut md: HashMap<String, String> = md.clone().into();
                md.insert(
                    metriken_model::METRIC_TYPE.to_string(),
                    metriken_model::COUNTER_HISTOGRAM.to_string(),
                );
                f.as_ref().clone().with_metadata(md)
            } else {
                f.as_ref().clone()
            }
        })
        .collect();
    // Without the schema-level metadata, which the arrow reader fills from
    // the file's key-value metadata: the segment's own keys are written as
    // key-value metadata, and a file's (its `descriptions` and
    // `systeminfo`) are the source's.
    Arc::new(Schema::new(fields))
}

/// The first and last of a segment's timestamps, as dendro's `i64`.
fn span(column: &dyn Array) -> Result<(i64, i64), Error> {
    let ts = column
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or("the `timestamp` column is not UInt64")?;
    if ts.null_count() > 0 {
        return Err("the `timestamp` column holds a null".into());
    }
    let (Some(min), Some(max)) = (arrow::compute::min(ts), arrow::compute::max(ts)) else {
        return Err("a segment holds no rows".into());
    };
    let fit = |v: u64| {
        i64::try_from(v).map_err(|_| {
            format!(
                "timestamp {v} is above i64::MAX; dendro stores timestamps as i64 and a \
                 cast would wrap it negative"
            )
        })
    };
    Ok((fit(min)?, fit(max)?))
}

/// The span and row count of a whole file, from its `timestamp` column.
#[cfg_attr(not(feature = "rez"), allow(dead_code))]
fn span_of_file<R: ChunkReader + 'static>(
    builder: ParquetRecordBatchReaderBuilder<R>,
) -> Result<(i64, i64, u64), Error> {
    let column = builder
        .schema()
        .fields()
        .iter()
        .position(|f| f.name() == "timestamp")
        .ok_or("the segment has no `timestamp` column")?;
    let mask = parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), [column]);
    let reader = builder.with_projection(mask).build()?;
    let (mut lo, mut hi, mut rows) = (i64::MAX, i64::MIN, 0u64);
    for batch in reader {
        let batch = batch?;
        if batch.num_rows() == 0 {
            continue;
        }
        let (a, b) = span(batch.column(0).as_ref())?;
        lo = lo.min(a);
        hi = hi.max(b);
        rows += batch.num_rows() as u64;
    }
    Ok((lo, hi, rows))
}
