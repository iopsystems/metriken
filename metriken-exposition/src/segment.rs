//! From this crate's types to the segment format's (`metriken-segment`): a
//! group's membership, and a group snapshot as a write-ahead-log row.
//!
//! The segment format keeps its own copy of the type because it has to build
//! for wasm32, which this crate (through `metriken`'s registry) does not. The
//! two must encode identically: a WAL row carries a schema as msgpack, and
//! rmp-serde writes a struct as a positional array, so a reordered or added
//! field would not fail to compile but would write rows a reader decodes into
//! the wrong fields. The tests below pin that.

use crate::{GroupSchema, GroupSnapshot, MetricDesc};
use metriken_segment::builder::{HISTOGRAM_BUCKET_BYTES, VALUE_SLOT_BYTES, WINDOW_SLOT_BYTES};
use metriken_segment::wal::WalGroupRow;

impl From<&MetricDesc> for metriken_segment::schema::MetricDesc {
    fn from(d: &MetricDesc) -> Self {
        Self {
            name: d.name.clone(),
            metadata: d.metadata.clone(),
        }
    }
}

impl From<&GroupSchema> for metriken_segment::schema::GroupSchema {
    fn from(s: &GroupSchema) -> Self {
        Self {
            counters: s.counters.iter().map(Into::into).collect(),
            gauges: s.gauges.iter().map(Into::into).collect(),
            histograms: s.histograms.iter().map(Into::into).collect(),
        }
    }
}

/// A group snapshot as a WAL row: its values, one shared window, and the
/// schema when the caller is anchoring it (`None` otherwise; the row always
/// carries `schema_hash`). Moved from rezolus's `crates/rez`; the row
/// endpoint and the WAL path both build rows here, so the two are the same
/// function of the same input.
pub fn wal_group_row(
    g: &GroupSnapshot,
    schema: Option<metriken_segment::schema::GroupSchema>,
) -> WalGroupRow {
    WalGroupRow {
        schema_hash: g.schema_hash,
        schema,
        window: g.window.map(|w| (w.begin_ns, w.end_ns)),
        counters: g.counters.clone(),
        gauges: g.gauges.clone(),
        histograms: g
            .histograms
            .iter()
            .map(|h| {
                h.as_ref().map(|h| {
                    (
                        h.config().grouping_power(),
                        h.config().max_value_power(),
                        h.as_slice().to_vec(),
                    )
                })
            })
            .collect(),
    }
}

/// A group snapshot's cost against a writer's segment byte budget: one
/// window slot for the row (a group table carries one window per row, not
/// one per member), a value slot per present member, and a bucket slot per
/// histogram bucket; an absent member costs nothing.
/// `metriken_segment::wal::wal_group_row_approx_bytes` meters the same row
/// after decoding, and the test below pins the two.
pub fn group_approx_bytes(g: &GroupSnapshot) -> usize {
    let mut bytes = WINDOW_SLOT_BYTES;
    bytes += g.counters.iter().filter(|v| v.is_some()).count() * VALUE_SLOT_BYTES;
    bytes += g.gauges.iter().filter(|v| v.is_some()).count() * VALUE_SLOT_BYTES;
    for h in g.histograms.iter().flatten() {
        bytes += VALUE_SLOT_BYTES + h.as_slice().len() * HISTOGRAM_BUCKET_BYTES;
    }
    bytes
}

#[cfg(all(test, feature = "msgpack"))]
mod tests {
    use super::*;
    use metriken_segment::schema::GroupSchema as SegmentSchema;

    fn producer_schema() -> GroupSchema {
        let desc = |n: &str, k: &str, v: &str| MetricDesc {
            name: n.to_string(),
            metadata: [(k.to_string(), v.to_string())].into_iter().collect(),
        };
        GroupSchema {
            counters: vec![desc("0", "metric", "cpu_cycles"), desc("1", "cpu", "3")],
            gauges: vec![desc("2", "metric", "cpu_freq")],
            histograms: vec![desc("3x0", "metric", "runqueue_latency")],
        }
    }

    #[test]
    fn mirrors_the_producers_encoding_byte_for_byte() {
        let theirs = producer_schema();
        let ours: SegmentSchema = (&theirs).into();
        assert_eq!(
            rmp_serde::to_vec(&ours).unwrap(),
            rmp_serde::to_vec(&theirs).unwrap(),
        );
    }

    /// A WAL row's `schema_hash` is written by one side and compared by the
    /// other: a disagreement makes every row look like schema drift.
    #[test]
    fn hashes_the_same_as_the_producer() {
        let theirs = producer_schema();
        let ours: SegmentSchema = (&theirs).into();
        assert_eq!(ours.hash(), theirs.hash());
    }

    /// An empty schema (a group registered with no members yet) agrees too.
    #[test]
    fn an_empty_schema_agrees_as_well() {
        let theirs = GroupSchema::default();
        let ours: SegmentSchema = (&theirs).into();
        assert_eq!(ours.hash(), theirs.hash());
        assert_eq!(
            rmp_serde::to_vec(&ours).unwrap(),
            rmp_serde::to_vec(&theirs).unwrap(),
        );
    }

    /// A recording taken off the stream (WAL rows) must seal where a scraped
    /// one (group snapshots) does, so the two meters must agree on the same
    /// data. Every slot kind is present and some are absent, because each is
    /// a separate term. Moved from rezolus with the two functions.
    #[test]
    fn a_decoded_row_is_metered_like_the_group_it_came_from() {
        let mut h = histogram::Histogram::new(7, 64).unwrap();
        h.increment(1_000).unwrap();
        let g = GroupSnapshot {
            name: "s/g".to_string(),
            schema_hash: (1, 2),
            schema: None,
            window: Some(metriken::Window::new(900, 1_000)),
            counters: vec![Some(1), None, Some(3)],
            gauges: vec![None, Some(-4)],
            histograms: vec![Some(h), None],
        };
        let row = wal_group_row(&g, None);
        let decoded = metriken_segment::wal::decode_wal_group_row(
            &metriken_segment::wal::encode_wal_group_row(&row).unwrap(),
        )
        .unwrap();
        let metered = metriken_segment::wal::wal_group_row_approx_bytes(&decoded);
        assert_eq!(metered, group_approx_bytes(&g));
        assert!(
            metered > WINDOW_SLOT_BYTES + 4 * VALUE_SLOT_BYTES + HISTOGRAM_BUCKET_BYTES,
            "the histogram's buckets must be charged"
        );
    }
}
