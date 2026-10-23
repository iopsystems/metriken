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
//! - [`convert_path`]: any of the three, chosen by the input's content.
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
//!   metadata (its labels). Two things are added or renamed: a
//!   counting histogram column whose `metric_type` is
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
//!   `max_bytes` metered as the input's average encoded bytes per row,
//!   where the recorder meters resident bytes; see
//!   `TableWriter::push_file`), never
//!   merged with another
//!   input file. `max_age` bounds what a crash loses while recording and
//!   does not apply. A long segment is refused, since its occupant list
//!   indexes its rows; no tar `.rez` holds one.
//!   An archive's reader takes columns of a table that share one label set
//!   as one series; split into segments, which of them that series reads
//!   can change at a segment boundary. Only an input whose producer dropped
//!   a distinguishing label has such columns.
//! - **The interval the input was read at is declared.** A parquet file's
//!   `sampling_interval_ms` is kept on every segment (1000 when it has
//!   none, which is what its reader takes then), and a tar table's
//!   segments are given the interval the tar's reader reads it at (the
//!   first segment's declared one, else the median gap of its first rows,
//!   in whole milliseconds; a table of fewer than two rows declares none,
//!   and both readers take 1 s for it). An archive's reader takes a declared interval
//!   over the one it measures from the rows, which differs on a recording
//!   with dropped or jittered rows and moves gauge staleness and instant
//!   `irate`.
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
//! - **A table with no rows is not written.** A tar table whose segments
//!   hold no rows has no segment to write (a dendro stream exists through
//!   its segments and WAL rows), so its metric names, which the tar's reader
//!   lists from the empty footer, are not listed by the archive's.
//! - **An existing output is refused** and left as it is, and so is an
//!   output beside which a `-wal`, `-shm` or `-journal` file exists, since
//!   SQLite would read a stale one into the new archive. On an error the
//!   archive and its sidecars are removed.
//!
//! The functions taking an [`ArchiveMut`] write into an archive the caller
//! made, which may be [`ArchiveMut::create_in_memory`] where there is no
//! filesystem (the browser); none of them spawns a thread. On an error they
//! leave what they wrote before it in that archive, a partial source
//! included; the path forms remove the archive then.

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

/// The footer key a standalone parquet file declares its interval under,
/// in milliseconds.
const SAMPLING_INTERVAL_KEY: &str = "sampling_interval_ms";

/// The interval the parquet reader takes for a file that declares none
/// (`parquet::parse_sampling_interval`).
const PARQUET_DEFAULT_INTERVAL_MS: u64 = 1000;

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

    /// Convert the parquet file at `input` into a new archive at `output`.
    /// An `output` that exists, or has a `-wal`, `-shm` or `-journal` file
    /// beside it, is refused. On an error the archive and its sidecars are
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
    ///
    /// On an error the source and the segments written before it stay in
    /// `out`; [`convert_file_path`](Self::convert_file_path) removes the
    /// archive then, and a caller writing into its own archive discards it.
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
        // The interval the parquet reader reads the file at, declared on
        // each segment so an archive's reader reads at it too: the file's
        // `sampling_interval_ms`, or the 1000 ms that reader takes when the
        // file has none.
        let segment_kv = vec![KeyValue::new(
            SAMPLING_INTERVAL_KEY.to_string(),
            kv.iter()
                .find(|e| e.key == SAMPLING_INTERVAL_KEY)
                .and_then(|e| e.value.clone())
                .unwrap_or_else(|| PARQUET_DEFAULT_INTERVAL_MS.to_string()),
        )];

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

/// What [`convert_path`] found its input to be, and what converting it
/// wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConvertedFrom {
    /// A v3 (SQLite) `.rez`, converted by
    /// `rez::to_dendro::convert_v3_to_dendro`.
    #[cfg(feature = "rez")]
    RezV3(crate::rez::to_dendro::Converted),
    /// A v1 or v2 tar `.rez`, converted by
    /// `rez::to_dendro::convert_tar_to_dendro`.
    #[cfg(feature = "rez")]
    RezTar(Written),
    /// A standalone parquet file, converted by [`ParquetToArchive`] with no
    /// labels.
    Parquet(Written),
}

/// Convert the recording at `input` into a new dendro archive at `output`,
/// choosing the converter by the input's content, not its name: a parquet
/// file (`PAR1`), a v3 `.rez` (SQLite) or a v1/v2 `.rez` (a tar holding
/// `manifest.json`). A dendro archive is refused as one already, and so is
/// anything else. The `.rez` arms need the `rez` feature; without it a
/// `.rez` is refused, naming the feature. Errors name `input`.
///
/// `output` is treated as the converters treat it: one that exists, or has
/// a sidecar beside it, is refused and never touched, and on an error the
/// archive written beside it is removed.
pub fn convert_path(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<ConvertedFrom, Error> {
    let (input, output) = (input.as_ref(), output.as_ref());
    let named = |e: &dyn std::fmt::Display| -> Error { format!("{}: {e}", input.display()).into() };
    if crate::DendroCatalog::is_archive(input).map_err(|e| named(&e))? {
        return Err(named(&"already a dendro archive"));
    }
    let head = {
        use std::io::Read as _;
        let file = std::fs::File::open(input).map_err(|e| named(&e))?;
        let mut head = Vec::with_capacity(512);
        file.take(512)
            .read_to_end(&mut head)
            .map_err(|e| named(&e))?;
        head
    };
    if head.starts_with(b"PAR1") {
        return ParquetToArchive::new()
            .convert_file_path(input, output)
            .map(ConvertedFrom::Parquet)
            .map_err(|e| named(&e));
    }
    convert_rez(input, output, &head).map_err(|e| named(&e))
}

/// Whether `head`, a file's first bytes, could begin a `.rez`, for a build
/// without the `rez` feature to say which feature a file needs: SQLite's
/// magic (v3) or a tar header's (v1, v2).
#[cfg_attr(feature = "rez", allow(dead_code))]
fn looks_like_rez(head: &[u8]) -> bool {
    head.starts_with(b"SQLite format 3\0") || head.get(257..262) == Some(b"ustar")
}

/// The message for a file that is none of the formats [`convert_path`]
/// converts.
const NOT_A_RECORDING: &str =
    "not a recording metriken reads: not parquet, a .rez or a dendro archive";

#[cfg(feature = "rez")]
fn convert_rez(input: &Path, output: &Path, _: &[u8]) -> Result<ConvertedFrom, Error> {
    use crate::rez::rez::{detect_rez_format, RezFormat};
    match detect_rez_format(input).map_err(|e| e.to_string())? {
        RezFormat::V3Sqlite => Ok(ConvertedFrom::RezV3(staged(output, |staging| {
            crate::rez::to_dendro::convert_v3_to_dendro(input, staging)
        })?)),
        RezFormat::V2Tar => Ok(ConvertedFrom::RezTar(
            crate::rez::to_dendro::convert_tar_to_dendro(input, output)?,
        )),
        RezFormat::NotRez => Err(NOT_A_RECORDING.into()),
    }
}

#[cfg(not(feature = "rez"))]
fn convert_rez(_: &Path, _: &Path, head: &[u8]) -> Result<ConvertedFrom, Error> {
    if looks_like_rez(head) {
        Err("converting a .rez needs metriken-storage's `rez` feature".into())
    } else {
        Err(NOT_A_RECORDING.into())
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

/// The paths an archive at `output` occupies: the archive and SQLite's
/// `-wal`, `-shm` and `-journal` sidecars beside it.
fn archive_paths(output: &Path) -> [PathBuf; 4] {
    let with = |suffix: &str| {
        let mut path = output.as_os_str().to_owned();
        path.push(suffix);
        PathBuf::from(path)
    };
    [
        output.to_path_buf(),
        with("-wal"),
        with("-shm"),
        with("-journal"),
    ]
}

/// Refuse an `output` that exists, or beside which a sidecar exists: SQLite
/// would replay a stale `-wal` or `-journal` into the new archive, and
/// deletes a `-shm` it finds.
pub(crate) fn refuse_existing(output: &Path) -> Result<(), String> {
    match archive_paths(output).into_iter().find(|p| p.exists()) {
        Some(p) => Err(format!("{} already exists", p.display())),
        None => Ok(()),
    }
}

/// Remove the archive at `output` and its sidecars.
pub(crate) fn remove_archive(output: &Path) {
    for path in archive_paths(output) {
        let _ = std::fs::remove_file(path);
    }
}

/// Write an archive at a staging path beside `output` with `f`, and move it
/// to `output` when `f` succeeds. An `output` that exists, or has a sidecar
/// beside it, is refused before anything is written, and the move does not
/// replace an `output` that appeared since: a file at `output` is never
/// removed or overwritten. On an error the staged archive and its sidecars
/// are removed.
pub(crate) fn staged<T, E: From<String>>(
    output: &Path,
    f: impl FnOnce(&Path) -> Result<T, E>,
) -> Result<T, E> {
    refuse_existing(output).map_err(E::from)?;
    let staging = staging_path(output).map_err(E::from)?;
    let result = f(&staging);
    let result = result.and_then(|v| publish(&staging, output).map(|()| v).map_err(E::from));
    remove_archive(&staging);
    result
}

/// A path beside `output` that no archive or sidecar occupies.
fn staging_path(output: &Path) -> Result<PathBuf, String> {
    let name = output
        .file_name()
        .ok_or_else(|| format!("{} names no file", output.display()))?
        .to_string_lossy()
        .into_owned();
    for n in 0..1000u32 {
        let candidate =
            output.with_file_name(format!(".{name}.{}.{n}.partial", std::process::id()));
        if refuse_existing(&candidate).is_ok() {
            return Ok(candidate);
        }
    }
    Err(format!("no free staging path beside {}", output.display()))
}

/// Give the finished archive at `staging` the name `output`, failing if
/// `output` exists. A hard link does that atomically; a filesystem without
/// hard links gets a rename after a check.
fn publish(staging: &Path, output: &Path) -> Result<(), String> {
    match std::fs::hard_link(staging, output) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(format!("{} already exists", output.display()))
        }
        Err(_) if !output.exists() => std::fs::rename(staging, output)
            .map_err(|e| format!("moving the archive to {}: {e}", output.display())),
        Err(e) => Err(format!("moving the archive to {}: {e}", output.display())),
    }
}

/// Create an archive with [`staged`], run `f` on it, and close it before it
/// is moved to `output`.
pub(crate) fn with_new_archive<T, E: From<String>>(
    output: &Path,
    f: impl FnOnce(&mut ArchiveMut) -> Result<T, E>,
) -> Result<T, E> {
    staged(output, |staging| {
        let mut out = ArchiveMut::create(staging).map_err(|e| E::from(e.to_string()))?;
        let result = f(&mut out);
        drop(out);
        result
    })
}

/// One encoded segment, ready to insert.
pub(crate) struct EncodedSegment {
    pub(crate) meta: SegmentMeta,
    pub(crate) bytes: Vec<u8>,
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

    /// An input segment from a container, re-encoded keeping its own
    /// key-value metadata, with `sampling_interval_ms` set to
    /// `interval_ms` when that is given. A long segment is refused: its
    /// occupant list indexes its rows, so it cannot be split, and no tar
    /// `.rez` holds one.
    #[cfg_attr(not(feature = "rez"), allow(dead_code))]
    pub(crate) fn push_segment(
        &mut self,
        bytes: Vec<u8>,
        interval_ms: Option<u64>,
    ) -> Result<(), String> {
        let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes))
            .map_err(|e| format!("table {}: {e}", self.table))?;
        let mut kv: Vec<KeyValue> = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| is_recording_key(&e.key))
            .collect();
        if kv.iter().any(|e| e.key == crate::long::LAYOUT_KEY) {
            return Err(format!(
                "table {} has a long segment, which this conversion does not split",
                self.table
            ));
        }
        if let Some(ms) = interval_ms {
            kv.retain(|e| e.key != SAMPLING_INTERVAL_KEY);
            kv.push(KeyValue::new(
                SAMPLING_INTERVAL_KEY.to_string(),
                ms.to_string(),
            ));
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
        // Decode about 1 MiB at a time: a batch's rows are held decoded, and
        // on a table thousands of columns wide (histograms among them) 1024
        // rows of it run to gigabytes.
        let (decoded, input_rows) = builder
            .metadata()
            .row_groups()
            .iter()
            .fold((0i64, 0i64), |(b, r), g| {
                (b + g.total_byte_size(), r + g.num_rows())
            });
        let row_width = (decoded.max(0) as usize / input_rows.max(1) as usize).max(1);
        let batch_rows = (BATCH_BYTES / row_width).clamp(1, 1024);
        // The byte cap is metered as the input file's encoded bytes per row
        // (its row groups' compressed size over its rows) times the rows in
        // the open segment. It is an average, so a segment of rows denser
        // than the file's average overshoots the cap in proportion. The
        // `ArrowWriter`'s own estimate of the open segment
        // (`in_progress_size`) counts the open page before compression and
        // cut the recordings rezolus publishes into 3 to 50 segments where
        // this cuts 1 or 2.
        //
        // The recorder's writer meters differently, on purpose: it bounds
        // what a live segment holds resident before it is sealed (a cell per
        // value, plus a histogram's buckets: `builder::cells_approx_bytes`).
        // This is a one-shot copy with nothing resident to bound, and each
        // segment carries its own arrow schema in its footer (about 1.5 MB
        // at 5000 columns), so cutting where the writer would made a wide
        // recording 10 to 48 times larger and its queries about 70 times
        // slower.
        let (encoded, input_rows) = builder
            .metadata()
            .row_groups()
            .iter()
            .fold((0i64, 0i64), |(b, r), g| {
                (b + g.compressed_size(), r + g.num_rows())
            });
        let per_row = (encoded.max(0) as usize / input_rows.max(1) as usize).max(1);
        let reader = builder.with_batch_size(batch_rows).build()?;
        let mut open: Option<OpenSegment> = None;
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
            let mut rest = batch;
            while rest.num_rows() > 0 {
                let segment = match &mut open {
                    Some(segment) => segment,
                    None => open.insert(OpenSegment::new(&schema, &props)?),
                };
                // As many rows as both caps leave room for, and at least one.
                let byte_room =
                    (self.max_bytes.saturating_sub(segment.rows * per_row) / per_row).max(1);
                let take = (self.max_rows - segment.rows)
                    .min(byte_room)
                    .min(rest.num_rows());
                segment.write(&rest.slice(0, take), ts_col)?;
                segment.rows += take;
                rest = rest.slice(take, rest.num_rows() - take);
                if segment.rows >= self.max_rows || segment.rows * per_row >= self.max_bytes {
                    let segment = open.take().expect("written to above");
                    (self.sink)(segment.finish()?)?;
                }
            }
        }
        if let Some(segment) = open {
            (self.sink)(segment.finish()?)?;
        }
        Ok(())
    }
}

/// How many decoded bytes of an input are read at a time.
const BATCH_BYTES: usize = 1 << 20;

/// A segment being written: rows are encoded into it as they arrive.
struct OpenSegment {
    writer: ArrowWriter<Vec<u8>>,
    rows: usize,
    span: Option<(i64, i64)>,
}

impl OpenSegment {
    fn new(
        schema: &Arc<Schema>,
        props: &parquet::file::properties::WriterProperties,
    ) -> Result<Self, Error> {
        Ok(Self {
            writer: ArrowWriter::try_new(Vec::new(), Arc::clone(schema), Some(props.clone()))?,
            rows: 0,
            span: None,
        })
    }

    fn write(&mut self, rows: &RecordBatch, ts_col: usize) -> Result<(), Error> {
        let (lo, hi) = span(rows.column(ts_col).as_ref())?;
        self.span = Some(match self.span {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        });
        self.writer.write(rows)?;
        Ok(())
    }

    fn finish(self) -> Result<EncodedSegment, Error> {
        let (first_ts, last_ts) = self.span.ok_or("a segment holds no rows")?;
        Ok(EncodedSegment {
            meta: SegmentMeta {
                rows: self.rows as u64,
                first_ts,
                last_ts,
            },
            bytes: self.writer.into_inner()?,
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

/// A segment's row timestamps in row order, from its `timestamp` column
/// alone.
#[cfg_attr(not(feature = "rez"), allow(dead_code))]
pub(crate) fn segment_timestamps(bytes: &[u8]) -> Result<Vec<u64>, Error> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(bytes))?;
    let column = builder
        .schema()
        .fields()
        .iter()
        .position(|f| f.name() == "timestamp" && f.data_type() == &DataType::UInt64)
        .ok_or("the segment has no UInt64 `timestamp` column")?;
    let mask = parquet::arrow::ProjectionMask::roots(builder.parquet_schema(), [column]);
    let mut out = Vec::new();
    for batch in builder.with_projection(mask).build()? {
        let batch = batch?;
        let ts = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or("the `timestamp` column is not UInt64")?;
        out.extend(ts.iter().flatten());
    }
    Ok(out)
}

/// The interval, in whole milliseconds, a reader of a table held in memory
/// as `segments` reads it at (`ArchiveReader::from_in_memory`, which reads a
/// tar `.rez`): the first segment's `sampling_interval_ms` when it declares
/// one, else the median gap between the rows of the first segments that
/// hold two rows between them, rounded to the millisecond. `None` when
/// there are fewer than two rows, where that reader takes 1 s, as an
/// archive's reader does for a table it cannot measure.
#[cfg_attr(not(feature = "rez"), allow(dead_code))]
pub(crate) fn in_memory_interval_ms(segments: &[Vec<u8>]) -> Result<Option<u64>, Error> {
    let Some(first) = segments.first() else {
        return Ok(None);
    };
    let kv = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(first))?
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .cloned()
        .unwrap_or_default();
    if let Some(v) = kv.iter().find(|e| e.key == SAMPLING_INTERVAL_KEY) {
        return Ok(v.value.as_deref().and_then(|v| v.parse().ok()));
    }
    let mut timestamps = Vec::new();
    for segment in segments {
        if timestamps.len() >= 2 {
            break;
        }
        timestamps.extend(segment_timestamps(segment)?);
    }
    let mut gaps: Vec<u64> = timestamps
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .collect();
    gaps.sort_unstable();
    Ok(match gaps.get(gaps.len() / 2) {
        Some(&gap) if gap > 0 => Some(((gap as f64 / 1e6).round() as u64).max(1)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_never_replaces_an_output() {
        let dir = tempfile::tempdir().unwrap();
        let (staging, output) = (dir.path().join("s"), dir.path().join("o"));
        std::fs::write(&staging, b"new").unwrap();
        std::fs::write(&output, b"theirs").unwrap();
        let err = publish(&staging, &output).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(std::fs::read(&output).unwrap(), b"theirs");
        std::fs::remove_file(&output).unwrap();
        publish(&staging, &output).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"new");
    }

    /// A failed conversion leaves nothing at the output or the staging path.
    #[test]
    fn a_failed_staged_write_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("o.dendro");
        let err = with_new_archive::<(), String>(&output, |_| Err("boom".to_string()));
        assert_eq!(err.unwrap_err(), "boom");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_rez_is_told_by_its_magic() {
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert!(looks_like_rez(&tar));
        assert!(looks_like_rez(b"SQLite format 3\0and the rest"));
        assert!(!looks_like_rez(b"PAR1"));
        assert!(!looks_like_rez(b"neither"));
    }
}
