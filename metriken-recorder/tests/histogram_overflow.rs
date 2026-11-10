//! A Prometheus histogram's observations above its highest finite bucket:
//! exposition text through `PrometheusConverter::convert` and
//! `SourceRecorder::stage` into an archive, read back by `ArchiveReader`
//! and queried through metriken-query.
//!
//! What is checked, from `docs/journal/2026-11-09-histogram-overflow.md`:
//! the overflow is the counter `<histogram>_overflow`; `histogram_quantile`
//! counts it in the population and reads a quantile in it as `+Inf` with the
//! band `(top finite bound, +Inf)`; a stride window, several pooled series,
//! and an older recording without the overflow; a scrape that has its own
//! `<histogram>_overflow`; and the JSON of a `+Inf` result.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use dendro::seal::SealPolicy;
use metriken_model::Snapshot;
use metriken_query::{MatrixSample, MetricsSource, QueryResult};
use metriken_recorder::PrometheusConverter;
use metriken_storage::{
    ArchiveReader, ArchiveWriter, BufferPool, CounterValues, DendroCatalog, Labels, WriterConfig,
    OVERFLOW_BOUND_KEY, OVERFLOW_KEY,
};

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;
const TICKS: u64 = 12;

// ─── What an exporter serves at tick `t` ────────────────────────────────────

/// The issue's scrape, `t` times over: per tick, 5 observations at or below
/// 1, 4 in `(1, 10]` and 1 above 10, so `le=1: 5, le=10: 9, le=+Inf: 10`
/// between consecutive ticks.
fn lat(t: u64, labels: &str) -> String {
    format!(
        "\
lat_bucket{{{labels}le=\"1\"}} {a}
lat_bucket{{{labels}le=\"10\"}} {b}
lat_bucket{{{labels}le=\"+Inf\"}} {c}
lat_sum{{{labels}}} {sum}
lat_count{{{labels}}} {c}
",
        a = 5 * t,
        b = 9 * t,
        c = 10 * t,
        sum = 100 * t,
    )
}

fn one_series(t: u64) -> String {
    format!("# TYPE lat histogram\n{}", lat(t, ""))
}

/// Two series: `op="a"` as [`one_series`], `op="b"` with 10 observations
/// at or below 1 a tick and none above 10.
fn two_series(t: u64) -> String {
    format!(
        "# TYPE lat histogram\n{}\
lat_bucket{{op=\"b\",le=\"1\"}} {n}
lat_bucket{{op=\"b\",le=\"10\"}} {n}
lat_bucket{{op=\"b\",le=\"+Inf\"}} {n}
",
        lat(t, "op=\"a\","),
        n = 10 * t,
    )
}

/// [`one_series`] and a gauge of the overflow series' name and labels.
fn colliding(t: u64) -> String {
    format!(
        "{}# TYPE lat_overflow gauge\nlat_overflow 42\n",
        one_series(t)
    )
}

// ─── Writing ────────────────────────────────────────────────────────────────

/// What the converter wrote before the overflow existed: the snapshot
/// without the overflow counters and without the histograms' keys.
fn without_overflow(mut snapshot: Snapshot) -> Snapshot {
    let Snapshot::V3(v3) = &mut snapshot else {
        panic!("a scrape is a V3 snapshot")
    };
    for group in &mut v3.groups {
        let schema = Arc::make_mut(group.schema.as_mut().unwrap());
        for h in &mut schema.histograms {
            assert!(h.metadata.contains_key(OVERFLOW_KEY));
            h.metadata.remove(OVERFLOW_KEY);
            h.metadata.remove(OVERFLOW_BOUND_KEY);
        }
        let keep: Vec<bool> = schema
            .float_counters
            .iter()
            .map(|d| !d.metadata["metric"].ends_with("_overflow"))
            .collect();
        let mut i = 0;
        schema.float_counters.retain(|_| {
            i += 1;
            keep[i - 1]
        });
        let mut i = 0;
        group.float_counters.retain(|_| {
            i += 1;
            keep[i - 1]
        });
        group.schema_hash = schema.hash();
    }
    snapshot
}

/// One recording to write: its `source` label, the text at each tick, its
/// ticks, and whether it is written as before the overflow existed.
struct Rec<'a> {
    source: &'a str,
    text: &'a dyn Fn(u64) -> String,
    ticks: std::ops::Range<u64>,
    old: bool,
    /// The producer epoch: recordings of one source label and epoch whose
    /// spans meet are one process.
    epoch: Option<&'a str>,
}

/// Each recording scraped once a second through its own converter into one
/// archive, sealing every 5 rows.
fn record(path: &Path, recs: &[Rec]) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    for rec in recs {
        let labels = BTreeMap::from([("source".to_string(), rec.source.to_string())]);
        let first = BASE + rec.ticks.start * S;
        let metadata = rec
            .epoch
            .map(|e| BTreeMap::from([("producer_epoch".to_string(), e.to_string())]))
            .unwrap_or_default();
        let mut source = writer.add_source(labels, metadata, first).unwrap();
        let mut converter = PrometheusConverter::new();
        for t in rec.ticks.clone() {
            let ts = BASE + t * S;
            let mut snapshot = converter.convert(&(rec.text)(t), ts - S / 2, ts);
            if rec.old {
                snapshot = without_overflow(snapshot);
            }
            let staged = source.stage(&snapshot, ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
            source.sync().unwrap();
        }
        source
            .finalize((BASE + (rec.ticks.end - 1) * S, 0))
            .unwrap();
    }
    writer.join().unwrap();
}

fn simple(path: &Path, text: &dyn Fn(u64) -> String, old: bool) {
    record(
        path,
        &[Rec {
            source: "exporter",
            text,
            ticks: 0..TICKS,
            old,
            epoch: None,
        }],
    );
}

// ─── Reading ────────────────────────────────────────────────────────────────

/// Every source of the archive as one reader.
fn open(path: &Path) -> ArchiveReader {
    let recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(path).unwrap()),
        None,
        BufferPool::new(64 << 20),
        None,
    )
    .unwrap();
    ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None)
}

/// `query`'s result series over the recorded ticks at a 1 s step.
fn matrix(reader: &ArchiveReader, query: &str) -> Vec<MatrixSample> {
    let (lo, hi) = (BASE / S, BASE / S + TICKS - 1);
    let result = reader
        .query_range(query, lo as f64, hi as f64, 1.0)
        .unwrap_or_else(|e| panic!("{query}: {e}"));
    let QueryResult::Matrix { result, .. } = result else {
        panic!("{query}: not a matrix")
    };
    result
}

/// The one series of `query`: each point's value and band.
fn points(reader: &ArchiveReader, query: &str) -> Vec<(f64, (f64, f64))> {
    let mut m = matrix(reader, query);
    assert_eq!(m.len(), 1, "{query}: {m:?}");
    let m = m.remove(0);
    let bands = m.intervals.unwrap_or_else(|| panic!("{query}: no bands"));
    m.values.iter().map(|(_, v)| *v).zip(bands).collect()
}

/// Every point of `query` reads `value` with band `band`, and there are
/// some.
fn all(reader: &ArchiveReader, query: &str, value: f64, band: (f64, f64)) {
    let got = points(reader, query);
    assert!(!got.is_empty(), "{query}: no points");
    assert!(
        got.iter().all(|p| *p == (value, band)),
        "{query}: want {value} in {band:?}: {got:?}"
    );
}

const OVER: (f64, (f64, f64)) = (f64::INFINITY, (10.0, f64::INFINITY));
const TEN: (f64, (f64, f64)) = (10.0, (10.0, 10.0));
const ONE: (f64, (f64, f64)) = (1.0, (1.0, 1.0));

// ─── Tests ──────────────────────────────────────────────────────────────────

/// The issue's test: the overflow counter reads 1 a tick, the 0.95
/// quantile is in it, and the 0.5 quantile is where it was.
#[test]
fn a_quantile_above_the_highest_finite_bucket_reads_inf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    simple(&path, &one_series, false);
    let reader = open(&path);

    let owners = reader.owners_of(&["lat_overflow".to_string()]);
    let source = owners[0].reader().unwrap().source();
    let c = source
        .counters("lat_overflow", &Labels::default(), 0, u64::MAX)
        .expect("the overflow counter")
        .series
        .remove(0);
    assert_eq!(
        c.values,
        CounterValues::Float((0..TICKS).map(|t| t as f64).collect())
    );
    // The histogram's keys are not labels.
    let owners = reader.owners_of(&["lat".to_string()]);
    let stream = owners[0]
        .reader()
        .unwrap()
        .source()
        .histogram_stream("lat", &Labels::default(), 0, u64::MAX)
        .unwrap();
    assert_eq!(stream.meta.series, vec![Labels::default()]);

    all(&reader, "histogram_quantile(0.95, lat)", OVER.0, OVER.1);
    all(&reader, "histogram_quantile(0.9, lat)", TEN.0, TEN.1);
    all(&reader, "histogram_quantile(0.5, lat)", ONE.0, ONE.1);
    // `irate` of the overflow counter: one observation a second above 10.
    let m = matrix(&reader, "irate(lat_overflow[5s])");
    assert!(m[0].values.iter().all(|(_, v)| *v == 1.0), "{m:?}");
}

/// The same scrapes recorded as before the overflow existed read as before:
/// the 0.95 quantile over the 9 finite observations is the `le=10` bucket.
#[test]
fn a_recording_without_the_overflow_reads_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    simple(&path, &one_series, true);
    let reader = open(&path);
    assert!(!reader.counter_names().contains(&"lat_overflow".to_string()));
    all(&reader, "histogram_quantile(0.95, lat)", TEN.0, TEN.1);
    all(&reader, "histogram_quantile(0.5, lat)", ONE.0, ONE.1);
}

/// A stride window pools its rows' overflow with their buckets: 3 s of
/// the issue's scrape is 27 finite observations and 3 above 10.
#[test]
fn a_stride_window_counts_its_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    simple(&path, &one_series, false);
    let reader = open(&path);
    let m = matrix(&reader, "histogram_quantiles([0.5, 0.9, 0.95], lat, 3)");
    let by_q: HashMap<String, &MatrixSample> = m
        .iter()
        .map(|s| (s.metric["quantile"].clone(), s))
        .collect();
    let check = |q: &str, want: (f64, (f64, f64))| {
        let s = by_q[q];
        let bands = s.intervals.as_ref().unwrap();
        assert!(!s.values.is_empty(), "{q}");
        for ((_, v), b) in s.values.iter().zip(bands) {
            assert_eq!((*v, *b), want, "{q}: {s:?}");
        }
    };
    check("0.95", OVER);
    check("0.9", TEN);
    check("0.5", ONE);
}

/// Two series of one histogram, one with an overflow, pool into one
/// population: 19 finite observations a tick and 1 above 10.
#[test]
fn pooled_series_count_each_ones_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    simple(&path, &two_series, false);
    let reader = open(&path);
    // ceil(0.97 · 20) = 20 > 19: in the overflow. Without it,
    // ceil(0.97 · 19) = 19 would be the `le=10` bucket.
    all(&reader, "histogram_quantile(0.97, lat)", OVER.0, OVER.1);
    // ceil(0.9 · 20) = 18 <= 19.
    all(&reader, "histogram_quantile(0.9, lat)", TEN.0, TEN.1);
    // A selector of the series without an overflow reads only it.
    all(
        &reader,
        "histogram_quantile(0.99, lat{op=\"b\"})",
        ONE.0,
        ONE.1,
    );
}

/// Two recordings of one endpoint, one written before the overflow
/// existed, pooled: only the newer one's overflow counts. Per tick, 18
/// finite observations and 1 above 10.
#[test]
fn an_older_recording_pools_with_a_newer_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    record(
        &path,
        &[
            Rec {
                source: "old",
                text: &one_series,
                ticks: 0..TICKS,
                old: true,
                epoch: None,
            },
            Rec {
                source: "new",
                text: &one_series,
                ticks: 0..TICKS,
                old: false,
                epoch: None,
            },
        ],
    );
    let reader = open(&path);
    // ceil(0.97 · 19) = 19 > 18.
    all(&reader, "histogram_quantile(0.97, lat)", OVER.0, OVER.1);
    // ceil(0.9 · 19) = 18 <= 18.
    all(&reader, "histogram_quantile(0.9, lat)", TEN.0, TEN.1);
    all(
        &reader,
        "histogram_quantile(0.95, lat{source=\"old\"})",
        TEN.0,
        TEN.1,
    );
    all(
        &reader,
        "histogram_quantile(0.95, lat{source=\"new\"})",
        OVER.0,
        OVER.1,
    );
}

/// One process recorded twice, first as before the overflow existed and
/// then with it, the two overlapping: the process's rows combine, and a
/// tick read from the newer recording counts its overflow.
#[test]
fn one_process_recorded_before_and_after_combines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    record(
        &path,
        &[
            Rec {
                source: "exporter",
                text: &one_series,
                ticks: 0..7,
                old: true,
                epoch: Some("e"),
            },
            Rec {
                source: "exporter",
                text: &one_series,
                ticks: 4..TICKS,
                old: false,
                epoch: Some("e"),
            },
        ],
    );
    let reader = open(&path);
    assert_eq!(reader.process_groups().groups.len(), 1);
    let got = points(&reader, "histogram_quantile(0.95, lat)");
    assert_eq!(got.len(), TICKS as usize - 1, "{got:?}");
    assert!(got.iter().all(|p| *p == TEN || *p == OVER), "{got:?}");
    assert_eq!(got[0], TEN, "{got:?}");
    assert_eq!(got[got.len() - 1], OVER, "{got:?}");
}

/// A scrape with its own `lat_overflow` keeps it, and the histogram has no
/// overflow series: it reads as before.
#[test]
fn a_scrape_with_its_own_overflow_name_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    simple(&path, &colliding, false);
    let reader = open(&path);
    assert!(reader.gauge_names().contains(&"lat_overflow".to_string()));
    assert!(!reader.counter_names().contains(&"lat_overflow".to_string()));
    all(&reader, "histogram_quantile(0.95, lat)", TEN.0, TEN.1);
}

/// A `+Inf` quantile and its band are the string `"+Inf"` in JSON, and the
/// JSON reads back.
#[test]
fn an_inf_quantile_round_trips_through_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    simple(&path, &one_series, false);
    let reader = open(&path);
    let (lo, hi) = (BASE / S, BASE / S + TICKS - 1);
    let result = reader
        .query_range("histogram_quantile(0.95, lat)", lo as f64, hi as f64, 1.0)
        .unwrap();
    let json = serde_json::to_value(&result).unwrap();
    let first = &json["result"][0];
    assert_eq!(first["values"][0][1], "+Inf");
    assert_eq!(first["intervals"][0], serde_json::json!([10.0, "+Inf"]));
    assert_eq!(first["bands"][0], serde_json::json!([10.0, "+Inf"]));
    let back: QueryResult = serde_json::from_value(json).unwrap();
    let QueryResult::Matrix { result: back, .. } = back else {
        panic!("not a matrix")
    };
    assert_eq!(back[0].values[0].1, f64::INFINITY);
    assert_eq!(back[0].intervals.as_ref().unwrap()[0], OVER.1);
}

/// The live path: the converter's snapshots ingested into a `MemoryStore`
/// read as the recording does.
#[test]
fn the_live_store_counts_the_overflow() {
    for (old, want) in [(false, OVER), (true, TEN)] {
        let store = metriken_query::MemoryStore::builder()
            .sampling_interval_ms(1000)
            .build();
        let mut converter = PrometheusConverter::new();
        for t in 0..TICKS {
            let ts = BASE + t * S;
            let mut snapshot = converter.convert(&one_series(t), ts - S / 2, ts);
            if old {
                snapshot = without_overflow(snapshot);
            }
            let Snapshot::V3(v3) = &mut snapshot else {
                panic!("a scrape is a V3 snapshot")
            };
            v3.systemtime = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ts);
            store.ingest_snapshot(snapshot);
        }
        let (lo, hi) = (BASE / S, BASE / S + TICKS - 1);
        let QueryResult::Matrix { result, .. } = store
            .query_range("histogram_quantile(0.95, lat)", lo as f64, hi as f64, 1.0)
            .unwrap()
        else {
            panic!("not a matrix")
        };
        let bands = result[0].intervals.as_ref().unwrap();
        assert!(!bands.is_empty());
        for ((_, v), b) in result[0].values.iter().zip(bands) {
            assert_eq!((*v, *b), want, "old={old}: {result:?}");
        }
    }
}
