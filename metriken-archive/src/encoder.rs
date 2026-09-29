//! [`Encoder`]: the segment encoder for metriken archives. The writer seals
//! with it; a copy, a dump, a report or a compaction of an archive encodes
//! a stream's live tail with it. Outside the `write` feature, so a reader
//! build (the browser viewer) can copy an archive too.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use dendro::archive::WalRow as DWalRow;
use dendro::segment::{EncodeResult, Segment, SegmentEncoder};
use metriken_segment::occupants::{self, Occupant};
use metriken_segment::wal::{self, WalRowSource};

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

/// A dendro WAL row, as metriken-segment's materialization reads one.
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
}
