//! Archives holding two sources with one uuid, as dendro 0.7 and earlier
//! wrote them, apart from the `bounds` table dendro 0.8 gives every archive
//! it creates: a copy of a source beside the source, or two dumps of one
//! buffer, combined with `copy_sources_into`. dendro 0.8 refuses a second
//! source with a uuid the archive holds (`Error::DuplicateSource`) and gives
//! a new archive a unique index on `sources.uuid`, but reads an older file
//! that holds such sources, and metriken-storage reads them as one read
//! group.

use std::path::Path;

use dendro::archive::{Archive, ArchiveMut};
use dendro::rewrite::{copy_sources_into, CopySpec};
use metriken_storage::writer::Encoder;

/// Copy the sources of `from` that `spec` selects into the archive at
/// `into`, created if there is none, keeping each uuid even where `into`
/// already holds it.
///
/// The copy goes through an archive of its own, where each source takes a
/// uuid of its own, into `into`; then `into` loses its `sources_by_uuid`
/// index, as a file dendro 0.7 created never had one, and each copied
/// source gets its uuid back. `into` must not be open for writing: an open
/// `ArchiveMut` holds the database lock the last step needs.
pub fn copy_keeping_uuids(from: &Path, into: &Path, spec: &CopySpec) {
    let dir = tempfile::tempdir().unwrap();
    let mid = dir.path().join("mid.dendro");
    {
        let src = Archive::open(from).unwrap();
        let encoder = Encoder::for_archive(&src).unwrap();
        let mut out = ArchiveMut::create(&mid).unwrap();
        out.transaction(|tx| copy_sources_into(&src, tx, spec, &encoder))
            .unwrap();
    }
    // Each source's uuid, and the one it travels under.
    let renamed: Vec<(String, String)> = {
        let db = rusqlite::Connection::open(&mid).unwrap();
        let uuids: Vec<(i64, String)> = db
            .prepare("SELECT id, uuid FROM sources")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        uuids
            .into_iter()
            .map(|(id, uuid)| {
                let temporary = format!("{uuid}.copy");
                db.execute(
                    "UPDATE sources SET uuid = ?1 WHERE id = ?2",
                    rusqlite::params![temporary, id],
                )
                .unwrap();
                (uuid, temporary)
            })
            .collect()
    };
    {
        let src = Archive::open(&mid).unwrap();
        let encoder = Encoder::for_archive(&src).unwrap();
        let mut out = if into.exists() {
            ArchiveMut::open(into).unwrap()
        } else {
            ArchiveMut::create(into).unwrap()
        };
        out.transaction(|tx| copy_sources_into(&src, tx, &CopySpec::everything(), &encoder))
            .unwrap();
    }
    let db = rusqlite::Connection::open(into).unwrap();
    db.execute_batch("DROP INDEX IF EXISTS sources_by_uuid")
        .unwrap();
    for (uuid, temporary) in renamed {
        db.execute(
            "UPDATE sources SET uuid = ?1 WHERE uuid = ?2",
            rusqlite::params![uuid, temporary],
        )
        .unwrap();
    }
}
