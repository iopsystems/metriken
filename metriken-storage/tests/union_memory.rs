//! How many points and rows the batched rate path holds while it combines a
//! process group's members (`batch_rate::UnionStage`): a member that stops
//! reading inside the range must release its group's points then, not at
//! the end of the scan, and rows held to compare sources (S4) are dropped a
//! chunk behind the scan. Measured with
//! `metriken_query::fixtures::take_union_held_peak` and
//! `take_union_rows_held_peak`, the most points and rows the stages of a
//! read held at once.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, SystemTime};

use dendro::archive::{Archive, ArchiveMut};
use dendro::rewrite::{copy_sources_into, CopySpec};
use dendro::seal::SealPolicy;
use metriken_exposition::{Counter, Snapshot, SnapshotV2};
use metriken_query::fixtures::{take_union_held_peak, take_union_rows_held_peak};
use metriken_query::{DisplayOptions, DisplayResult, MetricsSource, QueryOptions, QueryResult};
use metriken_storage::writer::Encoder;
use metriken_storage::{ArchiveReader, ArchiveWriter, BufferPool, DendroCatalog, WriterConfig};

const MS: u64 = 1_000_000;
const S: u64 = 1_000 * MS;
const BASE: u64 = 1_700_000_000 * S;
/// Rows, one a second.
const ROWS: u64 = 4000;
/// Series of `c`.
const N: u64 = 16;

/// An archive of one source per entry of `c_stops`, each of producer
/// epoch `e0` (so one process group: two recorders of one agent), with
/// `N + 1` series of counter `c` (label `id`, growing `id + 1` a second), a
/// row a second for `ROWS` seconds; a source's `c` with `id` below `N` has
/// no rows from its entry on. Segments of 20 rows, so a scan reads many
/// chunks.
fn write(path: &Path, c_stops: &[u64]) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 20,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    let meta = |metric: &str, id: Option<u64>| -> HashMap<String, String> {
        [("metric", metric), ("sampler", "cpu")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .chain(id.map(|i| ("id".to_string(), i.to_string())))
            .collect()
    };
    for &c_stop in c_stops {
        let labels: BTreeMap<String, String> = [("host".to_string(), "a".to_string())].into();
        let metadata: BTreeMap<String, String> =
            [("producer_epoch".to_string(), "e0".to_string())].into();
        let mut source = writer.add_source(labels, metadata, BASE).unwrap();
        for k in 0..ROWS {
            let ts = BASE + k * S;
            let w = Some(metriken::Window::new(ts - MS, ts));
            // `c` with `id` N has rows throughout, so a scan of `c` goes on.
            let mut counters =
                vec![Counter::new(format!("c{N}"), k * (N + 1), meta("c", Some(N))).with_window(w)];
            if k < c_stop {
                for i in 0..N {
                    counters.push(
                        Counter::new(format!("c{i}"), k * (i + 1), meta("c", Some(i)))
                            .with_window(w),
                    );
                }
            }
            let snapshot = Snapshot::V2(SnapshotV2 {
                systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
                duration: Duration::ZERO,
                metadata: HashMap::new(),
                counters,
                gauges: Vec::new(),
                histograms: Vec::new(),
                gauge_histograms: Vec::new(),
            });
            let staged = source.stage(&snapshot, ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
        }
        source.finalize((BASE + (ROWS - 1) * S, 0)).unwrap();
    }
    writer.join().unwrap();
}

/// An archive at `into` holding a copy of each of `from` (whole segments
/// with a row in `range`, as a hindsight dump): one uuid, so one process
/// group of two members.
fn combine(into: &Path, from: &[(&Path, Option<(u64, u64)>)]) {
    let mut out = ArchiveMut::create(into).unwrap();
    for (path, range) in from {
        let src = Archive::open(path).unwrap();
        let encoder = Encoder::for_archive(&src).unwrap();
        let spec = match range {
            Some((start, end)) => CopySpec {
                start: *start as i64,
                end: *end as i64,
                ..CopySpec::everything()
            },
            None => CopySpec::everything(),
        };
        out.transaction(|tx| copy_sources_into(&src, tx, &spec, &encoder))
            .unwrap();
    }
}

fn open(path: &Path) -> ArchiveReader {
    let catalog = DendroCatalog::open(path).unwrap();
    let recordings =
        ArchiveReader::from_catalog(Box::new(catalog), None, BufferPool::new(64 << 20), None)
            .unwrap();
    ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None)
}

type Points = Vec<(BTreeMap<String, String>, Vec<(f64, f64)>)>;

fn sorted(mut v: Points) -> Points {
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// `query` over the whole recording at a 1 s step, batched, and the most
/// points its union stages held at once.
fn batched(reader: &ArchiveReader, query: &str) -> (Points, usize) {
    let (start, end) = ((BASE / S) as f64, (BASE / S + ROWS - 1) as f64);
    take_union_held_peak();
    let result = reader
        .query_range_opts(query, start, end, 1.0, &QueryOptions::default())
        .unwrap();
    let peak = take_union_held_peak();
    let QueryResult::Matrix { result, .. } = result else {
        panic!("{query}: not a matrix");
    };
    let points = result
        .into_iter()
        .map(|m| (m.metric.into_iter().collect(), m.values))
        .collect();
    (sorted(points), peak)
}

/// `query` for display at full resolution over the whole recording at a
/// 1 s step (medians), and the most points its union stages held at once.
fn display(reader: &ArchiveReader, query: &str) -> (Points, usize) {
    let (start, end) = ((BASE / S) as f64, (BASE / S + ROWS - 1) as f64);
    let opts = DisplayOptions {
        budget: 0,
        ..DisplayOptions::default()
    };
    take_union_held_peak();
    let result = reader
        .query_range_display_opts(query, start, end, 1.0, &opts, &QueryOptions::default())
        .unwrap();
    let peak = take_union_held_peak();
    let DisplayResult::Series { result, .. } = result else {
        panic!("{query}: not series");
    };
    let points = result
        .into_iter()
        .map(|s| {
            (
                s.metric.into_iter().collect(),
                s.points.iter().map(|p| (p.t, p.median)).collect(),
            )
        })
        .collect();
    (sorted(points), peak)
}

fn assert_close(query: &str, got: &Points, want: &Points) {
    assert_eq!(got.len(), want.len(), "{query}");
    for ((gl, gp), (wl, wp)) in got.iter().zip(want) {
        assert_eq!(gl, wl, "{query}");
        assert_eq!(gp.len(), wp.len(), "{query} {gl:?}");
        for (a, b) in gp.iter().zip(wp) {
            assert!(
                a.0 == b.0 && (a.1 - b.1).abs() <= 1e-9 * b.1.abs().max(1.0),
                "{query} {gl:?}: {a:?} against {b:?}"
            );
        }
    }
}

/// The most points a read may hold at once, per group series: a member is
/// read up to a chunk (up to eight segments of 20 rows) ahead of another,
/// and about two chunks' points are held at the start (measured: 338 per
/// series with eight partitions). A member waited on to the end of the scan
/// costs about `ROWS * 3 / 4` = 3000 per series.
const BOUND: usize = (600 * (N + 1)) as usize;

/// The most rows the stages may hold at once to compare sources: a chunk of
/// one member per group series, a chunk being a segment of 20 rows per
/// batch thread (as `batch_rate::batch_threads` counts them:
/// `available_parallelism`, at most eight), and half a segment more.
/// Measured: exactly a chunk, 160 a series with eight threads. Two chunks
/// held exceed it at any thread count.
fn rows_bound() -> usize {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    (threads * 20 + 10) * (N as usize + 1)
}

/// A member whose recording ends at a quarter of the range (a dump of the
/// first quarter beside the whole recording): the scan reports it ended
/// (`ScanChunk::ended`), its series are finished, and the stage stops
/// waiting on them. Held to the end, each group series' points from the
/// quarter on would be held: about 3000 per series.
#[test]
fn a_member_that_ends_early_does_not_hold_its_groups_points() {
    let dir = tempfile::tempdir().unwrap();
    let whole = dir.path().join("whole.dendro");
    write(&whole, &[ROWS]);
    let both = dir.path().join("both.dendro");
    let quarter = BASE + ROWS / 4 * S;
    combine(&both, &[(&whole, None), (&whole, Some((BASE, quarter)))]);
    let one = open(&whole);
    let two = open(&both);
    for query in [
        "sum(irate(c[5s]))",
        "rate(c[1m])",
        "sum by (id) (irate(c[5s]))",
    ] {
        let (want, _) = batched(&one, query);
        let (got, peak) = batched(&two, query);
        assert_close(query, &got, &want);
        assert!(peak > 0, "{query}: no union stage held a point");
        assert!(
            peak <= BOUND,
            "{query}: held {peak} points at once, bound {BOUND}"
        );
    }
}

/// Series of a member that fall silent while its scan goes on (two
/// recorders of one agent; one's `c` with `id` below `N` stops at a quarter
/// while its `id` N and the other's series continue), in a grouped display
/// query: each is ended as
/// stale after ten spacings, and the stage stops waiting on it
/// (`UnionStage::end_member`). Waiting on it to the end of the scan, the
/// stage would hold the other member's points from the quarter on: about
/// 3000 per `c` series.
#[test]
fn a_stale_member_series_does_not_hold_its_groups_points() {
    let dir = tempfile::tempdir().unwrap();
    let whole = dir.path().join("whole.dendro");
    write(&whole, &[ROWS]);
    let both = dir.path().join("both.dendro");
    write(&both, &[ROWS / 4, ROWS]);
    let one = open(&whole);
    let two = open(&both);
    for query in ["sum(irate(c[5s]))", "sum(rate(c[1m]))"] {
        let (want, _) = display(&one, query);
        let (got, peak) = display(&two, query);
        assert_close(query, &got, &want);
        assert!(peak > 0, "{query}: no union stage held a point");
        assert!(
            peak <= BOUND,
            "{query}: held {peak} points at once, bound {BOUND}"
        );
    }
}

/// The rows the batched read holds to compare a group's sources (S4): two
/// copies of one recording, and two recorders of one agent where one's `c`
/// with `id` below `N` stops at a quarter while the rest goes on. A row is
/// dropped once every segment not yet read starts after it, so a series
/// that stops in one source does not hold the other's rows to the end of
/// the scan (about 3000 per series).
#[test]
fn comparing_sources_holds_rows_a_chunk_apart() {
    let dir = tempfile::tempdir().unwrap();
    let whole = dir.path().join("whole.dendro");
    write(&whole, &[ROWS]);
    let copies = dir.path().join("copies.dendro");
    combine(&copies, &[(&whole, None), (&whole, None)]);
    let stopping = dir.path().join("stopping.dendro");
    write(&stopping, &[ROWS / 4, ROWS]);
    let one = open(&whole);
    for (name, reader) in [("copies", open(&copies)), ("stopping", open(&stopping))] {
        for query in ["rate(c[1m])", "sum(irate(c[5s]))"] {
            let (want, _) = batched(&one, query);
            take_union_rows_held_peak();
            let (got, _) = batched(&reader, query);
            let peak = take_union_rows_held_peak();
            if name == "copies" {
                assert_close(query, &got, &want);
            }
            assert!(peak > 0, "{name} {query}: no row held");
            assert!(
                peak <= rows_bound(),
                "{name} {query}: held {peak} rows at once, bound {}",
                rows_bound()
            );
        }
    }
}
