//! The membership of one acquisition group: what a producer declares and an
//! archive stores.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The metadata key that names a metric's kind where a recording stores it
/// (a segment column's field metadata). The storage builders set it from a
/// value's shape when a producer has not, and keep it when one has.
pub const METRIC_TYPE: &str = "metric_type";

/// [`METRIC_TYPE`]'s value for a counting histogram, whose buckets count the
/// observations since start, so a reader differences two rows. Written by
/// every writer from metriken-storage 0.5; earlier ones wrote
/// [`LEGACY_HISTOGRAM`], which [`is_counter_histogram`] also accepts.
pub const COUNTER_HISTOGRAM: &str = "counter_histogram";

/// [`METRIC_TYPE`]'s value for a gauge histogram, whose buckets are the
/// current state of something, not counts since start. A reader that finds it
/// must read each row as it is and never take a difference between two rows.
/// On the wire the kind is the list an entry is in
/// ([`GroupSchema::gauge_histograms`], `SnapshotV2::gauge_histograms`, a
/// `WalValue::GaugeHistogram` cell); the string is how a segment column says
/// it.
pub const GAUGE_HISTOGRAM: &str = "gauge_histogram";

/// The counting histogram's [`METRIC_TYPE`] before [`COUNTER_HISTOGRAM`]:
/// every archive and parquet file written by metriken-storage 0.4 and
/// earlier carries it. Read, never written.
pub const LEGACY_HISTOGRAM: &str = "histogram";

/// Whether a column's [`METRIC_TYPE`] is the counting histogram's, under
/// either spelling. A list column whose `metric_type` is absent is also a
/// counting histogram (files written before the key existed); that is the
/// caller's case to handle, since this takes the value.
pub fn is_counter_histogram(metric_type: &str) -> bool {
    metric_type == COUNTER_HISTOGRAM || metric_type == LEGACY_HISTOGRAM
}

/// Whether a column's [`METRIC_TYPE`] is the gauge histogram's.
pub fn is_gauge_histogram(metric_type: &str) -> bool {
    metric_type == GAUGE_HISTOGRAM
}

/// One metric's identity within a [`GroupSchema`]: its column key (the
/// snapshot entry name, e.g. `"5"` / `"5x3"`) plus its annotations
/// (`metric`, `sampler`, labels, and for histograms `grouping_power` /
/// `max_value_power`). Metadata is a `BTreeMap` so serialization — and
/// therefore the group schema hash — is deterministic.
///
/// The field order is part of the encoding: a WAL row carries a
/// `GroupSchema` as msgpack, and rmp-serde encodes a struct as a positional
/// array.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricDesc {
    /// MUST be unique across ALL groups in a snapshot, not only within this
    /// one's own group. Downstream code keys metrics by name, so a
    /// cross-group collision silently drops one of the two readings rather
    /// than erroring. V1/V2 had this structurally for free — one flat,
    /// global counter/gauge/histogram list — so nothing enforced it
    /// explicitly. V3 splits names into per-group schemas, so producers are
    /// now responsible for preserving global uniqueness themselves.
    pub name: String,
    pub metadata: BTreeMap<String, String>,
}

/// The membership of one acquisition group: descriptors for every counter,
/// gauge, counting histogram and gauge histogram slot, in the order the
/// value arrays use.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSchema {
    pub counters: Vec<MetricDesc>,
    pub gauges: Vec<MetricDesc>,
    /// Counting histograms.
    pub histograms: Vec<MetricDesc>,
    /// Gauge histograms: the last field, and left out of the encoding when
    /// empty, so a row or snapshot without one encodes (and hashes) as it did
    /// before the field existed, and decoders from before it read it. One
    /// that holds a gauge histogram is an array one longer, which those
    /// decoders refuse (rmp-serde's `LengthMismatch`, serde's invalid length
    /// under the untagged [`Snapshot`](crate::Snapshot)) rather than read the
    /// state as counts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gauge_histograms: Vec<MetricDesc>,
}

impl GroupSchema {
    /// FNV-1a-128 over the schema's canonical msgpack encoding, returned as
    /// `(hi, lo)` because msgpack (and rmp-serde) has no 128-bit integer.
    ///
    /// Deterministic because `MetricDesc.metadata` is a `BTreeMap`. Only the
    /// producer computes this; receivers treat it as an opaque cache key —
    /// but the algorithm is still pinned by a known-answer test so hashes
    /// stay comparable across producer versions. 128 bits because a
    /// collision mis-associates an entire group's values with the wrong
    /// schema.
    pub fn hash(&self) -> (u64, u64) {
        let bytes = rmp_serde::to_vec(self).expect("GroupSchema serialization is infallible");
        fnv1a_128(&bytes)
    }
}

/// FNV-1a-128 of `bytes`, as `(hi, lo)`.
pub fn fnv1a_128(bytes: &[u8]) -> (u64, u64) {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u128;
        h = h.wrapping_mul(PRIME);
    }
    ((h >> 64) as u64, h as u64)
}

/// A borrowed descriptor, cloned, so `(&desc).into()` gives an owned one.
impl From<&MetricDesc> for MetricDesc {
    fn from(d: &MetricDesc) -> Self {
        d.clone()
    }
}

/// A borrowed schema, cloned, so `(&schema).into()` gives an owned one.
impl From<&GroupSchema> for GroupSchema {
    fn from(s: &GroupSchema) -> Self {
        s.clone()
    }
}
