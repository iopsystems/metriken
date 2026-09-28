//! The membership of one acquisition group, as the archive stores it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One member of an acquisition group: its name and the producer's metadata.
///
/// A structural mirror of `metriken_exposition::MetricDesc`, and the field
/// order is load-bearing: a WAL row carries a `GroupSchema` as msgpack, and
/// rmp-serde encodes a struct as a positional array, so the on-disk bytes ARE
/// this declaration order. `mirrors_the_producers_encoding_byte_for_byte`
/// pins that against the producer's type, in `metriken-exposition`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricDesc {
    pub name: String,
    pub metadata: BTreeMap<String, String>,
}

/// Descriptors for every counter, gauge and histogram slot of one group, in
/// the order its value arrays use.
///
/// Mirrors `metriken_exposition::GroupSchema` for the reason [`MetricDesc`]
/// does, plus one of its own: a group table's live WAL tail cannot be
/// materialized without decoding this, so having it be the producer's type
/// put `metriken-exposition` — and through it `metriken-core`'s `linkme`
/// distributed slice, which has no wasm32 implementation — on the archive's
/// READ path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSchema {
    pub counters: Vec<MetricDesc>,
    pub gauges: Vec<MetricDesc>,
    pub histograms: Vec<MetricDesc>,
}

impl GroupSchema {
    /// FNV-1a-128 over the schema's canonical msgpack encoding, as `(hi, lo)`.
    ///
    /// The same function the producer computes, because the value is compared
    /// against the producer's: a WAL row carries `schema_hash` so a decoder
    /// can tell schema drift from steady state, and a hash that disagreed
    /// with the one written would make every row look like drift.
    /// Deterministic because `MetricDesc::metadata` is a `BTreeMap`.
    pub fn hash(&self) -> (u64, u64) {
        let bytes = rmp_serde::to_vec(self).expect("GroupSchema serialization is infallible");
        fnv1a_128(&bytes)
    }
}

/// FNV-1a-128, returned as `(hi, lo)` because msgpack has no 128-bit integer.
///
/// Public so other content-addressed identifiers (rezolus's identity index
/// state) are computed by one function rather than copies of the same
/// constants. The domains are separate; the arithmetic is not.
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
