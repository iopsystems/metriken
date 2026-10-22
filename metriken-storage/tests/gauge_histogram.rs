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
    let entry = |name: &str, metric: &str, gauge: bool| {
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
        if gauge {
            metadata.insert("metric_type".to_string(), "gauge_histogram".to_string());
        }
        metriken_exposition::Histogram::new(name.to_string(), h, metadata)
    };
    Snapshot::V2(SnapshotV2 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::from_millis(1),
        metadata: HashMap::new(),
        counters: Vec::new(),
        gauges: Vec::new(),
        histograms: vec![
            entry("7", "free_extents", true),
            entry("8", "io_latency", false),
        ],
    })
}

/// `convert` keeps a gauge histogram's tag (it used to overwrite every
/// histogram's `metric_type` with `histogram`), writes it in the standard
/// layout even when the sparse one is asked for, and stamps the file
/// format 2.
#[cfg(feature = "convert")]
#[test]
fn convert_keeps_a_gauge_histogram() {
    use metriken_storage::convert::{MsgpackToParquet, ParquetHistogramType, ParquetOptions};
    use parquet::file::reader::FileReader;

    let mut msgpack = Vec::new();
    for t in 0..3 {
        msgpack.extend(rmp_serde::to_vec(&v2_snapshot(t)).unwrap());
    }
    for layout in [ParquetHistogramType::Standard, ParquetHistogramType::Sparse] {
        let mut out = Vec::new();
        MsgpackToParquet::with_options(ParquetOptions::new().histogram_type(layout))
            .convert_file_handle(std::io::Cursor::new(msgpack.clone()), &mut out)
            .unwrap();
        let file =
            parquet::file::reader::SerializedFileReader::new(bytes::Bytes::from(out.clone()))
                .unwrap();
        let format = file
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .find(|e| e.key == FORMAT_KEY)
            .and_then(|e| e.value);
        assert_eq!(format.as_deref(), Some("2"), "{layout:?}");

        let source = metriken_storage::parquet::MultiParquetSource::open_bytes_with_pool(
            out,
            BufferPool::new(1 << 20),
        )
        .unwrap();
        assert_eq!(
            source.gauge_histogram_names(),
            ["free_extents"],
            "{layout:?}"
        );
        let rows = gauge_rows(&source, "free_extents");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].1.count, cumulative(&state(1)).count);
        if layout == ParquetHistogramType::Standard {
            assert_eq!(source.histogram_names(), ["io_latency"]);
        }
    }
}

/// A V2 entry tagged `metric_type = gauge_histogram` (what `Snapshotter`
/// writes) is a gauge histogram in its sampler's table.
#[test]
fn a_tagged_v2_entry_is_a_gauge_histogram() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v2.dendro");
    let mut writer = ArchiveWriter::create(&path, WriterConfig::default()).unwrap();
    let labels = [("source".to_string(), "test".to_string())].into();
    let mut source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
    for t in 0..3u64 {
        let ts = BASE + t * S;
        let mut h = histogram::Histogram::new(0, 16).unwrap();
        h.as_mut_slice().copy_from_slice(&state(t));
        let metadata: HashMap<String, String> = [
            ("metric", "free_extents"),
            ("sampler", "ext4"),
            ("grouping_power", "0"),
            ("max_value_power", "16"),
            ("metric_type", "gauge_histogram"),
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
            histograms: vec![metriken_exposition::Histogram::new(
                "7".to_string(),
                h,
                metadata,
            )],
        });
        let staged = source.stage(&snap, ts, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
    }
    source.finalize((BASE + 2 * S, 0)).unwrap();
    writer.join().unwrap();

    let reader = open(&path);
    assert_eq!(reader.gauge_histogram_names(), ["free_extents"]);
    assert!(reader.histogram_names().is_empty());
    let rows = gauge_rows(
        table_source(&reader, "free_extents").as_ref(),
        "free_extents",
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].1.count, cumulative(&state(2)).count);
    assert_eq!(formats(&path, "ext4"), [Some("2".to_string())]);
}
