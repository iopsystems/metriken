//! Streamed rows at the edges `SourceRecorder::stage_streamed` must handle:
//! a group that changes form, columns sent on a row the writer skips, and an
//! occupant first seen on a row it cannot use.
#![cfg(all(feature = "stream", feature = "write"))]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use metriken_exposition::{
    GroupSchema as XSchema, GroupSnapshot, MetricDesc as XDesc, Snapshot, SnapshotV3,
};
use metriken_query::{BufferPool, MetricsSource, QueryResult};
use metriken_storage::occupants::Occupant;
use metriken_storage::schema::{GroupSchema, MetricDesc};
use metriken_storage::wal::{LongOccupant, WalLongRow};
use metriken_storage::{ArchiveReader, ArchiveWriter, DendroCatalog, StreamedGroup, WriterConfig};

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;
const GROUP: &str = "edge/ops";

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn open(path: &Path) -> ArchiveReader {
    let mut recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(path).unwrap()),
        None,
        BufferPool::new(16 << 20),
        None,
    )
    .unwrap();
    recordings.remove(0).1
}

/// Each series' labels (without `__occupant__`) and its number of points.
fn series(reader: &ArchiveReader, query: &str) -> Vec<(BTreeMap<String, String>, usize)> {
    let (lo, hi) = reader.time_range().unwrap();
    let QueryResult::Matrix { result } = reader.query_range(query, lo, hi, 1.0).unwrap() else {
        panic!("{query}: not a matrix")
    };
    let mut out: Vec<_> = result
        .into_iter()
        .map(|m| {
            let l = m
                .metric
                .into_iter()
                .filter(|(k, _)| k != "__occupant__")
                .collect();
            (l, m.values.len())
        })
        .collect();
    out.sort();
    out
}

fn columns(metric: &str) -> GroupSchema {
    GroupSchema {
        counters: vec![MetricDesc {
            name: "0".to_string(),
            metadata: labels(&[("metric", metric), ("op", "read")]),
        }],
        gauges: Vec::new(),
        histograms: Vec::new(),
    }
}

fn long_row(
    schema: Option<GroupSchema>,
    hash: (u64, u64),
    end: u64,
    occupants: &[(u64, usize, u64)],
) -> StreamedGroup {
    StreamedGroup::Long {
        name: GROUP.to_string(),
        row: WalLongRow {
            schema_hash: hash,
            schema,
            window: Some((end - S / 2, end)),
            occupants: occupants
                .iter()
                .map(|&(key, width, v)| LongOccupant {
                    occupant: key,
                    counters: vec![Some(v); width],
                    gauges: Vec::new(),
                    histograms: Vec::new(),
                })
                .collect(),
        },
    }
}

fn described(keys: &[(u64, &str)]) -> StreamedGroup {
    StreamedGroup::Occupants {
        table: GROUP.to_string(),
        occupants: keys
            .iter()
            .map(|(k, id)| Occupant {
                occupant: *k,
                labels: labels(&[("id", id)]),
            })
            .collect(),
    }
}

fn record(
    path: &Path,
    ticks: impl FnOnce(&mut metriken_storage::SourceRecorder, &mut ArchiveWriter),
) {
    let mut writer = ArchiveWriter::create(path, WriterConfig::default()).unwrap();
    let mut source = writer
        .add_source(labels(&[("source", "t")]), BTreeMap::new(), BASE)
        .unwrap();
    ticks(&mut source, &mut writer);
    source.finalize((BASE + 20 * S, 0)).unwrap();
    writer.join().unwrap();
}

/// A group recorded wide and then long keeps one series per occupant, when
/// its one metric carries a key (`op`) the wide path counts as the
/// occupant's.
#[test]
fn a_group_that_turns_long_keeps_its_series() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("form.dendro");
    record(&path, |source, writer| {
        let schema = Arc::new(XSchema {
            counters: (0..2)
                .map(|i| XDesc {
                    name: format!("0x{i}"),
                    metadata: labels(&[("metric", "ops"), ("op", "read"), ("id", &i.to_string())]),
                })
                .collect(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        });
        for t in 0..5u64 {
            let ts = BASE + t * S;
            let snap = Snapshot::V3(SnapshotV3 {
                systemtime: std::time::UNIX_EPOCH,
                duration: std::time::Duration::ZERO,
                metadata: Default::default(),
                groups: vec![GroupSnapshot {
                    name: GROUP.to_string(),
                    schema_hash: schema.hash(),
                    schema: Some(Arc::clone(&schema)),
                    window: Some(metriken::Window::new(ts - S / 2, ts)),
                    counters: vec![Some(t * 10), Some(t * 20)],
                    gauges: Vec::new(),
                    histograms: Vec::new(),
                }],
            });
            let staged = source.stage(&snap, ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
        }
        let cols = columns("ops");
        for t in 5..10u64 {
            let ts = BASE + t * S;
            let mut groups = Vec::new();
            if t == 5 {
                groups.push(described(&[(0, "0"), (1, "1")]));
            }
            let schema = (t == 5).then(|| cols.clone());
            let row = |key: u64, v| (key, 1usize, v);
            groups.push(long_row(
                schema,
                cols.hash(),
                ts,
                &[row(0, t * 10), row(1, t * 20)],
            ));
            let staged = source.stage_streamed(groups, ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
        }
    });
    let found = series(&open(&path), "irate(ops[2s])");
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found
            .iter()
            .all(|(l, _)| l.get("op").map(String::as_str) == Some("read")),
        "{found:?}"
    );
}

/// Columns sent on a row the writer skips as a repeat (its window did not
/// advance) are kept for the rows after it.
#[test]
fn columns_sent_on_a_repeated_row_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repeat.dendro");
    let (a, b) = (columns("a_ops"), {
        let mut s = columns("a_ops");
        s.counters.push(MetricDesc {
            name: "1".to_string(),
            metadata: labels(&[("metric", "b_ops"), ("op", "read")]),
        });
        s
    });
    record(&path, |source, writer| {
        let mut stage = |groups, t: u64| {
            let staged = source.stage_streamed(groups, BASE + t * S, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
        };
        stage(
            vec![
                described(&[(0, "0")]),
                long_row(Some(a.clone()), a.hash(), BASE + S, &[(0, 1, 1)]),
            ],
            1,
        );
        // New columns, on a row whose window repeats the last one.
        stage(
            vec![long_row(Some(b.clone()), b.hash(), BASE + S, &[(0, 2, 2)])],
            2,
        );
        for t in 3..8u64 {
            stage(
                vec![long_row(None, b.hash(), BASE + t * S, &[(0, 2, t)])],
                t,
            );
        }
    });
    let found = series(&open(&path), "irate(b_ops[2s])");
    assert_eq!(found.len(), 1, "{found:?}");
}

/// An occupant first seen on a row it does not fit (a width mismatch) is
/// recorded from its next row, without being described again.
#[test]
fn an_occupant_skipped_once_is_recorded_after() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("width.dendro");
    let cols = columns("ops");
    record(&path, |source, writer| {
        let mut stage = |groups, t: u64| {
            let staged = source.stage_streamed(groups, BASE + t * S, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
        };
        stage(
            vec![
                described(&[(0, "0"), (1, "1")]),
                long_row(
                    Some(cols.clone()),
                    cols.hash(),
                    BASE + S,
                    &[(0, 1, 1), (1, 3, 1)],
                ),
            ],
            1,
        );
        for t in 2..8u64 {
            stage(
                vec![long_row(
                    None,
                    cols.hash(),
                    BASE + t * S,
                    &[(0, 1, t), (1, 1, t)],
                )],
                t,
            );
        }
    });
    let found = series(&open(&path), "irate(ops[2s])");
    assert_eq!(found.len(), 2, "{found:?}");
}
