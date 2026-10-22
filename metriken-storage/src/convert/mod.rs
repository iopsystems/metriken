//! Msgpack snapshots into a `.dendro` archive (feature `convert`).
//!
//! [`MsgpackToArchive`] reads a file of concatenated msgpack
//! [`Snapshot`]s (a raw recording) and writes them to a new archive as one
//! source, each snapshot a tick at its own timestamp, through the same
//! [`ArchiveWriter`] a recorder uses.
//!
//! This replaced `MsgpackToParquet`, which wrote one standalone parquet file.
//! metriken writes no standalone parquet. A standalone file is opened by
//! readers that do not check its `metriken.format` key (pyarrow, DuckDB, a
//! script over its columns), so nothing stops one of them reading a new
//! column kind (a gauge histogram) as the kind it knows. Older metriken
//! readers refuse an archive: every reader before metriken-storage 0.4.0
//! (metriken-storage 0.1.0 to 0.3.2, and rezolus 5.25 to 5.28 on
//! metriken-archive 0.2.7) runs on a dendro up to 0.5.1, which opens only
//! schema 4, and dendro 0.6 writes schema 5, so they refuse every archive
//! this writes when opening it. metriken-storage 0.4.0 opens it and refuses
//! a format 2 segment (a gauge histogram) by its `metriken.format` key.
//! Parquet files written before keep reading (`parquet`, `MultiParquetSource`,
//! and metriken-query's `ParquetReader`).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::time::UNIX_EPOCH;

use metriken_model::Snapshot;

use crate::process::{PRODUCER_EPOCH, PRODUCER_EPOCHS};

/// The metadata key naming the producer's version (dendro's `keys::VERSION`).
const VERSION: &str = "version";
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
    /// archive at `output`.
    ///
    /// The source's metadata is the first snapshot metadata that is not
    /// empty (the producer's `version`, `sampling_interval_ms`,
    /// `producer_epoch` and the like), with the entries given to
    /// [`metadata`](Self::metadata) replacing those of the same key.
    ///
    /// A recording that spans a producer restart (a snapshot whose
    /// `producer_epoch` differs from the one before it) gets
    /// `producer_epochs`, one `{"epoch", "from_ts"}` entry per epoch (with
    /// the snapshot's `version` when it carries one), oldest first, the
    /// first from the first row; `producer_epoch` names the newest, and
    /// `version` becomes the newest process's, as metriken-recorder writes
    /// them. A caller that sets `producer_epoch` or `producer_epochs` through
    /// [`metadata`](Self::metadata) keeps its own and no list is written; a
    /// caller that sets `version` keeps it, and no entry carries a version.
    ///
    /// An input with no snapshot is an error, and no archive is created. A
    /// snapshot that does not decode, or a write that fails, ends the
    /// conversion with an error naming the snapshot's position, and the
    /// files the conversion created are removed: the archive and any of its
    /// `-wal`, `-shm` and `-journal` sidecars that did not exist when it was
    /// created. A file that was there before is left alone.
    pub fn convert_reader(self, reader: impl Read, output: impl AsRef<Path>) -> Result<u64, Error> {
        let output = output.as_ref();
        let mut created = Vec::new();
        let result = self.convert_into(reader, output, &mut created);
        if result.is_err() {
            for path in created {
                let _ = std::fs::remove_file(path);
            }
        }
        result
    }

    /// `created` receives the files the conversion owns, once
    /// [`ArchiveWriter::create`] has succeeded: the archive, and the
    /// sidecar paths that did not exist just before it.
    fn convert_into(
        self,
        reader: impl Read,
        output: &Path,
        created: &mut Vec<std::path::PathBuf>,
    ) -> Result<u64, Error> {
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
        let sidecars: Vec<std::path::PathBuf> = ["-wal", "-shm", "-journal"]
            .iter()
            .map(|suffix| {
                let mut path = output.as_os_str().to_owned();
                path.push(suffix);
                std::path::PathBuf::from(path)
            })
            .filter(|path| !path.exists())
            .collect();
        let mut writer = ArchiveWriter::create(output, self.config)?;
        created.push(output.to_path_buf());
        created.extend(sidecars);
        let first_ts = ts(&first)?;
        let mut source = writer.add_source(self.labels, self.metadata.clone(), first_ts)?;
        let track_epochs = !self.metadata.contains_key(PRODUCER_EPOCH)
            && !self.metadata.contains_key(PRODUCER_EPOCHS);
        let version_pinned = self.metadata.contains_key(VERSION);
        // Every epoch seen, oldest first, with its first row and version.
        let mut epochs: Vec<(String, u64, Option<String>)> = Vec::new();
        let mut carried = false;
        let mut written = 0u64;
        let mut last = 0u64;
        let mut snapshot = Some(first);
        while let Some(s) = snapshot {
            let found = snapshot_metadata(&s);
            if !carried && !found.is_empty() {
                let patch: BTreeMap<String, String> = found
                    .iter()
                    .filter(|(k, _)| !self.metadata.contains_key(*k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if !patch.is_empty() {
                    source.update_metadata(patch)?;
                }
                carried = true;
            }
            let at = ts(&s)?;
            if let Some(epoch) = found
                .get(PRODUCER_EPOCH)
                .filter(|e| track_epochs && !e.is_empty())
            {
                if epochs.last().is_none_or(|(last, _, _)| last != epoch) {
                    let from_ts = if epochs.is_empty() { first_ts } else { at };
                    // A `version` the caller set is its label, not what the
                    // process reported, so no entry carries one then.
                    let version = (!version_pinned)
                        .then(|| found.get(VERSION).cloned())
                        .flatten();
                    epochs.push((epoch.clone(), from_ts, version.clone()));
                    if epochs.len() > 1 {
                        let mut patch = epoch_patch(&epochs);
                        if let Some(version) = version {
                            patch.insert(VERSION.to_string(), version);
                        }
                        source.update_metadata(patch)?;
                    }
                }
            }
            last = last.max(at);
            let staged = source
                .stage(&s, at, 0)
                .map_err(|e| format!("snapshot {written}: {e}"))?;
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

/// `producer_epochs` for `epochs` and `producer_epoch` naming the newest.
fn epoch_patch(epochs: &[(String, u64, Option<String>)]) -> BTreeMap<String, String> {
    let list: Vec<serde_json::Value> = epochs
        .iter()
        .map(|(epoch, from_ts, version)| {
            let mut e = serde_json::json!({ "epoch": epoch, "from_ts": from_ts });
            if let Some(version) = version {
                e["version"] = serde_json::Value::String(version.clone());
            }
            e
        })
        .collect();
    let newest = &epochs.last().expect("a patch names at least one epoch").0;
    BTreeMap::from([
        (
            PRODUCER_EPOCHS.to_string(),
            serde_json::Value::Array(list).to_string(),
        ),
        (PRODUCER_EPOCH.to_string(), newest.clone()),
    ])
}

/// A snapshot's metadata, any version.
fn snapshot_metadata(s: &Snapshot) -> &std::collections::HashMap<String, String> {
    match s {
        Snapshot::V1(s) => &s.metadata,
        Snapshot::V2(s) => &s.metadata,
        Snapshot::V3(s) => &s.metadata,
    }
}
