//! The `.rez` v3 write-ahead log. The row format and the materialization of
//! a live WAL tail are the segment format's ([`crate::wal`]), re-exported
//! here. What is `.rez`'s: its WAL row type as a [`WalRowSource`], and the
//! conversion from a snapshot [`Entry`](crate::rez::rez::Entry).

pub use crate::wal::*;
#[cfg(feature = "write")]
pub use metriken_model::convert::wal_group_row;

use crate::rez::rez_sqlite::WalRow;

impl WalRowSource for WalRow {
    fn ts(&self) -> u64 {
        self.ts
    }

    fn wall_offset(&self) -> i64 {
        self.wall_offset
    }

    fn row(&self) -> &[u8] {
        &self.row
    }
}

/// The ingest boundary for a cell's value: a snapshot entry becomes the
/// WAL's own representation on the way in.
#[cfg(feature = "write")]
pub fn wal_value(entry: &crate::rez::rez::Entry<'_>) -> WalValue {
    use crate::rez::rez::Entry;
    match entry {
        Entry::Counter(c) => WalValue::Counter(c.value),
        Entry::Gauge(g) => WalValue::Gauge(g.value),
        Entry::Histogram(h) => WalValue::Histogram(
            h.value.config().grouping_power(),
            h.value.config().max_value_power(),
            h.value.as_slice().to_vec(),
        ),
    }
}
