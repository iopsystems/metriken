//! [`Encoder`]: the segment encoder for metriken archives. The writer seals
//! with it; a copy, a dump, a report or a compaction of an archive encodes
//! a stream's live tail with it. Outside the `write` feature, so a reader
//! build (the browser viewer) can copy an archive too.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::occupants::{self, Occupant};
use crate::wal::{self, WalRowSource};
use dendro::archive::WalRow as DWalRow;
use dendro::segment::{EncodeContext, EncodeResult, Segment, SegmentEncoder};

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

/// The streams written long, as `(source id, stream)`, shared between the
/// recorders (which decide) and the encoder (which seals them on dendro's
/// writer thread). Keyed by source because each source decides for itself:
/// two sources can record a group of the same name, one long and one wide.
pub(crate) type LongStreams = Arc<Mutex<HashSet<(i64, String)>>>;

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
    /// ranged dump, a compaction. `sources` is each source's id in that
    /// archive with its stream list (`Archive::all_streams(id)`); a stream
    /// of a source is long when its occupant stream is among that source's
    /// streams, which is how a reader decides too. dendro names the source
    /// of the rows it encodes by its id in the archive they are read from,
    /// a copy's input included, so these are the input archive's ids.
    ///
    /// The writer's own encoder learns which streams are long from its
    /// recorders; a second process has only the archive to go on.
    pub fn for_sources<'a, S>(sources: impl IntoIterator<Item = (i64, S)>) -> Self
    where
        S: IntoIterator<Item = &'a str>,
    {
        let long = sources
            .into_iter()
            .flat_map(|(id, streams)| {
                streams
                    .into_iter()
                    .filter_map(occupants::table_of)
                    .map(move |s| (id, s.to_string()))
            })
            .collect();
        Self {
            long: Arc::new(Mutex::new(long)),
            sort_long: false,
            props: sealed_props(default_compression()),
        }
    }

    /// [`for_sources`](Self::for_sources) over every source of `archive`,
    /// read in one snapshot.
    ///
    /// For a copy of an archive that is still being written, build the
    /// encoder inside the snapshot the copy reads. A long stream that
    /// appears between this call and a copy taking its own snapshot, in a
    /// new source or an existing one, is unknown to the encoder, which
    /// encodes its unsealed rows as wide, and the copy fails. dendro's
    /// `read_snapshot` is re-entrant, so this call and the copy share the
    /// outer one:
    ///
    /// ```ignore
    /// src.read_snapshot(|src| {
    ///     let encoder = Encoder::for_archive(src)?;
    ///     dst.transaction(|tx| copy_sources_into(src, tx, &spec, &encoder))
    /// })?;
    /// ```
    pub fn for_archive(archive: &dendro::archive::Archive) -> dendro::Result<Self> {
        archive.read_snapshot(|archive| {
            let mut sources = Vec::new();
            for source in archive.read_sources()? {
                sources.push((source.id, archive.all_streams(source.id)?));
            }
            Ok(Self::for_sources(sources.iter().map(|(id, streams)| {
                (*id, streams.iter().map(String::as_str))
            })))
        })
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
    fn encode(&self, ctx: &EncodeContext<'_>, rows: &[DWalRow]) -> EncodeResult {
        let stream = ctx.stream;
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
            .contains(&(ctx.source_id, stream.to_string()));
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
        //
        // Every row given is claimed, including a row skipped for want of a
        // schema anchor. Such a row has no schema in the WAL and never will,
        // so leaving it live would not let a later seal decode it, and a
        // skipped row in the middle of the run, reported, is a hole dendro
        // refuses as an encoder contract violation, which stops the writer.
        // The writer states the schema on every change of hash and seals
        // a stream before retention can cut its open segment's first row,
        // (dendro retries a deferred seal before it evicts and leaves out a
        // stream it still cannot seal), so this needs a tail whose anchor
        // was lost some other way: one a 0.2.2 writer left after retention
        // cut into it, or a deferred seal whose retry before an eviction
        // failed with an error that is not retryable, after which dendro
        // evicts the stream's live rows.
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
    fn encode_after(
        &self,
        ctx: &EncodeContext<'_>,
        before: &[DWalRow],
        rows: &[DWalRow],
    ) -> EncodeResult {
        let stream = ctx.stream;
        // An occupant stream's rows each carry their own labels.
        if occupants::table_of(stream).is_some() {
            return self.encode(ctx, rows);
        }
        let long = self
            .long
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&(ctx.source_id, stream.to_string()));
        let anchored = if long {
            anchor_long(before, rows)
        } else if wal::is_group_table_key(stream) {
            anchor_group(before, rows)
        } else {
            anchor_cells(before, rows)
        }
        .map_err(boxed)?;
        match anchored {
            Some(rows) => self.encode(ctx, &rows),
            None => self.encode(ctx, rows),
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

/// The schema with hash `hash` that a row in `before` last carried.
///
/// The writer states a schema whenever a row's hash differs from the row
/// before it, so for a row it wrote this is the last schema in `before`,
/// the one the WAL decoder would use. Searching by hash rather than taking
/// the last one also anchors rows written before that rule (metriken-storage
/// 0.2.2 stated each hash once per segment), whose schema can be an earlier
/// one.
fn schema_for<T>(
    before: &[DWalRow],
    hash: (u64, u64),
    decode: impl Fn(&[u8]) -> Result<T, String>,
    schema: impl Fn(T) -> Option<Anchor>,
) -> Result<Option<crate::schema::GroupSchema>, String> {
    for r in before.iter().rev() {
        if let Some((h, s)) = schema(decode(&r.row)?) {
            if h == hash {
                return Ok(Some(s));
            }
        }
    }
    Ok(None)
}

/// A group part whose first row has no schema, given the schema of its
/// hash from `before`. `None` when nothing needs changing or `before`
/// holds no such schema.
fn anchor_group(before: &[DWalRow], rows: &[DWalRow]) -> Result<Option<Vec<DWalRow>>, String> {
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let head = wal::decode_wal_group_row(&first.row)?;
    if head.schema.is_some() {
        return Ok(None);
    }
    let found = schema_for(before, head.schema_hash, wal::decode_wal_group_row, |r| {
        r.schema.map(|s| (r.schema_hash, s))
    })?;
    match found {
        Some(schema) => Ok(Some(replace_first(
            rows,
            wal::encode_wal_group_row_with_schema(&first.row, &schema)?,
        ))),
        None => Ok(None),
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
    let found = schema_for(before, head.schema_hash, wal::decode_wal_long_row, |r| {
        r.schema.map(|s| (r.schema_hash, s))
    })?;
    match found {
        Some(schema) => {
            head.schema = Some(schema);
            Ok(Some(replace_first(rows, wal::encode_wal_long_row(&head)?)))
        }
        None => Ok(None),
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

#[cfg(test)]
mod encode_after_tests {
    use super::*;
    use crate::schema::{GroupSchema, MetricDesc};
    use crate::wal::WalGroupRow;

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
            gauge_histograms: Vec::new(),
        }
    }

    fn row(ts: i64, hash: u64, schema: Option<GroupSchema>, n: usize) -> DWalRow {
        DWalRow {
            stream: "s/g".to_string(),
            ts,
            wall_offset: 0,
            row: wal::encode_wal_group_row(&WalGroupRow {
                schema_hash: (hash, hash),
                schema,
                window: Some((ts as u64, ts as u64 + 1)),
                counters: vec![Some(ts as u64); n],
                gauges: Vec::new(),
                histograms: Vec::new(),
                gauge_histograms: Vec::new(),
            })
            .unwrap(),
        }
    }

    /// The columns of a segment's parquet bytes and its row count.
    fn shape(segment: &Segment) -> (Vec<String>, i64) {
        use parquet::file::reader::{FileReader, SerializedFileReader};
        let reader = SerializedFileReader::new(bytes::Bytes::from(segment.bytes.clone())).unwrap();
        let meta = reader.metadata().file_metadata();
        let names = meta
            .schema_descr()
            .columns()
            .iter()
            .map(|c| c.path().string())
            .filter(|n| n == "a" || n == "b")
            .collect();
        (names, meta.num_rows())
    }

    /// A part whose first row's schema was stated before another one: the
    /// WAL of a group whose schema went A, B, A as a 0.2.2 writer wrote it,
    /// stating A once. The part is anchored with A, found by its hash, not
    /// with B, the last schema stated.
    #[test]
    fn a_part_is_anchored_with_the_schema_of_its_hash() {
        let before = vec![
            row(1, 1, Some(schema(&["a"])), 1),
            // Same arity as A, so anchoring with it would decode A's value
            // as column `b` rather than fail.
            row(2, 2, Some(schema(&["b"])), 1),
            row(3, 1, None, 1),
        ];
        let part = vec![row(4, 1, None, 1), row(5, 1, None, 1)];
        let encoder = Encoder::for_sources([(1, ["s/g"])]);
        let segment = encoder
            .encode_after(&EncodeContext::new(1, "s/g"), &before, &part)
            .unwrap()
            .unwrap();
        assert_eq!(shape(&segment), (vec!["a".to_string()], 2));
        // With nothing before it, the part has no anchor and decodes to
        // nothing: the rows the anchoring recovered.
        assert!(encoder
            .encode(&EncodeContext::new(1, "s/g"), &part)
            .unwrap()
            .is_none());
    }
}
