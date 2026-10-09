//! The write-ahead log's row format: what a row holds before it is sealed
//! into a segment, and the materialization that turns a live WAL tail back
//! into a parquet segment. Moved from rezolus's `crates/rez` (phase 2 of
//! `docs/journal/2026-09-28-high-cardinality-stack.md`).
//!
//! Materializing a tail is a READ operation (every reader of a live archive
//! does it, including one in a browser), so nothing here needs a metrics
//! registry. Building a row from a snapshot is the producer's, in
//! `metriken-exposition`.

use std::collections::HashMap;

use tracing::warn;

use crate::builder::{
    Cell, CellValue, GroupTableBuilder, TableBuilder, HISTOGRAM_BUCKET_BYTES, VALUE_SLOT_BYTES,
    WINDOW_SLOT_BYTES,
};
use crate::table::{segment_writer_props, write_table_parquet_with};
use crate::window::Window;
use parquet::file::properties::WriterProperties;

pub use metriken_model::wal::{
    decode_wal_group_row, decode_wal_long_row, decode_wal_row, encode_wal_group_row,
    encode_wal_group_row_with_schema, encode_wal_long_row, encode_wal_row, LongOccupant, WalCell,
    WalGroupRow, WalLongRow, WalValue,
};

/// A WAL row as materialization needs it: when it was taken and its
/// encoded payload. A container implements this for its own row type, so
/// rows are materialized where they are, without copying.
pub trait WalRowSource {
    /// The row's timestamp, nanoseconds since the Unix epoch.
    fn ts(&self) -> u64;
    /// The wall-clock reading minus `ts`, nanoseconds.
    fn wall_offset(&self) -> i64;
    /// The encoded row: a `WalGroupRow` for a group table, `WalCell`s for a
    /// table of individually windowed metrics.
    fn row(&self) -> &[u8];
}

impl<T: WalRowSource + ?Sized> WalRowSource for &T {
    fn ts(&self) -> u64 {
        (**self).ts()
    }

    fn wall_offset(&self) -> i64 {
        (**self).wall_offset()
    }

    fn row(&self) -> &[u8] {
        (**self).row()
    }
}

/// Encode a sampler's live WAL rows as one parquet segment — `None` when there
/// is no tail.
///
/// **This replays the rows through `TableBuilder::push_row`, the same call
/// `ingest` makes, rather than assembling columns directly.** That is not
/// stylistic. `WalCell::metadata` is the snapshot *entry's* metadata and does
/// not carry `metric_type` — `push_row` injects it. A tail built by copying
/// that metadata into a `Column` yields a segment a natively sealed one
/// does not match, and `read_table_parquet` then reads every gauge back as a
/// counter. Going through the writer's own call makes the two shapes identical
/// by construction instead of by careful duplication.
///
/// Metadata is carried only on the first WAL row in which a metric appears in
/// the current segment's WAL span, and `push_row` reads a column's metadata
/// only when it first creates that column — so passing each cell's metadata
/// through verbatim is exactly right: the first mention establishes the
/// column, later mentions are ignored.
fn materialize_sampler_wal_tail(
    sampler: &str,
    rows: &[impl WalRowSource],
    props: WriterProperties,
) -> Result<Option<MaterializedTail>, Box<dyn std::error::Error>> {
    if rows.is_empty() {
        return Ok(None);
    }
    // Never skips a row (unlike the group path, below) — every row in `rows`
    // ends up in the materialized table, so its extent IS `rows`' own span.
    let first_ts = rows[0].ts();
    let row_count = rows.len() as u64;
    let mut builder = TableBuilder::new(sampler.to_string());
    for row in rows {
        // Decoded once into owned parts, then borrowed as `Cell`s in the
        // cells' original order — column order is `push_row`'s insertion
        // order, so preserving it is what keeps a materialized segment's
        // schema in the same order a natively sealed one has.
        let decoded = decode_wal_row(row.row())?;
        let mut names: Vec<String> = Vec::with_capacity(decoded.len());
        let mut metas: Vec<HashMap<String, String>> = Vec::with_capacity(decoded.len());
        let mut windows: Vec<Option<Window>> = Vec::with_capacity(decoded.len());
        // Scalars are copied out; histograms are rebuilt (and thereby
        // VALIDATED) into a side vector the cells borrow from.
        let mut scalars: Vec<Option<WalValue>> = Vec::with_capacity(decoded.len());
        let mut hists: Vec<Option<histogram::Histogram>> = Vec::with_capacity(decoded.len());
        for cell in decoded {
            names.push(cell.name.clone());
            metas.push(
                cell.metadata
                    .map(|m| m.into_iter().collect())
                    .unwrap_or_default(),
            );
            windows.push(cell.window.map(|(begin, end)| Window::new(begin, end)));
            match cell.value {
                WalValue::Histogram(grouping_power, max_value_power, buckets) => {
                    // The H2 config travels with the buckets, so nothing has to
                    // be recovered from the metadata row.
                    let h = histogram::Histogram::from_buckets(
                        grouping_power,
                        max_value_power,
                        buckets,
                    )
                    .map_err(|e| {
                        format!(
                            "failed to rebuild the {sampler} histogram {}: {e}",
                            cell.name
                        )
                    })?;
                    scalars.push(None);
                    hists.push(Some(h));
                }
                v => {
                    scalars.push(Some(v));
                    hists.push(None);
                }
            }
        }
        let cells: Vec<Cell<'_>> = (0..names.len())
            .map(|i| Cell {
                name: &names[i],
                metadata: &metas[i],
                window: windows[i],
                value: match (&scalars[i], &hists[i]) {
                    (Some(WalValue::Counter(v)), _) => CellValue::Counter(*v),
                    (Some(WalValue::Gauge(v)), _) => CellValue::Gauge(*v),
                    (_, Some(h)) => CellValue::Histogram(h),
                    // `scalars[i]` and `hists[i]` are filled as a pair above:
                    // exactly one of them is `Some` for every index.
                    _ => unreachable!("every decoded WAL cell is a scalar or a histogram"),
                },
            })
            .collect();
        builder.push_row(row.ts(), row.wall_offset(), &cells);
    }
    Ok(Some(MaterializedTail {
        bytes: write_table_parquet_with(&builder.finish(), props)?,
        rows: row_count,
        first_ts,
    }))
}

/// A materialized segment's bytes plus the actual extent of INPUT rows that
/// went into it — which can differ from the caller's own `rows` slice for a
/// V3 group table whose leading rows were skipped as un-anchored (see
/// `materialize_group_wal_tail`). `materialize_sampler_wal_tail` never
/// skips, so its `rows`/`first_ts` are always the input slice's own span —
/// this type exists so both paths report the same two facts uniformly and a
/// caller (`seal_batch`) never has to know which one ran.
///
/// **`last_ts` is deliberately NOT here.** Unlike `first_ts`/`rows`, the
/// input slice's OWN last row's timestamp is always correct as a segment's
/// `last_ts` even when leading rows were skipped: a V3 group's un-anchored
/// run is always a LEADING prefix (retention removes a prefix, never punches
/// a hole — `RezDb::evict_before`'s doc), so the last input row is never
/// itself skipped. Callers already have that timestamp from the `WalRow`s
/// they read; duplicating it here would just be a second place for it to
/// drift from the one that is actually used.
#[derive(Debug, PartialEq)]
pub struct MaterializedTail {
    pub bytes: Vec<u8>,
    pub rows: u64,
    pub first_ts: u64,
}

/// True for a V3 acquisition-group table key (`"<sampler>/<group>"`); false
/// for a V1/V2 sampler table key, which never contains `/` for every
/// REGISTERED sampler of this build (see `group_by_sampler`'s `sampler_of`
/// and `no_registered_sampler_name_contains_a_slash`, below). `sampler_of`
/// itself reads the `"sampler"` metadata key straight off the wire,
/// unvalidated — a hostile or merely unusual endpoint could in principle
/// send a value containing `/` — which is exactly why this convention is
/// backed by more than good naming: see the fail-closed backstop below.
///
/// This is the ONLY discriminator available to [`materialize_wal_tail`]: a
/// WAL row is an opaque BLOB keyed only by this string (see the WAL-key
/// design note on rezolus's `StreamRecorderV3`), so there is nowhere else to look —
/// no separate "table kind" column, and a fresh reader process (rezolus's reader
/// opening a `.rez` some other process is still writing) has no in-memory
/// state from the writer to consult either. It is safe because the two
/// row shapes cannot be mistaken for one another even if this guess were
/// wrong: `decode_wal_group_row`/`decode_wal_row` decode structurally
/// different msgpack shapes (a `WalGroupRow` struct vs. an array of
/// `WalCell`s) and error rather than silently misinterpreting the bytes.
///
/// The convention itself is enforced at debug build time by a
/// `debug_assert!` at each end (`ingest`'s V1/V2 loop and `ingest_v3`'s
/// group loop, both in `StreamRecorderV3`) plus
/// `no_registered_sampler_name_contains_a_slash` pinning the invariant
/// against every registered `SAMPLERS` entry; the structural non-aliasing
/// above is the release-build backstop if that is ever violated anyway.
pub fn is_group_table_key(table_key: &str) -> bool {
    table_key.contains('/')
}

/// The rows of a live tail that name every column it holds: for a group or
/// long table, each row that anchors a schema; for a table of cells, each
/// row whose cell names differ from the row before. Materialized alone, they
/// make a segment with the tail's columns and a few of its rows, which is
/// what a reader needs to learn the tail's metric names without building the
/// whole tail.
///
/// A group or long row is read only as far as its schema: the values that
/// follow are never decoded.
pub fn schema_rows<'a, R: WalRowSource>(
    table_key: &str,
    long: bool,
    rows: &'a [R],
) -> Result<Vec<&'a R>, String> {
    let mut out = Vec::new();
    if long || is_group_table_key(table_key) {
        for row in rows {
            if anchors_schema(row.row())? {
                out.push(row);
            }
        }
    } else {
        let mut last: Option<Vec<String>> = None;
        for row in rows {
            let names: Vec<String> = decode_wal_row(row.row())?
                .into_iter()
                .map(|c| c.name)
                .collect();
            if last.as_ref() != Some(&names) {
                out.push(row);
                last = Some(names);
            }
        }
    }
    Ok(out)
}

/// Whether an encoded [`WalGroupRow`] or [`WalLongRow`] carries its schema.
/// Both are msgpack arrays that begin `[schema_hash, schema, ...]`, and
/// `schema` is nil unless the row anchors it.
fn anchors_schema(mut bytes: &[u8]) -> Result<bool, String> {
    let err = |e: &dyn std::fmt::Display| format!("failed to read a WAL row's schema: {e}");
    rmp::decode::read_array_len(&mut bytes).map_err(|e| err(&e))?;
    let n = rmp::decode::read_array_len(&mut bytes).map_err(|e| err(&e))?;
    for _ in 0..n {
        rmp::decode::read_int::<u64, _>(&mut bytes).map_err(|e| err(&e))?;
    }
    match bytes.first() {
        Some(&b) => Ok(b != rmp::Marker::Null.to_u8()),
        None => Err(err(&"the row ends before its schema")),
    }
}

/// Encode a `.rez` table's live WAL rows as one parquet segment — dispatches
/// on [`is_group_table_key`] to the V3 group-row path or the V1/V2
/// sampler-cell path. `None` when there is no tail.
///
/// Both the writer thread (`seal_batch`) and a completely independent reader
/// process (rezolus's reader, opening a `.rez` some other process is still
/// writing) call this — neither has access to `StreamRecorderV3`'s in-memory
/// schema cache, which is why a V3 group's WAL rows must be self-sufficient
/// (see [`WalGroupRow`]).
pub fn materialize_wal_tail(
    table_key: &str,
    rows: &[impl WalRowSource],
) -> Result<Option<MaterializedTail>, Box<dyn std::error::Error>> {
    materialize_wal_tail_with(table_key, rows, segment_writer_props())
}

/// [`materialize_wal_tail`] with the caller's writer properties: a writer
/// sealing with another codec. A reader rebuilding a tail in memory keeps the
/// default, which is the faster to encode.
pub fn materialize_wal_tail_with(
    table_key: &str,
    rows: &[impl WalRowSource],
    props: WriterProperties,
) -> Result<Option<MaterializedTail>, Box<dyn std::error::Error>> {
    if is_group_table_key(table_key) {
        materialize_group_wal_tail(table_key, rows, props)
    } else {
        materialize_sampler_wal_tail(table_key, rows, props)
    }
}

/// Encode a V3 acquisition-group's live WAL rows as one parquet segment —
/// `None` when there is no tail (including a tail every row of which had to
/// be skipped — see below). See [`WalGroupRow`] for why a decode walk needs
/// no external schema cache: it carries the current schema forward across
/// rows, normally requiring a `schema: Some` row before the first row that
/// needs it (an invariant `StreamRecorderV3::ingest_v3` upholds by always
/// anchoring a group's very first WAL row) — "normally" because retention
/// can delete that anchor out from under a still-live span (see below).
///
/// **Un-anchored rows degrade, they do not fail the recording.** Hindsight
/// retention (`RezDb::evict_before`) deletes WAL rows purely by `ts <
/// cutoff`, with no awareness of which row anchors a group's schema — a
/// `duration` under the seal policy's `max_age` (300s default) can delete a
/// group's anchor row while its later, still-live rows survive. Erroring
/// here on the resulting `schema: None` row with no matching anchor would
/// propagate through `seal_batch` and kill the writer thread — a live,
/// still-recording hindsight buffer going instantly and permanently
/// unreadable over a retention/seal-cadence interaction, not a corrupt
/// input. V1/V2 has no equivalent failure mode here (a column simply
/// rebuilds with whatever metadata its own surviving WAL span re-anchors),
/// so V3 matches that degrade-not-die posture: an un-anchored row — no
/// current anchor, or a hash that does not match the current one (the same
/// symptom a multi-anchor eviction gap would produce) — is skipped with a
/// rate-limited warning rather than erroring, and materialization resumes
/// from the next row that DOES carry a resolvable schema. A tail with no
/// resolvable row at all yields `None`, the same as an empty tail.
fn materialize_group_wal_tail(
    table_key: &str,
    rows: &[impl WalRowSource],
    props: WriterProperties,
) -> Result<Option<MaterializedTail>, Box<dyn std::error::Error>> {
    if rows.is_empty() {
        return Ok(None);
    }
    let mut builder = GroupTableBuilder::new(table_key.to_string());
    let mut current: Option<((u64, u64), crate::schema::GroupSchema)> = None;
    let mut warned_unanchored = false;
    // The ts of the first row actually pushed — `None` until then. This is
    // what makes a catalog `SegmentMeta::first_ts` correct even when a
    // leading un-anchored run was skipped: it is NOT `rows[0].ts` (the raw
    // WAL span's own start) unless nothing was skipped.
    let mut first_ts: Option<u64> = None;
    for row in rows {
        let decoded = decode_wal_group_row(row.row())?;
        let schema = match decoded.schema {
            Some(s) => {
                current = Some((decoded.schema_hash, s));
                &current.as_ref().unwrap().1
            }
            None => match &current {
                Some((hash, s)) if *hash == decoded.schema_hash => s,
                _ => {
                    // No schema anchored yet, or the last anchor's hash does
                    // not match this row's — either way there is nothing to
                    // decode this row's values against. Skip it (and update
                    // no state), warning once per materialization so a
                    // retention-driven gap is visible without spamming.
                    if !warned_unanchored {
                        warn!(
                            "group {table_key} WAL tail row at ts={} has no matching schema \
                             anchor (likely evicted by retention); skipping until the next \
                             anchored row (warned once)",
                            row.ts()
                        );
                        warned_unanchored = true;
                    }
                    continue;
                }
            },
        };
        let window = decoded.window.map(|(begin, end)| Window::new(begin, end));
        builder.push_row(
            row.ts(),
            row.wall_offset(),
            window,
            schema,
            &decoded.counters,
            &decoded.gauges,
            &decoded.histograms,
        );
        first_ts.get_or_insert(row.ts());
    }
    let row_count = builder.rows() as u64;
    if row_count == 0 {
        return Ok(None);
    }
    Ok(Some(MaterializedTail {
        bytes: write_table_parquet_with(&builder.finish(), props)?,
        rows: row_count,
        // `row_count > 0` implies the loop pushed at least one row, which is
        // exactly when `first_ts` gets set — never `None` here.
        first_ts: first_ts.expect("a non-empty materialized table has a first pushed row"),
    }))
}

/// A WAL group row's cost against a writer's segment byte budget: one
/// window slot for the row, a value slot per present member, and a bucket
/// slot per histogram bucket. `metriken-exposition`'s `group_approx_bytes`
/// meters a `GroupSnapshot` the same way, and a test there pins the two, so
/// a recording taken off the stream seals where a scraped one does.
pub fn wal_group_row_approx_bytes(row: &WalGroupRow) -> usize {
    let mut bytes = WINDOW_SLOT_BYTES;
    bytes += row.counters.iter().filter(|v| v.is_some()).count() * VALUE_SLOT_BYTES;
    bytes += row.gauges.iter().filter(|v| v.is_some()).count() * VALUE_SLOT_BYTES;
    for (_, _, buckets) in row.histograms.iter().flatten() {
        bytes += VALUE_SLOT_BYTES + buckets.len() * HISTOGRAM_BUCKET_BYTES;
    }
    bytes
}

/// A long row's cost against a writer's segment byte budget: one window
/// slot, and per occupant an occupant slot plus its present values.
pub fn wal_long_row_approx_bytes(row: &WalLongRow) -> usize {
    let mut bytes = WINDOW_SLOT_BYTES;
    for o in &row.occupants {
        bytes += VALUE_SLOT_BYTES;
        bytes += o.counters.iter().filter(|v| v.is_some()).count() * VALUE_SLOT_BYTES;
        bytes += o.gauges.iter().filter(|v| v.is_some()).count() * VALUE_SLOT_BYTES;
        for (_, _, buckets) in o.histograms.iter().flatten() {
            bytes += VALUE_SLOT_BYTES + buckets.len() * HISTOGRAM_BUCKET_BYTES;
        }
    }
    bytes
}

/// A long table's live WAL rows as one long segment (`None` when there is no
/// tail). A table is long when it has an occupant stream, which is how a
/// reader chooses this over [`materialize_wal_tail`]. Rows with no matching
/// schema anchor are skipped with a warning, as for a group table.
pub fn materialize_long_wal_tail(
    table_key: &str,
    rows: &[impl WalRowSource],
    sort: bool,
) -> Result<Option<MaterializedTail>, Box<dyn std::error::Error>> {
    materialize_long_wal_tail_with(table_key, rows, sort, segment_writer_props())
}

/// [`materialize_long_wal_tail`] with the caller's writer properties, as
/// [`materialize_wal_tail_with`].
pub fn materialize_long_wal_tail_with(
    table_key: &str,
    rows: &[impl WalRowSource],
    sort: bool,
    props: WriterProperties,
) -> Result<Option<MaterializedTail>, Box<dyn std::error::Error>> {
    let mut builder = crate::long_table::LongTableBuilder::new();
    let mut current: Option<((u64, u64), crate::schema::GroupSchema)> = None;
    let mut warned = false;
    let mut first_ts: Option<u64> = None;
    for row in rows {
        let decoded = decode_wal_long_row(row.row())?;
        if let Some(s) = decoded.schema {
            current = Some((decoded.schema_hash, s));
        }
        let schema = match &current {
            Some((hash, s)) if *hash == decoded.schema_hash => s,
            _ => {
                if !warned {
                    warn!(
                        "long table {table_key} WAL row at ts={} has no matching schema anchor; \
                         skipping until the next anchored row (warned once)",
                        row.ts()
                    );
                    warned = true;
                }
                continue;
            }
        };
        if decoded.occupants.is_empty() {
            continue;
        }
        builder.push_tick(
            row.ts(),
            row.wall_offset(),
            decoded.window.map(|(b, e)| Window::new(b, e)),
            schema,
            &decoded.occupants,
        );
        first_ts.get_or_insert(row.ts());
    }
    let rows = builder.rows() as u64;
    if rows == 0 {
        return Ok(None);
    }
    Ok(Some(MaterializedTail {
        bytes: builder.finish_with(sort, props)?,
        rows,
        first_ts: first_ts.expect("a non-empty table has a first row"),
    }))
}

#[cfg(test)]
mod schema_rows_tests {
    use super::*;
    use crate::schema::{GroupSchema, MetricDesc};

    struct Row(u64, Vec<u8>);
    impl WalRowSource for Row {
        fn ts(&self) -> u64 {
            self.0
        }
        fn wall_offset(&self) -> i64 {
            0
        }
        fn row(&self) -> &[u8] {
            &self.1
        }
    }

    fn schema(names: &[&str]) -> GroupSchema {
        GroupSchema {
            counters: names
                .iter()
                .map(|n| MetricDesc {
                    name: n.to_string(),
                    metadata: [("metric".to_string(), n.to_string())].into(),
                })
                .collect(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        }
    }

    fn group(ts: u64, s: Option<GroupSchema>, n: usize) -> Row {
        let hash = (u64::MAX, ts);
        let row = WalGroupRow {
            schema_hash: hash,
            schema: s,
            window: Some((ts, ts + 1)),
            counters: vec![Some(u64::MAX); n],
            gauges: Vec::new(),
            histograms: Vec::new(),
        };
        Row(ts, encode_wal_group_row(&row).unwrap())
    }

    /// Only the rows that carry a schema are kept, in a group table and a
    /// long one; a large hash and values decode as ints of any width.
    #[test]
    fn group_and_long_rows_are_kept_when_they_anchor_a_schema() {
        let rows = vec![
            group(1, Some(schema(&["a"])), 1),
            group(2, None, 1),
            group(3, Some(schema(&["a", "b"])), 2),
            group(4, None, 2),
        ];
        let kept: Vec<u64> = schema_rows("s/g", false, &rows)
            .unwrap()
            .iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(kept, vec![1, 3]);

        let long = |ts: u64, s: Option<GroupSchema>| {
            let row = WalLongRow {
                schema_hash: (7, 300),
                schema: s,
                window: None,
                occupants: vec![LongOccupant {
                    occupant: 0,
                    counters: vec![Some(1)],
                    gauges: Vec::new(),
                    histograms: Vec::new(),
                }],
            };
            Row(ts, encode_wal_long_row(&row).unwrap())
        };
        let rows = vec![long(1, Some(schema(&["a"]))), long(2, None)];
        let kept: Vec<u64> = schema_rows("s", true, &rows)
            .unwrap()
            .iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(kept, vec![1]);
    }

    /// A table of cells keeps each row whose names differ from the row
    /// before: the first, and every change.
    #[test]
    fn cell_rows_are_kept_when_their_names_change() {
        let cell = |name: &str| WalCell {
            name: name.to_string(),
            metadata: None,
            value: WalValue::Counter(1),
            window: None,
        };
        let row = |ts: u64, names: &[&str]| {
            let cells: Vec<WalCell> = names.iter().map(|n| cell(n)).collect();
            Row(ts, encode_wal_row(&cells).unwrap())
        };
        let rows = vec![
            row(1, &["a"]),
            row(2, &["a"]),
            row(3, &["a", "b"]),
            row(4, &["a", "b"]),
            row(5, &["a"]),
        ];
        let kept: Vec<u64> = schema_rows("sampler", false, &rows)
            .unwrap()
            .iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(kept, vec![1, 3, 5]);
    }
}
