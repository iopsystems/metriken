//! A group's membership, from this crate's `GroupSchema` to the segment
//! format's mirror of it (`metriken_segment::schema`).
//!
//! The segment format keeps its own copy of the type because it has to build
//! for wasm32, which this crate (through `metriken`'s registry) does not. The
//! two must encode identically: a WAL row carries a schema as msgpack, and
//! rmp-serde writes a struct as a positional array, so a reordered or added
//! field would not fail to compile but would write rows a reader decodes into
//! the wrong fields. The tests below pin that.

use crate::{GroupSchema, MetricDesc};

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
}
