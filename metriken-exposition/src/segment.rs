//! A group snapshot's cost against a writer's segment byte budget. Turning a
//! snapshot into a write-ahead-log row is `metriken-model`'s
//! ([`wal_group_row`]).

use crate::GroupSnapshot;
use metriken_segment::builder::{HISTOGRAM_BUCKET_BYTES, VALUE_SLOT_BYTES, WINDOW_SLOT_BYTES};

pub use metriken_model::convert::wal_group_row;

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

#[cfg(test)]
mod tests {
    use super::*;

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
            window: Some(metriken_types::Window::new(900, 1_000)),
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
