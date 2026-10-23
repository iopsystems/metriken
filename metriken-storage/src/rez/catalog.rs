//! A `.rez` (v3, SQLite) as a [`Catalog`], and choosing between the
//! containers an archive can be.
//!
//! The trait and dendro's implementation are in [`crate::catalog`]; a `.rez`
//! recording is a source and a `.rez` table (a sampler, or `sampler/group`) is
//! a table there. See `docs/journal/2026-09-28-dendro-reader.md`.

use std::path::Path;
use std::sync::Arc;

pub use crate::catalog::{Catalog, DendroCatalog, SegmentMeta, Source, Span, WalRow};

use crate::rez::rez_sqlite::RezDb;

/// Which catalog container a file is, decided by content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// A `.rez` v3 (SQLite).
    Rez,
    Dendro,
}

impl Container {
    /// The catalog container `path` is, or `None` for anything else (a v1/v2
    /// tar `.rez`, which is read eagerly, or not an archive at all).
    pub fn of_path(path: &Path) -> Result<Option<Self>, String> {
        if DendroCatalog::is_archive(path)? {
            return Ok(Some(Container::Dendro));
        }
        match crate::rez::rez::detect_rez_format(path).map_err(|e| e.to_string())? {
            crate::rez::rez::RezFormat::V3Sqlite => Ok(Some(Container::Rez)),
            _ => Ok(None),
        }
    }

    /// Whether `bytes` are this container.
    pub fn recognizes_bytes(self, bytes: &[u8]) -> bool {
        match self {
            Container::Dendro => DendroCatalog::is_archive_bytes(bytes),
            Container::Rez => {
                !DendroCatalog::is_archive_bytes(bytes) && crate::rez::rez::looks_like_v3(bytes)
            }
        }
    }

    /// Open the file read-only as this container.
    pub fn open(self, path: &Path) -> Result<Box<dyn Catalog>, String> {
        Ok(match self {
            Container::Rez => Box::new(RezDb::open(path)?),
            Container::Dendro => Box::new(DendroCatalog::open(path)?),
        })
    }

    /// Open an archive held as bytes as this container.
    pub fn open_bytes(self, bytes: Vec<u8>) -> Result<Box<dyn Catalog>, String> {
        Ok(match self {
            Container::Rez => Box::new(RezDb::open_bytes(bytes)?),
            Container::Dendro => Box::new(DendroCatalog::open_bytes(bytes)?),
        })
    }

    /// Opening `path` as this container again, for a table read later. The
    /// file at `path` now is the only one it opens: once another file is at
    /// `path`, such as a filtered copy renamed over it, it returns an error.
    pub fn reopen(self, path: &Path) -> crate::Reopen {
        let path = path.to_path_buf();
        let opened = crate::file_id::FileId::of(&path);
        Arc::new(move || {
            if opened.is_some() && crate::file_id::FileId::of(&path) != opened {
                return Err(format!(
                    "{} was replaced after it was opened",
                    path.display()
                ));
            }
            self.open(&path)
        })
    }
}

impl Catalog for RezDb {
    fn sources(&self) -> Result<Vec<Source>, String> {
        Ok(self
            .read_recordings()?
            .into_iter()
            .map(|r| Source {
                id: r.id,
                labels: r.meta.labels,
                metadata: r.meta.metadata,
                clock_anchor_wall_ns: r.meta.clock_anchor_wall_ns,
                complete: r.complete,
            })
            .collect())
    }

    fn tables(&self, source_id: i64) -> Result<Vec<String>, String> {
        self.all_samplers(source_id)
    }

    fn segment_meta(&self, source_id: i64, table: &str) -> Result<Vec<(u64, SegmentMeta)>, String> {
        Ok(self
            .read_segment_meta(source_id, table)?
            .into_iter()
            .map(|(seq, m)| {
                (
                    seq,
                    SegmentMeta {
                        rows: m.rows,
                        first_ts: m.first_ts,
                        last_ts: m.last_ts,
                    },
                )
            })
            .collect())
    }

    fn segment_bytes(
        &self,
        source_id: i64,
        table: &str,
        seq: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        self.read_segment_bytes(source_id, table, seq)
    }

    fn segment_bytes_matching(
        &self,
        source_id: i64,
        table: &str,
        seq: u64,
        meta: &SegmentMeta,
    ) -> Result<Option<Vec<u8>>, String> {
        // One statement, as in `DendroCatalog`'s.
        Ok(self
            .segments_overlapping(source_id, table, meta.first_ts, meta.last_ts)?
            .into_iter()
            .find(|s| {
                s.seq == seq
                    && s.meta.rows == meta.rows
                    && s.meta.first_ts == meta.first_ts
                    && s.meta.last_ts == meta.last_ts
            })
            .map(|s| s.bytes))
    }

    fn read_snapshot(&self, f: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        RezDb::read_snapshot(self, |_| f())
    }

    fn live_wal(&self, source_id: i64, table: &str) -> Result<Vec<WalRow>, String> {
        Ok(RezDb::live_wal(self, source_id, table)?
            .into_iter()
            .map(|r| WalRow {
                ts: r.ts,
                wall_offset: r.wall_offset,
                row: r.row,
            })
            .collect())
    }

    fn segment_span(&self, source_id: i64, table: &str) -> Result<(u64, Span), String> {
        let (n, s) = RezDb::segment_span(self, source_id, table)?;
        Ok((n, span(s)))
    }

    fn live_wal_span(&self, source_id: i64, table: &str) -> Result<Span, String> {
        Ok(span(RezDb::live_wal_span(self, source_id, table)?))
    }

    fn caller_row_streams(&self, source_id: i64) -> Result<Vec<String>, String> {
        RezDb::caller_row_streams(self, source_id)
    }

    fn caller_rows(
        &self,
        source_id: i64,
        stream: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, String> {
        self.read_caller_rows(source_id, stream, from, to)
    }

    fn last_caller_row_at_or_before(
        &self,
        source_id: i64,
        stream: &str,
        upto: u64,
        pred: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<Option<u64>, String> {
        RezDb::last_caller_row_at_or_before(self, source_id, stream, upto, pred)
    }
}

fn span(s: crate::rez::rez_sqlite::Span) -> Span {
    Span {
        rows: s.rows,
        first_ts: s.first_ts,
        last_ts: s.last_ts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rez::rez_sqlite::{RecordingMeta, SegmentMeta as RezMeta};

    fn archive(dir: &Path) -> (RezDb, i64) {
        let db = RezDb::create(&dir.join("t.rez")).unwrap();
        let rid = db
            .insert_recording(&RecordingMeta {
                labels: Default::default(),
                metadata: Default::default(),
                clock_anchor_wall_ns: 0,
            })
            .unwrap();
        (db, rid)
    }

    /// A number that names another segment than the one listed reads as gone.
    #[test]
    fn segment_bytes_matching_refuses_a_reused_number() {
        let dir = tempfile::tempdir().unwrap();
        let (mut db, rid) = archive(dir.path());
        let first = RezMeta {
            rows: 5,
            first_ts: 0,
            last_ts: 4,
        };
        db.insert_segment(rid, "cpu", 0, &first, b"first").unwrap();
        let listed = SegmentMeta {
            rows: 5,
            first_ts: 0,
            last_ts: 4,
        };
        let fetch = |db: &RezDb| Catalog::segment_bytes_matching(db, rid, "cpu", 0, &listed);
        assert_eq!(fetch(&db).unwrap().as_deref(), Some(&b"first"[..]));

        db.evict_before(rid, 5).unwrap();
        let renumbered = RezMeta {
            rows: 4,
            first_ts: 5,
            last_ts: 8,
        };
        db.insert_segment(rid, "cpu", 0, &renumbered, b"renumbered")
            .unwrap();
        assert_eq!(
            Catalog::segment_bytes(&db, rid, "cpu", 0)
                .unwrap()
                .as_deref(),
            Some(&b"renumbered"[..])
        );
        assert_eq!(fetch(&db).unwrap(), None);

        // Compaction's shape: the number kept, the span widened over the
        // listed one.
        db.evict_before(rid, 9).unwrap();
        let merged = RezMeta {
            rows: 15,
            first_ts: 0,
            last_ts: 14,
        };
        db.insert_segment(rid, "cpu", 0, &merged, b"merged")
            .unwrap();
        assert_eq!(fetch(&db).unwrap(), None);
    }

    /// A snapshot opened inside another runs in it rather than failing.
    #[test]
    fn read_snapshot_is_reentrant() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _) = archive(dir.path());
        let mut inner_ran = false;
        Catalog::read_snapshot(&db, &mut || {
            Catalog::read_snapshot(&db, &mut || {
                inner_ran = true;
                Ok(())
            })
        })
        .unwrap();
        assert!(inner_ran);
    }

    /// A panic inside a snapshot ends it: the next snapshot on the same
    /// connection sees what was committed after the panic.
    #[test]
    fn a_panic_inside_a_snapshot_does_not_keep_it_open() {
        let dir = tempfile::tempdir().unwrap();
        let (writer, rid) = archive(dir.path());
        let reader = RezDb::open(&dir.path().join("t.rez")).unwrap();

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Catalog::read_snapshot(&reader, &mut || {
                // A read, so the snapshot is pinned before the panic.
                reader.total_rows(rid, "cpu")?;
                panic!("a read that panics");
            });
        }));
        assert!(panicked.is_err());

        let meta = RezMeta {
            rows: 5,
            first_ts: 0,
            last_ts: 4,
        };
        writer.insert_segment(rid, "cpu", 0, &meta, b"seg").unwrap();
        let mut rows = None;
        Catalog::read_snapshot(&reader, &mut || {
            rows = Some(reader.total_rows(rid, "cpu")?);
            Ok(())
        })
        .unwrap();
        assert_eq!(rows, Some(5));
    }
}
