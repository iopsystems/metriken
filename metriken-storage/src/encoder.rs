//! [`Encoder`]: the segment encoder for metriken archives. The writer seals
//! with it; a copy, a dump, a report or a compaction of an archive encodes
//! a stream's live tail with it. Outside the `write` feature, so a reader
//! build (the browser viewer) can copy an archive too.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::occupants::{self, Occupant};
use crate::wal::{self, WalRowSource};
use dendro::archive::WalRow as DWalRow;
use dendro::segment::{EncodeResult, Segment, SegmentEncoder};

use crate::{default_compression, segment_props as sealed_props};

/// The encoding this module writes, recorded as dendro's `ENCODER` so a
/// reader can refuse one it does not know.
pub const ENCODER_VERSION: &str = "metriken-archive/1";

/// The encoder versions this crate's reader decodes: a source's WAL rows
/// are in one of these encodings. [`ArchiveReader`](crate::ArchiveReader)
/// refuses a source whose `encoder` key names any other, rather than
/// misreading its live tail. A source without the key (a `.rez`, or an
/// archive written before the key) is not checked.
pub const READABLE_ENCODERS: &[&str] = &[ENCODER_VERSION];

/// The streams written long, shared between the recorders (which decide)
/// and the encoder (which seals them on dendro's writer thread).
pub(crate) type LongStreams = Arc<Mutex<HashSet<String>>>;

/// The segment encoder: a long table's rows become a long segment, an
/// occupant stream's rows an occupant segment, anything else a wide table.
pub struct Encoder {
    long: LongStreams,
    sort_long: bool,
    /// Writer properties for every segment this encoder seals.
    props: parquet::file::properties::WriterProperties,
}

impl Encoder {
    /// The writer's encoder: `long` is shared with its recorders, which
    /// decide which streams are long as they ingest.
    #[cfg_attr(not(feature = "write"), allow(dead_code))]
    pub(crate) fn new(
        long: LongStreams,
        sort_long: bool,
        props: parquet::file::properties::WriterProperties,
    ) -> Self {
        Self {
            long,
            sort_long,
            props,
        }
    }

    /// The encoder for an archive this process is not writing: a copy, a
    /// ranged dump, a compaction. `streams` is the archive's stream list
    /// (`Archive::all_streams`, every source's); a stream is long when its
    /// occupant stream is among them, which is how a reader decides too.
    ///
    /// The writer's own encoder learns which streams are long from its
    /// recorders; a second process has only the archive to go on.
    pub fn for_streams<'a>(streams: impl IntoIterator<Item = &'a str>) -> Self {
        let long = streams
            .into_iter()
            .filter_map(occupants::table_of)
            .map(str::to_string)
            .collect();
        Self {
            long: Arc::new(Mutex::new(long)),
            sort_long: false,
            props: sealed_props(default_compression()),
        }
    }
}

/// A dendro WAL row, as metriken-storage's materialization reads one.
struct Row<'a>(&'a DWalRow);

impl WalRowSource for Row<'_> {
    fn ts(&self) -> u64 {
        self.0.ts.max(0) as u64
    }

    fn wall_offset(&self) -> i64 {
        self.0.wall_offset
    }

    fn row(&self) -> &[u8] {
        &self.0.row
    }
}

pub(crate) fn boxed(e: impl std::fmt::Display) -> Box<dyn std::error::Error + Send + Sync> {
    e.to_string().into()
}

impl SegmentEncoder for Encoder {
    fn encode(&self, stream: &str, rows: &[DWalRow]) -> EncodeResult {
        let Some(last) = rows.last() else {
            return Ok(None);
        };
        let last_ts = last.ts;
        if occupants::table_of(stream).is_some() {
            let mut decoded: Vec<(u64, Occupant)> = Vec::new();
            for r in rows {
                for o in occupants::decode_wal_row(&r.row).map_err(boxed)? {
                    decoded.push((r.ts.max(0) as u64, o));
                }
            }
            if decoded.is_empty() {
                return Ok(None);
            }
            let refs: Vec<(u64, &Occupant)> = decoded.iter().map(|(t, o)| (*t, o)).collect();
            let bytes = occupants::encode_segment(&refs, self.props.clone()).map_err(boxed)?;
            return Ok(Some(Segment {
                bytes,
                rows: rows.len() as u64,
                first_ts: rows[0].ts,
                last_ts,
                index: None,
            }));
        }
        let adapted: Vec<Row<'_>> = rows.iter().map(Row).collect();
        let long = self
            .long
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(stream);
        let tail = if long {
            wal::materialize_long_wal_tail_with(
                stream,
                &adapted,
                self.sort_long,
                self.props.clone(),
            )
        } else {
            wal::materialize_wal_tail_with(stream, &adapted, self.props.clone())
        }
        .map_err(boxed)?;
        // dendro counts the WAL rows a segment consumes; a long segment has
        // one parquet row per occupant, so `t.rows` is not that count.
        // The names fingerprint, so a reader probes one footer per distinct
        // set of metric names rather than only the table's first.
        Ok(tail.map(|t| Segment {
            index: crate::names::index_of(&t.bytes),
            bytes: t.bytes,
            rows: rows.len() as u64,
            first_ts: rows[0].ts,
            last_ts,
        }))
    }

    fn version(&self) -> Option<&str> {
        Some(ENCODER_VERSION)
    }

    /// A part of a split seal (dendro's `seal_capped`), after `before`.
    ///
    /// The writer puts a group's schema, a long table's columns and a
    /// sampler cell's metadata only on the first row since the last seal
    /// that needs them, so a part that starts after that row would decode
    /// with no schema (its rows skipped) or with columns that lost their
    /// labels. The part is anchored as the writer would have anchored it
    /// had it sealed there, from what `before` carried, and then encoded.
    fn encode_after(&self, stream: &str, before: &[DWalRow], rows: &[DWalRow]) -> EncodeResult {
        // An occupant stream's rows each carry their own labels.
        if occupants::table_of(stream).is_some() {
            return self.encode(stream, rows);
        }
        let long = self
            .long
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(stream);
        let anchored = if long {
            anchor_long(before, rows)
        } else if wal::is_group_table_key(stream) {
            anchor_group(before, rows)
        } else {
            anchor_cells(before, rows)
        }
        .map_err(boxed)?;
        match anchored {
            Some(rows) => self.encode(stream, &rows),
            None => self.encode(stream, rows),
        }
    }
}

/// `rows` with a copy of `first` in place of the first row.
fn replace_first(rows: &[DWalRow], first: Vec<u8>) -> Vec<DWalRow> {
    let mut out = rows.to_vec();
    out[0].row = first;
    out
}

/// A schema and its hash, as a group or long row carries them.
type Anchor = ((u64, u64), crate::schema::GroupSchema);

/// The schema in force after `before`: the last one a row carried. A
/// group's and a long table's materialization both take the schema of the
/// last row that carried one, so this is the one `rows[0]` would decode
/// against had it been sealed with `before`.
fn last_schema<T>(
    before: &[DWalRow],
    decode: impl Fn(&[u8]) -> Result<T, String>,
    schema: impl Fn(T) -> Option<Anchor>,
) -> Result<Option<Anchor>, String> {
    for r in before.iter().rev() {
        if let Some(found) = schema(decode(&r.row)?) {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// A group part whose first row has no schema, given the schema in force
/// when its hash matches. `None` when nothing needs changing.
fn anchor_group(before: &[DWalRow], rows: &[DWalRow]) -> Result<Option<Vec<DWalRow>>, String> {
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let head = wal::decode_wal_group_row(&first.row)?;
    if head.schema.is_some() {
        return Ok(None);
    }
    let found = last_schema(before, wal::decode_wal_group_row, |r| {
        r.schema.map(|s| (r.schema_hash, s))
    })?;
    match found {
        Some((hash, schema)) if hash == head.schema_hash => Ok(Some(replace_first(
            rows,
            wal::encode_wal_group_row_with_schema(&first.row, &schema)?,
        ))),
        _ => Ok(None),
    }
}

/// [`anchor_group`] for a long table's rows.
fn anchor_long(before: &[DWalRow], rows: &[DWalRow]) -> Result<Option<Vec<DWalRow>>, String> {
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let mut head = wal::decode_wal_long_row(&first.row)?;
    if head.schema.is_some() {
        return Ok(None);
    }
    let found = last_schema(before, wal::decode_wal_long_row, |r| {
        r.schema.map(|s| (r.schema_hash, s))
    })?;
    match found {
        Some((hash, schema)) if hash == head.schema_hash => {
            head.schema = Some(schema);
            Ok(Some(replace_first(rows, wal::encode_wal_long_row(&head)?)))
        }
        _ => Ok(None),
    }
}

/// A sampler table's part, with each cell that is the first of its metric
/// in the part given the metadata the metric last carried in `before`, when
/// it carries none. `None` when nothing needs changing.
fn anchor_cells(before: &[DWalRow], rows: &[DWalRow]) -> Result<Option<Vec<DWalRow>>, String> {
    let mut latest: std::collections::HashMap<String, std::collections::BTreeMap<String, String>> =
        std::collections::HashMap::new();
    for r in before {
        for cell in wal::decode_wal_row(&r.row)? {
            if let Some(m) = cell.metadata {
                latest.insert(cell.name, m);
            }
        }
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Option<Vec<DWalRow>> = None;
    for (i, r) in rows.iter().enumerate() {
        let mut cells = wal::decode_wal_row(&r.row)?;
        let mut changed = false;
        for cell in &mut cells {
            if !seen.insert(cell.name.clone()) || cell.metadata.is_some() {
                continue;
            }
            if let Some(m) = latest.get(&cell.name) {
                cell.metadata = Some(m.clone());
                changed = true;
            }
        }
        if changed {
            out.get_or_insert_with(|| rows.to_vec())[i].row = wal::encode_wal_row(&cells)?;
        }
    }
    Ok(out)
}
