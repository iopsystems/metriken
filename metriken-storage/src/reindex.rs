//! Rewriting a dendro archive's names indexes (`crate::names`) in place, so
//! segments sealed by an earlier version carry the current index, and with
//! it the parts a reader opens a table from without fetching its segments.
//! Each segment is read once to build its index; no segment is re-encoded.

use std::collections::HashMap;
use std::error::Error;
use std::path::Path;

use dendro::archive::ArchiveMut;

use crate::names::Stored;

/// What [`reindex`] did, counted in segments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reindexed {
    /// Sealed segments looked at.
    pub segments: u64,
    /// Segments whose index was replaced.
    pub replaced: u64,
    /// Segments whose index was already the current one.
    pub current: u64,
    /// Segments left as they were: another producer's index, an index from
    /// a version this one does not know, an occupant stream's segment
    /// without an index, or a segment whose footer does not read.
    pub left: u64,
}

/// Rewrite the names index of the sealed segments of the archive at `path`,
/// one transaction per stream, as this version's writer would have written
/// them: every table segment whose footer reads as a metriken segment, and
/// no occupant stream's segment.
///
/// A segment whose index another producer wrote, or a later version of this
/// crate, is left as it is, so an older build cannot replace a newer index.
/// The archive must not be open for writing elsewhere: this takes dendro's
/// writer lock.
///
/// Refuses an archive before schema 5. There a segment's index is a column
/// of its row, after its bytes: a re-index would rewrite every segment, and
/// every later open would read through every segment's bytes to read the
/// indexes. Copy such an archive into a current one first (rezolus's
/// `recording snapshot` does).
pub fn reindex(path: &Path) -> Result<Reindexed, Box<dyn Error>> {
    let mut db = ArchiveMut::open(path)?;
    let schema = db.schema_version()?;
    if schema < 5 {
        return Err(format!(
            "{}: schema {schema} keeps each segment's index in its row, after its bytes, \
             so indexes there cost a read of every segment; copy it into a current \
             archive first",
            path.display()
        )
        .into());
    }
    let mut report = Reindexed::default();
    for source in db.read_sources()? {
        for stream in db.all_streams(source.id)? {
            let segments = db.read_segment_meta(source.id, &stream)?;
            if segments.is_empty() {
                continue;
            }
            let mut stored: HashMap<u64, Option<Vec<u8>>> = db
                .read_segment_indexes(source.id, &stream)?
                .into_iter()
                .collect();
            let occupants = metriken_model::occupants::table_of(&stream).is_some();
            let mut new: Vec<(u64, Vec<u8>)> = Vec::new();
            for (seq, _) in &segments {
                report.segments += 1;
                let old = stored.remove(seq).flatten();
                let ours = match crate::names::read(old.as_deref()) {
                    // Not one a later version wrote.
                    Some(Stored::Names(..)) => !old
                        .as_deref()
                        .is_some_and(crate::names::has_unknown_open_frame),
                    Some(Stored::Fingerprint(_)) => true,
                    // No index: the writer gives one to a table's segments.
                    None if old.is_none() => !occupants,
                    _ => false,
                };
                if !ours {
                    report.left += 1;
                    continue;
                }
                let index = db
                    .read_segment_bytes(source.id, &stream, *seq)?
                    .and_then(|bytes| crate::names::index_of(&bytes));
                match index {
                    None => report.left += 1,
                    Some(index) if old.as_deref() == Some(index.as_slice()) => report.current += 1,
                    Some(index) => new.push((*seq, index)),
                }
            }
            if new.is_empty() {
                continue;
            }
            let replaced = db.transaction(|tx| {
                let mut replaced = 0;
                for (seq, index) in &new {
                    if tx.replace_segment_index(source.id, &stream, *seq, Some(index))? {
                        replaced += 1;
                    }
                }
                Ok(replaced)
            })?;
            report.replaced += replaced;
            report.left += new.len() as u64 - replaced;
        }
    }
    Ok(report)
}
