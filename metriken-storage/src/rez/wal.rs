//! The `.rez` v3 write-ahead log. The row format and the materialization of
//! a live WAL tail are the segment format's ([`crate::wal`]), re-exported
//! here. What is `.rez`'s: its WAL row type as a [`WalRowSource`], which is
//! how a reader materializes the unsealed rows of a live `.rez`.

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

/// Retention can delete a group's anchor row out from under a still-live
/// span (a hindsight buffer's `evict_before` deleted WAL rows by `ts` alone),
/// so a reader materializing a live `.rez` tail meets rows whose schema it
/// cannot resolve. It skips them rather than failing the read.
#[cfg(test)]
mod unanchored_tests {
    use super::*;
    use crate::schema::{GroupSchema, MetricDesc};

    fn group_schema(members: &[&str]) -> GroupSchema {
        GroupSchema {
            counters: members
                .iter()
                .map(|n| MetricDesc {
                    name: n.to_string(),
                    metadata: [("metric".to_string(), n.to_string())]
                        .into_iter()
                        .collect(),
                })
                .collect(),
            gauges: Vec::new(),
            histograms: Vec::new(),
            gauge_histograms: Vec::new(),
        }
    }

    #[test]
    fn materialize_wal_tail_skips_un_anchored_leading_rows() {
        let sch = group_schema(&["0"]);
        // Simulates retention having deleted this group's true anchor
        // row: the oldest surviving row has `schema: None` and there is
        // nothing before it in this span to resolve it against.
        let unanchored = WalRow {
            sampler: "cpu_usage/percpu".to_string(),
            ts: 1_000,
            wall_offset: 0,
            row: encode_wal_group_row(&WalGroupRow {
                schema_hash: sch.hash(),
                schema: None,
                window: Some((900, 1_000)),
                counters: vec![Some(1)],
                gauges: Vec::new(),
                histograms: Vec::new(),
                gauge_histograms: Vec::new(),
            })
            .unwrap(),
        };
        let anchored = WalRow {
            sampler: "cpu_usage/percpu".to_string(),
            ts: 2_000,
            wall_offset: 0,
            row: encode_wal_group_row(&WalGroupRow {
                schema_hash: sch.hash(),
                schema: Some((&sch).into()),
                window: Some((1_900, 2_000)),
                counters: vec![Some(2)],
                gauges: Vec::new(),
                histograms: Vec::new(),
                gauge_histograms: Vec::new(),
            })
            .unwrap(),
        };
        let tail = materialize_wal_tail("cpu_usage/percpu", &[unanchored, anchored])
            .expect("materialization must not error on an un-anchored leading row")
            .expect("the anchored row must still materialize");
        // THE catalog-accuracy fix: before it, the `.rez` writer's seal
        // derived `SegmentMeta::rows`/`first_ts` from the raw WAL span
        // regardless of skips — here that would have been `rows: 2`,
        // `first_ts: 1_000` for a segment that actually holds ONE row
        // starting at `ts=2_000`. `MaterializedTail` now reports the
        // truth: what actually went into `bytes`.
        assert_eq!(
            (tail.rows, tail.first_ts),
            (1, 2_000),
            "the catalog extent must reflect the ONE row that materialized \
             (ts=2_000), not the raw WAL span's rows=2/first_ts=1_000"
        );
        let table = crate::rez::rez::read_table_parquet("cpu_usage/percpu".to_string(), tail.bytes)
            .unwrap();
        assert_eq!(
            table.timestamps,
            vec![2_000],
            "the un-anchored row is skipped; the remainder materializes"
        );
    }

    #[test]
    fn materialize_wal_tail_of_a_fully_unanchored_span_is_none() {
        let sch = group_schema(&["0"]);
        let row = WalRow {
            sampler: "cpu_usage/percpu".to_string(),
            ts: 1_000,
            wall_offset: 0,
            row: encode_wal_group_row(&WalGroupRow {
                schema_hash: sch.hash(),
                schema: None,
                window: None,
                counters: vec![Some(1)],
                gauges: Vec::new(),
                histograms: Vec::new(),
                gauge_histograms: Vec::new(),
            })
            .unwrap(),
        };
        assert_eq!(
            materialize_wal_tail("cpu_usage/percpu", &[row]).unwrap(),
            None,
            "a span with nothing resolvable at all is the same as an empty tail"
        );
    }
}
