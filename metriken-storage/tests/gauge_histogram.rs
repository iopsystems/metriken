//! Gauge histograms through the archive: a `metriken::GaugeHistogram`
//! exposed by `GroupBuilder`, and a tagged V2 entry, written by
//! `ArchiveWriter` and read back by `ArchiveReader`.
//!
//! What is checked, from `docs/journal/2026-10-21-gauge-histogram.md`:
//! a gauge histogram is listed and read only through the `gauge_histogram_*`
//! methods, never through the histogram ones; its rows are the states as
//! written, sealed and in the live WAL tail alike; a segment holding one is
//! format 2 and a segment without one stays format 1; `convert` keeps the
//! tag and stamps the file.
//!
//! Its own test binary, so the registry holds only these metrics.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, SystemTime};

use dendro::seal::SealPolicy;
use metriken::{metric, AtomicCounterHistogram, GaugeHistogram, Window};
use metriken_exposition::group_builder::{DefaultRouter, GroupBuilder};
use metriken_exposition::{Snapshot, SnapshotV2, SnapshotV3};
use metriken_storage::format::FORMAT_KEY;
use metriken_storage::{
    ArchiveReader, ArchiveWriter, BufferPool, Catalog, DataSource, DendroCatalog,
    HistogramSnapshot, Labels, WriterConfig,
};

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;
const TICKS: u64 = 12;

#[metric(name = "free_extents", metadata = { acq_group = "ext4", device = "sda1" })]
static FREE_EXTENTS: GaugeHistogram = GaugeHistogram::new(0, 16);

#[metric(name = "io_latency", metadata = { acq_group = "lat" })]
static IO_LATENCY: AtomicCounterHistogram = AtomicCounterHistogram::new(0, 16);

/// The state at tick `t`: bucket 1 (size 1) shrinks, bucket 3 (sizes 4 to
/// 7) grows, so consecutive rows differ in both directions.
fn state(t: u64) -> Vec<u64> {
    let mut b = vec![0u64; FREE_EXTENTS.config().total_buckets()];
    b[1] = 100 - t;
    b[3] = 10 + 2 * t;
    b
}

/// `buckets` in the cumulative sparse form a stream row carries.
fn cumulative(buckets: &[u64]) -> HistogramSnapshot {
    let mut snap = HistogramSnapshot {
        index: Vec::new(),
        count: Vec::new(),
    };
    let mut running = 0;
    for (i, &c) in buckets.iter().enumerate() {
        if c > 0 {
            running += c;
            snap.index.push(i as u32);
            snap.count.push(running);
        }
    }
    snap
}

fn record(path: &Path, finalize: bool) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    let labels = [("source".to_string(), "test".to_string())].into();
    let mut source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
    let mut builder = GroupBuilder::new(DefaultRouter::new("fs"));
    for t in 0..TICKS {
        let ts = BASE + t * S;
        FREE_EXTENTS
            .set_with_window(&state(t), Window::new(ts - S / 2, ts))
            .unwrap();
        IO_LATENCY.increment(3).unwrap();
        let snap = Snapshot::V3(SnapshotV3 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
            duration: Duration::from_millis(1),
            metadata: HashMap::new(),
            groups: builder.build_groups(Vec::new()),
        });
        let staged = source.stage(&snap, ts, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
        source.maybe_seal().unwrap();
        source.sync().unwrap();
    }
    if finalize {
        source.finalize((BASE + (TICKS - 1) * S, 0)).unwrap();
    } else {
        source.sync().unwrap();
        std::mem::forget(source);
    }
    writer.join().unwrap();
}

fn open(path: &Path) -> ArchiveReader {
    let catalog = DendroCatalog::open(path).unwrap();
    let mut recordings = ArchiveReader::from_catalog(
        Box::new(catalog),
        None,
        BufferPool::new(64 * 1024 * 1024),
        None,
    )
    .unwrap();
    recordings.remove(0).1
}

fn table_source(reader: &ArchiveReader, name: &str) -> std::sync::Arc<dyn DataSource> {
    let owners = reader.owners_of(&[name.to_string()]);
    assert_eq!(owners.len(), 1, "one table holds {name}");
    owners[0].reader().expect("the table opens").source()
}

/// Every row of gauge histogram `name`, as `(timestamp, snapshot)`.
fn gauge_rows(source: &dyn DataSource, name: &str) -> Vec<(u64, HistogramSnapshot)> {
    let stream = source
        .gauge_histogram_stream(name, &Labels::default(), 0, u64::MAX)
        .expect("a gauge histogram stream");
    assert_eq!(stream.meta.config.grouping_power(), 0);
    assert_eq!(stream.meta.config.total_buckets(), 17);
    stream.rows.map(|r| (r.timestamp, r.snapshot)).collect()
}

fn check_reader(reader: &ArchiveReader) {
    assert_eq!(reader.gauge_histogram_names(), ["free_extents"]);
    assert_eq!(reader.histogram_names(), ["io_latency"]);
    let labels = reader.gauge_histogram_labels("free_extents");
    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0].get("device").map(String::as_str), Some("sda1"));

    let source = table_source(reader, "free_extents");
    assert_eq!(source.gauge_histogram_names(), ["free_extents"]);
    assert!(source.histogram_names().is_empty());
    assert!(
        source
            .histogram_stream("free_extents", &Labels::default(), 0, u64::MAX)
            .is_none(),
        "a gauge histogram is never read as a counting one"
    );
    let rows = gauge_rows(source.as_ref(), "free_extents");
    let expected: Vec<(u64, HistogramSnapshot)> = (0..TICKS)
        .map(|t| (BASE + t * S, cumulative(&state(t))))
        .collect();
    assert_eq!(rows.len(), expected.len());
    for ((ts, got), (want_ts, want)) in rows.iter().zip(&expected) {
        assert_eq!(ts, want_ts);
        assert_eq!(got.index, want.index, "at {ts}");
        assert_eq!(got.count, want.count, "at {ts}");
    }

    let latency = table_source(reader, "io_latency");
    assert!(latency
        .gauge_histogram_stream("io_latency", &Labels::default(), 0, u64::MAX)
        .is_none());
    assert!(latency
        .histogram_stream("io_latency", &Labels::default(), 0, u64::MAX)
        .is_some());
}

/// `table`'s sealed segments' `metriken.format` values.
fn formats(path: &Path, table: &str) -> Vec<Option<String>> {
    use parquet::file::reader::FileReader;
    let catalog = DendroCatalog::open(path).unwrap();
    let source = catalog.sources().unwrap().remove(0);
    catalog
        .segment_meta(source.id, table)
        .unwrap()
        .into_iter()
        .map(|(seq, _)| {
            let bytes = catalog
                .segment_bytes(source.id, table, seq)
                .unwrap()
                .unwrap();
            let reader =
                parquet::file::reader::SerializedFileReader::new(bytes::Bytes::from(bytes))
                    .unwrap();
            reader
                .metadata()
                .file_metadata()
                .key_value_metadata()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .find(|e| e.key == FORMAT_KEY)
                .and_then(|e| e.value)
        })
        .collect()
}

#[test]
fn a_gauge_histogram_round_trips_sealed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rec.dendro");
    record(&path, true);
    check_reader(&open(&path));

    let gauge = formats(&path, "fs/ext4");
    assert!(!gauge.is_empty());
    assert!(
        gauge.iter().all(|f| f.as_deref() == Some("2")),
        "every segment of the gauge histogram's table is format 2: {gauge:?}"
    );
    let counting = formats(&path, "fs/lat");
    assert!(!counting.is_empty());
    assert!(
        counting.iter().all(|f| f.as_deref() == Some("1")),
        "a table without one stays format 1: {counting:?}"
    );
}

/// Unsealed rows (12 ticks, a seal every 5, so 2 rows in the tail) are
/// materialized by the reader and classified the same way.
#[test]
fn a_gauge_histogram_round_trips_from_the_live_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.dendro");
    record(&path, false);
    check_reader(&open(&path));
}

/// A V2 snapshot holding one gauge histogram and one counting histogram.
#[cfg(feature = "convert")]
fn v2_snapshot(t: u64) -> Snapshot {
    let ts = BASE + t * S;
    let entry = |name: &str, metric: &str| {
        let mut h = histogram::Histogram::new(0, 16).unwrap();
        h.as_mut_slice().copy_from_slice(&state(t));
        let metadata: HashMap<String, String> = [
            ("metric", metric),
            ("sampler", "ext4"),
            ("grouping_power", "0"),
            ("max_value_power", "16"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        metriken_exposition::Histogram::new(name.to_string(), h, metadata)
    };
    Snapshot::V2(SnapshotV2 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::from_millis(1),
        metadata: [
            ("version".to_string(), "9.9.9".to_string()),
            ("producer_epoch".to_string(), "epoch-a".to_string()),
        ]
        .into(),
        counters: Vec::new(),
        gauges: Vec::new(),
        histograms: vec![entry("8", "io_latency")],
        gauge_histograms: vec![entry("7", "free_extents")],
    })
}

/// `convert` writes a raw recording to an archive: the gauge histogram is a
/// `gauge_histogram` column in a format 2 segment, the counting one a
/// `counter_histogram` column.
#[cfg(feature = "convert")]
#[test]
fn convert_writes_a_gauge_histogram_to_an_archive() {
    use metriken_storage::convert::MsgpackToArchive;

    let mut msgpack = Vec::new();
    for t in 0..3 {
        msgpack.extend(rmp_serde::to_vec(&v2_snapshot(t)).unwrap());
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("converted.dendro");
    let written = MsgpackToArchive::new()
        .label("source", "raw")
        .metadata("version", "given")
        .convert_reader(std::io::Cursor::new(msgpack.clone()), &path)
        .unwrap();
    assert_eq!(written, 3);
    // The snapshots' metadata is the source's, the caller's entries first.
    let catalog = DendroCatalog::open(&path).unwrap();
    let meta = catalog.sources().unwrap().remove(0).metadata;
    assert_eq!(meta.get("version").map(String::as_str), Some("given"));
    assert_eq!(
        meta.get("producer_epoch").map(String::as_str),
        Some("epoch-a")
    );
    drop(catalog);
    let reader = open(&path);
    assert_eq!(reader.gauge_histogram_names(), ["free_extents"]);
    assert_eq!(reader.histogram_names(), ["io_latency"]);
    let rows = gauge_rows(
        table_source(&reader, "free_extents").as_ref(),
        "free_extents",
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].1.count, cumulative(&state(1)).count);
    assert_eq!(formats(&path, "ext4"), [Some("2".to_string())]);
    assert_eq!(
        column_types(&path, "ext4"),
        [
            ("7:buckets".to_string(), "gauge_histogram".to_string()),
            ("8:buckets".to_string(), "counter_histogram".to_string()),
        ]
    );
    // An empty input writes nothing.
    let empty = dir.path().join("empty.dendro");
    assert!(MsgpackToArchive::new()
        .convert_reader(std::io::Cursor::new(Vec::new()), &empty)
        .is_err());
    assert!(!empty.exists());
    // A snapshot that does not decode, after some that do: an error naming
    // it, and no partial archive.
    let mut broken = msgpack;
    broken.extend([0xc1, 0x00]);
    let partial = dir.path().join("partial.dendro");
    let err = MsgpackToArchive::new()
        .convert_reader(std::io::Cursor::new(broken), &partial)
        .unwrap_err();
    assert!(err.to_string().contains("snapshot 3"), "{err}");
    assert!(!partial.exists());
}

/// A V2 snapshot's `gauge_histograms` entry (what `Snapshotter` writes for
/// a `GaugeHistogram`) is a gauge histogram in its sampler's table. The list
/// is the kind: a counting entry whose metadata claims `metric_type =
/// gauge_histogram` stays a counting histogram. Columns carry the new
/// strings, `counter_histogram` and `gauge_histogram`.
#[test]
fn a_v2_gauge_histogram_entry_is_a_gauge_histogram() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v2.dendro");
    let mut writer = ArchiveWriter::create(&path, WriterConfig::default()).unwrap();
    let labels = [("source".to_string(), "test".to_string())].into();
    let mut source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
    let entry = |name: &str, metric: &str, extra: &[(&str, &str)], t: u64| {
        let mut h = histogram::Histogram::new(0, 16).unwrap();
        h.as_mut_slice().copy_from_slice(&state(t));
        let mut metadata: HashMap<String, String> = [
            ("metric", metric),
            ("sampler", "ext4"),
            ("grouping_power", "0"),
            ("max_value_power", "16"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in extra {
            metadata.insert(k.to_string(), v.to_string());
        }
        metriken_exposition::Histogram::new(name.to_string(), h, metadata)
    };
    for t in 0..3u64 {
        let ts = BASE + t * S;
        let snap = Snapshot::V2(SnapshotV2 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
            duration: Duration::from_millis(1),
            metadata: HashMap::new(),
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: vec![entry(
                "8",
                "io_latency",
                &[("metric_type", "gauge_histogram")],
                t,
            )],
            gauge_histograms: vec![entry("7", "free_extents", &[], t)],
        });
        let staged = source.stage(&snap, ts, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
    }
    source.finalize((BASE + 2 * S, 0)).unwrap();
    writer.join().unwrap();

    let reader = open(&path);
    assert_eq!(reader.gauge_histogram_names(), ["free_extents"]);
    assert_eq!(reader.histogram_names(), ["io_latency"]);
    let rows = gauge_rows(
        table_source(&reader, "free_extents").as_ref(),
        "free_extents",
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].1.count, cumulative(&state(2)).count);
    assert_eq!(formats(&path, "ext4"), [Some("2".to_string())]);
    assert_eq!(
        column_types(&path, "ext4"),
        [
            ("7:buckets".to_string(), "gauge_histogram".to_string()),
            ("8:buckets".to_string(), "counter_histogram".to_string()),
        ]
    );
}

/// `table`'s first sealed segment's list columns and their `metric_type`.
fn column_types(path: &Path, table: &str) -> Vec<(String, String)> {
    use parquet::file::reader::FileReader;
    let catalog = DendroCatalog::open(path).unwrap();
    let source = catalog.sources().unwrap().remove(0);
    let (seq, _) = catalog.segment_meta(source.id, table).unwrap()[0];
    let bytes = catalog
        .segment_bytes(source.id, table, seq)
        .unwrap()
        .unwrap();
    let reader =
        parquet::file::reader::SerializedFileReader::new(bytes::Bytes::from(bytes)).unwrap();
    let schema = parquet::arrow::parquet_to_arrow_schema(
        reader.metadata().file_metadata().schema_descr(),
        reader.metadata().file_metadata().key_value_metadata(),
    )
    .unwrap();
    let mut out: Vec<(String, String)> = schema
        .fields()
        .iter()
        .filter(|f| f.name().ends_with(":buckets"))
        .map(|f| (f.name().clone(), f.metadata()["metric_type"].clone()))
        .collect();
    out.sort();
    out
}

// ─── Queries through the archive ────────────────────────────────────────────

use metriken_query::{MetricsSource, QueryError, QueryResult};

fn query_matrix(
    reader: &ArchiveReader,
    q: &str,
    from: u64,
    to: u64,
    step: f64,
) -> Vec<Vec<(f64, f64)>> {
    let (start, end) = ((BASE + from * S) as f64 / 1e9, (BASE + to * S) as f64 / 1e9);
    match reader.query_range(q, start, end, step).unwrap() {
        QueryResult::Matrix { result } => result.into_iter().map(|m| m.values).collect(),
        other => panic!("{q}: {other:?}"),
    }
}

/// `(t, items)` of `state` from tick `from` to `to`.
fn state_counts(from: u64, to: u64) -> Vec<(f64, f64)> {
    (from..=to)
        .map(|t| {
            (
                (BASE + t * S) as f64 / 1e9,
                state(t).iter().sum::<u64>() as f64,
            )
        })
        .collect()
}

fn open_flat(path: &Path) -> ArchiveReader {
    let catalog = DendroCatalog::open(path).unwrap();
    let recordings =
        ArchiveReader::from_catalog(Box::new(catalog), None, BufferPool::new(64 << 20), None)
            .unwrap();
    ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None)
}

/// Through `ArchiveReader` as a `MetricsSource`: the histogram functions
/// read the state at each step; the counting histogram beside it still
/// reads deltas; the delta functions refuse.
#[test]
fn an_archive_answers_with_the_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rec.dendro");
    record(&path, true);
    let reader = open(&path);
    assert_eq!(
        MetricsSource::gauge_histogram_names(&reader),
        ["free_extents"]
    );
    assert_eq!(MetricsSource::histogram_names(&reader), ["io_latency"]);

    assert_eq!(
        query_matrix(&reader, "histogram_count(free_extents)", 2, 9, 1.0),
        [state_counts(2, 9)]
    );
    assert_eq!(
        query_matrix(&reader, "histogram_quantile(1.0, free_extents)", 2, 2, 1.0),
        [[((BASE + 2 * S) as f64 / 1e9, 7.0)]]
    );
    // The counting histogram: one observation a tick.
    let latency = query_matrix(&reader, "histogram_count(io_latency)", 2, 9, 1.0);
    assert!(latency[0].iter().all(|(_, v)| *v == 1.0), "{latency:?}");

    let (start, end) = ((BASE + 2 * S) as f64 / 1e9, (BASE + 9 * S) as f64 / 1e9);
    for q in [
        "histogram_irate(free_extents)",
        "histogram_count(free_extents, 5)",
        "rate(free_extents[5s])",
    ] {
        match reader.query_range(q, start, end, 1.0) {
            Err(QueryError::Unsupported(msg)) => {
                assert!(
                    msg.contains("free_extents is a gauge histogram"),
                    "{q}: {msg}"
                )
            }
            other => panic!("{q}: expected a refusal, got {other:?}"),
        }
    }
}

/// A source and its copy are one process group (S1): the copy's rows are
/// the same states, merged into one series, so the count is the source's
/// alone rather than twice it.
#[test]
fn a_source_and_its_copy_count_once() {
    use dendro::archive::{Archive, ArchiveMut};
    use dendro::rewrite::{copy_sources_into, CopySpec};
    use metriken_storage::writer::Encoder;

    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.dendro");
    record(&a, true);
    let both = dir.path().join("both.dendro");
    {
        let mut out = ArchiveMut::create(&both).unwrap();
        for _ in 0..2 {
            let src = Archive::open(&a).unwrap();
            let encoder = Encoder::for_archive(&src).unwrap();
            out.transaction(|tx| copy_sources_into(&src, tx, &CopySpec::everything(), &encoder))
                .unwrap();
        }
    }
    let union = open_flat(&both);
    assert_eq!(union.sources().len(), 2);
    for q in [
        "histogram_count(free_extents)",
        "histogram_quantiles([0.5, 1.0], free_extents)",
    ] {
        assert_eq!(
            query_matrix(&union, q, 2, 9, 1.0),
            query_matrix(&open(&a), q, 2, 9, 1.0),
            "{q}"
        );
    }
    assert_eq!(
        query_matrix(&union, "histogram_count(free_extents)", 2, 9, 1.0),
        [state_counts(2, 9)]
    );
}

/// Two recordings of one metric under different bucket configurations sum
/// after rebucketing to the coarsest (the lower grouping power).
#[test]
fn mixed_bucket_configs_sum_at_the_coarsest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.dendro");
    let mut writer = ArchiveWriter::create(&path, WriterConfig::default()).unwrap();
    for (host, gp) in [("a", 0u8), ("b", 2u8)] {
        let labels = [
            ("source".to_string(), "test".to_string()),
            ("host".to_string(), host.to_string()),
        ]
        .into();
        let mut source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
        for t in 0..3u64 {
            let ts = BASE + t * S;
            let mut h = histogram::Histogram::new(gp, 16).unwrap();
            // 3 items of value 5 and 1 of value 6: at grouping power 2 they
            // are in two buckets ([5, 5] and [6, 6]); at 0, in [4, 7].
            for v in [5, 5, 5, 6] {
                h.increment(v).unwrap();
            }
            let metadata: HashMap<String, String> = [
                ("metric", "free_extents".to_string()),
                ("sampler", "ext4".to_string()),
                ("grouping_power", gp.to_string()),
                ("max_value_power", "16".to_string()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
            let snap = Snapshot::V2(SnapshotV2 {
                systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
                duration: Duration::from_millis(1),
                metadata: HashMap::new(),
                counters: Vec::new(),
                gauges: Vec::new(),
                histograms: Vec::new(),
                gauge_histograms: vec![metriken_exposition::Histogram::new(
                    "7".to_string(),
                    h,
                    metadata,
                )],
            });
            let staged = source.stage(&snap, ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
        }
        source.finalize((BASE + 2 * S, 0)).unwrap();
    }
    writer.join().unwrap();
    let reader = open_flat(&path);
    let (t, _) = state_counts(1, 1)[0];
    assert_eq!(
        query_matrix(&reader, "histogram_count(free_extents)", 1, 1, 1.0),
        [[(t, 8.0)]]
    );
    let QueryResult::HistogramHeatmap { result } = reader
        .query_range("histogram_heatmap(free_extents)", t, t, 1.0)
        .unwrap()
    else {
        panic!("a heatmap")
    };
    // One bucket of the coarser configuration holds all eight.
    assert_eq!(result.bucket_bounds, [7]);
    assert_eq!(result.data, [(0, 0, 8.0)]);
}

/// Two recorders of one agent (one producer epoch, so one process group): a
/// fine source with rows every second from 0 to 10 s and a coarse one every
/// 5 s from 0 to 40 s, each row's state holding `t + 1` items. After the
/// fine source ends the coarse rows answer at every step: the group's
/// staleness is the coarse source's 5 s, not the fine source's 1 s, which
/// left points only at 15, 16, 20, 21, 25, 26 and 30.
#[test]
fn a_group_reads_its_coarse_source_after_the_fine_one_ends() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.dendro");
    let mut writer = ArchiveWriter::create(&path, WriterConfig::default()).unwrap();
    for ticks in [
        (0..=10).collect::<Vec<u64>>(),
        (0..=40).step_by(5).collect(),
    ] {
        let mut source = writer
            .add_source(
                [("source".to_string(), "agent".to_string())].into(),
                [("producer_epoch".to_string(), "e1".to_string())].into(),
                BASE,
            )
            .unwrap();
        let mut last = BASE;
        for t in ticks {
            let ts = BASE + t * S;
            last = ts;
            let mut h = histogram::Histogram::new(0, 16).unwrap();
            h.as_mut_slice()[1] = t + 1;
            let metadata: HashMap<String, String> = [
                ("metric", "free"),
                ("sampler", "ext4"),
                ("grouping_power", "0"),
                ("max_value_power", "16"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
            let snap = Snapshot::V2(SnapshotV2 {
                systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
                duration: Duration::from_millis(1),
                metadata: HashMap::new(),
                counters: Vec::new(),
                gauges: Vec::new(),
                histograms: Vec::new(),
                gauge_histograms: vec![metriken_exposition::Histogram::new(
                    "7".to_string(),
                    h,
                    metadata,
                )],
            });
            let staged = source.stage(&snap, ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
        }
        source.finalize((last, 0)).unwrap();
    }
    writer.join().unwrap();
    let reader = open_flat(&path);
    assert_eq!(reader.sources().len(), 2);
    let got = query_matrix(&reader, "histogram_count(free)", 0, 30, 1.0);
    assert_eq!(got.len(), 1, "one series for the group: {got:?}");
    let base = BASE as f64 / 1e9;
    let want: Vec<(f64, f64)> = (0..=30u64)
        .map(|t| {
            // The latest row at or before t: every second to 10 s, then
            // every 5 s.
            let row = if t <= 10 { t } else { t / 5 * 5 };
            (base + t as f64, (row + 1) as f64)
        })
        .collect();
    assert_eq!(got[0], want);
}

/// An archive of one producer epoch, one source per `ticks` list (so the
/// sources form one process group), each row holding gauge histogram
/// `free` with `t + 1` items and gauge `g` = `t + 1`.
fn write_group(path: &Path, sources: &[Vec<u64>]) {
    let mut writer = ArchiveWriter::create(path, WriterConfig::default()).unwrap();
    for ticks in sources {
        let mut source = writer
            .add_source(
                [("source".to_string(), "agent".to_string())].into(),
                [("producer_epoch".to_string(), "e1".to_string())].into(),
                BASE,
            )
            .unwrap();
        let mut last = BASE;
        for &t in ticks {
            let ts = BASE + t * S;
            last = ts;
            let mut h = histogram::Histogram::new(0, 16).unwrap();
            h.as_mut_slice()[1] = t + 1;
            let md = |metric: &str, histogram: bool| -> HashMap<String, String> {
                let mut m: HashMap<String, String> = [("metric", metric), ("sampler", "ext4")]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                if histogram {
                    m.insert("grouping_power".to_string(), "0".to_string());
                    m.insert("max_value_power".to_string(), "16".to_string());
                }
                m
            };
            let snap = Snapshot::V2(SnapshotV2 {
                systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
                duration: Duration::from_millis(1),
                metadata: HashMap::new(),
                counters: Vec::new(),
                gauges: vec![metriken_exposition::Gauge::new(
                    "8".to_string(),
                    t as i64 + 1,
                    md("g", false),
                )],
                histograms: Vec::new(),
                gauge_histograms: vec![metriken_exposition::Histogram::new(
                    "7".to_string(),
                    h,
                    md("free", true),
                )],
            });
            let staged = source.stage(&snap, ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
        }
        source.finalize((last, 0)).unwrap();
    }
    writer.join().unwrap();
}

/// The reviewer's probe: a fine source (1 s) with a gap from 20 to 28 s
/// and a coarse one (10 s) holding only 40 to 60 s, one process group. A
/// fine row is held for the fine source's 1 s, not the coarse source's
/// 10 s, so in the gap there is no point, as there is none for the gauge
/// `g` beside it.
#[test]
fn a_group_holds_each_row_for_its_own_sources_interval() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gap.dendro");
    let fine: Vec<u64> = (0..=19).chain(29..=60).collect();
    let coarse: Vec<u64> = (40..=60).step_by(10).collect();
    write_group(&path, &[fine, coarse]);
    let reader = open_flat(&path);
    let times = |q: &str| -> Vec<i64> {
        let got = query_matrix(&reader, q, 0, 60, 1.0);
        assert_eq!(got.len(), 1, "{q}: {got:?}");
        got[0]
            .iter()
            .map(|(t, _)| (t - BASE as f64 / 1e9).round() as i64)
            .collect()
    };
    let want: Vec<i64> = (0..=20).chain(29..=60).collect();
    assert_eq!(times("histogram_count(free)"), want);
    assert_eq!(times("g"), want);
}
