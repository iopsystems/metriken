//! The write-ahead log's row format: what a row holds before it is sealed
//! into a segment, and its msgpack encoding. The same rows are what a
//! producer sends on dendro's replication stream. Turning rows into a parquet
//! segment is storage's (`metriken-segment`'s `wal` module).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One metric's contribution to a WAL row: exactly what
/// `TableBuilder::push_row` needs to place the value in its column, and nothing
/// else. The recorder's own `Snapshot` entry carries a good deal more, and
/// carrying it per tick would cost several times the payload for information
/// that does not change between ticks.
///
/// Encoded with `rmp_serde::to_vec`, which writes structs as ARRAYS and enums
/// as `[index, payload]`, so these field names cost nothing on the wire and are
/// chosen for readability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalCell {
    /// The snapshot entry's name — the segment's column key (`"5"`, `"5x3"`).
    /// Numeric-id strings of a few bytes, so carrying one per cell per tick is
    /// noise next to the value; dropping them and relying on positional order
    /// would not be, because cgroup metrics appear and vanish mid-recording and
    /// a positional decode would silently reattribute every later column.
    pub name: String,
    /// The snapshot **entry's** metadata, verbatim — NOT the parquet column's.
    ///
    /// The difference matters to a reader. `metric_type` is **not** in here:
    /// `TableBuilder::push_row` injects it (`rez.rs`, the `or_insert_with` that
    /// builds a `Column`) and `metriken-exposition` never carries it. A
    /// recovery path that built `Column { metadata: cell.metadata, .. }`
    /// directly would produce a column a natively sealed segment does not
    /// match, and `read_table_parquet` would then read every gauge back as a
    /// counter. Derive `metric_type` from the [`WalValue`] tag — or, simplest
    /// and what makes the two paths identical by construction, rebuild owned
    /// `Counter`/`Gauge`/`Histogram` entries and replay them through
    /// `TableBuilder::push_row`, which injects it exactly as the writer did.
    /// (A histogram's `grouping_power`/`max_value_power` DO appear here, put
    /// there by the agent's exposition; [`WalValue::Histogram`] carries them
    /// too, so a cell decodes without consulting metadata at all.)
    ///
    /// Carried ONLY on the first WAL row in which this metric appears **in the
    /// current segment** — `maybe_seal` clears the tracking for a sampler when
    /// it seals, so each segment's WAL span re-anchors its own metadata.
    ///
    /// Repeating it every tick is exactly the full-msgpack cost values-only
    /// rows exist to avoid; re-anchoring costs one payload per metric per
    /// *segment*, i.e. roughly one tick in `max_rows`. What that buys is an
    /// invariant contained entirely in the live WAL: **the
    /// first live WAL row mentioning a metric carries its metadata.** No
    /// segment lookup, so no decoding an arbitrarily old segment footer to
    /// learn a tail's labels — the cost the WAL exists to avoid — and nothing
    /// breaks when hindsight retention deletes old segments
    /// (`DELETE FROM segments WHERE last_ts < cutoff`).
    ///
    /// It also makes the WAL's metadata semantics *identical* to a segment
    /// column's, which an anchor held for the recording's lifetime did not:
    /// `seal_completed` installs a fresh `TableBuilder` at every rotation, so a
    /// column re-latches its labels each segment. A metric whose labels drift
    /// mid-recording (a unit correction, an agent restart remapping an id) is
    /// therefore captured in the WAL exactly where it is captured in segments.
    /// And the tracking set no longer grows without bound as cgroup metric
    /// names churn.
    pub metadata: Option<BTreeMap<String, String>>,
    pub value: WalValue,
    /// The acquisition window, as `(begin_ns, end_ns)`.
    pub window: Option<(u64, u64)>,
}

/// A cell's value, tagged by shape — which is also what tells a reader which
/// `RezValues` column the cell belongs in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WalValue {
    Counter(u64),
    Gauge(i64),
    /// `(grouping_power, max_value_power, buckets)`. The H2 config travels with
    /// the buckets so `histogram::Histogram::from_buckets` needs nothing else —
    /// two bytes against a 7,424-bucket payload, and it keeps the cell decodable
    /// without consulting the metadata row.
    Histogram(u8, u8, Vec<u64>),
}

/// One V3 acquisition-group's WAL payload for one tick: values + ONE shared
/// window, with member names/metadata resolved from a schema rather than
/// carried per cell — the WAL row shrinks to values + one window, per the
/// schema-hash cache design (see `StreamRecorderV3`'s `schemas` field).
///
/// **Self-sufficiency, not just bandwidth.** `schema` is `Some` only on the
/// row that (re-)anchors this group's schema for the segment currently
/// accumulating in this table's live WAL — mirroring `WalCell::metadata`'s
/// "first mention in this segment" rule, at group granularity instead of
/// per-metric (`StreamRecorderV3`'s `segment_schema` map decides this,
/// independently of whether the AGENT'S payload included a schema this
/// tick). `schema_hash` is always present so a decoder can tell schema drift
/// from steady state even when `schema` is `None`. This is what lets
/// `materialize_wal_tail` rebuild a group table from WAL rows ALONE, with no
/// external schema cache — required because both the writer thread and a
/// fresh reader process call it (see `materialize_wal_tail`'s doc).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalGroupRow {
    /// Which schema (by content hash) these values align with.
    pub schema_hash: (u64, u64),
    /// The schema itself, present only on the row that (re-)anchors it —
    /// see the struct doc. `None` means "same schema as the nearest earlier
    /// row in this table's live WAL span."
    pub schema: Option<crate::schema::GroupSchema>,
    pub window: Option<(u64, u64)>,
    pub counters: Vec<Option<u64>>,
    pub gauges: Vec<Option<i64>>,
    /// `(grouping_power, max_value_power, buckets)` per histogram slot — the
    /// same shape `WalValue::Histogram` carries.
    pub histograms: Vec<Option<(u8, u8, Vec<u64>)>>,
}

pub fn encode_wal_group_row(row: &WalGroupRow) -> Result<Vec<u8>, String> {
    rmp_serde::to_vec(row).map_err(|e| format!("failed to encode a group WAL row: {e}"))
}

/// `payload`, an encoded [`WalGroupRow`] whose `schema` is `None`, with
/// `schema` put in: the bytes [`encode_wal_group_row`] gives for the same row
/// with `schema: Some`.
///
/// A producer encodes a group's values once per pass without the schema and
/// adds the schema only for a subscriber that has not been sent it. This does
/// that without decoding the values or cloning the schema: the row is a
/// msgpack array whose second element is the schema, so the `nil` there is
/// replaced by the encoded schema and the other bytes are copied. A payload
/// laid out any other way (a schema already present, or a different
/// encoding) is decoded, given the schema and encoded again.
pub fn encode_wal_group_row_with_schema(
    payload: &[u8],
    schema: &crate::schema::GroupSchema,
) -> Result<Vec<u8>, String> {
    if let Some(at) = schema_nil_offset(payload) {
        let mut out = Vec::with_capacity(payload.len() + 64);
        out.extend_from_slice(&payload[..at]);
        rmp_serde::encode::write(&mut out, schema)
            .map_err(|e| format!("failed to encode a group schema: {e}"))?;
        out.extend_from_slice(&payload[at + 1..]);
        return Ok(out);
    }
    let mut row = decode_wal_group_row(payload)?;
    row.schema = Some(schema.clone());
    encode_wal_group_row(&row)
}

/// Where the `nil` of a schema-less [`WalGroupRow`] sits: after the array
/// header and the two-integer `schema_hash`. `None` when the payload does not
/// start that way.
fn schema_nil_offset(payload: &[u8]) -> Option<usize> {
    let mut rest = payload;
    if rmp::decode::read_array_len(&mut rest).ok()? != 6 {
        return None;
    }
    if rmp::decode::read_array_len(&mut rest).ok()? != 2 {
        return None;
    }
    rmp::decode::read_int::<u64, _>(&mut rest).ok()?;
    rmp::decode::read_int::<u64, _>(&mut rest).ok()?;
    let at = payload.len() - rest.len();
    (payload.get(at) == Some(&rmp::Marker::Null.to_u8())).then_some(at)
}

/// The inverse of [`encode_wal_group_row`].
pub fn decode_wal_group_row(bytes: &[u8]) -> Result<WalGroupRow, String> {
    rmp_serde::from_slice(bytes).map_err(|e| format!("failed to decode a group WAL row: {e}"))
}

// Encode one sampler's cells for one tick into a `wal.row` BLOB.
pub fn encode_wal_row(cells: &[WalCell]) -> Result<Vec<u8>, String> {
    rmp_serde::to_vec(cells).map_err(|e| format!("failed to encode a WAL row: {e}"))
}

/// The inverse of [`encode_wal_row`] — the recovery entry point.
pub fn decode_wal_row(bytes: &[u8]) -> Result<Vec<WalCell>, String> {
    rmp_serde::from_slice(bytes).map_err(|e| format!("failed to decode a WAL row: {e}"))
}

/// One long table's WAL payload for one tick: the occupants present, each
/// with its values, and one shared window.
///
/// Occupant numbers are assigned when the row is written, so a reader
/// materializing an unsealed tail needs nothing from the writer. `schema`
/// holds the metric columns' fixed descriptors (no occupant labels, which
/// live in the table's occupant stream) and, like [`WalGroupRow`]'s, is
/// `Some` only on a row that anchors it for the current segment; every row
/// carries `schema_hash`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalLongRow {
    pub schema_hash: (u64, u64),
    pub schema: Option<crate::schema::GroupSchema>,
    pub window: Option<(u64, u64)>,
    pub occupants: Vec<LongOccupant>,
}

/// One occupant's values in a [`WalLongRow`], in its schema's order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LongOccupant {
    pub occupant: u64,
    pub counters: Vec<Option<u64>>,
    pub gauges: Vec<Option<i64>>,
    /// `(grouping_power, max_value_power, buckets)`.
    pub histograms: Vec<Option<(u8, u8, Vec<u64>)>>,
}

pub fn encode_wal_long_row(row: &WalLongRow) -> Result<Vec<u8>, String> {
    rmp_serde::to_vec(row).map_err(|e| format!("failed to encode a WAL long row: {e}"))
}

pub fn decode_wal_long_row(bytes: &[u8]) -> Result<WalLongRow, String> {
    rmp_serde::from_slice(bytes).map_err(|e| format!("failed to decode a WAL long row: {e}"))
}

#[cfg(test)]
mod with_schema_tests {
    use super::*;
    use crate::schema::{GroupSchema, MetricDesc};

    fn schema(n: usize) -> GroupSchema {
        GroupSchema {
            counters: (0..n)
                .map(|i| MetricDesc {
                    name: format!("{i}"),
                    metadata: [
                        ("metric".to_string(), "cpu_usage_user".to_string()),
                        ("__uid__".to_string(), format!("{:016x}", i * 7919)),
                    ]
                    .into(),
                })
                .collect(),
            gauges: vec![MetricDesc {
                name: "g".to_string(),
                metadata: BTreeMap::new(),
            }],
            histograms: vec![MetricDesc {
                name: "h".to_string(),
                metadata: [("metric".to_string(), "latency".to_string())].into(),
            }],
        }
    }

    fn row(schema_hash: (u64, u64), window: Option<(u64, u64)>) -> WalGroupRow {
        WalGroupRow {
            schema_hash,
            schema: None,
            window,
            counters: vec![Some(0), None, Some(u64::MAX)],
            gauges: vec![Some(-5)],
            histograms: vec![Some((3, 8, vec![0, 1, 2]))],
        }
    }

    /// The splice gives exactly the bytes a full encode of the anchored row
    /// gives, across the integer widths a hash and a window can take.
    #[test]
    fn the_splice_equals_encoding_the_anchored_row() {
        let s = schema(3);
        for hash in [
            (0, 0),
            (1, 127),
            (128, 65_536),
            (u64::MAX, u64::MAX),
            s.hash(),
        ] {
            for window in [None, Some((1_000, 2_000)), Some((0, u64::MAX))] {
                let bare = encode_wal_group_row(&row(hash, window)).unwrap();
                assert!(
                    schema_nil_offset(&bare).is_some(),
                    "the splice path is taken"
                );
                let mut anchored = row(hash, window);
                anchored.schema = Some(s.clone());
                assert_eq!(
                    encode_wal_group_row_with_schema(&bare, &s).unwrap(),
                    encode_wal_group_row(&anchored).unwrap(),
                    "hash {hash:?}, window {window:?}"
                );
            }
        }
    }

    /// A payload that already carries a schema is not spliced: it is decoded
    /// and the new schema replaces the old one.
    #[test]
    fn a_payload_with_a_schema_is_re_encoded_with_the_new_one() {
        let mut anchored = row((1, 2), None);
        anchored.schema = Some(schema(1));
        let payload = encode_wal_group_row(&anchored).unwrap();
        let out = encode_wal_group_row_with_schema(&payload, &schema(3)).unwrap();
        assert_eq!(decode_wal_group_row(&out).unwrap().schema, Some(schema(3)));
    }

    #[test]
    fn a_payload_that_is_not_a_row_is_an_error() {
        assert!(encode_wal_group_row_with_schema(&[0x93, 0x01, 0x02], &schema(1)).is_err());
    }
}
