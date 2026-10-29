//! Queries over process groups (S1 to S9 of
//! `docs/journal/2026-10-19-recording-identity-and-views.md`), through the
//! entry points a consumer uses: `.dendro` fixtures written with
//! `ArchiveWriter` (copies made with dendro's `copy_sources_into`, as
//! hindsight's dumps and rezolus's `combine` make them), opened through
//! `DendroCatalog` → `ArchiveReader::from_catalog` → `flatten`, and queried
//! as a `MetricsSource`. Each assertion runs on the batched rate path
//! (`QueryOptions::default()`), the per-series path
//! (`with_per_series_rates(true)`) and the display path at full resolution.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, SystemTime};

use dendro::archive::{Archive, ArchiveMut};
use dendro::rewrite::{copy_sources_into, CopySpec};
use dendro::seal::SealPolicy;
use metriken_exposition::{Counter, Gauge, Snapshot, SnapshotV2};
use metriken_query::{DisplayOptions, DisplayResult, MetricsSource, QueryOptions, QueryResult};
use metriken_storage::labeled::RECORDING_LABEL;
use metriken_storage::writer::Encoder;
use metriken_storage::{ArchiveReader, ArchiveWriter, BufferPool, DendroCatalog, WriterConfig};

const MS: u64 = 1_000_000;
const S: u64 = 1_000 * MS;
const MIN: u64 = 60 * S;
/// 2023-11-14 22:13:20 UTC.
const BASE: u64 = 1_700_000_000 * S;

/// `hh:mm:ss` after `BASE` as a timestamp.
fn at(h: u64, m: u64, s: u64) -> u64 {
    BASE + (h * 60 + m) * MIN + s * S
}

/// `BASE` plus `ms` milliseconds; the S9 fixtures' clock ("1000 s").
fn sec(ms: u64) -> u64 {
    BASE + ms * MS
}

fn rows(from: u64, to: u64, every: u64) -> Vec<u64> {
    (0..)
        .map(|k| from + k * every)
        .take_while(|t| *t <= to)
        .collect()
}

/// What a source reads at a row: counters and gauges by name, and the
/// width of each read's acquisition window, ending at the stamp.
struct Read {
    counters: Vec<(&'static str, u64)>,
    gauges: Vec<(&'static str, i64)>,
    /// Cumulative observations at 1000 and at 50,000.
    histograms: Vec<(&'static str, u64, u64)>,
    window: u64,
}

/// One source to write: labels, producer epochs (each with the `from_ts` it
/// starts at), row stamps, and what it reads at each.
struct Spec {
    labels: Vec<(&'static str, &'static str)>,
    epochs: Vec<(&'static str, u64)>,
    rows: Vec<u64>,
    read: Box<dyn Fn(u64) -> Read>,
}

fn spec(
    labels: &[(&'static str, &'static str)],
    epochs: &[(&'static str, u64)],
    rows: Vec<u64>,
    read: impl Fn(u64) -> Read + 'static,
) -> Spec {
    Spec {
        labels: labels.to_vec(),
        epochs: epochs.to_vec(),
        rows,
        read: Box::new(read),
    }
}

/// `cpu_cycles` growing 1000/s from `BASE`, read at the stamp, 1 ms
/// windows, a gauge `g` holding the row's second, and a histogram
/// `latency` observing 9 values of 1000 and one of 50,000 a second.
fn cycles(ts: u64) -> Read {
    let s = (ts - BASE) / S;
    Read {
        counters: vec![("cpu_cycles", (ts - BASE) / MS)],
        gauges: vec![("g", s as i64)],
        histograms: vec![("latency", 9 * s, s)],
        window: MS,
    }
}

fn snapshot(ts: u64, read: &Read) -> Snapshot {
    let meta = |name: &str| -> HashMap<String, String> {
        [("metric", name), ("sampler", "cpu")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let window = Some(metriken::Window::new(ts - read.window, ts));
    Snapshot::V2(SnapshotV2 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::ZERO,
        metadata: HashMap::new(),
        counters: read
            .counters
            .iter()
            .map(|(n, v)| Counter::new(n.to_string(), *v, meta(n)).with_window(window))
            .collect(),
        gauges: read
            .gauges
            .iter()
            .map(|(n, v)| Gauge::new(n.to_string(), *v, meta(n)).with_window(window))
            .collect(),
        histograms: read
            .histograms
            .iter()
            .map(|(n, low, high)| {
                let mut h = histogram::Histogram::new(4, 20).unwrap();
                h.add(1000, *low).unwrap();
                h.add(50_000, *high).unwrap();
                let mut m = meta(n);
                m.insert("grouping_power".to_string(), "4".to_string());
                m.insert("max_value_power".to_string(), "20".to_string());
                metriken_exposition::Histogram::new(n.to_string(), h, m).with_window(window)
            })
            .collect(),
        gauge_histograms: Vec::new(),
    })
}

/// Write `specs` as the sources of a new archive at `path`, each with a
/// fresh uuid.
fn write(path: &Path, specs: &[Spec]) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 50,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    for spec in specs {
        let labels: BTreeMap<String, String> = spec
            .labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut metadata = BTreeMap::new();
        match spec.epochs.as_slice() {
            [] => {}
            [(epoch, _)] => {
                metadata.insert("producer_epoch".to_string(), epoch.to_string());
            }
            several => {
                let list: Vec<String> = several
                    .iter()
                    .map(|(e, from)| format!(r#"{{"epoch":"{e}","from_ts":{from}}}"#))
                    .collect();
                metadata.insert(
                    "producer_epochs".to_string(),
                    format!("[{}]", list.join(",")),
                );
                metadata.insert(
                    "producer_epoch".to_string(),
                    several.last().unwrap().0.to_string(),
                );
            }
        }
        let mut source = writer.add_source(labels, metadata, BASE).unwrap();
        for &ts in &spec.rows {
            let staged = source
                .stage(&snapshot(ts, &(spec.read)(ts)), ts, 0)
                .unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
        }
        source.finalize((*spec.rows.last().unwrap(), 0)).unwrap();
    }
    writer.join().unwrap();
}

/// Copy every source of `from` into `into`, keeping each uuid; `range`
/// narrows the copy to rows in it (whole segments, as a hindsight dump).
fn copy(from: &Path, into: &mut ArchiveMut, range: Option<(u64, u64)>) {
    let src = Archive::open(from).unwrap();
    let encoder = Encoder::for_archive(&src).unwrap();
    let spec = match range {
        Some((start, end)) => CopySpec {
            start: start as i64,
            end: end as i64,
            ..CopySpec::everything()
        },
        None => CopySpec::everything(),
    };
    into.transaction(|tx| copy_sources_into(&src, tx, &spec, &encoder))
        .unwrap();
}

/// An archive at `into` holding a copy of each of `from` (with `range`).
fn combine(into: &Path, from: &[(&Path, Option<(u64, u64)>)]) {
    let mut out = ArchiveMut::create(into).unwrap();
    for (path, range) in from {
        copy(path, &mut out, *range);
    }
}

fn open(path: &Path) -> ArchiveReader {
    let catalog = DendroCatalog::open(path).unwrap();
    let recordings =
        ArchiveReader::from_catalog(Box::new(catalog), None, BufferPool::new(64 << 20), None)
            .unwrap();
    ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None)
}

type Labels = BTreeMap<String, String>;

/// One result series: labels (no `__name__`), points `(t, v)`, and each
/// point's band.
#[derive(Debug, Clone, PartialEq)]
struct Series {
    labels: Labels,
    points: Vec<(f64, f64)>,
    bands: Vec<Option<(f64, f64)>>,
}

fn from_matrix(result: QueryResult) -> Vec<Series> {
    let QueryResult::Matrix { result } = result else {
        panic!("not a matrix: {result:?}");
    };
    let mut out: Vec<Series> = result
        .into_iter()
        .map(|m| {
            let bands = m
                .bands
                .clone()
                .unwrap_or_else(|| vec![None; m.values.len()]);
            Series {
                labels: m
                    .metric
                    .into_iter()
                    .filter(|(k, _)| k != "__name__")
                    .collect(),
                points: m.values,
                bands,
            }
        })
        .collect();
    out.sort_by(|a, b| a.labels.cmp(&b.labels));
    out
}

/// The paths a range query can take: the batched rate path, the per-series
/// path, and the display path at full resolution (its median is the value).
#[derive(Clone, Copy, Debug)]
enum Path_ {
    Batched,
    PerSeries,
    Display,
}

const PATHS: [Path_; 3] = [Path_::Batched, Path_::PerSeries, Path_::Display];

fn range(
    reader: &ArchiveReader,
    query: &str,
    (start, end, step): (u64, u64, f64),
    path: Path_,
) -> Vec<Series> {
    let (start, end) = (start as f64 / 1e9, end as f64 / 1e9);
    let ctx = |e: metriken_query::QueryError| format!("{query} ({path:?}): {e}");
    match path {
        Path_::Batched => from_matrix(
            reader
                .query_range_opts(query, start, end, step, &QueryOptions::default())
                .unwrap_or_else(|e| panic!("{}", ctx(e))),
        ),
        Path_::PerSeries => from_matrix(
            reader
                .query_range_opts(
                    query,
                    start,
                    end,
                    step,
                    &QueryOptions::default().with_per_series_rates(true),
                )
                .unwrap_or_else(|e| panic!("{}", ctx(e))),
        ),
        Path_::Display => {
            let opts = DisplayOptions {
                budget: 0,
                ..DisplayOptions::default()
            };
            let result = reader
                .query_range_display_opts(query, start, end, step, &opts, &QueryOptions::default())
                .unwrap_or_else(|e| panic!("{}", ctx(e)));
            let DisplayResult::Series { result, .. } = result else {
                panic!("{query}: not series");
            };
            let mut out: Vec<Series> = result
                .into_iter()
                .map(|s| Series {
                    labels: s
                        .metric
                        .into_iter()
                        .filter(|(k, _)| k != "__name__")
                        .collect(),
                    points: s.points.iter().map(|p| (p.t, p.median)).collect(),
                    bands: s.points.iter().map(|p| p.unc_lo.zip(p.unc_hi)).collect(),
                })
                .collect();
            out.sort_by(|a, b| a.labels.cmp(&b.labels));
            out
        }
    }
}

/// `query`'s one series on every path, checked to agree in time and value.
fn one_series(reader: &ArchiveReader, query: &str, window: (u64, u64, f64)) -> Series {
    let mut first: Option<Series> = None;
    for path in PATHS {
        let got = range(reader, query, window, path);
        assert_eq!(got.len(), 1, "{query} ({path:?}): {:?}", labels_of(&got));
        let got = got.into_iter().next().unwrap();
        if let Some(f) = &first {
            assert_eq!(f.points.len(), got.points.len(), "{query} ({path:?})");
            for (a, b) in f.points.iter().zip(&got.points) {
                assert!(
                    (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() <= 1e-9 * a.1.abs().max(1.0),
                    "{query} ({path:?}): {a:?} against {b:?}"
                );
            }
        } else {
            first = Some(got);
        }
    }
    first.unwrap()
}

fn labels_of(series: &[Series]) -> Vec<&Labels> {
    series.iter().map(|s| &s.labels).collect()
}

fn ns(t: f64) -> u64 {
    (t * 1e9).round() as u64
}

/// S1: an archive holding a source and its dendro copy answers
/// `sum(rate(cpu_cycles[1m]))` as the source alone does. Upstream it was
/// twice that.
#[test]
fn a_source_and_its_copy_count_once() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.dendro");
    write(
        &a,
        &[spec(
            &[("host", "a"), ("source", "rezolus")],
            &[("e", at(10, 0, 0))],
            rows(at(10, 0, 0), at(10, 10, 0), S),
            cycles,
        )],
    );
    let both = dir.path().join("both.dendro");
    combine(&both, &[(&a, None), (&a, None)]);
    let (alone, union) = (open(&a), open(&both));
    assert_eq!(union.sources().len(), 2);
    let window = (at(10, 2, 0), at(10, 8, 0), 1.0);
    for query in [
        "sum(rate(cpu_cycles[1m]))",
        "rate(cpu_cycles[1m])",
        "sum(irate(cpu_cycles[5s]))",
        "sum(g)",
    ] {
        let want = one_series(&alone, query, window);
        let got = one_series(&union, query, window);
        assert_eq!(got.labels, want.labels, "{query}");
        assert_eq!(got.points, want.points, "{query}");
    }
    let sum = one_series(&union, "sum(rate(cpu_cycles[1m]))", window);
    assert!(sum.points.iter().all(|p| (p.1 - 1000.0).abs() < 1e-6));
    // Histograms combine per source in their rows, so they count once too.
    for query in [
        "histogram_count(latency)",
        "histogram_count(latency, 10)",
        "histogram_quantiles([0.5, 0.99], latency)",
    ] {
        let want = from_matrix(
            alone
                .query_range(query, window.0 as f64 / 1e9, window.1 as f64 / 1e9, 1.0)
                .unwrap(),
        );
        let got = from_matrix(
            union
                .query_range(query, window.0 as f64 / 1e9, window.1 as f64 / 1e9, 1.0)
                .unwrap(),
        );
        assert_eq!(got, want, "{query}");
        assert!(!want.is_empty());
    }
    // Raw placement: each source's own rows, so the copy adds nothing.
    for query in ["rate(cpu_cycles[1m])", "irate(cpu_cycles[5s])", "g"] {
        let raw = QueryOptions::with_rate_mode(metriken_query::RateMode::Raw);
        let (s, e) = (window.0 as f64 / 1e9, window.1 as f64 / 1e9);
        let want = from_matrix(alone.query_range_opts(query, s, e, 1.0, &raw).unwrap());
        let got = from_matrix(union.query_range_opts(query, s, e, 1.0, &raw).unwrap());
        assert_eq!(got, want, "raw {query}");
    }
    // An instant query too.
    let QueryResult::Vector { result } = union
        .query("sum(rate(cpu_cycles[1m]))", Some(at(10, 5, 0) as f64 / 1e9))
        .unwrap()
    else {
        panic!("not a vector");
    };
    assert_eq!(result.len(), 1);
    assert!((result[0].value.1 - 1000.0).abs() < 1e-6, "{result:?}");
}

/// S2: two copies with equal label sets are one series set: no
/// `__recording__`, one series per label set in a query and in the listing,
/// and `count` counts one copy's.
#[test]
fn copies_are_one_series_set() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.dendro");
    write(
        &a,
        &[spec(
            &[("host", "a"), ("source", "rezolus")],
            &[("e", at(10, 0, 0))],
            rows(at(10, 0, 0), at(10, 5, 0), S),
            cycles,
        )],
    );
    let both = dir.path().join("both.dendro");
    combine(&both, &[(&a, None), (&a, None)]);
    let (alone, union) = (open(&a), open(&both));
    let window = (at(10, 1, 0), at(10, 4, 0), 1.0);
    for query in ["count(rate(cpu_cycles[1m]))", "count(g)"] {
        assert_eq!(
            one_series(&union, query, window).points,
            one_series(&alone, query, window).points,
            "{query}"
        );
    }
    let series = range(&union, "rate(cpu_cycles[1m])", window, Path_::Batched);
    assert_eq!(series.len(), 1);
    assert!(!series[0].labels.contains_key(RECORDING_LABEL));
    for listing in [union.counter_labels("cpu_cycles"), union.gauge_labels("g")] {
        assert_eq!(listing.len(), 1, "{listing:?}");
        assert!(!listing[0].contains_key(RECORDING_LABEL));
    }
    assert_eq!(
        union.counter_labels("cpu_cycles"),
        alone.counter_labels("cpu_cycles")
    );
    // Each recording's series labels are the group's, without
    // `__recording__`.
    assert_eq!(union.read_groups().len(), 1);
}

/// Two processes recorded with equal labels (different epochs, so two
/// process groups) stay apart, as before, by `__recording__`.
#[test]
fn twins_of_two_processes_stay_apart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twins.dendro");
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e1", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 5, 0), S),
                cycles,
            ),
            spec(
                &[("host", "a")],
                &[("e2", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 5, 0), S),
                cycles,
            ),
        ],
    );
    let reader = open(&path);
    let window = (at(10, 1, 0), at(10, 4, 0), 1.0);
    let series = range(&reader, "rate(cpu_cycles[1m])", window, Path_::Batched);
    assert_eq!(series.len(), 2);
    assert!(series
        .iter()
        .all(|s| s.labels.contains_key(RECORDING_LABEL)));
    let sum = one_series(&reader, "sum(rate(cpu_cycles[1m]))", window);
    assert!(sum.points.iter().all(|p| (p.1 - 2000.0).abs() < 1e-6));
}

/// Which source answered a point: the fine source reads with 1 ms windows,
/// the coarse one with 10 ms windows, so a `rate`'s band is ten times wider
/// from the coarse one.
fn band_width(s: &Series, i: usize) -> f64 {
    let (lo, hi) = s.bands[i].expect("a band");
    hi - lo
}

/// S3: a 1 s source over 10:00 to 10:10 and a 100 ms source over 10:04 to
/// 10:05; `rate(c[10s])` at a 100 ms step takes the 100 ms source's results
/// from 10:04:10 (the first point whose window starts at or after its first
/// row) to 10:05, and the 1 s source's elsewhere.
#[test]
fn the_finest_source_answers_where_it_covers_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s3.dendro");
    let read = |window: u64| {
        move |ts: u64| Read {
            counters: vec![("c", (ts - BASE) / MS)],
            gauges: Vec::new(),
            histograms: Vec::new(),
            window,
        }
    };
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 10, 0), S),
                read(10 * MS),
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 4, 0), at(10, 5, 0), 100 * MS),
                read(MS),
            ),
        ],
    );
    let reader = open(&path);
    assert_eq!(reader.read_groups().len(), 1);
    let window = (at(10, 3, 0), at(10, 6, 0), 0.1);
    for path in [Path_::Batched, Path_::PerSeries] {
        let got = range(&reader, "rate(c[10s])", window, path);
        assert_eq!(got.len(), 1, "{path:?}");
        let s = &got[0];
        assert_eq!(s.points.len(), 1801, "{path:?}: one point per 100 ms");
        for (i, &(t, v)) in s.points.iter().enumerate() {
            assert!((v - 1000.0).abs() < 1e-6, "{path:?} at {t}: {v}");
            let fine = (at(10, 4, 10)..=at(10, 5, 0)).contains(&ns(t));
            let w = band_width(s, i);
            // 0.2 from the fine source, 2.0 from the coarse one.
            if fine {
                assert!(w < 1.0, "{path:?} at {t}: band {w}, want the fine source's");
            } else {
                assert!(
                    w > 1.0,
                    "{path:?} at {t}: band {w}, want the coarse source's"
                );
            }
        }
    }
}

/// S3 and S5: a 1 s source with one 5 s gap, beside its copy: the gap is a
/// hole (5 s against a 2 s threshold), so no point falls in it beyond one
/// interval past the last row, though the source alone gives a rate there.
#[test]
fn a_gap_in_every_member_is_a_hole() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.dendro");
    let mut stamps = rows(at(10, 0, 0), at(10, 1, 0), S);
    stamps.extend(rows(at(10, 1, 5), at(10, 2, 0), S));
    write(
        &a,
        &[spec(
            &[("host", "a")],
            &[("e", at(10, 0, 0))],
            stamps,
            cycles,
        )],
    );
    let both = dir.path().join("both.dendro");
    combine(&both, &[(&a, None), (&a, None)]);
    let window = (at(10, 0, 30), at(10, 1, 30), 1.0);
    let alone = range(&open(&a), "rate(cpu_cycles[10s])", window, Path_::Batched);
    let union = one_series(&open(&both), "rate(cpu_cycles[10s])", window);
    let inside = |t: f64| (at(10, 1, 2)..at(10, 1, 5)).contains(&ns(t));
    assert!(
        alone[0].points.iter().any(|p| inside(p.0)),
        "the source alone gives a rate inside the gap"
    );
    assert!(
        !union.points.iter().any(|p| inside(p.0)),
        "{:?}",
        union.points
    );
    // Outside the hole the union is the source.
    let outside: Vec<_> = alone[0]
        .points
        .iter()
        .copied()
        .filter(|p| !inside(p.0))
        .collect();
    assert_eq!(union.points, outside);
}

/// S9's interleaved fixture: a coarse source with rows every 1 s from 1000
/// to 1180 s, read 80 ms before its stamps, and a fine source with rows
/// every 100 ms from 1060.05 to 1119.95 s, read at its stamps, of one
/// process. `irate(c[5s])` at step 0.1 and `rate(c[10s])` read 1000/s within
/// 1e-3 everywhere, junctions included.
fn s9_fixture(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("s9.dendro");
    // The true counters: `c` 1000/s; `c2` 1000/s, 3000/s from 1060 to
    // 1120 s, then 1000/s (both in ms since BASE, so every read is an
    // integer).
    fn c2(ms: u64) -> u64 {
        let (a, b) = (1_060_000, 1_120_000);
        if ms <= a {
            ms
        } else if ms <= b {
            a + 3 * (ms - a)
        } else {
            a + 3 * (b - a) + (ms - b)
        }
    }
    let read = |stale_ms: u64| {
        move |ts: u64| {
            let ms = (ts - BASE) / MS - stale_ms;
            Read {
                counters: vec![("c", ms), ("c2", c2(ms))],
                gauges: Vec::new(),
                histograms: Vec::new(),
                window: MS,
            }
        }
    };
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", sec(0))],
                rows(sec(1_000_000), sec(1_180_000), S),
                read(80),
            ),
            spec(
                &[("host", "a")],
                &[("e", sec(0))],
                rows(sec(1_060_050), sec(1_119_950), 100 * MS),
                read(0),
            ),
        ],
    );
    path
}

#[test]
fn interleaved_sources_read_the_true_rate_at_every_point() {
    let dir = tempfile::tempdir().unwrap();
    let reader = open(&s9_fixture(dir.path()));
    assert_eq!(reader.read_groups().len(), 1);
    let window = (sec(1_010_000), sec(1_170_000), 0.1);
    for query in ["irate(c[5s])", "rate(c[10s])"] {
        let s = one_series(&reader, query, window);
        assert_eq!(s.points.len(), 1601, "{query}");
        for &(t, v) in &s.points {
            assert!((v - 1000.0).abs() < 1e-3, "{query} at {t}: {v}");
        }
        // Raw placement: the points at each source's own rows.
        let raw = QueryOptions::with_rate_mode(metriken_query::RateMode::Raw);
        let got = from_matrix(
            reader
                .query_range_opts(
                    query,
                    window.0 as f64 / 1e9,
                    window.1 as f64 / 1e9,
                    0.1,
                    &raw,
                )
                .unwrap(),
        );
        assert_eq!(got.len(), 1);
        assert!(
            got[0].points.len() > 600,
            "raw {query}: {}",
            got[0].points.len()
        );
        for &(t, v) in &got[0].points {
            assert!((v - 1000.0).abs() < 1e-3, "raw {query} at {t}: {v}");
        }
    }
}

/// S9's `c2` test: the fine source's result from 1070.1 s (the first point
/// whose window starts at or after its first row, 1060.05 s) to 1119.9 s
/// (its last point), and the coarse source's elsewhere; the reference is the
/// rows of the source used, interpolated at `t - 10 s` and `t`.
#[test]
fn each_point_is_one_sources_rate() {
    let dir = tempfile::tempdir().unwrap();
    let reader = open(&s9_fixture(dir.path()));
    // A source's rows as (stamp s, value): the true counter at the stamp
    // less the source's staleness.
    let c2 = |ms: f64| {
        let (a, b) = (1_060_000.0, 1_120_000.0);
        if ms <= a {
            ms
        } else if ms <= b {
            a + 3.0 * (ms - a)
        } else {
            a + 3.0 * (b - a) + (ms - b)
        }
    };
    let coarse: Vec<(f64, f64)> = (1000..=1180)
        .map(|s| (s as f64, c2(s as f64 * 1000.0 - 80.0)))
        .collect();
    let fine: Vec<(f64, f64)> = (0..600)
        .map(|k| {
            let t = 1060.05 + k as f64 * 0.1;
            (t, c2((t * 1000.0).round()))
        })
        .collect();
    let interp = |rows: &[(f64, f64)], t: f64| -> f64 {
        let i = rows.partition_point(|r| r.0 <= t + 1e-9);
        let (a, b) = (rows[i - 1], rows[i.min(rows.len() - 1)]);
        if (b.0 - a.0).abs() < 1e-12 {
            return a.1;
        }
        a.1 + (b.1 - a.1) * (t - a.0) / (b.0 - a.0)
    };
    let rate = |rows: &[(f64, f64)], t: f64| (interp(rows, t) - interp(rows, t - 10.0)) / 10.0;
    let window = (sec(1_010_000), sec(1_170_000), 0.1);
    let s = one_series(&reader, "rate(c2[10s])", window);
    assert_eq!(s.points.len(), 1601);
    for &(t, v) in &s.points {
        let rel = (ns(t) - BASE) as f64 / 1e9;
        let fine_span = (1070.1 - 1e-6..=1119.9 + 1e-6).contains(&rel);
        let want = if fine_span {
            rate(&fine, rel)
        } else {
            rate(&coarse, rel)
        };
        assert!(
            (v - want).abs() < 1e-6,
            "at {rel:.1} s: {v} against {want} ({})",
            if fine_span { "fine" } else { "coarse" }
        );
        if (rel - 1065.0).abs() < 1e-6 {
            assert!((v - 1984.0).abs() < 1e-6, "the worked point: {v}");
        }
        if fine_span {
            assert!((v - 3000.0).abs() < 1e-6, "at {rel:.1} s: {v}");
        }
    }
}

/// Case 1: one source spanning a restart (two producer epochs). Each epoch's
/// `from_ts` starts a new run of rows, so no increase is taken across it:
/// `rate(cpu_cycles[1m])` has no spike at the restart, whatever value the
/// counter restarts at.
#[test]
fn a_restart_is_a_counter_reset_without_a_spike() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restart.dendro");
    let restart = at(10, 5, 0);
    // Before the restart the counter has counted for a long time; after it,
    // the new process's counter starts at half that, which a reset taken as
    // an increase would read as a rate of millions.
    let read = move |ts: u64| {
        let ms = (ts - BASE) / MS;
        let value = if ts < restart {
            10_000_000_000 + ms
        } else {
            5_000_000_000 + (ts - restart) / MS
        };
        Read {
            counters: vec![("cpu_cycles", value)],
            gauges: Vec::new(),
            histograms: Vec::new(),
            window: MS,
        }
    };
    write(
        &path,
        &[spec(
            &[("host", "a")],
            &[("e1", at(10, 0, 0)), ("e2", restart)],
            rows(at(10, 0, 0), at(10, 10, 0), S),
            read,
        )],
    );
    let reader = open(&path);
    let window = (at(10, 2, 0), at(10, 8, 0), 1.0);
    // `deriv` reads the counter's values as they are, so it sees a restart
    // only if rows of two epochs share a window.
    for query in [
        "rate(cpu_cycles[1m])",
        "irate(cpu_cycles[5s])",
        "deriv(cpu_cycles[30s])",
    ] {
        let s = one_series(&reader, query, window);
        assert!(!s.points.is_empty(), "{query}");
        for &(t, v) in &s.points {
            assert!(
                (v - 1000.0).abs() < 1e-6,
                "{query} at {}: {v}",
                t - BASE as f64 / 1e9
            );
        }
    }
}

/// Case 3: a recording of agent A and a hindsight buffer's recording of
/// part of it (two uuids, one epoch): `sum(rate(cpu_cycles[1m]))` equals
/// the record's alone.
#[test]
fn a_record_and_a_dump_count_once() {
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record.dendro");
    write(
        &record,
        &[spec(
            &[("host", "a"), ("source", "rezolus")],
            &[("e", at(10, 0, 0))],
            rows(at(10, 0, 0), at(10, 30, 0), S),
            cycles,
        )],
    );
    let buffer = dir.path().join("buffer.dendro");
    write(
        &buffer,
        &[spec(
            &[("host", "a"), ("source", "rezolus")],
            &[("e", at(10, 0, 0))],
            rows(at(10, 5, 0), at(10, 20, 0), S),
            cycles,
        )],
    );
    let both = dir.path().join("both.dendro");
    combine(
        &both,
        &[
            (&record, None),
            (&buffer, Some((at(10, 8, 0), at(10, 12, 0)))),
        ],
    );
    let union = open(&both);
    assert_eq!(union.sources().len(), 2);
    assert_eq!(union.read_groups().len(), 1);
    let window = (at(10, 2, 0), at(10, 28, 0), 1.0);
    let want = one_series(&open(&record), "sum(rate(cpu_cycles[1m]))", window);
    let got = one_series(&union, "sum(rate(cpu_cycles[1m]))", window);
    assert_eq!(got.points, want.points);
}

/// Case 3: two dumps of one hindsight buffer (one uuid) covering 10:00 to
/// 10:05 and 10:20 to 10:25 are one recording with a gap between them.
#[test]
fn two_dumps_of_one_buffer_are_one_recording_with_a_gap() {
    let dir = tempfile::tempdir().unwrap();
    let buffer = dir.path().join("buffer.dendro");
    write(
        &buffer,
        &[spec(
            &[("host", "a")],
            &[("e", at(10, 0, 0))],
            rows(at(10, 0, 0), at(10, 30, 0), S),
            cycles,
        )],
    );
    let dumps = dir.path().join("dumps.dendro");
    combine(
        &dumps,
        &[
            (&buffer, Some((at(10, 0, 0), at(10, 5, 0)))),
            (&buffer, Some((at(10, 20, 0), at(10, 25, 0)))),
        ],
    );
    let reader = open(&dumps);
    assert_eq!(reader.read_groups().len(), 1);
    let window = (at(10, 0, 0), at(10, 28, 0), 1.0);
    let s = one_series(&reader, "rate(cpu_cycles[1m])", window);
    let spans: Vec<(u64, u64)> = reader
        .sources()
        .iter()
        .map(|s| s.span_ns.unwrap())
        .collect();
    // Points only where a dump holds rows, and every one at the true rate.
    for &(t, v) in &s.points {
        let t = ns(t);
        assert!(
            spans.iter().any(|&(a, b)| a <= t && t <= b + S),
            "a point at {t} in the gap"
        );
        assert!((v - 1000.0).abs() < 1e-6);
    }
    assert!(s.points.iter().any(|p| ns(p.0) < at(10, 10, 0)));
    assert!(s.points.iter().any(|p| ns(p.0) > at(10, 20, 0)));
    assert!(!s
        .points
        .iter()
        .any(|p| (at(10, 12, 0)..at(10, 18, 0)).contains(&ns(p.0))));
}

/// Case 3: two recorders of one agent at once, at 1 s and 100 ms:
/// `irate(cpu_cycles[5s])` at step 0.1 gives the 100 ms recording's points
/// (1 ms windows; the 1 s recorder's are 10 ms) over its span.
#[test]
fn two_recorders_give_the_fine_points_over_the_fine_span() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.dendro");
    let read = |window: u64| {
        move |ts: u64| Read {
            counters: vec![("cpu_cycles", (ts - BASE) / MS)],
            gauges: Vec::new(),
            histograms: Vec::new(),
            window,
        }
    };
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 4, 0), S),
                read(10 * MS),
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 1, 0), at(10, 2, 0), 100 * MS),
                read(MS),
            ),
        ],
    );
    let reader = open(&path);
    let window = (at(10, 0, 30), at(10, 3, 0), 0.1);
    for path in [Path_::Batched, Path_::PerSeries] {
        let got = range(&reader, "irate(cpu_cycles[5s])", window, path);
        assert_eq!(got.len(), 1);
        let s = &got[0];
        for (i, &(t, v)) in s.points.iter().enumerate() {
            assert!((v - 1000.0).abs() < 1e-6, "{path:?} at {t}: {v}");
            // An irate's window is one step: the fine recording covers it
            // from its first row plus a step to its last row.
            let fine = (at(10, 1, 0) + 100 * MS..=at(10, 2, 0)).contains(&ns(t));
            if fine {
                let w = band_width(s, i);
                assert!(w < 25.0, "{path:?} at {t}: band {w}");
            }
        }
    }
    // The coarse recorder alone gives no band this narrow anywhere.
    let coarse = dir.path().join("coarse.dendro");
    write(
        &coarse,
        &[spec(
            &[("host", "a")],
            &[("e", at(10, 0, 0))],
            rows(at(10, 0, 0), at(10, 4, 0), S),
            read(10 * MS),
        )],
    );
    let alone = range(
        &open(&coarse),
        "irate(cpu_cycles[5s])",
        window,
        Path_::Batched,
    );
    assert!((0..alone[0].points.len())
        .filter(|&i| alone[0].bands[i].is_some())
        .all(|i| band_width(&alone[0], i) >= 25.0));
}

/// Case 3: dumps D1 (10:00 to 10:05) and D2 (10:20 to 10:25) of one buffer
/// and a copy L of the live buffer (09:50 to 10:30), one uuid: one recording
/// with all of L's rows; with a record R of the same agent (09:00 to 11:00,
/// another uuid, one epoch), one recording from 09:00 to 11:00.
#[test]
fn copies_of_one_buffer_with_a_record_are_one_recording() {
    let dir = tempfile::tempdir().unwrap();
    let buffer = dir.path().join("buffer.dendro");
    write(
        &buffer,
        &[spec(
            &[("host", "a")],
            &[("e", at(9, 0, 0))],
            rows(at(9, 50, 0), at(10, 30, 0), S),
            cycles,
        )],
    );
    let copies = dir.path().join("copies.dendro");
    combine(
        &copies,
        &[
            (&buffer, Some((at(10, 0, 0), at(10, 5, 0)))),
            (&buffer, Some((at(10, 20, 0), at(10, 25, 0)))),
            (&buffer, None),
        ],
    );
    let reader = open(&copies);
    assert_eq!(reader.read_groups().len(), 1);
    let window = (at(9, 51, 0), at(10, 30, 0), 1.0);
    let alone = one_series(&open(&buffer), "sum(rate(cpu_cycles[1m]))", window);
    assert_eq!(
        one_series(&reader, "sum(rate(cpu_cycles[1m]))", window).points,
        alone.points
    );

    let record = dir.path().join("record.dendro");
    write(
        &record,
        &[spec(
            &[("host", "a")],
            &[("e", at(9, 0, 0))],
            rows(at(9, 0, 0), at(11, 0, 0), S),
            cycles,
        )],
    );
    let all = dir.path().join("all.dendro");
    combine(
        &all,
        &[
            (&buffer, Some((at(10, 0, 0), at(10, 5, 0)))),
            (&buffer, Some((at(10, 20, 0), at(10, 25, 0)))),
            (&buffer, None),
            (&record, None),
        ],
    );
    let reader = open(&all);
    assert_eq!(reader.read_groups().len(), 1);
    let window = (at(9, 1, 0), at(11, 0, 0), 1.0);
    let s = one_series(&reader, "sum(rate(cpu_cycles[1m]))", window);
    assert_eq!(
        s.points.len(),
        7141,
        "one point per second from 09:01 to 11:00"
    );
    assert!(s.points.iter().all(|p| (p.1 - 1000.0).abs() < 1e-6));
}

/// Case 3's epoch chain: A holds e1 then e2 (a restart), B holds e2 only,
/// overlapping A in e2 (a dump taken after the restart). One process group
/// through e2; e1's span comes from A alone, and no increase is taken across
/// e2's `from_ts`, so the counter's restart value never enters a rate.
#[test]
fn an_epoch_chain_resets_at_the_shared_epochs_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chain.dendro");
    let restart = at(10, 5, 0);
    let read = move |ts: u64| {
        let value = if ts < restart {
            10_000_000_000 + (ts - BASE) / MS
        } else {
            (ts - restart) / MS + 7
        };
        Read {
            counters: vec![("cpu_cycles", value)],
            gauges: Vec::new(),
            histograms: Vec::new(),
            window: MS,
        }
    };
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e1", at(10, 0, 0)), ("e2", restart)],
                rows(at(10, 0, 0), at(10, 10, 0), S),
                read,
            ),
            spec(
                &[("host", "a")],
                &[("e2", restart)],
                rows(at(10, 6, 0), at(10, 9, 0), S),
                read,
            ),
        ],
    );
    let reader = open(&path);
    assert_eq!(reader.read_groups().len(), 1);
    let window = (at(10, 2, 0), at(10, 9, 30), 1.0);
    for query in ["sum(rate(cpu_cycles[1m]))", "rate(cpu_cycles[30s])"] {
        let s = one_series(&reader, query, window);
        for &(t, v) in &s.points {
            assert!((v - 1000.0).abs() < 1e-6, "{query} at {t}: {v}");
        }
    }
}

/// Recordings whose sources form no group of more than one answer as they
/// did: two hosts, and one source.
#[test]
fn sources_of_different_processes_are_read_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hosts.dendro");
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e1", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 5, 0), S),
                cycles,
            ),
            spec(
                &[("host", "b")],
                &[("e2", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 5, 0), S),
                cycles,
            ),
        ],
    );
    let reader = open(&path);
    assert_eq!(reader.read_groups().len(), 2);
    let window = (at(10, 1, 0), at(10, 4, 0), 1.0);
    let s = one_series(&reader, "sum(rate(cpu_cycles[1m]))", window);
    assert!(s.points.iter().all(|p| (p.1 - 2000.0).abs() < 1e-6));
    let by_host = range(&reader, "rate(cpu_cycles[1m])", window, Path_::Batched);
    assert_eq!(by_host.len(), 2);
}

/// A source of a group carrying a label the others lack gives the group
/// that label; a group whose sources carry two values of a label is read as
/// two recordings, one per value.
#[test]
fn labels_of_a_group() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("labels.dendro");
    write(
        &path,
        &[
            spec(
                &[("host", "a"), ("arm", "x")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 5, 0), S),
                cycles,
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 1, 0), at(10, 4, 0), 100 * MS),
                cycles,
            ),
        ],
    );
    let reader = open(&path);
    let window = (at(10, 1, 30), at(10, 3, 0), 1.0);
    let s = one_series(&reader, "rate(cpu_cycles[1m])", window);
    assert_eq!(s.labels.get("arm").map(String::as_str), Some("x"));
    assert_eq!(
        one_series(&reader, r#"sum(rate(cpu_cycles{arm="x"}[1m]))"#, window).points,
        s.points
    );

    let split = dir.path().join("split.dendro");
    write(
        &split,
        &[
            spec(
                &[("host", "a"), ("arm", "x")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 5, 0), S),
                cycles,
            ),
            spec(
                &[("host", "a"), ("arm", "y")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 1, 0), at(10, 4, 0), S),
                cycles,
            ),
        ],
    );
    let reader = open(&split);
    assert_eq!(reader.read_groups().len(), 2);
    let arms: Vec<_> = range(&reader, "rate(cpu_cycles[1m])", window, Path_::Batched)
        .into_iter()
        .map(|s| s.labels["arm"].clone())
        .collect();
    assert_eq!(arms, ["x", "y"]);
}

/// A filtered copy holds fewer metrics: a metric only one source of a group
/// holds is that source's, read by the group's rules, so a hole in it is a
/// gap (S5) as it is for a metric both hold.
#[test]
fn a_metric_one_source_holds_is_read_by_the_group_rules() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("filtered.dendro");
    let both = |ts: u64| Read {
        counters: vec![("c", (ts - BASE) / MS), ("d", 2 * (ts - BASE) / MS)],
        gauges: Vec::new(),
        histograms: Vec::new(),
        window: MS,
    };
    let only_c = |ts: u64| Read {
        counters: vec![("c", (ts - BASE) / MS)],
        gauges: Vec::new(),
        histograms: Vec::new(),
        window: MS,
    };
    let mut gapped = rows(at(10, 0, 0), at(10, 1, 0), S);
    gapped.extend(rows(at(10, 1, 5), at(10, 2, 0), S));
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                gapped.clone(),
                both,
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 2, 0), S),
                only_c,
            ),
        ],
    );
    let alone = dir.path().join("alone.dendro");
    write(
        &alone,
        &[spec(&[("host", "a")], &[("e", at(10, 0, 0))], gapped, both)],
    );
    let reader = open(&path);
    assert_eq!(reader.read_groups().len(), 1);
    let window = (at(10, 0, 30), at(10, 1, 50), 1.0);
    let inside = |t: f64| (at(10, 1, 2)..at(10, 1, 5)).contains(&ns(t));
    // `c`: the gap-free source answers through the gap.
    let c = one_series(&reader, "rate(c[10s])", window);
    assert!(c.points.iter().any(|p| inside(p.0)));
    assert!(c.points.iter().all(|p| (p.1 - 1000.0).abs() < 1e-6));
    // `d`: only the gapped source holds it, and its gap is a hole.
    let d = one_series(&reader, "rate(d[10s])", window);
    let d_alone = one_series(&open(&alone), "rate(d[10s])", window);
    assert!(d_alone.points.iter().any(|p| inside(p.0)));
    assert!(!d.points.iter().any(|p| inside(p.0)), "{:?}", d.points);
    let outside: Vec<_> = d_alone
        .points
        .iter()
        .copied()
        .filter(|p| !inside(p.0))
        .collect();
    assert_eq!(d.points, outside);
}

/// A series' labels and points.
type Points = (Labels, Vec<(f64, f64)>);

fn two_metrics(ts: u64) -> Read {
    Read {
        counters: vec![("c", (ts - BASE) / MS), ("d", 2 * (ts - BASE) / MS)],
        gauges: vec![("g", ((ts - BASE) / S) as i64)],
        histograms: Vec::new(),
        window: MS,
    }
}

fn c_only(ts: u64) -> Read {
    Read {
        counters: vec![("c", (ts - BASE) / MS)],
        gauges: Vec::new(),
        histograms: Vec::new(),
        window: MS,
    }
}

/// The series of `query` on the batched and the per-series path, unsorted
/// (as the engine orders them), or the error.
fn as_ordered(
    reader: &ArchiveReader,
    query: &str,
    (start, end, step): (u64, u64, f64),
    per_series: bool,
) -> Result<Vec<Points>, String> {
    let opts = QueryOptions::default().with_per_series_rates(per_series);
    match reader.query_range_opts(query, start as f64 / 1e9, end as f64 / 1e9, step, &opts) {
        Ok(QueryResult::Matrix { result }) => Ok(result
            .into_iter()
            .map(|m| {
                let labels = m
                    .metric
                    .into_iter()
                    .filter(|(k, _)| k != "__name__")
                    .collect();
                (labels, m.values)
            })
            .collect()),
        Ok(other) => Err(format!("not a matrix: {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// A recording that does not hold a metric a query names adds no series and
/// no error, beside a process group as beside a single source: a source of
/// host a and its copy, and a source of host b holding `c` only.
#[test]
fn a_recording_lacking_a_metric_is_left_out_of_its_leaf() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.dendro");
    write(
        &a,
        &[spec(
            &[("host", "a")],
            &[("e1", at(10, 0, 0))],
            rows(at(10, 0, 0), at(10, 5, 0), S),
            two_metrics,
        )],
    );
    let b = dir.path().join("b.dendro");
    write(
        &b,
        &[spec(
            &[("host", "b")],
            &[("e2", at(10, 0, 0))],
            rows(at(10, 0, 0), at(10, 5, 0), S),
            c_only,
        )],
    );
    let with_copy = dir.path().join("with_copy.dendro");
    combine(&with_copy, &[(&a, None), (&a, None), (&b, None)]);
    let without = dir.path().join("without.dendro");
    combine(&without, &[(&a, None), (&b, None)]);
    let (with_copy, without) = (open(&with_copy), open(&without));
    assert_eq!(with_copy.read_groups().len(), 2);
    let window = (at(10, 1, 0), at(10, 4, 0), 1.0);
    for query in [
        "sum(rate(d[1m])) + sum(rate(c[1m]))",
        "rate(d[1m])",
        "sum(rate(c[1m]))",
        "g",
    ] {
        for per_series in [false, true] {
            let want = as_ordered(&without, query, window, per_series);
            let got = as_ordered(&with_copy, query, window, per_series);
            assert!(want.is_ok(), "{query}: {want:?}");
            assert_eq!(got, want, "{query} (per series {per_series})");
        }
        assert_eq!(
            one_series_or_none(&with_copy, query, window),
            one_series_or_none(&without, query, window),
            "{query} (display)"
        );
    }
    let sum = as_ordered(
        &with_copy,
        "sum(rate(d[1m])) + sum(rate(c[1m]))",
        window,
        false,
    )
    .unwrap();
    assert!(
        sum[0].1.iter().all(|p| (p.1 - 4000.0).abs() < 1e-6),
        "{sum:?}"
    );
}

/// The display path's series, sorted by labels, as `(labels, points)`.
fn one_series_or_none(reader: &ArchiveReader, query: &str, window: (u64, u64, f64)) -> Vec<Points> {
    range(reader, query, window, Path_::Display)
        .into_iter()
        .map(|s| (s.labels, s.points))
        .collect()
}

/// A process group whose filtered copy lacks `d` and `g` answers binary
/// queries over them as the source alone does.
#[test]
fn a_filtered_copy_lacking_a_metric_leaves_binary_queries_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("filtered.dendro");
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 5, 0), S),
                two_metrics,
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 5, 0), S),
                c_only,
            ),
        ],
    );
    let alone = dir.path().join("alone.dendro");
    write(
        &alone,
        &[spec(
            &[("host", "a")],
            &[("e", at(10, 0, 0))],
            rows(at(10, 0, 0), at(10, 5, 0), S),
            two_metrics,
        )],
    );
    let (union, alone) = (open(&path), open(&alone));
    assert_eq!(union.read_groups().len(), 1);
    let window = (at(10, 1, 0), at(10, 4, 0), 1.0);
    for query in [
        "rate(d[1m]) / rate(c[1m])",
        "sum(rate(d[1m])) / sum(rate(c[1m]))",
        "g + rate(c[1m])",
    ] {
        for per_series in [false, true] {
            let want = as_ordered(&alone, query, window, per_series);
            assert!(
                want.as_ref().is_ok_and(|w| !w.is_empty()),
                "{query}: {want:?}"
            );
            assert_eq!(
                as_ordered(&union, query, window, per_series),
                want,
                "{query} (per series {per_series})"
            );
        }
        assert_eq!(
            one_series_or_none(&union, query, window),
            one_series_or_none(&alone, query, window),
            "{query} (display)"
        );
    }
}

/// Series come in catalog order whether or not a recording is a group:
/// host a (with a copy) before host b.
#[test]
fn series_keep_catalog_order_beside_a_group() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.dendro");
    write(
        &a,
        &[spec(
            &[("host", "a")],
            &[("e1", at(10, 0, 0))],
            rows(at(10, 0, 0), at(10, 5, 0), S),
            cycles,
        )],
    );
    let b = dir.path().join("b.dendro");
    write(
        &b,
        &[spec(
            &[("host", "b")],
            &[("e2", at(10, 0, 0))],
            rows(at(10, 0, 0), at(10, 5, 0), S),
            cycles,
        )],
    );
    let with_copy = dir.path().join("with_copy.dendro");
    combine(
        &with_copy,
        &[
            (&a, None),
            (&a, Some((at(10, 1, 0), at(10, 3, 0)))),
            (&b, None),
        ],
    );
    let without = dir.path().join("without.dendro");
    combine(&without, &[(&a, None), (&b, None)]);
    let (with_copy, without) = (open(&with_copy), open(&without));
    let window = (at(10, 1, 0), at(10, 4, 0), 1.0);
    for query in ["rate(cpu_cycles[1m])", "g", "irate(cpu_cycles[5s]) * 2"] {
        for per_series in [false, true] {
            let got = as_ordered(&with_copy, query, window, per_series).unwrap();
            let hosts: Vec<&str> = got.iter().map(|(l, _)| l["host"].as_str()).collect();
            assert_eq!(hosts, ["a", "b"], "{query}");
            assert_eq!(
                got,
                as_ordered(&without, query, window, per_series).unwrap(),
                "{query}"
            );
        }
    }
}

/// A group's histogram combines its sources per span: a record over 10:00
/// to 11:00 and a dump of another recording of the process over 10:20 to
/// 10:25, holding the same observations, give the record's every point,
/// whichever uuid sorts first and so answers for 10:20 to 10:25.
#[test]
fn a_groups_histogram_reads_the_source_covering_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record.dendro");
    write(
        &record,
        &[spec(
            &[("host", "a")],
            &[("e", at(10, 0, 0))],
            rows(at(10, 0, 0), at(11, 0, 0), S),
            cycles,
        )],
    );
    let buffer = dir.path().join("buffer.dendro");
    write(
        &buffer,
        &[spec(
            &[("host", "a")],
            &[("e", at(10, 0, 0))],
            rows(at(10, 15, 0), at(10, 30, 0), S),
            cycles,
        )],
    );
    let window = (at(10, 0, 0), at(11, 0, 0), 1.0);
    let want_alone = open(&record);
    // Both catalog orders, so the uuid tie cannot pick the record by luck.
    for order in [[0, 1], [1, 0]] {
        let path = dir
            .path()
            .join(format!("rd{}{}.dendro", order[0], order[1]));
        let inputs: [(&Path, Option<(u64, u64)>); 2] = [
            (&record, None),
            (&buffer, Some((at(10, 20, 0), at(10, 25, 0)))),
        ];
        combine(&path, &[inputs[order[0]], inputs[order[1]]]);
        let union = open(&path);
        assert_eq!(union.read_groups().len(), 1);
        for query in [
            "histogram_count(latency)",
            "histogram_quantiles([0.5, 0.99], latency)",
            "histogram_count(latency, 10)",
            "histogram_irate(latency)",
        ] {
            let (s, e) = (window.0 as f64 / 1e9, window.1 as f64 / 1e9);
            let want = from_matrix(want_alone.query_range(query, s, e, 1.0).unwrap());
            let got = from_matrix(union.query_range(query, s, e, 1.0).unwrap());
            assert!(
                want[0].points.len() >= 360,
                "{query}: {}",
                want[0].points.len()
            );
            assert_eq!(got, want, "{query} order {order:?}");
        }
    }
}

/// A group's histogram combines its sources per span (S9): two sources of
/// one process over 10:00 to 10:10 and 10:05 to 10:15 give one count a
/// second over the whole 10:00 to 10:15, each of the ten observations a
/// second once, where reading one source left five minutes out; a stride
/// counts each window's observations once.
#[test]
fn a_groups_histogram_combines_its_sources() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("overlap.dendro");
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 10, 0), S),
                cycles,
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 5, 0), at(10, 15, 0), S),
                cycles,
            ),
        ],
    );
    let union = open(&path);
    assert_eq!(union.read_groups().len(), 1);
    let (s, e) = (at(10, 0, 0) as f64 / 1e9, at(10, 15, 0) as f64 / 1e9);
    let per_row = from_matrix(
        union
            .query_range("histogram_count(latency)", s, e, 1.0)
            .unwrap(),
    );
    assert_eq!(per_row.len(), 1);
    let points = &per_row[0].points;
    // One point a row from 10:00:01 to 10:15:00, each ten observations.
    assert_eq!(points.len(), 900, "{}", points.len());
    assert!(points.iter().all(|p| p.1 == 10.0), "{points:?}");
    let strided = from_matrix(
        union
            .query_range("histogram_count(latency, 60)", s, e, 60.0)
            .unwrap(),
    );
    let windows = &strided[0].points;
    assert!(windows.len() >= 14, "{windows:?}");
    assert!(windows.iter().all(|p| p.1 <= 600.0), "{windows:?}");
    assert!(
        windows.iter().filter(|p| p.1 == 600.0).count() >= 13,
        "{windows:?}"
    );
}

/// A fine source beside a coarse one of the same process: the fine source
/// answers where its rows run (its rows' points, a second apart), the coarse
/// one elsewhere, and a coarse pair straddling the fine run's edge is taken
/// in place of the fine pairs it overlaps, so a minute window loses less
/// than one fine interval's observations and counts none twice; a rate has
/// no point at the row a junction resumes from.
#[test]
fn a_fine_source_answers_inside_a_coarse_ones_span() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fine_coarse.dendro");
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 0, 5), at(10, 15, 5), 10 * S),
                cycles,
            ),
            // Half a second out of phase with the coarse rows, so each
            // junction resumes from a row of the other source.
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 4, 0) + S / 2, at(10, 5, 0), S),
                cycles,
            ),
        ],
    );
    let union = open(&path);
    assert_eq!(union.read_groups().len(), 1);
    let (s, e) = (at(10, 1, 0) as f64 / 1e9, at(10, 15, 0) as f64 / 1e9);
    let strided = from_matrix(
        union
            .query_range("histogram_count(latency, 60)", s, e, 60.0)
            .unwrap(),
    );
    // The whole minutes (the last window is the range's partial one).
    let windows = &strided[0].points;
    let whole = &windows[..windows.len() - 1];
    assert!(whole.len() >= 13, "{windows:?}");
    assert!(
        whole.iter().all(|p| (590.0..=600.0).contains(&p.1)),
        "{windows:?}"
    );
    // Inside the fine run its rows answer, a second apart.
    let per_row = from_matrix(
        union
            .query_range("histogram_count(latency)", s, e, 1.0)
            .unwrap(),
    );
    let fine: Vec<_> = per_row[0]
        .points
        .iter()
        .filter(|p| (at(10, 4, 10) as f64 / 1e9..=at(10, 4, 50) as f64 / 1e9).contains(&p.0))
        .collect();
    assert_eq!(fine.len(), 40, "{fine:?}");
    assert!(fine.iter().all(|p| p.1 == 10.0), "{fine:?}");
    let irate = from_matrix(
        union
            .query_range("histogram_irate(latency)", s, e, 1.0)
            .unwrap(),
    );
    assert!(!irate[0].points.is_empty());
    assert!(
        irate[0].points.iter().all(|p| (p.1 - 10.0).abs() < 1e-9),
        "{:?}",
        irate[0].points
    );
}

/// Where a read ends does not move a junction: a 1 s source on the half
/// second beside a 10 s source on the second, read to 10:10:00 (on a coarse
/// row, half a second after a fine one), ends on the fine source's rows;
/// and a 2 s source beside a 3 s one, out of phase, gives the same points
/// up to each of several ends as one read past them all.
#[test]
fn a_reads_end_does_not_move_a_junction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("end.dendro");
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 0, 0), at(10, 15, 0), 10 * S),
                cycles,
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at(10, 0, 0) + S / 2, at(10, 15, 0), S),
                cycles,
            ),
        ],
    );
    let union = open(&path);
    let (s, e) = (at(10, 5, 0) as f64 / 1e9, at(10, 10, 0) as f64 / 1e9);
    let per_row = from_matrix(
        union
            .query_range("histogram_count(latency)", s, e, 1.0)
            .unwrap(),
    );
    let points = &per_row[0].points;
    let tail = &points[points.len() - 10..];
    assert!(tail.iter().all(|p| p.1 == 10.0), "{tail:?}");
    assert!(
        tail.windows(2)
            .all(|w| (w[1].0 - w[0].0 - 1.0).abs() < 1e-9),
        "{tail:?}"
    );

    let path = dir.path().join("end23.dendro");
    let at_s = |s: u64| at(10, 0, 0) + s * S;
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at_s(60) + S / 4, at_s(200), 2 * S),
                cycles,
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at_s(10) + S / 2, at_s(200), 3 * S),
                cycles,
            ),
        ],
    );
    let union = open(&path);
    let start = at_s(55) as f64 / 1e9;
    let read = |end: f64| {
        from_matrix(
            union
                .query_range("histogram_count(latency)", start, end, 1.0)
                .unwrap(),
        )[0]
        .points
        .clone()
    };
    let long = read(at_s(100) as f64 / 1e9);
    for end_ms in [68_312u64, 69_000, 70_400, 71_750, 74_100] {
        let end = (at_s(0) + end_ms * MS) as f64 / 1e9;
        let want: Vec<_> = long.iter().copied().filter(|p| p.0 <= end).collect();
        assert_eq!(read(end), want, "end at {end_ms} ms");
    }
}

/// A finer run too short for its pairs to fit between the coarse pairs that
/// straddle its edges: the coarse pair inside it covers that time, so the
/// group counts every observation the coarse source alone does.
#[test]
fn a_short_finer_run_inside_a_coarse_one_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("short.dendro");
    let at_s = |s: u64| at(10, 0, 0) + s * S;
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at_s(20) + S / 4, at_s(28) + S / 4, 2 * S),
                cycles,
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at_s(20), at_s(59), 3 * S),
                cycles,
            ),
        ],
    );
    let union = open(&path);
    assert_eq!(union.read_groups().len(), 1);
    let (s, e) = (at_s(20) as f64 / 1e9, at_s(59) as f64 / 1e9);
    let points = from_matrix(
        union
            .query_range("histogram_count(latency)", s, e, 1.0)
            .unwrap(),
    );
    let total: f64 = points[0].points.iter().map(|p| p.1).sum();
    assert_eq!(total, 390.0, "{:?}", points[0].points);
}

/// Three members, 1 s, 5 s and 10 s, whose runs end near each other: the
/// 10 s source takes over from the 1 s one without a gap left by a 5 s pair
/// it overlaps, and every 10 s window holds at most 100 observations and at
/// least 50.
#[test]
fn three_members_hand_over_without_a_cascade_gap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("three.dendro");
    let at_s = |s: u64| at(10, 0, 0) + s * S;
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at_s(100), at_s(200), S),
                cycles,
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at_s(52), at_s(202), 5 * S),
                cycles,
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 0, 0))],
                rows(at_s(9), at_s(599), 10 * S),
                cycles,
            ),
        ],
    );
    let union = open(&path);
    assert_eq!(union.read_groups().len(), 1);
    let (s, e) = (at_s(60) as f64 / 1e9, at_s(300) as f64 / 1e9);
    let per_row = from_matrix(
        union
            .query_range("histogram_count(latency)", s, e, 1.0)
            .unwrap(),
    );
    let points = &per_row[0].points;
    // A's rows answer up to its run's end, a second apart, before C takes
    // over: no gap of two seconds or more inside A's run.
    let in_a: Vec<f64> = points
        .iter()
        .map(|p| p.0)
        .filter(|t| (at_s(101) as f64 / 1e9..=at_s(199) as f64 / 1e9).contains(t))
        .collect();
    assert!(in_a.windows(2).all(|w| w[1] - w[0] < 1.5), "{in_a:?}");
    // C's pair straddling B's run end takes over from A at 199, so A's rows
    // answer up to it, not to where B's straddling pair began.
    assert!(
        in_a.last().is_some_and(|&t| t >= at_s(199) as f64 / 1e9),
        "{in_a:?}"
    );
    let strided = from_matrix(
        union
            .query_range("histogram_count(latency, 10)", s, e, 10.0)
            .unwrap(),
    );
    let windows = &strided[0].points;
    let whole = &windows[..windows.len() - 1];
    assert!(
        whole.iter().all(|p| (50.0..=100.0).contains(&p.1)),
        "{windows:?}"
    );
}

/// A recording that is a group of one, read beside a process group, is its
/// own answers as they are: a rate across a gap in it (which a group's
/// rules would leave empty) is the recording's alone, on the batched scan
/// of every recording at once.
#[test]
fn a_single_beside_a_group_keeps_its_own_answers() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.dendro");
    write(
        &a,
        &[spec(
            &[("host", "a")],
            &[("e1", at(10, 0, 0))],
            rows(at(10, 0, 0), at(10, 2, 0), S),
            cycles,
        )],
    );
    let mut gapped = rows(at(10, 0, 0), at(10, 1, 0), S);
    gapped.extend(rows(at(10, 1, 5), at(10, 2, 0), S));
    let b = dir.path().join("b.dendro");
    write(
        &b,
        &[spec(
            &[("host", "b")],
            &[("e2", at(10, 0, 0))],
            gapped,
            cycles,
        )],
    );
    let all = dir.path().join("all.dendro");
    combine(&all, &[(&a, None), (&a, None), (&b, None)]);
    let union = open(&all);
    assert_eq!(union.read_groups().len(), 2);
    let window = (at(10, 0, 30), at(10, 1, 30), 1.0);
    let alone = one_series(&open(&b), "rate(cpu_cycles[10s])", window);
    let got = range(
        &union,
        r#"rate(cpu_cycles{host="b"}[10s])"#,
        window,
        Path_::Batched,
    );
    let inside = |t: f64| (at(10, 1, 2)..at(10, 1, 5)).contains(&ns(t));
    assert!(alone.points.iter().any(|p| inside(p.0)));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].points, alone.points);
    // And through the scan of both recordings: the sum over both hosts.
    let sum = range(
        &union,
        "sum by (host) (rate(cpu_cycles[10s]))",
        window,
        Path_::Batched,
    );
    let b_sum = sum
        .iter()
        .find(|s| s.labels.get("host").map(String::as_str) == Some("b"))
        .unwrap();
    assert_eq!(b_sum.points, alone.points);
}
