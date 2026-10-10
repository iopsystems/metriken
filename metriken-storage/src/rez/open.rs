//! Opening a `.rez` or dendro archive by content, as one
//! [`ArchiveReader`] per recording or flattened into one.
//!
//! A v3 `.rez` and a dendro archive are catalogs and open lazily; a v1/v2 tar
//! `.rez` is read eagerly into memory. A 5.x `record --stream` `.rez`'s
//! identity index (its caller rows) is not read: its slot columns carry the
//! labels each segment was built with.

use std::path::Path;
use std::sync::Arc;

use crate::rez::catalog::Container;
use crate::rez::rez::{self, RecordingBytes};
use crate::rez::rez_sqlite::RezDb;
use crate::rez::wal::materialize_wal_tail;
use crate::{ArchiveReader, BufferPool, InMemorySource, LabeledRecordings};

/// Open the archive at `path`, flattening every recording into one reader.
/// A query over it that names one sampler in two recordings is refused by
/// the router, so prefer [`open_recordings`].
pub fn open_with_pool(
    path: &Path,
    pool: Arc<BufferPool>,
) -> Result<ArchiveReader, Box<dyn std::error::Error>> {
    let filename = path.file_name().map(|s| s.to_string_lossy().into_owned());
    if let Some(opened) = from_catalog_path(path, Arc::clone(&pool))? {
        return Ok(ArchiveReader::flatten(
            opened.into_iter().map(|(_, r)| r).collect(),
            filename,
        ));
    }
    let recordings = read_recordings(path)?;
    ArchiveReader::from_in_memory(
        recordings.into_iter().map(in_memory).collect(),
        filename,
        pool,
    )
}

/// [`open_recordings`] for an archive that exists only as bytes. Containers
/// are recognized by content, as for a path.
pub fn open_recordings_from_bytes(
    bytes: Vec<u8>,
    pool: Arc<BufferPool>,
) -> Result<LabeledRecordings, Box<dyn std::error::Error>> {
    for container in [Container::Dendro, Container::Rez] {
        if container.recognizes_bytes(&bytes) {
            return ArchiveReader::from_catalog(container.open_bytes(bytes)?, None, pool, None);
        }
    }
    let recordings = rez::read_archive_reader(std::io::Cursor::new(bytes))?.1;
    let mut out = Vec::with_capacity(recordings.len());
    for rec in recordings {
        let labels = rec.labels.clone();
        let filename = Some(rec.dir.clone());
        let reader =
            ArchiveReader::from_in_memory(vec![in_memory(rec)], filename, Arc::clone(&pool))?;
        out.push((labels, reader));
    }
    Ok(out)
}

/// Open the archive at `path` as one reader per recording, paired with that
/// recording's labels.
pub fn open_recordings(
    path: &Path,
    pool: Arc<BufferPool>,
) -> Result<LabeledRecordings, Box<dyn std::error::Error>> {
    if let Some(opened) = from_catalog_path(path, Arc::clone(&pool))? {
        return Ok(opened);
    }
    let recordings = read_recordings(path)?;
    let mut out = Vec::with_capacity(recordings.len());
    for rec in recordings {
        let labels = rec.labels.clone();
        let filename = Some(rec.dir.clone());
        let reader =
            ArchiveReader::from_in_memory(vec![in_memory(rec)], filename, Arc::clone(&pool))?;
        out.push((labels, reader));
    }
    Ok(out)
}

/// Open `path` as one reader per recording when it is one of the catalog
/// containers (a dendro archive or a `.rez` v3), decided by content; `None`
/// for a v1/v2 tar `.rez`, which the callers read eagerly.
fn from_catalog_path(
    path: &Path,
    pool: Arc<BufferPool>,
) -> Result<Option<LabeledRecordings>, Box<dyn std::error::Error>> {
    let Some(container) = Container::of_path(path)? else {
        return Ok(None);
    };
    // The reopen notes the file first, so a file swapped in before the
    // catalog open below is refused on a table's first read.
    let reopen = container.reopen(path);
    Ok(Some(ArchiveReader::from_catalog(
        container.open(path)?,
        Some(reopen),
        pool,
        None,
    )?))
}

fn in_memory(rec: RecordingBytes) -> InMemorySource {
    InMemorySource {
        name: rec.dir,
        labels: rec.labels,
        metadata: rec.metadata,
        complete: rec.complete,
        tables: rec.tables,
    }
}

/// Read either container into the one shape the reader consumes: per
/// recording, `(sampler, segments-newest-last)`.
///
/// Dispatch is by CONTENT (`detect_rez_format`), not by extension, and the
/// non-v3 arm deliberately falls through to `read_archive_bytes` unchanged —
/// including for `NotRez`, so a caller handed something that is not a `.rez`
/// at all keeps getting the tar reader's own error rather than a new one.
fn read_recordings(path: &Path) -> Result<Vec<RecordingBytes>, Box<dyn std::error::Error>> {
    match rez::detect_rez_format(path)? {
        rez::RezFormat::V3Sqlite => read_v3_recordings(path),
        rez::RezFormat::V2Tar | rez::RezFormat::NotRez => Ok(rez::read_archive_bytes(path)?.1),
    }
}

/// Resolve a v3 (SQLite) `.rez` into the same `RecordingBytes` the tar reader
/// produces, so everything downstream is container-agnostic.
///
/// Two things differ from a mechanical transcription of the catalog:
///
/// * Tables are enumerated with `all_samplers`, NOT `samplers`. The latter
///   sees only `segments`, so a table still inside its first seal period —
///   16 of 26 in the fleet measurement that motivated this container — would
///   be invisible, which is precisely the data v3 exists to keep.
/// * Each table's live WAL tail is materialized into an in-memory parquet
///   segment and appended as the NEWEST segment. `live_wal`'s watermark
///   (`ts > MAX(last_ts)` of that sampler's own segments) is what guarantees
///   the seam has no duplicate row, so nothing here has to de-duplicate.
fn read_v3_recordings(path: &Path) -> Result<Vec<RecordingBytes>, Box<dyn std::error::Error>> {
    let db = RezDb::open(path)?;
    let mut out = Vec::new();
    for rec in db.read_recordings()? {
        let mut tables = Vec::new();
        for sampler in db.all_samplers(rec.id)? {
            let segments = table_segments(&db, rec.id, &sampler)?;
            // Only reachable if a sampler's every WAL row was pruned without
            // its segment landing — which the seal ordering rules out. A table
            // with no bytes has nothing to open, so skip rather than hand the
            // reader an empty segment list.
            if segments.is_empty() {
                continue;
            }
            tables.push((sampler, segments));
        }
        out.push(RecordingBytes {
            // v3 has no tar directory. `dir` survives only as a display name,
            // and this is the function that produced it in the first place.
            dir: rez::recording_dir_slug(&rec.meta.labels),
            labels: rec.meta.labels,
            metadata: rec.meta.metadata,
            complete: rec.complete,
            tables,
        });
    }
    Ok(out)
}

/// One sampler's parquet segments, oldest first: its sealed segments in `seq`
/// order, then its live WAL tail materialized as the newest segment.
///
/// `live_wal`, NOT `read_wal`: the watermark (`ts > MAX(last_ts)` over that
/// sampler's own segments) is the only thing keeping the seam free of
/// duplicates. The prune runs outside the seal transaction, so `wal` routinely
/// still holds rows a sealed segment already covers; replaying the raw table
/// would splice those rows in a second time.
fn table_segments(
    db: &RezDb,
    recording_id: i64,
    sampler: &str,
) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
    let mut segments: Vec<Vec<u8>> = db
        .read_segments(recording_id, sampler)?
        .into_iter()
        .map(|s| s.bytes)
        .collect();
    if let Some(tail) = materialize_wal_tail(sampler, &db.live_wal(recording_id, sampler)?)? {
        segments.push(tail.bytes);
    }
    Ok(segments)
}
