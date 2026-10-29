//! A gauge histogram over the replication stream: `GroupBuilder::build_stream`
//! → `EncodedStreamGroup::encode` → `FrameProducer::interval` →
//! `StreamDecoder::decode` → `SourceRecorder::stage_streamed`, read back
//! through `ArchiveReader`. The path `metriken-recorder`, `rezolus record` to
//! `.dendro`, hindsight and the live viewer take. The group holding the
//! gauge histogram also holds a gauge, and a control group holds a gauge
//! alone: all three are recorded.
//!
//! Its own test binary, so the registry holds only these metrics.
#![cfg(all(feature = "stream", feature = "write"))]

use std::collections::BTreeMap;

use dendro::replicate::Frame;
use metriken::{metric, Gauge, GaugeHistogram};
use metriken_exposition::group_builder::{DefaultRouter, GroupBuilder};
use metriken_query::{MetricsSource, QueryResult};
use metriken_storage::stream::{EncodedStreamGroup, FrameProducer, SchemaCache};
use metriken_storage::{
    ArchiveReader, ArchiveWriter, BufferPool, DendroCatalog, Labels, StreamDecoder, StreamedGroup,
    WriterConfig,
};

#[metric(name = "st_free", metadata = { acq_group = "ext4" })]
static FREE: GaugeHistogram = GaugeHistogram::new(0, 8);

#[metric(name = "st_mem", metadata = { acq_group = "ext4" })]
static MEM: Gauge = Gauge::new();

#[metric(name = "st_plain", metadata = { acq_group = "plain" })]
static PLAIN: Gauge = Gauge::new();

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;

fn state(t: u64) -> Vec<u64> {
    let mut b = vec![0u64; 9];
    b[1] = 100 - t;
    b[3] = 10 + t;
    b
}

#[test]
fn a_gauge_histogram_is_recorded_off_the_stream() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stream.dendro");
    let mut writer = ArchiveWriter::create(&path, WriterConfig::default()).unwrap();
    let mut source = writer
        .add_source(
            [("source".to_string(), "stream".to_string())].into(),
            BTreeMap::new(),
            BASE,
        )
        .unwrap();
    let mut builder = GroupBuilder::new(DefaultRouter::new("t"));
    let mut schemas = SchemaCache::new();
    let mut producer = FrameProducer::for_source(
        "00000000-0000-4000-8000-000000000000".to_string(),
        0,
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let mut decoder = StreamDecoder::new();
    for t in 0..4u64 {
        let ts = BASE + t * S;
        FREE.set(&state(t)).unwrap();
        MEM.set(t as i64);
        PLAIN.set(10 * t as i64);
        let groups = builder.build_stream(Vec::new());
        let encoded: Vec<EncodedStreamGroup> = groups
            .iter()
            .map(|g| EncodedStreamGroup::encode(g, &mut schemas).unwrap())
            .collect();
        let Frame::Rows { rows, .. } = producer.interval(&encoded, ts as i64, 0, t + 1) else {
            panic!("a rows frame")
        };
        let decoded = decoder.decode(rows).unwrap();
        let ext4 = decoded
            .iter()
            .find_map(|g| match g {
                StreamedGroup::Wide(g) if g.name == "t/ext4" => Some(g),
                _ => None,
            })
            .expect("the ext4 group decoded wide");
        assert_eq!(ext4.gauge_histograms.len(), 1, "tick {t}");
        let staged = source.stage_streamed(decoded, ts, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
    }
    source.finalize((BASE + 3 * S, 0)).unwrap();
    writer.join().unwrap();

    let mut recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(&path).unwrap()),
        None,
        BufferPool::new(16 << 20),
        None,
    )
    .unwrap();
    let reader = recordings.remove(0).1;
    assert_eq!(ArchiveReader::gauge_histogram_names(&reader), ["st_free"]);
    let mut gauges = ArchiveReader::gauge_names(&reader);
    gauges.sort();
    assert_eq!(gauges, ["st_mem", "st_plain"]);

    let owners = reader.owners_of(&["st_free".to_string()]);
    let rows: Vec<Vec<u64>> = owners[0]
        .reader()
        .unwrap()
        .source()
        .gauge_histogram_stream("st_free", &Labels::default(), 0, u64::MAX)
        .unwrap()
        .rows
        .map(|r| r.snapshot.count)
        .collect();
    // Cumulative over buckets 1 and 3: 100 − t, then 110.
    let want: Vec<Vec<u64>> = (0..4).map(|t| vec![100 - t, 110]).collect();
    assert_eq!(rows, want);

    let start = BASE as f64 / 1e9;
    for (q, want) in [
        ("histogram_count(st_free)", [110.0; 4]),
        ("st_mem", [0.0, 1.0, 2.0, 3.0]),
        ("st_plain", [0.0, 10.0, 20.0, 30.0]),
    ] {
        let QueryResult::Matrix { result, .. } =
            reader.query_range(q, start, start + 3.0, 1.0).unwrap()
        else {
            panic!("{q}: a matrix")
        };
        let got: Vec<f64> = result[0].values.iter().map(|p| p.1).collect();
        assert_eq!(got, want, "{q}");
    }
}

/// `depth`'s points in the archive at `path`, one recording.
fn depth_points(path: &std::path::Path, ticks: u64) -> Vec<f64> {
    let mut recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(path).unwrap()),
        None,
        BufferPool::new(16 << 20),
        None,
    )
    .unwrap();
    let reader = recordings.remove(0).1;
    assert!(ArchiveReader::gauge_histogram_names(&reader).is_empty());
    let start = BASE as f64 / 1e9;
    let QueryResult::Matrix { result, .. } = reader
        .query_range("depth", start, start + (ticks - 1) as f64, 1.0)
        .unwrap()
    else {
        panic!("a matrix")
    };
    result[0].values.iter().map(|p| p.1).collect()
}

/// A long row carrying gauge histograms (which `GroupBuilder` never sends:
/// a group holding one goes wide) is written without their values, with a
/// warning once per group, rather than ending the recording: a long table
/// has no gauge histogram columns. The row's other values are written.
#[test]
fn a_long_row_with_gauge_histograms_drops_them() {
    use metriken_storage::occupants::Occupant;
    use metriken_storage::wal::{LongOccupant, WalLongRow};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.dendro");
    let mut writer = ArchiveWriter::create(&path, WriterConfig::default()).unwrap();
    let mut source = writer
        .add_source(
            [("source".to_string(), "stream".to_string())].into(),
            BTreeMap::new(),
            BASE,
        )
        .unwrap();
    // Tick 0: an occupant with a gauge histogram value. Tick 1: columns that
    // name a gauge histogram, no value. Tick 2: neither.
    let row = |t: u64| {
        let mut schema = metriken_exposition::GroupSchema {
            gauges: vec![metriken_exposition::MetricDesc {
                name: "0".to_string(),
                metadata: [("metric".to_string(), "depth".to_string())].into(),
            }],
            ..Default::default()
        };
        if t == 1 {
            schema.gauge_histograms = vec![metriken_exposition::MetricDesc {
                name: "1".to_string(),
                metadata: [("metric".to_string(), "free".to_string())].into(),
            }];
        }
        WalLongRow {
            schema_hash: (1, t),
            schema: Some(schema),
            window: None,
            occupants: vec![LongOccupant {
                occupant: 7,
                counters: Vec::new(),
                gauges: vec![Some(t as i64 + 1)],
                histograms: Vec::new(),
                gauge_histograms: if t == 0 {
                    vec![Some((0, 8, vec![0; 9]))]
                } else {
                    Vec::new()
                },
            }],
        }
    };
    for t in 0..3u64 {
        let mut groups = Vec::new();
        if t == 0 {
            groups.push(StreamedGroup::Occupants {
                table: "t/queues".to_string(),
                occupants: vec![Occupant {
                    occupant: 7,
                    labels: [("id".to_string(), "0".to_string())].into(),
                }],
            });
        }
        groups.push(StreamedGroup::Long {
            name: "t/queues".to_string(),
            row: row(t),
        });
        let staged = source.stage_streamed(groups, BASE + t * S, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
    }
    source.finalize((BASE + 2 * S, 0)).unwrap();
    writer.join().unwrap();
    assert_eq!(depth_points(&path, 3), [1.0, 2.0, 3.0]);
}

/// A group the writer made long (its first schema was slotted and held no
/// gauge histogram) that later carries one is written long without the
/// gauge histogram's values, warned once, rather than ending the recording.
#[test]
fn a_long_written_group_that_gains_a_gauge_histogram_drops_it() {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use metriken_exposition::{GroupSchema, GroupSnapshot, MetricDesc, Snapshot, SnapshotV3};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.dendro");
    let config = WriterConfig {
        long_groups: true,
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(&path, config).unwrap();
    let mut source = writer
        .add_source(
            [("source".to_string(), "t".to_string())].into(),
            BTreeMap::new(),
            BASE,
        )
        .unwrap();
    let snapshot = |t: u64, gauge: bool| {
        let schema = GroupSchema {
            gauges: vec![MetricDesc {
                name: "0x0".to_string(),
                metadata: [
                    ("metric".to_string(), "depth".to_string()),
                    ("id".to_string(), "0".to_string()),
                ]
                .into(),
            }],
            gauge_histograms: if gauge {
                vec![MetricDesc {
                    name: "1".to_string(),
                    metadata: [("metric".to_string(), "free".to_string())].into(),
                }]
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        Snapshot::V3(SnapshotV3 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(BASE + t * S),
            duration: Duration::from_millis(1),
            metadata: Default::default(),
            groups: vec![GroupSnapshot {
                name: "t/queues".to_string(),
                schema_hash: schema.hash(),
                schema: Some(Arc::new(schema)),
                window: None,
                counters: Vec::new(),
                gauges: vec![Some(t as i64 + 1)],
                histograms: Vec::new(),
                gauge_histograms: if gauge {
                    vec![Some(histogram::Histogram::new(0, 8).unwrap())]
                } else {
                    Vec::new()
                },
            }],
        })
    };
    for t in 0..3u64 {
        let staged = source.stage(&snapshot(t, t > 0), BASE + t * S, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
    }
    source.finalize((BASE + 2 * S, 0)).unwrap();
    writer.join().unwrap();
    assert_eq!(depth_points(&path, 3), [1.0, 2.0, 3.0]);
}
