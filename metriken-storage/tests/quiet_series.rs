//! Series that fall silent and come back, in a grouped display read over
//! one recording and over a process group: the batched read keeps going
//! (`metriken_query::fixtures::take_batch_abandoned`) and answers as the
//! per-series path does (`QueryOptions::per_series_rates`).
#![cfg(feature = "write")]

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use dendro::seal::SealPolicy;
use metriken_exposition::{
    Counter, GroupSchema, GroupSnapshot, MetricDesc, Snapshot, SnapshotV2, SnapshotV3,
};
use metriken_query::fixtures::take_batch_abandoned;
use metriken_query::{DisplayOptions, MetricsSource, QueryOptions};
use metriken_storage::{ArchiveReader, ArchiveWriter, BufferPool, DendroCatalog, WriterConfig};

const MS: u64 = 1_000_000;
const S: u64 = 1_000 * MS;
const BASE: u64 = 1_700_000_000 * S;
/// Rows, one a second; the queries read the first `RANGE` seconds.
const ROWS: u64 = 2200;
const RANGE: u64 = 2000;

/// Group labels for generated series.
const GROUPS: [&str; 20] = [
    "g0", "g1", "g2", "g3", "g4", "g5", "g6", "g7", "g8", "g9", "g10", "g11", "g12", "g13", "g14",
    "g15", "g16", "g17", "g18", "g19",
];

/// A series of `c`: its `g` label and the seconds it has no rows in.
struct Series {
    g: &'static str,
    silent: Vec<(u64, u64)>,
    /// Restart its counter at this second.
    restart: Option<u64>,
}

fn series(g: &'static str, silent: &[(u64, u64)]) -> Series {
    Series {
        g,
        silent: silent.to_vec(),
        restart: None,
    }
}

/// The shapes: silent once and twice, back for the range's last seconds,
/// back only after the range's end, silent across the range's start, and a
/// counter restarted when it comes back; then `many` series in `g` = `m`,
/// each silent for a while somewhere.
fn shapes(many: u64) -> Vec<Series> {
    let mut v = vec![
        series("a", &[]),
        Series {
            restart: Some(700),
            ..series("a", &[(300, 700)])
        },
        series("a", &[(100, 400), (900, 1500)]),
        series("b", &[(150, 1990)]),
        series("b", &[(20, 1900)]),
        series("d", &[(1200, 2050)]),
        series("b", &[]),
        series("e", &[(0, 30), (600, 2001)]),
        series("e", &[]),
    ];
    for i in 0..many {
        let from = 40 + (i * 157) % 1800;
        v.push(series("m", &[(from, from + 30 + (i * 61) % 400)]));
    }
    v
}

/// How the series are stored: wide snapshots (a column per series, absent
/// from a segment where it has no row), one acquisition group in a wide
/// table (every series' column in every segment, null where it has no
/// row), or that group in the long layout (a row per series present, by
/// occupant).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    V2,
    V3Wide,
    Long,
}

const LAYOUTS: [Layout; 3] = [Layout::V2, Layout::V3Wide, Layout::Long];

/// One source per entry of `members`, each of producer epoch `e0` (one
/// process group when there are several), with the series of `shapes`;
/// member `k` is also silent for `members[k]`; `rows` rows a second,
/// segments of `max_rows`. The layouts: [`Layout`].
fn write(
    path: &Path,
    layout: Layout,
    shapes: &[Series],
    members: &[Vec<(usize, Range<u64>)>],
    rows: u64,
    max_rows: usize,
) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows,
            ..SealPolicy::default()
        },
        long_groups: layout == Layout::Long,
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    let meta = |id: usize, g: &str| -> HashMap<String, String> {
        [
            ("metric", "c".to_string()),
            ("sampler", "cpu".to_string()),
            ("id", id.to_string()),
            ("g", g.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    };
    let schema = Arc::new(GroupSchema {
        counters: shapes
            .iter()
            .enumerate()
            .map(|(id, s)| MetricDesc {
                name: format!("c{id}"),
                metadata: meta(id, s.g).into_iter().collect(),
            })
            .collect(),
        ..GroupSchema::default()
    });
    for extra in members {
        let labels: BTreeMap<String, String> = [("host".to_string(), "a".to_string())].into();
        let metadata: BTreeMap<String, String> =
            [("producer_epoch".to_string(), "e0".to_string())].into();
        let mut source = writer.add_source(labels, metadata, BASE).unwrap();
        for k in 0..rows {
            let ts = BASE + k * S;
            let w = Some(metriken::Window::new(ts - MS, ts));
            let values: Vec<Option<u64>> = shapes
                .iter()
                .enumerate()
                .map(|(id, s)| {
                    let silent = s.silent.iter().any(|(a, b)| (*a..*b).contains(&k))
                        || extra.iter().any(|(e, r)| *e == id && r.contains(&k));
                    let k0 = s.restart.filter(|r| k >= *r).map_or(k, |r| k - r);
                    let v = k0 * (id as u64 + 1) * 3 + (k0 * 7919 * (id as u64 + 1)) % 5;
                    (!silent).then_some(v)
                })
                .collect();
            let systemtime = SystemTime::UNIX_EPOCH + Duration::from_nanos(ts);
            let snapshot = if layout != Layout::V2 {
                Snapshot::V3(SnapshotV3 {
                    systemtime,
                    duration: Duration::ZERO,
                    metadata: Default::default(),
                    groups: vec![GroupSnapshot {
                        name: "cpu/c".to_string(),
                        schema_hash: schema.hash(),
                        schema: Some(Arc::clone(&schema)),
                        window: w,
                        counters: values,
                        gauges: Vec::new(),
                        histograms: Vec::new(),
                    }],
                })
            } else {
                let counters = shapes
                    .iter()
                    .zip(values)
                    .enumerate()
                    .filter_map(|(id, (s, v))| {
                        Some(Counter::new(format!("c{id}"), v?, meta(id, s.g)).with_window(w))
                    })
                    .collect();
                Snapshot::V2(SnapshotV2 {
                    systemtime,
                    duration: Duration::ZERO,
                    metadata: HashMap::new(),
                    counters,
                    gauges: Vec::new(),
                    histograms: Vec::new(),
                })
            };
            let staged = source.stage(&snapshot, ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
        }
        source.finalize((BASE + (rows - 1) * S, 0)).unwrap();
    }
    writer.join().unwrap();
}

fn open(path: &Path) -> ArchiveReader {
    let catalog = DendroCatalog::open(path).unwrap();
    let recordings =
        ArchiveReader::from_catalog(Box::new(catalog), None, BufferPool::new(64 << 20), None)
            .unwrap();
    ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None)
}

const QUERIES: &[&str] = &[
    "sum(irate(c[5s]))",
    "sum by (g) (irate(c[5s]))",
    "count by (g) (irate(c[2s]))",
    "sum(rate(c[30s]))",
    "sum by (g) (rate(c[2m]))",
    "avg by (g) (rate(c[10s])) * 2",
    "max by (g) (irate(c[5s]))",
];

/// Each display series' labels and points (time, min, median, max, band).
type Shown = Vec<(BTreeMap<String, String>, Vec<[Option<f64>; 6]>)>;

fn display(
    reader: &ArchiveReader,
    query: &str,
    range: (u64, u64),
    budget: usize,
    per_series: bool,
) -> Shown {
    let opts = DisplayOptions {
        budget,
        ..DisplayOptions::default()
    };
    let mut qopts = QueryOptions::default();
    qopts.per_series_rates = per_series;
    let (start, end) = ((BASE / S + range.0) as f64, (BASE / S + range.1) as f64);
    let result = reader
        .query_range_display_opts(query, start, end, 1.0, &opts, &qopts)
        .unwrap();
    let metriken_query::DisplayResult::Series { result, .. } = result else {
        panic!("{query}: not series");
    };
    let mut shown: Shown = result
        .into_iter()
        .map(|s| {
            (
                s.metric.into_iter().collect(),
                s.points
                    .iter()
                    .map(|p| {
                        [
                            Some(p.t),
                            Some(p.min),
                            Some(p.median),
                            Some(p.max),
                            p.unc_lo,
                            p.unc_hi,
                        ]
                    })
                    .collect(),
            )
        })
        .collect();
    shown.sort_by(|a, b| a.0.cmp(&b.0));
    shown
}

/// Equal, values within 1e-9 relative (summation order).
fn assert_close(what: &str, got: &Shown, want: &Shown) {
    assert_eq!(got.len(), want.len(), "{what}: series");
    for ((gl, gp), (wl, wp)) in got.iter().zip(want) {
        assert_eq!(gl, wl, "{what}");
        assert_eq!(gp.len(), wp.len(), "{what} {gl:?}: points");
        for (a, b) in gp.iter().zip(wp) {
            for (x, y) in a.iter().zip(b) {
                let same = match (x, y) {
                    (Some(x), Some(y)) => (x - y).abs() <= 1e-9 * y.abs().max(1.0),
                    (None, None) => true,
                    _ => false,
                };
                assert!(same, "{what} {gl:?}: {a:?} against {b:?}");
            }
        }
    }
}

/// Every query over `reader`, batched and per series, at each range and
/// budget; the batched read must not give up.
fn check(name: &str, reader: &ArchiveReader) {
    for range in [(0, RANGE), (120, RANGE - 40), (1950, RANGE)] {
        for query in QUERIES {
            for budget in [0, 50] {
                let what = format!("{name} {query} {range:?} budget {budget}");
                take_batch_abandoned();
                let got = display(reader, query, range, budget, false);
                assert_eq!(
                    take_batch_abandoned(),
                    0,
                    "{what}: the batched read gave up"
                );
                let want = display(reader, query, range, budget, true);
                assert_close(&what, &got, &want);
            }
        }
    }
}

#[test]
fn one_recording() {
    let dir = tempfile::tempdir().unwrap();
    for layout in LAYOUTS {
        let path = dir.path().join(format!("one-{layout:?}.dendro"));
        write(&path, layout, &shapes(30), &[vec![]], ROWS, 20);
        check(&format!("one {layout:?}"), &open(&path));
    }
}

/// Two recorders of one agent that agree, and two where the second is also
/// silent where the first is not (S3 then answers from the first): the
/// union's stage holds a returning member's unit until its next row.
#[test]
fn a_process_group() {
    let dir = tempfile::tempdir().unwrap();
    for layout in LAYOUTS {
        let same = dir.path().join(format!("same-{layout:?}.dendro"));
        write(&same, layout, &shapes(30), &[vec![], vec![]], ROWS, 20);
        check(&format!("copies {layout:?}"), &open(&same));
        let apart = dir.path().join(format!("apart-{layout:?}.dendro"));
        let extra = vec![
            (0, 500..800),
            (6, 1000..1400),
            (8, 50..1960),
            (11, 300..600),
        ];
        write(&apart, layout, &shapes(30), &[vec![], extra], ROWS, 20);
        check(&format!("apart {layout:?}"), &open(&apart));
    }
}

/// Series that fall silent for good at 5% of the range: in a wide table
/// their columns stay in every later segment with null cells, and the
/// footer's statistics are what say no row follows, so each is ended and
/// stops holding its group. Held to the end, no group would be fed before
/// the last chunk. 40 series in 20 groups, each group with one series
/// that stops.
#[test]
fn a_series_silent_for_good_frees_its_group_in_every_layout() {
    use metriken_query::fixtures::take_fed_before_end;
    const ROWS: u64 = 6000;
    let groups = 20;
    let shapes: Vec<Series> = (0..40)
        .map(|i| Series {
            g: GROUPS[i % groups],
            silent: if i < groups {
                vec![(ROWS / 20, u64::MAX)]
            } else {
                vec![]
            },
            restart: None,
        })
        .collect();
    let blocks = ROWS.div_ceil(1024) as usize;
    let dir = tempfile::tempdir().unwrap();
    for layout in LAYOUTS {
        let path = dir.path().join(format!("dead-{layout:?}.dendro"));
        write(&path, layout, &shapes, &[vec![]], ROWS, 200);
        let reader = open(&path);
        for budget in [0, 500] {
            let query = "sum by (g) (irate(c[5s]))";
            let what = format!("{layout:?} budget {budget}");
            take_batch_abandoned();
            take_fed_before_end();
            let got = display(&reader, query, (0, ROWS - 1), budget, false);
            let fed = take_fed_before_end();
            assert_eq!(
                take_batch_abandoned(),
                0,
                "{what}: the batched read gave up"
            );
            // Every block but the last of each group, less one held by the
            // chunk still being read.
            assert!(
                fed >= groups * (blocks - 2),
                "{what}: {fed} blocks fed before the end, of {}",
                groups * blocks
            );
            let want = display(&reader, query, (0, ROWS - 1), budget, true);
            assert_close(&what, &got, &want);
        }
    }
}
