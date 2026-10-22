//! A group snapshot as a write-ahead-log row.

use crate::schema::GroupSchema;
use crate::snapshot::GroupSnapshot;
use crate::wal::WalGroupRow;

/// `g` as a [`WalGroupRow`], with `schema` carried when the row anchors it.
pub fn wal_group_row(g: &GroupSnapshot, schema: Option<GroupSchema>) -> WalGroupRow {
    WalGroupRow {
        schema_hash: g.schema_hash,
        schema,
        window: g.window.map(|w| (w.begin_ns, w.end_ns)),
        counters: g.counters.clone(),
        gauges: g.gauges.clone(),
        histograms: cells(&g.histograms),
        gauge_histograms: cells(&g.gauge_histograms),
    }
}

/// Histogram values as WAL row slots.
fn cells(values: &[Option<histogram::Histogram>]) -> Vec<Option<(u8, u8, Vec<u64>)>> {
    values
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
        .collect()
}
