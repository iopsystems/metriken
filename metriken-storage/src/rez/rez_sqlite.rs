//! The `.rez` v3 container: a single SQLite file. SQLite is used as a
//! transactional allocator with a queryable catalog, not as a query engine —
//! segments stay parquet BLOBs the database never looks inside. See
//! docs/journal/2026-08-12-rez-sqlite-container.md § "Why parquet blobs inside
//! a database".
//!
//! This is the ONLY module that knows SQL. Everything above it speaks in
//! recordings, segments, and WAL rows.
//!
//! Read only: metriken-storage 0.5.0 removed the writer. An archive may still
//! be live (a 5.x recorder or hindsight buffer appending to it), so the reads
//! here tolerate rows the writer has not sealed yet; see [`RezDb::live_wal`].

use std::collections::BTreeMap;
#[cfg(test)]
use std::fs::OpenOptions;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// Page cache for a connection that reads segments back, as a negative
/// (kibibyte-denominated) `cache_size`. Large because a reader replays whole
/// segments and benefits from holding them.
const READER_CACHE_SIZE_KIB: i32 = -262_144;

/// The page size the writer created every `.rez` with (tests only).
#[cfg(test)]
const PAGE_SIZE: u32 = 4096;

/// The schema version the last writer stamped (tests only).
///
/// 4 added `caller_rows`. The container shape is otherwise unchanged and the
/// addition is purely additive, so a v3 archive still opens: reads of that
/// table tolerate its absence rather than failing, because an archive written
/// before it existed genuinely has no caller rows. See `read_caller_rows`.
#[cfg(test)]
const SCHEMA_VERSION: i64 = 4;

/// One recording's identity: everything known when the recording starts.
#[derive(Clone)]
pub struct RecordingMeta {
    pub labels: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
    /// Wall-clock reading (ns since epoch) at recording start. Row timestamps
    /// are `anchor + monotonic elapsed`, so this pins the timeline to wall time.
    pub clock_anchor_wall_ns: u64,
}

/// A row of the `recordings` table.
pub struct RecordingRow {
    pub id: i64,
    pub meta: RecordingMeta,
    /// Whether the recording was cleanly finalized. This is what replaced the
    /// `.partial` filename convention: a `.rez` is a valid file from creation,
    /// so "was it finished" has to be a queryable property.
    pub complete: bool,
}

/// The catalog facts about one sealed segment. The segment's own bytes are an
/// opaque parquet BLOB the database never looks inside — this is everything
/// SQLite is asked to know about it.
pub struct SegmentMeta {
    pub rows: u64,
    pub first_ts: u64,
    pub last_ts: u64,
}

/// A row of the `segments` table for one `(recording, sampler)`.
pub struct SegmentRow {
    pub seq: u64,
    pub meta: SegmentMeta,
    pub bytes: Vec<u8>,
}

/// A row of the `wal` table: one sampler's values for one tick, keyed by
/// `(recording_id, sampler, ts)`.
///
/// Values-only, not the raw msgpack snapshot: names and metadata repeat
/// unchanged every tick, so carrying them per row costs several times the
/// payload for nothing. They are re-anchored once per segment instead — see
/// `WalCell`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRow {
    pub sampler: String,
    pub ts: u64,
    pub wall_offset: i64,
    pub row: Vec<u8>,
}

/// How many rows a table holds and what time span they cover, answered from
/// catalog columns alone — no segment or WAL payload is read. `first_ts` and
/// `last_ts` are `None` when `rows` is 0.
pub struct Span {
    pub rows: u64,
    pub first_ts: Option<u64>,
    pub last_ts: Option<u64>,
}

/// The recovery rule, as a `WHERE` clause: a WAL row is live iff its `ts` is
/// past the watermark of the sealed segments **for its own sampler in its own
/// recording**. Written once and shared by `live_wal` and `live_wal_span` so a
/// reported WAL depth can never disagree with the rows the reader will replay.
/// See [`RezDb::live_wal`] for why the rule is what it is.
const LIVE_WAL_PREDICATE: &str = "recording_id = ?1 AND sampler = ?2 \
     AND ts > COALESCE( \
           (SELECT MAX(last_ts) FROM segments \
            WHERE recording_id = ?1 AND sampler = ?2), \
           0)";

/// An open handle on a `.rez` v3 file.
pub struct RezDb {
    conn: Connection,
}

impl RezDb {
    /// Open an existing `.rez`.
    ///
    /// The connection is read-write, as it was when this crate also wrote
    /// `.rez`, though nothing here writes through it. No `SQLITE_OPEN_CREATE`:
    /// opening a `.rez` that is not there is an error, not an empty new
    /// recording.
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
        let db = RezDb { conn };
        db.refuse_dendro(&path.display().to_string())?;
        db.set_pragma("cache_size", READER_CACHE_SIZE_KIB)?;
        Ok(db)
    }

    /// Refuse a dendro archive by name. Both are SQLite files, so format
    /// detection calls a dendro archive a v3 `.rez`, and without this the
    /// first query fails with `no such table: recordings`, which says nothing
    /// about why. dendro stamps its `application_id` into the header; a
    /// `.rez` leaves it zero.
    fn refuse_dendro(&self, what: &str) -> Result<(), String> {
        let id: i64 = self
            .conn
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(|e| format!("failed to read {what}'s application_id: {e}"))?;
        if id == i64::from(dendro::archive::APPLICATION_ID) {
            return Err(format!(
                "{what} is a dendro archive, not a .rez; this command reads .rez archives \
                 only"
            ));
        }
        Ok(())
    }

    /// Whether `path` is a dendro archive rather than a `.rez`. Both are
    /// SQLite files, told apart by the `application_id` dendro stamps into
    /// the header.
    pub fn is_dendro(path: &Path) -> Result<bool, String> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
        let id: i64 = conn
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(|e| format!("failed to read {}'s application_id: {e}", path.display()))?;
        Ok(id == i64::from(dendro::archive::APPLICATION_ID))
    }

    /// Open a `.rez` that exists only as bytes — an upload in a browser,
    /// where there is no filesystem to point `open` at.
    ///
    /// **The bytes are copied into SQLite, not borrowed.** `sqlite3_deserialize`
    /// takes ownership of an in-memory database image, and the connection reads
    /// pages out of it for as long as it lives.
    ///
    /// **The image's journal mode is rewritten from WAL to rollback first, and
    /// that needs justifying.** A `.rez` is created with `journal_mode=WAL`,
    /// which persists in the file header (bytes 18 and 19, the file-format
    /// write and read versions, both `2`). An in-memory database cannot do
    /// WAL — there is no sidecar to write — so SQLite refuses the deserialized
    /// image outright, with `unable to open database file`. Setting those two
    /// bytes to `1` says "rollback journal", which is exactly what
    /// `PRAGMA journal_mode = DELETE` would have persisted.
    ///
    /// What that costs: any SQLite transaction still living only in a `-wal`
    /// sidecar is not in these bytes and is not read. That is not a new gap —
    /// the sidecar is a separate file that a caller holding one `.rez` blob
    /// never has — and it is not where a `.rez`'s own liveness lives: unsealed
    /// rows are rows of the `wal` TABLE, inside this image, and
    /// `materialize_wal_tail` reads them like any other.
    pub fn open_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        const HEADER: &[u8] = b"SQLite format 3\0";
        const JOURNAL_MODE_ROLLBACK: u8 = 1;
        // Byte 19 is the read version; a value above 2 means a format this
        // SQLite cannot read, and quietly stamping it down to 1 would turn
        // that into a wrong answer rather than an error.
        const FILE_FORMAT_WAL: u8 = 2;

        let mut bytes = bytes;
        if bytes.len() < 20 || !bytes.starts_with(HEADER) {
            return Err("not a v3 (SQLite) .rez archive".to_string());
        }
        if bytes[18] == FILE_FORMAT_WAL && bytes[19] == FILE_FORMAT_WAL {
            bytes[18] = JOURNAL_MODE_ROLLBACK;
            bytes[19] = JOURNAL_MODE_ROLLBACK;
        }

        let mut conn = Connection::open_in_memory()
            .map_err(|e| format!("failed to open an in-memory database: {e}"))?;
        // `deserialize_read_exact` copies from the reader into SQLite's own
        // allocation, so the caller's `Vec` is dropped here rather than leaked
        // for the connection's lifetime.
        let len = bytes.len();
        // Read-only: nothing here writes, and SQLite then never has to grow
        // its own copy of the image.
        conn.deserialize_read_exact(rusqlite::MAIN_DB, &mut bytes.as_slice(), len, true)
            .map_err(|e| format!("failed to read the .rez archive: {e}"))?;
        let db = RezDb { conn };
        db.refuse_dendro("the uploaded file")?;
        db.set_pragma("cache_size", READER_CACHE_SIZE_KIB)?;

        // A `.rez` always has a `recordings` table. Its absence has one
        // overwhelmingly likely cause worth naming: the bytes are a plain copy
        // of an archive a writer still held. SQLite commits into a `-wal`
        // SIDECAR, a second file that a single blob does not carry, so such a
        // copy can be a valid SQLite database with none of the archive in it.
        // Left as bare "no such table: recordings", that reads like a corrupt
        // file rather than a copy taken the wrong way.
        let has_catalog: bool = db
            .conn
            .query_row(
                "select count(*) from sqlite_master where type = 'table' and name = 'recordings'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .map_err(|e| format!("failed to inspect the archive: {e}"))?;
        if !has_catalog {
            return Err(
                "not a .rez archive, or a copy taken while it was still being \
                        written — an archive's most recent pages live in a `-wal` sidecar \
                        that a single copied file does not carry. Open the archive by \
                        path with its `-wal` file beside it, or copy it after the \
                        recorder has stopped"
                    .to_string(),
            );
        }
        Ok(db)
    }

    fn set_pragma<V: rusqlite::ToSql>(&self, name: &str, value: V) -> Result<(), String> {
        self.conn
            .pragma_update(None, name, value)
            .map_err(|e| format!("failed to set pragma {name}: {e}"))
    }

    /// Every recording in the file, in insertion order. A `.rez` may hold
    /// several (multi-host, or an A/B pair).
    pub fn read_recordings(&self) -> Result<Vec<RecordingRow>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, labels, metadata, complete, clock_anchor_wall_ns \
                 FROM recordings ORDER BY id",
            )
            .map_err(|e| format!("failed to query recordings: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(|e| format!("failed to query recordings: {e}"))?;

        let mut out = Vec::new();
        for row in rows {
            let (id, labels, metadata, complete, anchor) =
                row.map_err(|e| format!("failed to read recording: {e}"))?;
            out.push(RecordingRow {
                id,
                meta: RecordingMeta {
                    labels: serde_json::from_str(&labels)
                        .map_err(|e| format!("recording {id} has invalid labels: {e}"))?,
                    metadata: serde_json::from_str(&metadata)
                        .map_err(|e| format!("recording {id} has invalid metadata: {e}"))?,
                    // Round-trips through INTEGER; wall-clock nanoseconds stay
                    // inside i64 until the year 2262.
                    clock_anchor_wall_ns: anchor as u64,
                },
                complete: complete != 0,
            });
        }
        Ok(out)
    }

    /// Every segment for `(recording_id, sampler)`, in `seq` order.
    ///
    /// The `ORDER BY seq` is load-bearing, not cosmetic: the reader splices
    /// segment bytes together assuming they arrive in sequence order, and SQL
    /// makes no ordering guarantee without it. Confirmed with
    /// `EXPLAIN QUERY PLAN`: dropping the clause does NOT fall back to
    /// insertion order or to the primary key — the planner instead picks the
    /// `segments_by_time` index for the `(recording_id, sampler)` equality
    /// filter, which is ordered by `last_ts`, not `seq`, and is not even
    /// covering (it still fetches `bytes` per row from the table). `last_ts`
    /// happens to track `seq` in the common case (segments seal in order),
    /// which is exactly the kind of coincidence that makes a missing
    /// `ORDER BY` dangerous rather than obviously wrong.
    /// Segment metadata for one sampler — `seq`, `rows` and the timestamp
    /// bounds — WITHOUT the payload.
    ///
    /// `read_segments` selects `bytes` too, so calling it to learn a table's
    /// span reads the whole table off disk. The reader needs the span for every
    /// table at open (`time_range` is asked before any query runs) and the
    /// payload for almost none of them, so the two questions get separate
    /// queries.
    pub fn read_segment_meta(
        &self,
        recording_id: i64,
        sampler: &str,
    ) -> Result<Vec<(u64, SegmentMeta)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, rows, first_ts, last_ts FROM segments \
                 WHERE recording_id = ?1 AND sampler = ?2 ORDER BY seq",
            )
            .map_err(|e| format!("failed to query segment meta for {sampler}: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params![recording_id, sampler], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    SegmentMeta {
                        rows: r.get::<_, i64>(1)? as u64,
                        first_ts: r.get::<_, i64>(2)? as u64,
                        last_ts: r.get::<_, i64>(3)? as u64,
                    },
                ))
            })
            .map_err(|e| format!("failed to read segment meta for {sampler}: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("failed to read segment meta for {sampler}: {e}"))
    }

    /// One segment's payload, by sequence number — for the reader's name probe,
    /// which needs a single segment's schema and none of the rest.
    pub fn read_segment_bytes(
        &self,
        recording_id: i64,
        sampler: &str,
        seq: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT bytes FROM segments \
                 WHERE recording_id = ?1 AND sampler = ?2 AND seq = ?3",
            )
            .map_err(|e| format!("failed to query segment bytes for {sampler}: {e}"))?;
        let mut rows = stmt
            .query(rusqlite::params![recording_id, sampler, seq as i64])
            .map_err(|e| format!("failed to read segment bytes for {sampler}: {e}"))?;
        match rows.next() {
            Ok(Some(r)) => Ok(Some(r.get(0).map_err(|e| {
                format!("failed to read segment bytes for {sampler}: {e}")
            })?)),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("failed to read segment bytes for {sampler}: {e}")),
        }
    }

    pub fn read_segments(
        &self,
        recording_id: i64,
        sampler: &str,
    ) -> Result<Vec<SegmentRow>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, rows, first_ts, last_ts, bytes FROM segments \
                 WHERE recording_id = ?1 AND sampler = ?2 ORDER BY seq",
            )
            .map_err(|e| format!("failed to query segments for {sampler}: {e}"))?;
        Self::collect_segments(&mut stmt, rusqlite::params![recording_id, sampler], sampler)
    }

    /// Shared row-materialization for the two segment queries, which differ
    /// only in their `WHERE` clause.
    fn collect_segments(
        stmt: &mut rusqlite::Statement<'_>,
        params: &[&dyn rusqlite::ToSql],
        sampler: &str,
    ) -> Result<Vec<SegmentRow>, String> {
        let rows = stmt
            .query_map(params, |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })
            .map_err(|e| format!("failed to query segments for {sampler}: {e}"))?;

        let mut out = Vec::new();
        for row in rows {
            let (seq, n_rows, first_ts, last_ts, bytes) =
                row.map_err(|e| format!("failed to read segment row for {sampler}: {e}"))?;
            out.push(SegmentRow {
                // Round-trips through INTEGER, same as elsewhere in this
                // file: these stay inside i64 for any recording anyone will
                // ever make.
                seq: seq as u64,
                meta: SegmentMeta {
                    rows: n_rows as u64,
                    first_ts: first_ts as u64,
                    last_ts: last_ts as u64,
                },
                bytes,
            });
        }
        Ok(out)
    }

    /// Every segment for `(recording_id, sampler)` that OVERLAPS `[start, end]`
    /// — `last_ts >= start AND first_ts <= end` — in `seq` order.
    ///
    /// This is the ranged dump's selection, and it is a range scan rather than
    /// a table walk: `segments_by_time` is `(recording_id, sampler, last_ts)`,
    /// so the `last_ts >= start` half is served by the index.
    ///
    /// **Whole segments, always.** A segment is an immutable parquet BLOB, so
    /// selecting part of one would mean decoding and re-encoding it — the cost
    /// the container exists to avoid. A caller gets a little more than it asked
    /// for at each edge and should report the span it actually got.
    pub fn segments_overlapping(
        &self,
        recording_id: i64,
        sampler: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<SegmentRow>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, rows, first_ts, last_ts, bytes FROM segments \
                 WHERE recording_id = ?1 AND sampler = ?2 \
                   AND last_ts >= ?3 AND first_ts <= ?4 ORDER BY seq",
            )
            .map_err(|e| format!("failed to query segments for {sampler}: {e}"))?;
        // Clamped, not cast: `u64::MAX as i64` is -1, which would silently
        // select nothing at all for an unbounded upper edge.
        let params = rusqlite::params![
            recording_id,
            sampler,
            start.min(i64::MAX as u64) as i64,
            end.min(i64::MAX as u64) as i64,
        ];
        Self::collect_segments(&mut stmt, params, sampler)
    }

    /// Run `f` with every read inside ONE transaction, so all of its queries
    /// see the same snapshot of the database.
    ///
    /// The dump needs this: without it, retention can evict a segment between
    /// the query that selected it and the read that copies its bytes, and the
    /// result is a file whose catalog references a BLOB that was never
    /// written. `BEGIN DEFERRED` takes no locks until the first read and never
    /// blocks the writer in WAL mode — it just pins the snapshot.
    ///
    /// `f` gets `&Self`, so it may call any reader here; it must not write
    /// through this handle, which is why this is not exposed as a general
    /// transaction.
    ///
    /// Re-entrant: called inside a snapshot, `f` runs in that one rather
    /// than failing on SQLite's "cannot start a transaction within a
    /// transaction".
    pub fn read_snapshot<T>(
        &self,
        f: impl FnOnce(&Self) -> Result<T, String>,
    ) -> Result<T, String> {
        if !self.conn.is_autocommit() {
            return f(self);
        }
        self.conn
            .execute_batch("BEGIN DEFERRED")
            .map_err(|e| format!("failed to open a read snapshot: {e}"))?;

        /// Ends the snapshot however `f` leaves, including by unwinding.
        ///
        /// Ending it on the line after `f(self)` misses a panic in `f`: the
        /// `BEGIN DEFERRED` stays open, and the re-entrancy check above then
        /// runs every later snapshot on this connection inside the stale one.
        /// The reader recovers its catalog lock from poisoning, so one
        /// panicking read would leave every later read on that handle at the
        /// old snapshot, and a live archive's checkpoints could not copy past
        /// it.
        ///
        /// `f` only reads, so `ROLLBACK` releases the same snapshot `COMMIT`
        /// would.
        struct EndSnapshot<'a>(&'a Connection);
        impl Drop for EndSnapshot<'_> {
            fn drop(&mut self) {
                let _ = self.0.execute_batch("ROLLBACK");
            }
        }

        let guard = EndSnapshot(&self.conn);
        let out = f(self);
        drop(guard);
        out
    }

    /// Sum of `rows` across every segment for `(recording_id, sampler)`. Does
    /// not include WAL rows — callers combining sealed and unsealed row
    /// counts must add `live_wal().len()` themselves.
    pub fn total_rows(&self, recording_id: i64, sampler: &str) -> Result<u64, String> {
        let total: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(rows), 0) FROM segments WHERE recording_id = ?1 AND sampler = ?2",
                rusqlite::params![recording_id, sampler],
                |row| row.get(0),
            )
            .map_err(|e| format!("failed to sum rows for {sampler}: {e}"))?;
        Ok(total as u64)
    }

    /// Every distinct sampler with at least one segment for `recording_id`,
    /// alphabetically. A sampler with only unsealed WAL rows and no sealed
    /// segment yet will NOT appear here — use `all_samplers` for "every
    /// sampler this recording has ever seen".
    pub fn samplers(&self, recording_id: i64) -> Result<Vec<String>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DISTINCT sampler FROM segments WHERE recording_id = ?1 ORDER BY sampler",
            )
            .map_err(|e| format!("failed to query samplers: {e}"))?;
        let rows = stmt
            .query_map([recording_id], |row| row.get::<_, String>(0))
            .map_err(|e| format!("failed to query samplers: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("failed to read sampler name: {e}"))?);
        }
        Ok(out)
    }

    /// Every distinct sampler this recording has ever seen, alphabetically —
    /// the union of `segments.sampler` and `wal.sampler`. This is what
    /// closes the gap `samplers()` deliberately leaves open: a sampler that
    /// has never sealed a segment (a quiet table, still inside its first
    /// seal period — the 16-of-26 case this whole design exists to fix) is
    /// otherwise unnameable, because `samplers()` only sees `segments` and
    /// this module is the only place that knows the schema well enough to
    /// look at both tables. Recovery/inventory callers should call this, not
    /// `samplers()`, when they need to know which tables exist at all.
    pub fn all_samplers(&self, recording_id: i64) -> Result<Vec<String>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT sampler FROM segments WHERE recording_id = ?1 \
                 UNION \
                 SELECT sampler FROM wal WHERE recording_id = ?1 \
                 ORDER BY sampler",
            )
            .map_err(|e| format!("failed to query all_samplers: {e}"))?;
        // `?1` is the SAME parameter both times it appears (SQLite numbers
        // parameters, not occurrences), so this binds once, not twice.
        let rows = stmt
            .query_map([recording_id], |row| row.get::<_, String>(0))
            .map_err(|e| format!("failed to query all_samplers: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("failed to read sampler name: {e}"))?);
        }
        Ok(out)
    }

    /// Every WAL row for `(recording_id, sampler)`, sealed or not, oldest
    /// first. Recovery should use `live_wal` instead — this is the raw table,
    /// kept for inspection and for the WAL tests to compare against.
    pub fn read_wal(&self, recording_id: i64, sampler: &str) -> Result<Vec<WalRow>, String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT sampler, ts, wall_offset, row FROM wal \
                 WHERE recording_id = ?1 AND sampler = ?2 ORDER BY ts",
            )
            .map_err(|e| format!("failed to query WAL for {sampler}: {e}"))?;
        Self::collect_wal_rows(&mut stmt, recording_id, sampler)
    }

    /// Rows not covered by any sealed segment — this filter IS the recovery
    /// rule, not just a helper for it: **`ts > COALESCE(MAX(last_ts) of that
    /// sampler's segments, 0)`.**
    ///
    /// The writer pruned WAL rows OUTSIDE the seal transaction, because a
    /// quiet sampler accumulates thousands of rows before it seals and
    /// deleting them in the seal's own commit put tens of megabytes of delete
    /// on the tick path. So a crash between "segment committed" and "prune
    /// ran" can have left WAL rows whose `ts` is already covered by a sealed
    /// segment. Rather than prevent that straddle, recovery tolerates
    /// it: a row is live iff its `ts` is past the watermark of the sealed
    /// segments for its own sampler, full stop — one idempotent rule that needs
    /// no ordering guarantee between sealing and pruning.
    ///
    /// `COALESCE(..., 0)` is what makes the rule correct for a sampler with
    /// no segments at all, not just a straddling one: the subquery's `MAX`
    /// over zero rows is SQL `NULL`, which `COALESCE` turns into `0`, so
    /// `ts > 0` — every row with a real timestamp — is live. That is exactly
    /// the quiet-table case: a sampler that has never sealed keeps its WHOLE
    /// history live. That is the property the tar container could not offer,
    /// where kill-safety was per-segment and a table that had not sealed one
    /// yet recovered nothing at all.
    ///
    /// This turns the prune into a pure background optimisation with no
    /// correctness role.
    pub fn live_wal(&self, recording_id: i64, sampler: &str) -> Result<Vec<WalRow>, String> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT sampler, ts, wall_offset, row FROM wal \
                 WHERE {LIVE_WAL_PREDICATE} ORDER BY ts"
            ))
            .map_err(|e| format!("failed to query live WAL for {sampler}: {e}"))?;
        Self::collect_wal_rows(&mut stmt, recording_id, sampler)
    }

    /// How many rows a sampler's live WAL holds, and the span they cover —
    /// **without materializing them**. Same watermark as `live_wal` (they
    /// share `LIVE_WAL_PREDICATE`, so the depth cannot drift from the rows the
    /// reader replays); this is the aggregate form, for callers that want the
    /// number rather than the payload.
    pub fn live_wal_span(&self, recording_id: i64, sampler: &str) -> Result<Span, String> {
        self.query_span(
            &format!("SELECT COUNT(*), MIN(ts), MAX(ts) FROM wal WHERE {LIVE_WAL_PREDICATE}"),
            recording_id,
            sampler,
        )
        .map_err(|e| format!("failed to measure the live WAL for {sampler}: {e}"))
    }

    /// A sampler's sealed segments as the CATALOG sees them: how many segments,
    /// how many rows across them, and the span they cover. **No BLOB is read** —
    /// `parquet metadata` describes a 197 MB archive from this, and pulling
    /// `bytes` back only to discard it is exactly the cost the catalog exists to
    /// avoid.
    pub fn segment_span(&self, recording_id: i64, sampler: &str) -> Result<(u64, Span), String> {
        let segments: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE recording_id = ?1 AND sampler = ?2",
                rusqlite::params![recording_id, sampler],
                |row| row.get(0),
            )
            .map_err(|e| format!("failed to count segments for {sampler}: {e}"))?;
        let span = self
            .query_span(
                "SELECT COALESCE(SUM(rows), 0), MIN(first_ts), MAX(last_ts) FROM segments \
                 WHERE recording_id = ?1 AND sampler = ?2",
                recording_id,
                sampler,
            )
            .map_err(|e| format!("failed to measure the segments of {sampler}: {e}"))?;
        Ok((segments as u64, span))
    }

    /// Shared shape of the two aggregate queries above: `(rows, MIN(ts),
    /// MAX(ts))`, bound to `(recording_id, sampler)`.
    fn query_span(&self, sql: &str, recording_id: i64, sampler: &str) -> rusqlite::Result<Span> {
        self.conn
            .query_row(sql, rusqlite::params![recording_id, sampler], |row| {
                Ok(Span {
                    rows: row.get::<_, i64>(0)? as u64,
                    first_ts: row.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                    last_ts: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                })
            })
    }

    /// Shared row-materialization for `read_wal` and `live_wal` — they differ
    /// only in the `WHERE` clause of the prepared statement.
    fn collect_wal_rows(
        stmt: &mut rusqlite::Statement<'_>,
        recording_id: i64,
        sampler: &str,
    ) -> Result<Vec<WalRow>, String> {
        let rows = stmt
            .query_map(rusqlite::params![recording_id, sampler], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(|e| format!("failed to query WAL rows for {sampler}: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            let (sampler, ts, wall_offset, data) =
                row.map_err(|e| format!("failed to read WAL row for {sampler}: {e}"))?;
            out.push(WalRow {
                sampler,
                ts: ts as u64,
                wall_offset,
                row: data,
            });
        }
        Ok(out)
    }

    /// The oldest row `sampler` still holds, segments and WAL together, or
    /// `None` when it holds none.
    ///
    /// Can be older than the last retention cutoff: a segment is evicted only
    /// when its NEWEST row is older than the cutoff, so a segment spanning
    /// the cutoff keeps older rows, and the identity index has to keep what
    /// those rows depend on.
    pub fn oldest_row_ts(&self, recording_id: i64, sampler: &str) -> Result<Option<u64>, String> {
        self.conn
            .query_row(
                "SELECT MIN(oldest) FROM ( \
                   SELECT MIN(first_ts) AS oldest FROM segments \
                     WHERE recording_id = ?1 AND sampler = ?2 \
                   UNION ALL \
                   SELECT MIN(ts) FROM wal WHERE recording_id = ?1 AND sampler = ?2)",
                rusqlite::params![recording_id, sampler],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map(|ts| ts.map(|ts| ts.max(0) as u64))
            .map_err(|e| format!("failed to find the oldest {sampler} row: {e}"))
    }

    /// The whole recording's time span — every sampler, segments and WAL
    /// together — from catalog columns alone. `None` when the recording holds
    /// no rows at all, which for a rolling buffer means "nothing within the
    /// lookback".
    pub fn recording_time_span(
        &self,
        recording_id: i64,
    ) -> Result<(Option<u64>, Option<u64>), String> {
        self.conn
            .query_row(
                "SELECT MIN(first_ts), MAX(last_ts) FROM ( \
                   SELECT first_ts, last_ts FROM segments WHERE recording_id = ?1 \
                   UNION ALL \
                   SELECT ts, ts FROM wal WHERE recording_id = ?1)",
                [recording_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?.map(|v| v as u64),
                        row.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                    ))
                },
            )
            .map_err(|e| format!("failed to measure recording {recording_id}: {e}"))
    }

    pub fn pragma_u32(&self, name: &str) -> Result<u32, String> {
        let value = self.pragma_i64(name)?;
        u32::try_from(value).map_err(|_| format!("pragma {name} is {value}, not a u32"))
    }

    /// Signed, because `cache_size` is negative when denominated in kibibytes.
    pub fn pragma_i64(&self, name: &str) -> Result<i64, String> {
        self.conn
            .pragma_query_value(None, name, |row| row.get(0))
            .map_err(|e| format!("failed to read pragma {name}: {e}"))
    }

    /// The recording's `(ts, offset_ns)` clock observations, oldest first.
    /// The caller's rows for one stream, oldest first and in insertion order
    /// within a timestamp.
    ///
    /// An archive written before schema 4 has no such table. That is not an
    /// error and not an empty result standing in for one: it genuinely has no
    /// caller rows, so it reads as none.
    pub fn read_caller_rows(
        &self,
        recording_id: i64,
        stream: &str,
        start_ts: u64,
        end_ts: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, String> {
        if !self.has_table("caller_rows")? {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT ts, blob FROM caller_rows \
                 WHERE recording_id = ?1 AND stream = ?2 AND ts >= ?3 AND ts <= ?4 \
                 ORDER BY ts, seq",
            )
            .map_err(|e| format!("failed to query caller rows: {e}"))?;
        let rows = stmt
            .query_map(
                // Saturating, not `as`. SQLite integers are signed, and
                // `u64::MAX as i64` is -1 — an open-ended read spelt the
                // obvious way would match nothing at all and read as "this
                // recording has no caller rows".
                rusqlite::params![
                    recording_id,
                    stream,
                    start_ts.min(i64::MAX as u64) as i64,
                    end_ts.min(i64::MAX as u64) as i64,
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .map_err(|e| format!("failed to query caller rows: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            let (ts, blob) = row.map_err(|e| format!("failed to read a caller row: {e}"))?;
            out.push((ts as u64, blob));
        }
        Ok(out)
    }

    /// The timestamp of the newest entry at or before `upto` that `pred`
    /// accepts, walking newest-first and stopping at the first hit.
    ///
    /// The catalog does not decode blobs, so what to look for is the caller's:
    /// a reader looking for the last `Full` before a table's first row passes
    /// a predicate that decodes the entry's kind. Newest-first because the hit
    /// is expected close to `upto` — a full restatement lands every seal age —
    /// so this reads a few entries rather than a stream's history.
    pub fn last_caller_row_at_or_before(
        &self,
        recording_id: i64,
        stream: &str,
        upto: u64,
        mut pred: impl FnMut(&[u8]) -> bool,
    ) -> Result<Option<u64>, String> {
        if !self.has_table("caller_rows")? {
            return Ok(None);
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT ts, blob FROM caller_rows \
                 WHERE recording_id = ?1 AND stream = ?2 AND ts <= ?3 \
                 ORDER BY ts DESC, seq DESC",
            )
            .map_err(|e| format!("failed to query caller rows: {e}"))?;
        let rows = stmt
            .query_map(
                rusqlite::params![recording_id, stream, upto.min(i64::MAX as u64) as i64],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .map_err(|e| format!("failed to query caller rows: {e}"))?;
        for row in rows {
            let (ts, blob) = row.map_err(|e| format!("failed to read a caller row: {e}"))?;
            if pred(&blob) {
                return Ok(Some(ts as u64));
            }
        }
        Ok(None)
    }

    /// Every stream that has caller rows in this recording.
    pub fn caller_row_streams(&self, recording_id: i64) -> Result<Vec<String>, String> {
        if !self.has_table("caller_rows")? {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DISTINCT stream FROM caller_rows WHERE recording_id = ?1 ORDER BY stream",
            )
            .map_err(|e| format!("failed to query caller row streams: {e}"))?;
        let rows = stmt
            .query_map([recording_id], |row| row.get::<_, String>(0))
            .map_err(|e| format!("failed to query caller row streams: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("failed to read a caller row stream: {e}"))?);
        }
        Ok(out)
    }

    fn has_table(&self, name: &str) -> Result<bool, String> {
        self.conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |_| Ok(()),
            )
            .map(|_| true)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(false),
                other => Err(format!("failed to look for table `{name}`: {other}")),
            })
    }

    pub fn read_clock_offsets(&self, recording_id: i64) -> Result<Vec<(u64, i64)>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT ts, offset_ns FROM clock_offsets WHERE recording_id = ?1 ORDER BY ts")
            .map_err(|e| format!("failed to query clock offsets: {e}"))?;
        let rows = stmt
            .query_map([recording_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| format!("failed to query clock offsets: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            let (ts, offset) = row.map_err(|e| format!("failed to read clock offset: {e}"))?;
            out.push((ts as u64, offset));
        }
        Ok(out)
    }

    pub fn pragma_string(&self, name: &str) -> Result<String, String> {
        self.conn
            .pragma_query_value(None, name, |row| row.get(0))
            .map_err(|e| format!("failed to read pragma {name}: {e}"))
    }
}

/// Building a v3 `.rez`, for this crate's tests: nothing ships that writes
/// one. The schema and inserts are the ones metriken-storage 0.4.0's writer
/// used, so a test can lay out exactly the rows a reader is asked about.
/// Archives the writer itself produced are under `tests/fixtures/rez`.
#[cfg(test)]
impl RezDb {
    /// Create a `.rez` at `path` as the writer did: `page_size`, incremental
    /// auto-vacuum and `journal_mode=WAL` before the first table, then the
    /// schema. Fails if `path` exists.
    pub fn create(path: &Path) -> Result<Self, String> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| format!("failed to create {}: {e}", path.display()))?;
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
        let db = RezDb { conn };
        db.set_pragma("page_size", PAGE_SIZE)?;
        db.set_pragma("auto_vacuum", "INCREMENTAL")?;
        let mode: String = db
            .conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(|e| format!("failed to set journal_mode=WAL: {e}"))?;
        assert!(mode.eq_ignore_ascii_case("wal"), "journal_mode is {mode}");
        db.conn
            .execute_batch(SCHEMA_SQL)
            .map_err(|e| format!("failed to create .rez schema: {e}"))?;
        db.conn
            .execute(
                "INSERT INTO schema_version(version) VALUES (?1)",
                [SCHEMA_VERSION],
            )
            .map_err(|e| format!("failed to record .rez schema version: {e}"))?;
        Ok(db)
    }

    /// Start a recording, returning its id.
    pub fn insert_recording(&self, meta: &RecordingMeta) -> Result<i64, String> {
        insert_recording_sql(&self.conn, meta)
    }

    /// Run `f` in one transaction: it commits when `f` returns `Ok` and rolls
    /// back otherwise.
    pub fn transaction<T>(
        &mut self,
        f: impl FnOnce(&RezTx<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let tx = RezTx {
            tx: self
                .conn
                .transaction()
                .map_err(|e| format!("failed to begin transaction: {e}"))?,
        };
        let out = f(&tx)?;
        tx.tx
            .commit()
            .map_err(|e| format!("failed to commit transaction: {e}"))?;
        Ok(out)
    }

    /// Insert one sealed segment's bytes and catalog facts.
    pub fn insert_segment(
        &self,
        recording_id: i64,
        sampler: &str,
        seq: u64,
        meta: &SegmentMeta,
        bytes: &[u8],
    ) -> Result<(), String> {
        insert_segment_sql(&self.conn, recording_id, sampler, seq, meta, bytes)
    }

    /// Insert WAL rows in their own transaction.
    pub fn insert_wal_rows(&mut self, recording_id: i64, rows: &[WalRow]) -> Result<(), String> {
        self.transaction(|tx| tx.insert_wal_rows(recording_id, rows))
    }

    /// Hindsight's retention as the writer ran it: every segment wholly
    /// before `cutoff_ts` and every WAL row stamped before it, in one
    /// transaction.
    pub fn evict_before(&mut self, recording_id: i64, cutoff_ts: u64) -> Result<(), String> {
        self.transaction(|tx| {
            let params = rusqlite::params![recording_id, cutoff_ts as i64];
            tx.tx
                .execute(
                    "DELETE FROM segments WHERE recording_id = ?1 AND last_ts < ?2",
                    params,
                )
                .map_err(|e| format!("failed to evict segments: {e}"))?;
            tx.tx
                .execute(
                    "DELETE FROM wal WHERE recording_id = ?1 AND ts < ?2",
                    params,
                )
                .map_err(|e| format!("failed to evict WAL rows: {e}"))?;
            Ok(())
        })
    }
}

/// The writes that may share one transaction, handed to `RezDb::transaction`'s
/// closure (tests only). Everything here lands or nothing does.
#[cfg(test)]
pub struct RezTx<'a> {
    tx: rusqlite::Transaction<'a>,
}

#[cfg(test)]
impl RezTx<'_> {
    /// Start a recording, returning its id.
    ///
    /// In a transaction because a `.rez` can be *assembled* as well as
    /// recorded: the ranged dump writes a recording row and every segment it
    /// selected, and either the whole file is that recording or there is no
    /// file at all.
    pub fn insert_recording(&self, meta: &RecordingMeta) -> Result<i64, String> {
        insert_recording_sql(&self.tx, meta)
    }

    /// Insert one sealed segment's bytes and catalog facts.
    ///
    /// A plain `INSERT` with a `&[u8]` parameter, NOT incremental BLOB I/O
    /// (`blob_open`). At the sizes a segment reaches, `blob_open`'s two-step
    /// (reserve, then stream) is measurably slower than handing SQLite the
    /// whole buffer, so the simpler API is also the faster one here.
    pub fn insert_segment(
        &self,
        recording_id: i64,
        sampler: &str,
        seq: u64,
        meta: &SegmentMeta,
        bytes: &[u8],
    ) -> Result<(), String> {
        insert_segment_sql(&self.tx, recording_id, sampler, seq, meta, bytes)
    }

    /// Insert every WAL row for one tick — one sampler each, typically.
    pub fn insert_wal_rows(&self, recording_id: i64, rows: &[WalRow]) -> Result<(), String> {
        let mut stmt = self
            .tx
            .prepare(
                "INSERT INTO wal(recording_id, sampler, ts, wall_offset, row) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| format!("failed to prepare WAL insert: {e}"))?;
        for r in rows {
            stmt.execute(rusqlite::params![
                recording_id,
                r.sampler,
                r.ts as i64,
                r.wall_offset,
                r.row,
            ])
            .map_err(|e| format!("failed to insert WAL row for {}: {e}", r.sampler))?;
        }
        Ok(())
    }

    /// Append one `(ts, offset_ns)` clock observation for the recording.
    /// Append caller rows for one stream, in the order given.
    ///
    /// `seq` disambiguates rows sharing a timestamp, which is a real case: an
    /// interval that moves several groups writes one entry per group at the
    /// same tick. Assigned here from what is already stored rather than by the
    /// caller, so two writers cannot pick the same one.
    pub fn insert_caller_rows(
        &self,
        recording_id: i64,
        stream: &str,
        rows: &[(u64, Vec<u8>)],
    ) -> Result<(), String> {
        let mut stmt = self
            .tx
            .prepare(
                "INSERT INTO caller_rows(recording_id, stream, ts, seq, blob) \
                 VALUES (?1, ?2, ?3, \
                   (SELECT COALESCE(MAX(seq) + 1, 0) FROM caller_rows \
                    WHERE recording_id = ?1 AND stream = ?2 AND ts = ?3), \
                   ?4)",
            )
            .map_err(|e| format!("failed to prepare a caller row insert: {e}"))?;
        for (ts, blob) in rows {
            stmt.execute(rusqlite::params![recording_id, stream, *ts as i64, blob])
                .map_err(|e| format!("failed to insert a caller row: {e}"))?;
        }
        Ok(())
    }

    pub fn insert_clock_offset(
        &self,
        recording_id: i64,
        ts: u64,
        offset_ns: i64,
    ) -> Result<(), String> {
        self.tx
            .execute(
                "INSERT INTO clock_offsets(recording_id, ts, offset_ns) VALUES (?1, ?2, ?3)",
                rusqlite::params![recording_id, ts as i64, offset_ns],
            )
            .map_err(|e| format!("failed to insert clock offset: {e}"))?;
        Ok(())
    }

    /// Mark the recording cleanly finalized. This is what replaced the
    /// `.partial` filename convention: the file is valid from creation, so
    /// "was it finished" is a queryable property instead of a name.
    pub fn mark_complete(&self, recording_id: i64) -> Result<(), String> {
        self.tx
            .execute(
                "UPDATE recordings SET complete = 1 WHERE id = ?1",
                [recording_id],
            )
            .map_err(|e| format!("failed to mark recording {recording_id} complete: {e}"))?;
        Ok(())
    }
}

/// Shared by `RezDb::insert_recording` (its own commit) and
/// `RezTx::insert_recording` (part of a batch).
#[cfg(test)]
fn insert_recording_sql(conn: &Connection, meta: &RecordingMeta) -> Result<i64, String> {
    let labels = serde_json::to_string(&meta.labels)
        .map_err(|e| format!("failed to encode recording labels: {e}"))?;
    let metadata = serde_json::to_string(&meta.metadata)
        .map_err(|e| format!("failed to encode recording metadata: {e}"))?;
    conn.execute(
        "INSERT INTO recordings(labels, metadata, complete, clock_anchor_wall_ns) \
         VALUES (?1, ?2, 0, ?3)",
        rusqlite::params![labels, metadata, meta.clock_anchor_wall_ns as i64],
    )
    .map_err(|e| format!("failed to insert recording: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// Shared by `RezDb::insert_segment` (its own commit) and
/// `RezTx::insert_segment` (part of a batch): `Transaction` derefs to
/// `Connection`, so both reach the same statement.
#[cfg(test)]
fn insert_segment_sql(
    conn: &Connection,
    recording_id: i64,
    sampler: &str,
    seq: u64,
    meta: &SegmentMeta,
    bytes: &[u8],
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO segments(recording_id, sampler, seq, rows, first_ts, last_ts, bytes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            recording_id,
            sampler,
            seq as i64,
            meta.rows as i64,
            meta.first_ts as i64,
            meta.last_ts as i64,
            bytes,
        ],
    )
    .map_err(|e| format!("failed to insert segment {sampler}#{seq}: {e}"))?;
    Ok(())
}

/// The catalog. Segment and WAL payloads are opaque BLOBs; everything the
/// container needs to answer questions about them is a column.
#[cfg(test)]
const SCHEMA_SQL: &str = "
CREATE TABLE recordings(
  id INTEGER PRIMARY KEY,
  labels TEXT NOT NULL,               -- JSON
  metadata TEXT NOT NULL,             -- JSON
  complete INTEGER NOT NULL DEFAULT 0,
  clock_anchor_wall_ns INTEGER NOT NULL
);
CREATE TABLE segments(
  recording_id INTEGER NOT NULL REFERENCES recordings(id),
  sampler TEXT NOT NULL,
  seq INTEGER NOT NULL,
  rows INTEGER NOT NULL,
  first_ts INTEGER NOT NULL,
  last_ts INTEGER NOT NULL,
  bytes BLOB NOT NULL,
  PRIMARY KEY (recording_id, sampler, seq)
);
-- The catalog half of the design: it makes hindsight retention
-- (`WHERE last_ts < cutoff`) and range reads indexed lookups rather than
-- scans. `live_wal`'s subquery (`SELECT MAX(last_ts) FROM segments WHERE
-- recording_id = ? AND sampler = ?`) already uses it — confirmed by
-- `EXPLAIN QUERY PLAN` during review — so this is not a speculative index
-- sitting unused; keep it maintained.
CREATE INDEX segments_by_time ON segments(recording_id, sampler, last_ts);
CREATE TABLE wal(
  recording_id INTEGER NOT NULL,
  sampler TEXT NOT NULL,
  ts INTEGER NOT NULL,
  wall_offset INTEGER NOT NULL,
  row BLOB NOT NULL,
  PRIMARY KEY (recording_id, sampler, ts)
);
CREATE TABLE clock_offsets(
  recording_id INTEGER NOT NULL,
  ts INTEGER NOT NULL,
  offset_ns INTEGER NOT NULL
);
-- The caller's time-keyed store: what a slot MEANT, from when. A column's
-- metadata cannot express change, so a fact that changes over time — slot v3
-- held task A and now holds task B — belongs here, keyed by the time it
-- changed, and never in a schema. Opaque: the container stores and returns the
-- blob and never decodes it. Several rows may share a timestamp and read back
-- in insertion order, which is why the key carries `seq` rather than being
-- (recording, stream, ts).
CREATE TABLE caller_rows(
  recording_id INTEGER NOT NULL REFERENCES recordings(id),
  stream TEXT NOT NULL,
  ts INTEGER NOT NULL,
  seq INTEGER NOT NULL,
  blob BLOB NOT NULL,
  PRIMARY KEY (recording_id, stream, ts, seq)
);
CREATE TABLE schema_version(version INTEGER NOT NULL);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_recording(db: &RezDb) -> i64 {
        db.insert_recording(&RecordingMeta {
            labels: [("source".to_string(), "rezolus".to_string())]
                .into_iter()
                .collect(),
            metadata: BTreeMap::new(),
            clock_anchor_wall_ns: 1_700_000_000_000_000_000,
        })
        .unwrap()
    }

    /// The blob is the caller's and stays the caller's: the container stores
    /// and returns it and never decodes it. Round-tripping bytes rather than a
    /// decoded shape is what says so.
    #[test]
    fn caller_rows_round_trip_as_opaque_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let rec = seed_recording(&db);

        let entries = vec![
            (1_000u64, vec![0x93, 0x01, 0x02]),
            (2_000, vec![0xff, 0x00]),
        ];
        db.transaction(|tx| tx.insert_caller_rows(rec, "cpu_usage/task", &entries))
            .unwrap();

        let back = db
            .read_caller_rows(rec, "cpu_usage/task", 0, u64::MAX)
            .unwrap();
        assert_eq!(back, entries);
    }

    /// The newest-first probe stops at the first entry the predicate accepts
    /// and never looks past `upto`.
    #[test]
    fn the_last_caller_row_probe_walks_newest_first_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let rec = seed_recording(&db);
        // Blob `1` marks the entries a caller is looking for.
        let entries: Vec<(u64, Vec<u8>)> = vec![
            (1_000, vec![1]),
            (2_000, vec![0]),
            (3_000, vec![1]),
            (4_000, vec![0]),
            (5_000, vec![1]),
        ];
        db.transaction(|tx| tx.insert_caller_rows(rec, "s", &entries))
            .unwrap();

        let mut looked_at = Vec::new();
        let hit = db
            .last_caller_row_at_or_before(rec, "s", 4_500, |b| {
                looked_at.push(b[0]);
                b[0] == 1
            })
            .unwrap();
        assert_eq!(hit, Some(3_000));
        assert_eq!(looked_at, vec![0, 1], "4_000 then 3_000, and no further");

        assert_eq!(
            db.last_caller_row_at_or_before(rec, "s", 500, |_| true)
                .unwrap(),
            None,
            "nothing at or before the bound"
        );
        assert_eq!(
            db.last_caller_row_at_or_before(rec, "s", u64::MAX, |b| b[0] == 1)
                .unwrap(),
            Some(5_000)
        );
        assert_eq!(
            db.last_caller_row_at_or_before(rec, "s", 2_500, |b| b[0] == 7)
                .unwrap(),
            None,
            "a predicate nothing satisfies"
        );
    }

    /// Several entries may share a timestamp — one interval that moves three
    /// groups writes three at the same tick — and they must read back in the
    /// order they were written. A consumer applies them in sequence, so an
    /// order that varied would apply a later state before an earlier one.
    #[test]
    fn entries_sharing_a_timestamp_read_back_in_insertion_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let rec = seed_recording(&db);

        let entries: Vec<(u64, Vec<u8>)> = (0u8..5).map(|n| (7_000u64, vec![n])).collect();
        db.transaction(|tx| tx.insert_caller_rows(rec, "s", &entries))
            .unwrap();

        let back = db.read_caller_rows(rec, "s", 0, u64::MAX).unwrap();
        assert_eq!(
            back.iter().map(|(_, b)| b[0]).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4],
        );
    }

    /// And across separate transactions, since a recording writes one tick per
    /// transaction. `seq` is assigned from what is already stored, so the
    /// second batch must continue the first rather than collide with it.
    #[test]
    fn a_later_batch_continues_the_sequence_at_the_same_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let rec = seed_recording(&db);

        db.transaction(|tx| tx.insert_caller_rows(rec, "s", &[(9_000, vec![1])]))
            .unwrap();
        db.transaction(|tx| tx.insert_caller_rows(rec, "s", &[(9_000, vec![2])]))
            .unwrap();

        let back = db.read_caller_rows(rec, "s", 0, u64::MAX).unwrap();
        assert_eq!(
            back.iter().map(|(_, b)| b[0]).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    /// Entries are per stream and per recording. A read that leaked across
    /// either would attribute one group's identity to another, which is the
    /// failure the whole index exists to prevent.
    #[test]
    fn entries_do_not_leak_across_streams_or_recordings() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let a = seed_recording(&db);
        let b = seed_recording(&db);

        db.transaction(|tx| {
            tx.insert_caller_rows(a, "one", &[(1, vec![b'a'])])?;
            tx.insert_caller_rows(a, "two", &[(1, vec![b'b'])])?;
            tx.insert_caller_rows(b, "one", &[(1, vec![b'c'])])
        })
        .unwrap();

        assert_eq!(
            db.read_caller_rows(a, "one", 0, u64::MAX).unwrap(),
            vec![(1u64, vec![b'a'])]
        );
        assert_eq!(
            db.read_caller_rows(b, "one", 0, u64::MAX).unwrap(),
            vec![(1u64, vec![b'c'])]
        );
        assert_eq!(
            db.caller_row_streams(a).unwrap(),
            vec!["one".to_string(), "two".to_string()]
        );
        assert_eq!(db.caller_row_streams(b).unwrap(), vec!["one".to_string()]);
    }

    /// An archive written before schema 4 has no such table. It reads as no
    /// caller rows, which is what it is — not an error, and not an empty result
    /// standing in for one.
    #[test]
    fn an_archive_without_the_table_reads_as_having_no_caller_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v3.rez");
        let db = RezDb::create(&path).unwrap();
        let rec = seed_recording(&db);
        db.conn.execute("DROP TABLE caller_rows", []).unwrap();

        assert_eq!(db.read_caller_rows(rec, "s", 0, u64::MAX).unwrap(), vec![]);
        assert_eq!(db.caller_row_streams(rec).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn open_and_open_bytes_take_the_reader_cache() {
        // Only a value that differs from SQLite's -2000 KiB default detects an
        // open that forgets the pragma.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.rez");
        drop(RezDb::create(&path).unwrap());
        let db = RezDb::open(&path).unwrap();
        assert_eq!(
            db.pragma_i64("cache_size").unwrap(),
            READER_CACHE_SIZE_KIB as i64,
            "256 MiB reader cache, not SQLite's -2000 default"
        );
        drop(db);
        let db = RezDb::open_bytes(std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            db.pragma_i64("cache_size").unwrap(),
            READER_CACHE_SIZE_KIB as i64
        );
    }

    #[test]
    fn schema_round_trips_a_recording() {
        let dir = tempfile::tempdir().unwrap();
        let db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let id = db
            .insert_recording(&RecordingMeta {
                labels: [("host".to_string(), "h1".to_string())]
                    .into_iter()
                    .collect(),
                metadata: [("source".to_string(), "rezolus".to_string())]
                    .into_iter()
                    .collect(),
                clock_anchor_wall_ns: 1_700_000_000_000_000_000,
            })
            .unwrap();
        let got = db.read_recordings().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, id);
        assert_eq!(got[0].meta.labels["host"], "h1");
        assert_eq!(got[0].meta.metadata["source"], "rezolus");
        assert_eq!(got[0].meta.clock_anchor_wall_ns, 1_700_000_000_000_000_000);
        assert!(!got[0].complete, "a fresh recording is not complete");
    }

    #[test]
    fn segments_read_back_in_seq_order_not_insertion_order() {
        // Insert seq 1 BEFORE seq 0, AND give seq 1 the smaller `last_ts`.
        // That makes the three plausible orderings mutually distinct, so only
        // a genuinely seq-ordered result can pass:
        //   - insertion order:        (1, 0)
        //   - `segments_by_time` order (by last_ts, the index the planner
        //     falls back to without an explicit ORDER BY — see the doc
        //     comment on `read_segments`): (1, 0), since 99 < 200
        //   - primary-key / seq order:  (0, 1)  <- the only correct one
        // A same-direction fixture (seq tracking last_ts, as in an earlier
        // version of this test) leaves `segments_by_time` order coinciding
        // with the correct order, so dropping `ORDER BY seq` would silently
        // pass. This fixture doesn't have that escape hatch.
        let dir = tempfile::tempdir().unwrap();
        let db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let rid = db
            .insert_recording(&RecordingMeta {
                labels: BTreeMap::new(),
                metadata: BTreeMap::new(),
                clock_anchor_wall_ns: 0,
            })
            .unwrap();
        db.insert_segment(
            rid,
            "cpu_usage",
            1,
            &SegmentMeta {
                rows: 10,
                first_ts: 90,
                last_ts: 99,
            },
            b"seq-one-bytes",
        )
        .unwrap();
        db.insert_segment(
            rid,
            "cpu_usage",
            0,
            &SegmentMeta {
                rows: 5,
                first_ts: 100,
                last_ts: 200,
            },
            b"seq-zero-bytes",
        )
        .unwrap();

        let got = db.read_segments(rid, "cpu_usage").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0].seq, 0,
            "seq 0 must come first despite being inserted second"
        );
        assert_eq!(got[0].bytes, b"seq-zero-bytes");
        assert_eq!(got[1].seq, 1);
        assert_eq!(got[1].bytes, b"seq-one-bytes");
    }

    #[test]
    fn total_rows_sums_across_segments() {
        let dir = tempfile::tempdir().unwrap();
        let db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let rid = db
            .insert_recording(&RecordingMeta {
                labels: BTreeMap::new(),
                metadata: BTreeMap::new(),
                clock_anchor_wall_ns: 0,
            })
            .unwrap();
        db.insert_segment(
            rid,
            "cpu_usage",
            0,
            &SegmentMeta {
                rows: 3,
                first_ts: 0,
                last_ts: 29,
            },
            b"a",
        )
        .unwrap();
        db.insert_segment(
            rid,
            "cpu_usage",
            1,
            &SegmentMeta {
                rows: 2,
                first_ts: 30,
                last_ts: 49,
            },
            b"b",
        )
        .unwrap();

        assert_eq!(db.total_rows(rid, "cpu_usage").unwrap(), 5);
    }

    #[test]
    fn samplers_lists_each_sampler_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let rid = db
            .insert_recording(&RecordingMeta {
                labels: BTreeMap::new(),
                metadata: BTreeMap::new(),
                clock_anchor_wall_ns: 0,
            })
            .unwrap();
        let meta = SegmentMeta {
            rows: 1,
            first_ts: 0,
            last_ts: 9,
        };
        db.insert_segment(rid, "cpu_usage", 0, &meta, b"a").unwrap();
        db.insert_segment(rid, "cpu_usage", 1, &meta, b"b").unwrap();
        db.insert_segment(rid, "blockio", 0, &meta, b"c").unwrap();

        assert_eq!(db.samplers(rid).unwrap(), vec!["blockio", "cpu_usage"]);
    }

    #[test]
    fn all_samplers_includes_a_sampler_that_has_never_sealed() {
        // This is the API gap `all_samplers` exists to close: `samplers()`
        // only sees `segments`, so a quiet table still inside its first seal
        // period — the 16-of-26 fleet case — is nameless to it. A caller with
        // only `samplers()` cannot discover, let alone recover, a sampler
        // that has never sealed.
        let dir = tempfile::tempdir().unwrap();
        let mut db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let rid = db
            .insert_recording(&RecordingMeta {
                labels: BTreeMap::new(),
                metadata: BTreeMap::new(),
                clock_anchor_wall_ns: 0,
            })
            .unwrap();
        db.insert_segment(
            rid,
            "cpu_usage",
            0,
            &SegmentMeta {
                rows: 1,
                first_ts: 0,
                last_ts: 9,
            },
            b"a",
        )
        .unwrap();
        // "drivehealth" never seals in this test — only a WAL row.
        db.insert_wal_rows(rid, &[wal_row("drivehealth", 5)])
            .unwrap();

        assert_eq!(
            db.samplers(rid).unwrap(),
            vec!["cpu_usage"],
            "samplers() legitimately does not see the WAL-only sampler"
        );
        assert_eq!(
            db.all_samplers(rid).unwrap(),
            vec!["cpu_usage", "drivehealth"],
            "all_samplers() must see it — this is the whole point of the accessor"
        );
    }

    #[test]
    fn segments_are_scoped_per_recording() {
        // A .rez can hold several recordings (multi-host / A-B). Reading one
        // recording's segments must never see another recording's rows for a
        // sampler of the same name.
        let dir = tempfile::tempdir().unwrap();
        let db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let meta = |labels: &str| RecordingMeta {
            labels: [("host".to_string(), labels.to_string())]
                .into_iter()
                .collect(),
            metadata: BTreeMap::new(),
            clock_anchor_wall_ns: 0,
        };
        let r1 = db.insert_recording(&meta("h1")).unwrap();
        let r2 = db.insert_recording(&meta("h2")).unwrap();

        let sm = SegmentMeta {
            rows: 1,
            first_ts: 0,
            last_ts: 9,
        };
        db.insert_segment(r1, "cpu_usage", 0, &sm, b"r1-bytes")
            .unwrap();
        db.insert_segment(r2, "cpu_usage", 0, &sm, b"r2-bytes")
            .unwrap();

        let got1 = db.read_segments(r1, "cpu_usage").unwrap();
        assert_eq!(got1.len(), 1);
        assert_eq!(got1[0].bytes, b"r1-bytes");

        let got2 = db.read_segments(r2, "cpu_usage").unwrap();
        assert_eq!(got2.len(), 1);
        assert_eq!(got2[0].bytes, b"r2-bytes");

        assert_eq!(db.total_rows(r1, "cpu_usage").unwrap(), 1);
        assert_eq!(db.samplers(r1).unwrap(), vec!["cpu_usage"]);
    }

    /// Shared setup for the WAL tests: a fresh `.rez` with one recording.
    fn wal_test_db() -> (tempfile::TempDir, RezDb, i64) {
        let dir = tempfile::tempdir().unwrap();
        let db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let rid = db
            .insert_recording(&RecordingMeta {
                labels: BTreeMap::new(),
                metadata: BTreeMap::new(),
                clock_anchor_wall_ns: 0,
            })
            .unwrap();
        (dir, db, rid)
    }

    fn wal_row(sampler: &str, ts: u64) -> WalRow {
        WalRow {
            sampler: sampler.to_string(),
            ts,
            wall_offset: ts as i64,
            row: format!("row@{ts}").into_bytes(),
        }
    }

    #[test]
    fn live_wal_excludes_rows_already_covered_by_a_sealed_segment() {
        // THE recovery rule. Seal a segment covering ts<=30, then insert WAL
        // rows at 10/20/30/40 WITHOUT pruning — simulating a crash between
        // "segment committed" and "prune ran". live_wal must return only the
        // row past the watermark (ts=40); read_wal must still return all
        // four, because the raw table is untouched.
        let (_dir, mut db, rid) = wal_test_db();
        db.insert_segment(
            rid,
            "cpu_usage",
            0,
            &SegmentMeta {
                rows: 3,
                first_ts: 10,
                last_ts: 30,
            },
            b"sealed-bytes",
        )
        .unwrap();
        db.insert_wal_rows(
            rid,
            &[
                wal_row("cpu_usage", 10),
                wal_row("cpu_usage", 20),
                wal_row("cpu_usage", 30),
                wal_row("cpu_usage", 40),
            ],
        )
        .unwrap();

        let live = db.live_wal(rid, "cpu_usage").unwrap();
        assert_eq!(live.len(), 1, "only ts=40 is past the sealed watermark");
        assert_eq!(live[0].ts, 40);

        let all = db.read_wal(rid, "cpu_usage").unwrap();
        assert_eq!(all.len(), 4, "the raw WAL table is untouched by sealing");
    }

    #[test]
    fn live_wal_returns_everything_when_nothing_has_sealed() {
        // A quiet sampler that has never sealed a segment: every WAL row is
        // live. This is the case that recovered NOTHING under v2 (16 of 26
        // fleet tables at kill -9 120s in).
        let (_dir, mut db, rid) = wal_test_db();
        db.insert_wal_rows(
            rid,
            &[wal_row("drivehealth", 5), wal_row("drivehealth", 15)],
        )
        .unwrap();

        let live = db.live_wal(rid, "drivehealth").unwrap();
        assert_eq!(
            live.len(),
            2,
            "no segments sealed yet, so every row is live"
        );
        assert_eq!(live[0].ts, 5);
        assert_eq!(live[1].ts, 15);
    }

    #[test]
    fn live_wal_watermark_is_scoped_to_its_own_sampler_and_recording() {
        // The keystone query has TWO filters inside the watermark subquery
        // (`sampler = ?2` and `recording_id = ?1`), and either one being
        // dropped is invisible to the tests above: both are single-sampler,
        // single-recording, and the multi-sampler / multi-recording tests
        // elsewhere have no segments at all, so the subquery returns NULL
        // everywhere it could otherwise discriminate.
        //
        // Two recordings x two samplers, seal a segment for (r1, cpu_usage)
        // ONLY. If the subquery's `sampler` filter is missing, cpu_usage's
        // watermark leaks into blockio's live_wal within r1. If the
        // `recording_id` filter is missing, it leaks into r2's cpu_usage
        // too. Either leak would silently truncate a quiet sampler's — or a
        // second recording's — live WAL using a watermark that has nothing
        // to do with it: exactly the failure mode this design exists to
        // rule out.
        let dir = tempfile::tempdir().unwrap();
        let mut db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let meta = |host: &str| RecordingMeta {
            labels: [("host".to_string(), host.to_string())]
                .into_iter()
                .collect(),
            metadata: BTreeMap::new(),
            clock_anchor_wall_ns: 0,
        };
        let r1 = db.insert_recording(&meta("h1")).unwrap();
        let r2 = db.insert_recording(&meta("h2")).unwrap();

        // Seal (r1, cpu_usage) up to ts=30 — a high watermark, so a leaked
        // filter would visibly truncate whichever WAL it leaked into.
        db.insert_segment(
            r1,
            "cpu_usage",
            0,
            &SegmentMeta {
                rows: 3,
                first_ts: 10,
                last_ts: 30,
            },
            b"r1-cpu_usage-sealed",
        )
        .unwrap();

        db.insert_wal_rows(r1, &[wal_row("cpu_usage", 40), wal_row("blockio", 5)])
            .unwrap();
        db.insert_wal_rows(r2, &[wal_row("cpu_usage", 5)]).unwrap();

        // (r1, blockio) has never sealed — its watermark must be its own
        // (nothing), not cpu_usage's 30.
        let r1_blockio = db.live_wal(r1, "blockio").unwrap();
        assert_eq!(
            r1_blockio.len(),
            1,
            "blockio in r1 must not inherit cpu_usage's sealed watermark"
        );
        assert_eq!(r1_blockio[0].ts, 5);

        // (r2, cpu_usage) has never sealed either — its watermark must not
        // be r1's cpu_usage watermark, even though the sampler name matches.
        let r2_cpu_usage = db.live_wal(r2, "cpu_usage").unwrap();
        assert_eq!(
            r2_cpu_usage.len(),
            1,
            "cpu_usage in r2 must not inherit r1's sealed watermark"
        );
        assert_eq!(r2_cpu_usage[0].ts, 5);

        // Sanity: (r1, cpu_usage) itself is correctly filtered by its own
        // watermark.
        let r1_cpu_usage = db.live_wal(r1, "cpu_usage").unwrap();
        assert_eq!(r1_cpu_usage.len(), 1);
        assert_eq!(r1_cpu_usage[0].ts, 40);
    }

    #[test]
    fn segment_span_summarizes_the_catalog_without_reading_a_blob() {
        // `parquet metadata` describes a fleet archive (197 MB, 149 segments)
        // from these numbers, so they must come from the catalog columns and
        // nothing else. The bytes here are deliberately NOT parquet: an
        // implementation that reached into a segment to count its rows — or
        // that pulled `bytes` back merely to discard it — fails or wastes the
        // whole archive's worth of I/O, and this fixture is what makes the
        // first of those visible.
        let (_dir, db, rid) = wal_test_db();
        db.insert_segment(
            rid,
            "cpu_usage",
            0,
            &SegmentMeta {
                rows: 3,
                first_ts: 10,
                last_ts: 29,
            },
            b"not-parquet",
        )
        .unwrap();
        db.insert_segment(
            rid,
            "cpu_usage",
            1,
            &SegmentMeta {
                rows: 2,
                first_ts: 30,
                last_ts: 49,
            },
            b"not-parquet-either",
        )
        .unwrap();
        // Another sampler's segments must not be counted into this one's.
        db.insert_segment(
            rid,
            "blockio",
            0,
            &SegmentMeta {
                rows: 99,
                first_ts: 0,
                last_ts: 99,
            },
            b"nor-this",
        )
        .unwrap();

        let (segments, span) = db.segment_span(rid, "cpu_usage").unwrap();
        assert_eq!(segments, 2);
        assert_eq!(span.rows, 5, "the SUM of the catalog's row counts");
        assert_eq!((span.first_ts, span.last_ts), (Some(10), Some(49)));

        // A sampler with no segments at all is a span of nothing, not an error:
        // that is the quiet-table case the WAL exists for.
        let (segments, span) = db.segment_span(rid, "drivehealth").unwrap();
        assert_eq!(segments, 0);
        assert_eq!(span.rows, 0);
        assert_eq!((span.first_ts, span.last_ts), (None, None));
    }

    #[test]
    fn live_wal_span_counts_the_same_rows_live_wal_returns() {
        // The depth `parquet metadata` reports is "how many unsealed rows are
        // recoverable", which is exactly what the reader will materialize —
        // so it must apply the SAME watermark `live_wal` does, not count the
        // raw table. Sealed-but-not-yet-pruned rows (the straddle the deferred
        // prune deliberately allows) are the case that tells the two apart.
        let (_dir, mut db, rid) = wal_test_db();
        db.insert_segment(
            rid,
            "cpu_usage",
            0,
            &SegmentMeta {
                rows: 3,
                first_ts: 10,
                last_ts: 30,
            },
            b"not-parquet",
        )
        .unwrap();
        db.insert_wal_rows(
            rid,
            &[
                wal_row("cpu_usage", 10),
                wal_row("cpu_usage", 20),
                wal_row("cpu_usage", 30),
                wal_row("cpu_usage", 40),
                wal_row("cpu_usage", 50),
            ],
        )
        .unwrap();

        let span = db.live_wal_span(rid, "cpu_usage").unwrap();
        assert_eq!(
            span.rows,
            db.live_wal(rid, "cpu_usage").unwrap().len() as u64,
            "the depth must agree with the rows the reader will replay"
        );
        assert_eq!(span.rows, 2, "ts=40 and ts=50 are past the watermark");
        assert_eq!((span.first_ts, span.last_ts), (Some(40), Some(50)));

        // A never-sealed sampler keeps its whole history live.
        db.insert_wal_rows(rid, &[wal_row("drivehealth", 5)])
            .unwrap();
        let span = db.live_wal_span(rid, "drivehealth").unwrap();
        assert_eq!(span.rows, 1);
        assert_eq!((span.first_ts, span.last_ts), (Some(5), Some(5)));
    }

    #[test]
    fn wal_rows_are_scoped_per_recording() {
        // Same as segments: a .rez can hold several recordings, and reading
        // one must not see another's WAL rows for a same-named sampler.
        let dir = tempfile::tempdir().unwrap();
        let mut db = RezDb::create(&dir.path().join("t.rez")).unwrap();
        let meta = |host: &str| RecordingMeta {
            labels: [("host".to_string(), host.to_string())]
                .into_iter()
                .collect(),
            metadata: BTreeMap::new(),
            clock_anchor_wall_ns: 0,
        };
        let r1 = db.insert_recording(&meta("h1")).unwrap();
        let r2 = db.insert_recording(&meta("h2")).unwrap();

        db.insert_wal_rows(r1, &[wal_row("cpu_usage", 10)]).unwrap();
        db.insert_wal_rows(r2, &[wal_row("cpu_usage", 20)]).unwrap();

        let got1 = db.read_wal(r1, "cpu_usage").unwrap();
        assert_eq!(got1.len(), 1);
        assert_eq!(got1[0].ts, 10);

        let got2 = db.read_wal(r2, "cpu_usage").unwrap();
        assert_eq!(got2.len(), 1);
        assert_eq!(got2[0].ts, 20);

        // live_wal must also stay scoped: neither recording has sealed
        // anything, so each sees only its own row.
        let live1 = db.live_wal(r1, "cpu_usage").unwrap();
        assert_eq!(live1.len(), 1);
        assert_eq!(live1[0].ts, 10);
    }
}
