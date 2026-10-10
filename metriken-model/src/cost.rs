//! A row's cost against a writer's segment byte budget, metered the same way
//! whether the row arrives as a group snapshot (a scrape) or as a WAL row (a
//! stream), so a recording taken either way seals at the same points.

use crate::wal::{WalGroupRow, WalLongRow};
use crate::GroupSnapshot;

/// In-memory cost of a cell's value slot: `Option<u64>` / `Option<i64>` /
/// `Option<Box<[u64]>>` are all 16 B (the histogram's buckets are counted
/// separately).
pub const VALUE_SLOT_BYTES: usize = 16;
/// In-memory cost of the `Option<Window>` a table builder pushes alongside
/// every counted cell: 24 B, because `Window` is two `u64`s with no niche, so
/// the option tag costs a whole word of padding.
pub const WINDOW_SLOT_BYTES: usize = 24;
/// Bytes per histogram bucket: a table builder clones the histogram's bucket
/// `Box<[u64]>` into the column.
pub const HISTOGRAM_BUCKET_BYTES: usize = 8;

/// A group snapshot's cost: one window slot for the row (a group table
/// carries one window per row, not one per member), a value slot per present
/// member, and a bucket slot per histogram bucket; an absent member costs
/// nothing.
pub fn group_approx_bytes(g: &GroupSnapshot) -> usize {
    let mut bytes = WINDOW_SLOT_BYTES;
    bytes += g.counters.iter().filter(|v| v.is_some()).count() * VALUE_SLOT_BYTES;
    bytes += g.gauges.iter().filter(|v| v.is_some()).count() * VALUE_SLOT_BYTES;
    for h in g.histograms.iter().flatten() {
        bytes += VALUE_SLOT_BYTES + h.as_slice().len() * HISTOGRAM_BUCKET_BYTES;
    }
    bytes
}

/// A WAL group row's cost, metered as [`group_approx_bytes`] meters the
/// snapshot it came from.
pub fn wal_group_row_approx_bytes(row: &WalGroupRow) -> usize {
    let mut bytes = WINDOW_SLOT_BYTES;
    bytes += row.counters.iter().filter(|v| v.is_some()).count() * VALUE_SLOT_BYTES;
    bytes += row.gauges.iter().filter(|v| v.is_some()).count() * VALUE_SLOT_BYTES;
    for (_, _, buckets) in row.histograms.iter().flatten() {
        bytes += VALUE_SLOT_BYTES + buckets.len() * HISTOGRAM_BUCKET_BYTES;
    }
    bytes
}

/// A long row's cost: one window slot, and per occupant an occupant slot
/// plus its present values.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::wal_group_row;
    use crate::wal::{decode_wal_group_row, encode_wal_group_row};

    /// A recording taken off the stream (WAL rows) must seal where a scraped
    /// one (group snapshots) does, so the two meters must agree on the same
    /// data. Every slot kind is present and some are absent, because each is
    /// a separate term.
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
        let decoded = decode_wal_group_row(&encode_wal_group_row(&row).unwrap()).unwrap();
        let metered = wal_group_row_approx_bytes(&decoded);
        assert_eq!(metered, group_approx_bytes(&g));
        assert!(
            metered > WINDOW_SLOT_BYTES + 4 * VALUE_SLOT_BYTES + HISTOGRAM_BUCKET_BYTES,
            "the histogram's buckets must be charged"
        );
    }
}
