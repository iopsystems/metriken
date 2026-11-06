//! A row's cost against a writer's segment byte budget, metered the same way
//! whether the row arrives as a group snapshot (a scrape) or as a WAL row (a
//! stream), so a recording taken either way seals at the same points.

use crate::wal::{WalGroupRow, WalLongRow};
use crate::GroupSnapshot;

/// In-memory cost of a cell's value slot: `Option<u64>` / `Option<i64>` /
/// `Option<f64>` / `Option<Box<[u64]>>` are all 16 B (the histogram's buckets
/// are counted separately).
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
    bytes += float_slots(&g.float_counters, &g.float_gauges) * VALUE_SLOT_BYTES;
    for h in g.histograms.iter().chain(&g.gauge_histograms).flatten() {
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
    bytes += float_slots(&row.float_counters, &row.float_gauges) * VALUE_SLOT_BYTES;
    for (_, _, buckets) in row.histograms.iter().chain(&row.gauge_histograms).flatten() {
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
        bytes += float_slots(&o.float_counters, &o.float_gauges) * VALUE_SLOT_BYTES;
        for (_, _, buckets) in o.histograms.iter().chain(&o.gauge_histograms).flatten() {
            bytes += VALUE_SLOT_BYTES + buckets.len() * HISTOGRAM_BUCKET_BYTES;
        }
    }
    bytes
}

/// The present float counter and float gauge slots.
fn float_slots(counters: &[Option<f64>], gauges: &[Option<f64>]) -> usize {
    counters
        .iter()
        .chain(gauges)
        .filter(|v| v.is_some())
        .count()
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
            gauge_histograms: Vec::new(),
            float_counters: vec![Some(0.25), None],
            float_gauges: vec![None, Some(-1.5), Some(f64::MAX)],
        };
        let row = wal_group_row(&g, None);
        let decoded = decode_wal_group_row(&encode_wal_group_row(&row).unwrap()).unwrap();
        let metered = wal_group_row_approx_bytes(&decoded);
        assert_eq!(metered, group_approx_bytes(&g));
        assert!(
            metered > WINDOW_SLOT_BYTES + 7 * VALUE_SLOT_BYTES + HISTOGRAM_BUCKET_BYTES,
            "the histogram's buckets must be charged"
        );
    }

    /// Each present float slot costs a value slot in all three meters, as a
    /// counter or gauge slot does; an absent one costs nothing.
    #[test]
    fn present_float_slots_are_charged_as_value_slots() {
        let g = |float_counters: Vec<Option<f64>>, float_gauges: Vec<Option<f64>>| GroupSnapshot {
            name: "s/g".to_string(),
            schema_hash: (1, 2),
            schema: None,
            window: None,
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: Vec::new(),
            gauge_histograms: Vec::new(),
            float_counters,
            float_gauges,
        };
        let empty = g(Vec::new(), Vec::new());
        let floats = g(vec![Some(0.5), None], vec![Some(1.0), Some(2.0), None]);
        assert_eq!(
            group_approx_bytes(&floats) - group_approx_bytes(&empty),
            3 * VALUE_SLOT_BYTES
        );
        assert_eq!(
            wal_group_row_approx_bytes(&wal_group_row(&floats, None))
                - wal_group_row_approx_bytes(&wal_group_row(&empty, None)),
            3 * VALUE_SLOT_BYTES
        );
        let long = |o: crate::wal::LongOccupant| WalLongRow {
            schema_hash: (1, 2),
            schema: None,
            window: None,
            occupants: vec![o],
        };
        let occupant = |float_counters, float_gauges| crate::wal::LongOccupant {
            occupant: 0,
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: Vec::new(),
            gauge_histograms: Vec::new(),
            float_counters,
            float_gauges,
        };
        assert_eq!(
            wal_long_row_approx_bytes(&long(occupant(
                vec![Some(0.5), None],
                vec![Some(1.0), Some(2.0), None]
            ))) - wal_long_row_approx_bytes(&long(occupant(Vec::new(), Vec::new()))),
            3 * VALUE_SLOT_BYTES
        );
    }
}
