//! Msgpack snapshots into a `.dendro` archive (feature `convert`).
//!
//! [`MsgpackToArchive`] reads a file of concatenated msgpack
//! [`Snapshot`]s (a raw recording) and writes them to a new archive as one
//! source, each snapshot a tick at its own timestamp, through the same
//! [`ArchiveWriter`] a recorder uses.
//!
//! This replaced `MsgpackToParquet`, which wrote one standalone parquet file.
//! metriken writes no standalone parquet: a parquet file carries no format
//! version a reader checks, so a reader from before a new column kind (a
//! gauge histogram) would read the new kind with nothing to stop it. An
//! archive's segments carry `metriken.format`, which those readers check.
//! Parquet files written before keep reading (`parquet`, `MultiParquetSource`,
//! and metriken-query's `ParquetReader`).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::time::UNIX_EPOCH;

use metriken_model::Snapshot;

use crate::{ArchiveWriter, WriterConfig};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Converts a raw msgpack recording into a `.dendro` archive.
///
/// ```no_run
/// use metriken_storage::convert::MsgpackToArchive;
///
/// let ticks = MsgpackToArchive::new()
///     .label("source", "rezolus")
///     .metadata("host", "web-01")
///     .convert_file_path("rezolus.raw", "rezolus.dendro")
///     .unwrap();
/// println!("{ticks} snapshots");
/// ```
#[derive(Default)]
pub struct MsgpackToArchive {
    labels: BTreeMap<String, String>,
    metadata: BTreeMap<String, String>,
    config: WriterConfig,
}

impl MsgpackToArchive {
    /// A converter writing one source with no labels or metadata, under the
    /// default [`WriterConfig`].
    pub fn new() -> Self {
        Self::default()
    }

    /// A label of the archive's source (`source`, `host`, ...).
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// A metadata entry of the archive's source.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// The writer's configuration (seal policy, compression, long groups).
    pub fn writer_config(mut self, config: WriterConfig) -> Self {
        self.config = config;
        self
    }

    /// Convert the msgpack recording at `input` into a new archive at
    /// `output`, which must not exist. Returns the number of snapshots
    /// written.
    pub fn convert_file_path(
        self,
        input: impl AsRef<Path>,
        output: impl AsRef<Path>,
    ) -> Result<u64, Error> {
        let input = input.as_ref();
        let file =
            std::fs::File::open(input).map_err(|e| format!("opening {}: {e}", input.display()))?;
        self.convert_reader(file, output)
    }

    /// Convert concatenated msgpack snapshots read from `reader` into a new
    /// archive at `output`. A snapshot that does not decode ends the
    /// conversion with an error naming its position; an input with no
    /// snapshot is an error, and no archive is created.
    pub fn convert_reader(self, reader: impl Read, output: impl AsRef<Path>) -> Result<u64, Error> {
        let mut reader = BufReader::new(reader);
        let mut next = |index: u64| -> Result<Option<Snapshot>, Error> {
            if reader.fill_buf()?.is_empty() {
                return Ok(None);
            }
            rmp_serde::from_read(&mut reader)
                .map(Some)
                .map_err(|e| format!("snapshot {index}: {e}").into())
        };
        let Some(first) = next(0)? else {
            return Err("the input holds no snapshot".into());
        };
        let ts = |s: &Snapshot| -> Result<u64, Error> {
            Ok(s.systemtime().duration_since(UNIX_EPOCH)?.as_nanos() as u64)
        };
        let mut writer = ArchiveWriter::create(output.as_ref(), self.config)?;
        let mut source = writer.add_source(self.labels, self.metadata, ts(&first)?)?;
        let mut written = 0u64;
        let mut last = 0u64;
        let mut snapshot = Some(first);
        while let Some(s) = snapshot {
            let at = ts(&s)?;
            last = last.max(at);
            let staged = source.stage(&s, at, 0)?;
            writer.commit(vec![staged])?;
            source.maybe_seal()?;
            written += 1;
            snapshot = next(written)?;
        }
        source.finalize((last, 0))?;
        writer.join()?;
        Ok(written)
    }
}
