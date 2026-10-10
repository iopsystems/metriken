//! What the reader needs from an archive's container.
//!
//! A container holds sources (a recording each), streams (a table each, keyed
//! `sampler` or `sampler/group`), sealed parquet segments, a WAL of unsealed
//! rows and a store of caller rows. dendro archives implement [`Catalog`] here
//! ([`DendroCatalog`]); another container (rezolus's `.rez`) implements it
//! where it lives.

use metriken_storage::wal::WalRowSource;

/// One source (a recording) of an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub id: i64,
    pub labels: std::collections::BTreeMap<String, String>,
    pub metadata: std::collections::BTreeMap<String, String>,
    /// Wall-clock nanoseconds the source's monotonic timestamps anchor to.
    pub clock_anchor_wall_ns: u64,
    /// Whether the source was cleanly finalized.
    pub complete: bool,
}

/// A sealed segment's catalog entry. No bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentMeta {
    pub rows: u64,
    pub first_ts: u64,
    pub last_ts: u64,
}

/// How many rows a table holds and the span they cover, from the catalog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub rows: u64,
    pub first_ts: Option<u64>,
    pub last_ts: Option<u64>,
}

/// One unsealed row of a table's WAL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalRow {
    pub ts: u64,
    pub wall_offset: i64,
    pub row: Vec<u8>,
}

impl WalRowSource for WalRow {
    fn ts(&self) -> u64 {
        self.ts
    }

    fn wall_offset(&self) -> i64 {
        self.wall_offset
    }

    fn row(&self) -> &[u8] {
        &self.row
    }
}

/// A sealed segment's sequence number and its caller index, if it has one.
pub type SegmentIndex = (u64, Option<Vec<u8>>);

/// The read side of an archive's container.
///
/// `Send` so a catalog can sit behind the `Mutex` a byte-backed archive's
/// tables share: a SQLite connection is `Send` but not `Sync`.
pub trait Catalog: Send {
    /// Every source, in id order.
    fn sources(&self) -> Result<Vec<Source>, String>;

    /// Every table of a source that has sealed segments or WAL rows.
    fn tables(&self, source_id: i64) -> Result<Vec<String>, String>;

    /// A table's sealed segments, `(seq, meta)`, oldest first. No bytes.
    fn segment_meta(&self, source_id: i64, table: &str) -> Result<Vec<(u64, SegmentMeta)>, String>;

    /// One sealed segment's bytes; `None` when it no longer exists.
    fn segment_bytes(
        &self,
        source_id: i64,
        table: &str,
        seq: u64,
    ) -> Result<Option<Vec<u8>>, String>;

    /// A table's WAL rows newer than its last sealed segment.
    fn live_wal(&self, source_id: i64, table: &str) -> Result<Vec<WalRow>, String>;

    /// Each sealed segment's caller index, by sequence number, without the
    /// payload: `None` where a segment has none. A container that keeps no
    /// index answers empty, which the reader treats as every segment having
    /// none.
    fn segment_indexes(&self, _source_id: i64, _table: &str) -> Result<Vec<SegmentIndex>, String> {
        Ok(Vec::new())
    }

    /// A table's sealed segment count and span, from the catalog.
    fn segment_span(&self, source_id: i64, table: &str) -> Result<(u64, Span), String>;

    /// The span of [`live_wal`](Self::live_wal), from the catalog.
    fn live_wal_span(&self, source_id: i64, table: &str) -> Result<Span, String>;

    /// Every stream a source holds caller rows under.
    fn caller_row_streams(&self, source_id: i64) -> Result<Vec<String>, String>;

    /// A stream's caller rows with `from <= ts <= to`, oldest first.
    fn caller_rows(
        &self,
        source_id: i64,
        stream: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, String>;

    /// The timestamp of the newest caller row at or before `upto` that
    /// `pred` accepts.
    fn last_caller_row_at_or_before(
        &self,
        source_id: i64,
        stream: &str,
        upto: u64,
        pred: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<Option<u64>, String>;
}

/// A dendro archive.
pub struct DendroCatalog(pub dendro::archive::Archive);

impl DendroCatalog {
    /// Open the archive at `path` read-only.
    pub fn open(path: &std::path::Path) -> Result<Self, String> {
        dendro::archive::Archive::open(path)
            .map(Self)
            .map_err(|e| e.to_string())
    }

    /// Open an archive held as bytes (a browser upload).
    pub fn open_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        dendro::archive::Archive::open_bytes(bytes)
            .map(Self)
            .map_err(|e| e.to_string())
    }

    /// Whether `path` is a dendro archive, by its header.
    pub fn is_archive(path: &std::path::Path) -> Result<bool, String> {
        dendro::archive::sniff(path)
            .map(|s| matches!(s, dendro::archive::Sniff::Stamped { .. }))
            .map_err(|e| e.to_string())
    }

    /// Whether `bytes` are a dendro archive, by their header.
    pub fn is_archive_bytes(bytes: &[u8]) -> bool {
        matches!(
            dendro::archive::sniff_bytes(bytes),
            dendro::archive::Sniff::Stamped { .. }
        )
    }
}

/// A dendro timestamp as an unsigned one. dendro stores `i64`; metrics
/// timestamps are nanoseconds since the epoch and never negative, so a
/// negative one is an archive this reader does not understand.
fn ts(t: i64) -> Result<u64, String> {
    u64::try_from(t).map_err(|_| format!("negative timestamp {t} in a dendro archive"))
}

/// An unsigned bound as a dendro one. Above `i64::MAX` there is nothing to
/// find, so the bound saturates.
fn bound(t: u64) -> i64 {
    i64::try_from(t).unwrap_or(i64::MAX)
}

fn span(s: dendro::archive::Span) -> Result<Span, String> {
    Ok(Span {
        rows: s.rows,
        first_ts: s.first_ts.map(ts).transpose()?,
        last_ts: s.last_ts.map(ts).transpose()?,
    })
}

fn err(e: dendro::Error) -> String {
    e.to_string()
}

impl Catalog for DendroCatalog {
    fn sources(&self) -> Result<Vec<Source>, String> {
        self.0
            .read_sources()
            .map_err(err)?
            .into_iter()
            .map(|s| {
                Ok(Source {
                    id: s.id,
                    labels: s.meta.labels,
                    metadata: s.meta.metadata,
                    clock_anchor_wall_ns: ts(s.meta.clock_anchor_wall_ns)?,
                    complete: s.complete,
                })
            })
            .collect()
    }

    fn tables(&self, source_id: i64) -> Result<Vec<String>, String> {
        self.0.all_streams(source_id).map_err(err)
    }

    fn segment_meta(&self, source_id: i64, table: &str) -> Result<Vec<(u64, SegmentMeta)>, String> {
        self.0
            .read_segment_meta(source_id, table)
            .map_err(err)?
            .into_iter()
            .map(|(seq, m)| {
                Ok((
                    seq,
                    SegmentMeta {
                        rows: m.rows,
                        first_ts: ts(m.first_ts)?,
                        last_ts: ts(m.last_ts)?,
                    },
                ))
            })
            .collect()
    }

    fn segment_bytes(
        &self,
        source_id: i64,
        table: &str,
        seq: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        self.0
            .read_segment_bytes(source_id, table, seq)
            .map_err(err)
    }

    fn segment_indexes(&self, source_id: i64, table: &str) -> Result<Vec<SegmentIndex>, String> {
        self.0.read_segment_indexes(source_id, table).map_err(err)
    }

    fn live_wal(&self, source_id: i64, table: &str) -> Result<Vec<WalRow>, String> {
        self.0
            .live_wal(source_id, table)
            .map_err(err)?
            .into_iter()
            .map(|r| {
                Ok(WalRow {
                    ts: ts(r.ts)?,
                    wall_offset: r.wall_offset,
                    row: r.row,
                })
            })
            .collect()
    }

    fn segment_span(&self, source_id: i64, table: &str) -> Result<(u64, Span), String> {
        let (n, s) = self.0.segment_span(source_id, table).map_err(err)?;
        Ok((n, span(s)?))
    }

    fn live_wal_span(&self, source_id: i64, table: &str) -> Result<Span, String> {
        span(self.0.live_wal_span(source_id, table).map_err(err)?)
    }

    fn caller_row_streams(&self, source_id: i64) -> Result<Vec<String>, String> {
        self.0.caller_row_streams(source_id).map_err(err)
    }

    fn caller_rows(
        &self,
        source_id: i64,
        stream: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, String> {
        self.0
            .read_caller_rows(source_id, stream, bound(from), bound(to))
            .map_err(err)?
            .into_iter()
            .map(|r| Ok((ts(r.ts)?, r.blob)))
            .collect()
    }

    fn last_caller_row_at_or_before(
        &self,
        source_id: i64,
        stream: &str,
        upto: u64,
        pred: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<Option<u64>, String> {
        self.0
            .last_caller_row_at_or_before(source_id, stream, bound(upto), pred)
            .map_err(err)?
            .map(|r| ts(r.ts))
            .transpose()
    }
}
