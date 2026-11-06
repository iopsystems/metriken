//! Float counters and gauges through the archive: groups holding
//! `float_counters` and `float_gauges`, written by `ArchiveWriter` from
//! snapshots and from a replication stream's rows, sealed or left in the
//! live WAL tail, and read back by `ArchiveReader`.
//!
//! What is checked, from `docs/journal/2026-11-06-float-scalars.md`: a float
//! column is listed and read as a counter or a gauge (decision 1); a group
//! whose members carry an `id` stays long with float members (decision 2);
//! values read back bit for bit, fractions included, and integer columns
//! beside them stay exact above 2^53; a segment holding a float column is
//! format 3, one without stays 1; a producer's `metric_type` label does not
//! change a float column's kind; two recordings of one name, one integer
//! and one float, read as two series.
#![cfg(feature = "write")]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use dendro::archive::WalRow;
use dendro::seal::SealPolicy;
use metriken_exposition::{GroupSnapshot, Snapshot, SnapshotV3};
use metriken_storage::format::FORMAT_KEY;
use metriken_storage::occupants::{self, Occupant};
use metriken_storage::schema::{GroupSchema, MetricDesc};
use metriken_storage::wal::{self, LongOccupant, WalGroupRow, WalLongRow};
use metriken_storage::{
    ArchiveReader, ArchiveWriter, BufferPool, Catalog, CounterValue, CounterValues, DataSource,
    DendroCatalog, GaugeValues, Labels, SourceRecorder, StreamDecoder, WriterConfig,
};

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;
const TICKS: u64 = 12;

/// A wide group: an integer counter and gauge beside float ones.
const NODE: &str = "prom/node";
/// A group whose members carry an `id`: written long.
const CADVISOR: &str = "prom/cadvisor";
/// A group of integers only.
const INTS: &str = "prom/ints";
/// A group of float members only, each with an `id`: written long, as a
/// Prometheus endpoint's per-container group is once every scalar is a
/// float.
const CGROUPS: &str = "prom/cgroups";

/// Above 2^53, where an `f64` cannot hold every integer.
const BIG: u64 = (1 << 60) + 1;
const DEEP: i64 = -(1 << 55) - 1;

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn desc(name: &str, pairs: &[(&str, &str)]) -> MetricDesc {
    MetricDesc {
        name: name.to_string(),
        metadata: labels(pairs),
    }
}

// ─── The values written at tick `t` ─────────────────────────────────────────

fn ops_total(t: u64) -> u64 {
    BIG + t
}

fn depth(t: u64) -> i64 {
    DEEP - t as i64
}

/// Not a dyadic fraction, so a value that passed through anything but an
/// exact `f64` path would differ in its last bits.
fn cpu_seconds(t: u64) -> f64 {
    0.1 * t as f64 + 0.125
}

/// Absent at tick 3.
fn cpu_limit(t: u64) -> Option<f64> {
    (t != 3).then_some(0.25 - 0.001 * t as f64)
}

fn container_cpu(id: usize, t: u64) -> f64 {
    (id as f64 + 1.0) * 1.5 + 0.3 * t as f64
}

fn container_quota(id: usize, t: u64) -> f64 {
    -0.5 * (id as f64 + 1.0) + 1e-9 * t as f64
}

fn restarts(id: usize, t: u64) -> u64 {
    BIG + (id as u64) * 1000 + t
}

const IDS: [&str; 2] = ["a", "b"];

// ─── Schemas ────────────────────────────────────────────────────────────────

/// The wide group. Its float counter's descriptor carries a producer's
/// `metric_type = gauge`, which must not make it a gauge.
fn node_schema() -> GroupSchema {
    GroupSchema {
        counters: vec![desc("0", &[("metric", "ops_total")])],
        gauges: vec![desc("1", &[("metric", "depth")])],
        float_counters: vec![desc(
            "2",
            &[("metric", "cpu_seconds_total"), ("metric_type", "gauge")],
        )],
        float_gauges: vec![desc("3", &[("metric", "cpu_limit")])],
        ..GroupSchema::default()
    }
}

/// The slotted group, as a snapshot carries it: one member per metric and
/// occupant, each with the occupant's `id`.
fn cadvisor_schema() -> GroupSchema {
    let per = |metric: &str, extra: &[(&str, &str)]| -> Vec<MetricDesc> {
        IDS.iter()
            .enumerate()
            .map(|(i, id)| {
                let mut pairs = vec![("metric", metric), ("id", *id)];
                pairs.extend_from_slice(extra);
                desc(&format!("{metric}x{i}"), &pairs)
            })
            .collect()
    };
    GroupSchema {
        counters: per("container_restarts", &[]),
        float_counters: per(
            "container_cpu_usage_seconds_total",
            &[("metric_type", "gauge")],
        ),
        float_gauges: per("container_spec_cpu_quota", &[]),
        ..GroupSchema::default()
    }
}

fn cgroups_schema() -> GroupSchema {
    GroupSchema {
        float_gauges: IDS
            .iter()
            .enumerate()
            .map(|(i, id)| desc(&format!("m{i}"), &[("metric", "memory_ratio"), ("id", id)]))
            .collect(),
        ..GroupSchema::default()
    }
}

fn memory_ratio(id: usize, t: u64) -> f64 {
    0.01 * (id as f64 + 1.0) * t as f64
}

fn ints_schema() -> GroupSchema {
    GroupSchema {
        counters: vec![desc("0", &[("metric", "plain_total")])],
        ..GroupSchema::default()
    }
}

fn window(t: u64) -> metriken::Window {
    let ts = BASE + t * S;
    metriken::Window::new(ts - S / 2, ts)
}

fn group(name: &str, schema: &Arc<GroupSchema>, t: u64) -> GroupSnapshot {
    GroupSnapshot {
        name: name.to_string(),
        schema_hash: schema.hash(),
        schema: Some(Arc::clone(schema)),
        window: Some(window(t)),
        counters: Vec::new(),
        gauges: Vec::new(),
        histograms: Vec::new(),
        gauge_histograms: Vec::new(),
        float_counters: Vec::new(),
        float_gauges: Vec::new(),
    }
}

/// Tick `t`'s snapshot of the three groups.
fn snapshot(t: u64) -> Snapshot {
    let (node, cadvisor, ints) = (
        Arc::new(node_schema()),
        Arc::new(cadvisor_schema()),
        Arc::new(ints_schema()),
    );
    let mut n = group(NODE, &node, t);
    n.counters = vec![Some(ops_total(t))];
    n.gauges = vec![Some(depth(t))];
    n.float_counters = vec![Some(cpu_seconds(t))];
    n.float_gauges = vec![cpu_limit(t)];
    let mut c = group(CADVISOR, &cadvisor, t);
    c.counters = (0..IDS.len()).map(|i| Some(restarts(i, t))).collect();
    c.float_counters = (0..IDS.len()).map(|i| Some(container_cpu(i, t))).collect();
    c.float_gauges = (0..IDS.len())
        .map(|i| Some(container_quota(i, t)))
        .collect();
    let mut i = group(INTS, &ints, t);
    i.counters = vec![Some(t)];
    let mut g = group(CGROUPS, &Arc::new(cgroups_schema()), t);
    g.float_gauges = (0..IDS.len()).map(|i| Some(memory_ratio(i, t))).collect();
    Snapshot::V3(SnapshotV3 {
        systemtime: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(BASE + t * S),
        duration: std::time::Duration::from_millis(1),
        metadata: Default::default(),
        groups: vec![n, c, i, g],
    })
}

// ─── Writing ────────────────────────────────────────────────────────────────

fn config() -> WriterConfig {
    WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    }
}

/// Record `ticks` with one source, sealing every 5 rows; `finalize` false
/// leaves the last rows in the live WAL tail.
fn record(
    path: &Path,
    finalize: bool,
    mut tick: impl FnMut(&mut SourceRecorder, u64) -> metriken_storage::writer::Staged,
) {
    let mut writer = ArchiveWriter::create(path, config()).unwrap();
    let mut source = writer
        .add_source(labels(&[("source", "prom")]), BTreeMap::new(), BASE)
        .unwrap();
    for t in 0..TICKS {
        let staged = tick(&mut source, t);
        writer.commit(vec![staged]).unwrap();
        source.maybe_seal().unwrap();
        source.sync().unwrap();
    }
    if finalize {
        source.finalize((BASE + (TICKS - 1) * S, 0)).unwrap();
    } else {
        std::mem::forget(source);
    }
    writer.join().unwrap();
}

/// Through `SourceRecorder::stage`: the groups as snapshots.
fn record_snapshots(path: &Path, finalize: bool) {
    record(path, finalize, |source, t| {
        source.stage(&snapshot(t), BASE + t * S, 0).unwrap()
    });
}

/// A wide row as a producer's stream sends it: the schema on the first.
fn wide_row(name: &str, g: &GroupSnapshot, first: bool) -> WalRow {
    let row = WalGroupRow {
        schema_hash: g.schema_hash,
        schema: first.then(|| g.schema.as_deref().unwrap().clone()),
        window: g.window.map(|w| (w.begin_ns, w.end_ns)),
        counters: g.counters.clone(),
        gauges: g.gauges.clone(),
        histograms: Vec::new(),
        gauge_histograms: Vec::new(),
        float_counters: g.float_counters.clone(),
        float_gauges: g.float_gauges.clone(),
    };
    WalRow {
        stream: name.to_string(),
        ts: g.window.unwrap().end_ns as i64,
        wall_offset: 0,
        row: wal::encode_wal_group_row(&row).unwrap(),
    }
}

/// The slotted group's columns as a producer's long rows carry them: one
/// per metric, the occupant's labels sent apart.
fn cadvisor_columns() -> GroupSchema {
    GroupSchema {
        counters: vec![desc("0", &[("metric", "container_restarts")])],
        float_counters: vec![desc(
            "1",
            &[
                ("metric", "container_cpu_usage_seconds_total"),
                ("metric_type", "gauge"),
            ],
        )],
        float_gauges: vec![desc("2", &[("metric", "container_spec_cpu_quota")])],
        ..GroupSchema::default()
    }
}

/// Through `StreamDecoder` and `SourceRecorder::stage_streamed`: the wide
/// groups as encoded group rows, the slotted one as long rows after its
/// occupants, all decoded off the wire.
fn record_stream(path: &Path, finalize: bool) {
    let mut decoder = StreamDecoder::new();
    let columns = cadvisor_columns();
    record(path, finalize, |source, t| {
        let Snapshot::V3(snap) = snapshot(t) else {
            unreachable!()
        };
        let ts = BASE + t * S;
        let mut rows = vec![
            wide_row(NODE, &snap.groups[0], t == 0),
            wide_row(INTS, &snap.groups[2], t == 0),
        ];
        if t == 0 {
            let described: Vec<Occupant> = IDS
                .iter()
                .enumerate()
                .map(|(key, id)| Occupant {
                    occupant: key as u64,
                    labels: labels(&[("id", id)]),
                })
                .collect();
            rows.push(WalRow {
                stream: occupants::stream_of(CADVISOR),
                ts: ts as i64,
                wall_offset: 0,
                row: occupants::encode_wal_row(&described),
            });
        }
        let long = WalLongRow {
            schema_hash: columns.hash(),
            schema: (t == 0).then(|| columns.clone()),
            window: Some((ts - S / 2, ts)),
            occupants: (0..IDS.len())
                .map(|i| LongOccupant {
                    occupant: i as u64,
                    counters: vec![Some(restarts(i, t))],
                    gauges: Vec::new(),
                    histograms: Vec::new(),
                    gauge_histograms: Vec::new(),
                    float_counters: vec![Some(container_cpu(i, t))],
                    float_gauges: vec![Some(container_quota(i, t))],
                })
                .collect(),
        };
        rows.push(WalRow {
            stream: CADVISOR.to_string(),
            ts: ts as i64,
            wall_offset: 0,
            row: wal::encode_wal_long_row(&long).unwrap(),
        });
        let groups = decoder.decode(rows).unwrap();
        assert_eq!(decoder.unresolved, 0);
        source.stage_streamed(groups, ts, 0).unwrap()
    });
}

// ─── Reading ────────────────────────────────────────────────────────────────

fn open_all(path: &Path) -> Vec<(BTreeMap<String, String>, ArchiveReader)> {
    ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(path).unwrap()),
        None,
        BufferPool::new(64 << 20),
        None,
    )
    .unwrap()
}

fn open(path: &Path) -> ArchiveReader {
    open_all(path).remove(0).1
}

fn table_source(reader: &ArchiveReader, name: &str) -> Arc<dyn DataSource> {
    let owners = reader.owners_of(&[name.to_string()]);
    assert_eq!(owners.len(), 1, "one table holds {name}");
    owners[0].reader().expect("the table opens").source()
}

fn times() -> Vec<u64> {
    (0..TICKS).map(|t| BASE + t * S).collect()
}

/// The one series of counter `name` (of occupant `id`, when given).
fn counter(source: &dyn DataSource, name: &str, id: Option<&str>) -> (Vec<u64>, CounterValues) {
    let filter = Labels::from(labels(&id.map(|id| vec![("id", id)]).unwrap_or_default()));
    let mut found = source
        .counters(name, &filter, 0, u64::MAX)
        .unwrap_or_else(|| panic!("counter {name} {id:?}"))
        .series;
    assert_eq!(found.len(), 1, "{name} {id:?}");
    let c = found.remove(0);
    (c.timestamps, c.values)
}

fn gauge(source: &dyn DataSource, name: &str, id: Option<&str>) -> (Vec<u64>, GaugeValues) {
    let filter = Labels::from(labels(&id.map(|id| vec![("id", id)]).unwrap_or_default()));
    let mut found = source
        .gauges(name, &filter, 0, u64::MAX)
        .unwrap_or_else(|| panic!("gauge {name} {id:?}"))
        .series;
    assert_eq!(found.len(), 1, "{name} {id:?}");
    let g = found.remove(0);
    (g.timestamps, g.values)
}

/// Floats compared by their bits.
fn bits(v: &[f64]) -> Vec<u64> {
    v.iter().map(|x| x.to_bits()).collect()
}

fn float_counter(source: &dyn DataSource, name: &str, id: Option<&str>) -> (Vec<u64>, Vec<u64>) {
    let (ts, values) = counter(source, name, id);
    let CounterValues::Float(v) = values else {
        panic!("{name}: an integer series: {values:?}")
    };
    (ts, bits(&v))
}

fn float_gauge(source: &dyn DataSource, name: &str, id: Option<&str>) -> (Vec<u64>, Vec<u64>) {
    let (ts, values) = gauge(source, name, id);
    let GaugeValues::Float(v) = values else {
        panic!("{name}: an integer series: {values:?}")
    };
    (ts, bits(&v))
}

fn check_listings(reader: &ArchiveReader) {
    let counters = reader.counter_names();
    let gauges = reader.gauge_names();
    for name in [
        "ops_total",
        "cpu_seconds_total",
        "container_restarts",
        "container_cpu_usage_seconds_total",
        "plain_total",
    ] {
        assert!(counters.contains(&name.to_string()), "{name}: {counters:?}");
        assert!(!gauges.contains(&name.to_string()), "{name}: {gauges:?}");
    }
    for name in ["depth", "cpu_limit", "container_spec_cpu_quota"] {
        assert!(gauges.contains(&name.to_string()), "{name}: {gauges:?}");
        assert!(
            !counters.contains(&name.to_string()),
            "{name}: {counters:?}"
        );
    }
    for (name, counter) in [
        ("cpu_seconds_total", true),
        ("cpu_limit", false),
        ("container_cpu_usage_seconds_total", true),
        ("container_spec_cpu_quota", false),
    ] {
        let owners = reader.owners_of(&[name.to_string()]);
        assert_eq!(owners.len(), 1, "{name}");
        assert_eq!(owners[0].holds_counter(name), counter, "{name}");
        assert_eq!(owners[0].holds_gauge(name), !counter, "{name}");
    }
    let ids = |sets: Vec<BTreeMap<String, String>>| -> Vec<String> {
        let mut ids: Vec<String> = sets.iter().filter_map(|l| l.get("id").cloned()).collect();
        ids.sort();
        ids
    };
    assert_eq!(
        ids(reader.counter_labels("container_cpu_usage_seconds_total")),
        IDS
    );
    assert_eq!(ids(reader.gauge_labels("container_spec_cpu_quota")), IDS);
    assert_eq!(reader.counter_labels("cpu_seconds_total").len(), 1);
    assert_eq!(reader.gauge_labels("cpu_limit").len(), 1);
}

fn check_values(reader: &ArchiveReader) {
    let node = table_source(reader, "cpu_seconds_total");
    assert!(node
        .counter_names()
        .contains(&"cpu_seconds_total".to_string()));
    assert!(node.gauge_names().contains(&"cpu_limit".to_string()));

    let (ts, values) = counter(node.as_ref(), "ops_total", None);
    assert_eq!(ts, times());
    assert_eq!(
        values,
        CounterValues::Int((0..TICKS).map(ops_total).collect())
    );
    let (ts, values) = gauge(node.as_ref(), "depth", None);
    assert_eq!(ts, times());
    assert_eq!(values, GaugeValues::Int((0..TICKS).map(depth).collect()));

    let want: Vec<f64> = (0..TICKS).map(cpu_seconds).collect();
    assert_eq!(
        float_counter(node.as_ref(), "cpu_seconds_total", None),
        (times(), bits(&want))
    );
    let present: Vec<u64> = (0..TICKS).filter(|t| cpu_limit(*t).is_some()).collect();
    let want: Vec<f64> = present.iter().filter_map(|t| cpu_limit(*t)).collect();
    assert_eq!(
        float_gauge(node.as_ref(), "cpu_limit", None),
        (present.iter().map(|t| BASE + t * S).collect(), bits(&want))
    );

    // The stream read of a float counter says so before its first sample,
    // and carries the values; an integer one is not marked.
    let streams = node
        .counter_streams("cpu_seconds_total", &Labels::default(), 0, u64::MAX)
        .unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].float);
    let streams = node
        .counter_streams("cpu_seconds_total", &Labels::default(), 0, u64::MAX)
        .unwrap();
    let samples: Vec<CounterValue> = streams
        .into_iter()
        .next()
        .unwrap()
        .samples
        .map(|s| s.value)
        .collect();
    assert_eq!(
        samples,
        (0..TICKS)
            .map(|t| CounterValue::Float(cpu_seconds(t)))
            .collect::<Vec<_>>()
    );
    let streams = node
        .counter_streams("ops_total", &Labels::default(), 0, u64::MAX)
        .unwrap();
    assert!(!streams[0].float);
    let samples: Vec<CounterValue> = streams
        .into_iter()
        .next()
        .unwrap()
        .samples
        .map(|s| s.value)
        .collect();
    assert_eq!(
        samples,
        (0..TICKS)
            .map(|t| CounterValue::Int(ops_total(t)))
            .collect::<Vec<_>>()
    );

    let cadvisor = table_source(reader, "container_cpu_usage_seconds_total");
    for (i, id) in IDS.iter().enumerate() {
        let want: Vec<f64> = (0..TICKS).map(|t| container_cpu(i, t)).collect();
        assert_eq!(
            float_counter(
                cadvisor.as_ref(),
                "container_cpu_usage_seconds_total",
                Some(id)
            ),
            (times(), bits(&want)),
            "{id}"
        );
        let want: Vec<f64> = (0..TICKS).map(|t| container_quota(i, t)).collect();
        assert_eq!(
            float_gauge(cadvisor.as_ref(), "container_spec_cpu_quota", Some(id)),
            (times(), bits(&want)),
            "{id}"
        );
        let (ts, values) = counter(cadvisor.as_ref(), "container_restarts", Some(id));
        assert_eq!(ts, times());
        assert_eq!(
            values,
            CounterValues::Int((0..TICKS).map(|t| restarts(i, t)).collect()),
            "{id}"
        );
    }
}

/// Each sealed segment of `table`: its `metriken.format` and whether it is
/// a long segment.
fn segments(path: &Path, table: &str) -> Vec<(Option<String>, bool)> {
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
            let kv = reader
                .metadata()
                .file_metadata()
                .key_value_metadata()
                .cloned()
                .unwrap_or_default();
            let get = |key: &str| {
                kv.iter()
                    .find(|e| e.key == key)
                    .and_then(|e| e.value.clone())
            };
            (
                get(FORMAT_KEY),
                get(metriken_storage::long::LAYOUT_KEY).as_deref()
                    == Some(metriken_storage::long::LAYOUT_LONG),
            )
        })
        .collect()
}

fn check_segments(path: &Path) {
    for (table, format, long) in [
        (NODE, "3", false),
        (CADVISOR, "3", true),
        (INTS, "1", false),
    ] {
        let found = segments(path, table);
        assert!(!found.is_empty(), "{table} has sealed segments");
        assert!(
            found
                .iter()
                .all(|(f, l)| f.as_deref() == Some(format) && *l == long),
            "{table}: every segment is format {format}, long {long}: {found:?}"
        );
    }
}

fn check(path: &Path) {
    let reader = open(path);
    check_listings(&reader);
    check_values(&reader);
}

/// The float-only slotted group, written from snapshots only.
fn check_cgroups(path: &Path) {
    let reader = open(path);
    let source = table_source(&reader, "memory_ratio");
    for (i, id) in IDS.iter().enumerate() {
        let want: Vec<f64> = (0..TICKS).map(|t| memory_ratio(i, t)).collect();
        assert_eq!(
            float_gauge(source.as_ref(), "memory_ratio", Some(id)),
            (times(), bits(&want)),
            "{id}"
        );
    }
}

#[test]
fn float_scalars_round_trip_sealed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sealed.dendro");
    record_snapshots(&path, true);
    check(&path);
    check_cgroups(&path);
    check_segments(&path);
    let found = segments(&path, CGROUPS);
    assert!(
        !found.is_empty()
            && found
                .iter()
                .all(|(f, long)| f.as_deref() == Some("3") && *long),
        "a float-only slotted group is long: {found:?}"
    );
}

/// 12 ticks sealed every 5 rows leave 2 in the WAL tail, which the reader
/// materializes.
#[test]
fn float_scalars_round_trip_from_the_live_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.dendro");
    record_snapshots(&path, false);
    check(&path);
    check_cgroups(&path);
}

#[test]
fn float_scalars_round_trip_off_the_stream() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stream.dendro");
    record_stream(&path, true);
    check(&path);
    check_segments(&path);

    let live = dir.path().join("stream-live.dendro");
    record_stream(&live, false);
    check(&live);
}

/// A query reads integer counters as before and refuses a float counter or
/// gauge with an error naming it, rather than answering without it.
#[test]
fn a_query_refuses_a_float_series_by_name() {
    use metriken_query::{MetricsSource, QueryResult};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("query.dendro");
    record_snapshots(&path, true);
    let reader = open(&path);
    let (lo, hi) = reader.time_range().unwrap();
    let QueryResult::Matrix { result, .. } = reader
        .query_range("irate(ops_total[2s])", lo, hi, 1.0)
        .unwrap()
    else {
        panic!("not a matrix")
    };
    assert_eq!(result.len(), 1);
    assert!(result[0].values.iter().all(|(_, v)| *v == 1.0));
    for q in [
        "irate(cpu_seconds_total[2s])",
        "rate(container_cpu_usage_seconds_total[3s])",
        "cpu_limit",
        "avg_over_time(container_spec_cpu_quota[3s])",
    ] {
        let err = reader
            .query_range(q, lo, hi, 1.0)
            .err()
            .unwrap_or_else(|| panic!("{q}: answered"))
            .to_string();
        assert!(err.contains("float values"), "{q}: {err}");
    }
}

/// One counter name recorded by two sources, one as an integer column and
/// one as a float column (an endpoint before and after its recorder
/// stored floats), reads as two series, each with its own values.
#[test]
fn an_integer_and_a_float_recording_of_one_name_read_together() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.dendro");
    let mut writer = ArchiveWriter::create(&path, config()).unwrap();
    let mut int_source = writer
        .add_source(labels(&[("source", "before")]), BTreeMap::new(), BASE)
        .unwrap();
    let mut float_source = writer
        .add_source(labels(&[("source", "after")]), BTreeMap::new(), BASE)
        .unwrap();
    let int_schema = Arc::new(GroupSchema {
        counters: vec![desc("0", &[("metric", "requests_total")])],
        ..GroupSchema::default()
    });
    let float_schema = Arc::new(GroupSchema {
        float_counters: vec![desc("0", &[("metric", "requests_total")])],
        ..GroupSchema::default()
    });
    for t in 0..TICKS {
        let ts = BASE + t * S;
        let snap = |g: GroupSnapshot| {
            Snapshot::V3(SnapshotV3 {
                systemtime: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ts),
                duration: std::time::Duration::from_millis(1),
                metadata: Default::default(),
                groups: vec![g],
            })
        };
        let mut i = group(NODE, &int_schema, t);
        i.counters = vec![Some(BIG + t)];
        let mut f = group(NODE, &float_schema, t);
        f.float_counters = vec![Some(cpu_seconds(t))];
        let a = int_source.stage(&snap(i), ts, 0).unwrap();
        let b = float_source.stage(&snap(f), ts, 0).unwrap();
        writer.commit(vec![a, b]).unwrap();
    }
    int_source.finalize((BASE + (TICKS - 1) * S, 0)).unwrap();
    float_source.finalize((BASE + (TICKS - 1) * S, 0)).unwrap();
    writer.join().unwrap();

    let readers: Vec<ArchiveReader> = open_all(&path).into_iter().map(|(_, r)| r).collect();
    assert_eq!(readers.len(), 2);
    let reader = ArchiveReader::flatten(readers, None);
    assert_eq!(reader.counter_names(), ["requests_total"]);
    assert!(reader.gauge_names().is_empty());
    let mut sources: Vec<String> = reader
        .counter_labels("requests_total")
        .iter()
        .filter_map(|l| l.get("source").cloned())
        .collect();
    sources.sort();
    assert_eq!(sources, ["after", "before"]);

    let owners = reader.owners_of(&["requests_total".to_string()]);
    assert_eq!(owners.len(), 2);
    let union = metriken_storage::recordings::RecordingsSource::new(
        owners
            .iter()
            .map(|t| t.reader().expect("the table opens").source())
            .collect(),
    );
    let mut read: Vec<CounterValues> = union
        .counters("requests_total", &Labels::default(), 0, u64::MAX)
        .unwrap()
        .series
        .into_iter()
        .map(|c| c.values)
        .collect();
    read.sort_by_key(|v| v.is_float());
    assert_eq!(
        read,
        [
            CounterValues::Int((0..TICKS).map(|t| BIG + t).collect()),
            CounterValues::Float((0..TICKS).map(cpu_seconds).collect()),
        ]
    );
}

/// A `Float64` column is a counter or a gauge by its `metric_type`; one of
/// another `metric_type`, or none, is not read, by the column readers or
/// by the eager table decoder.
#[test]
fn a_float_column_of_another_metric_type_is_not_read() {
    use arrow::array::{ArrayRef, Float64Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;

    let field = |name: &str, kind: Option<&str>| {
        let mut meta = std::collections::HashMap::from([("metric".to_string(), name.to_string())]);
        if let Some(kind) = kind {
            meta.insert("metric_type".to_string(), kind.to_string());
        }
        Field::new(name, DataType::Float64, true).with_metadata(meta)
    };
    let schema = Arc::new(Schema::new(vec![
        Field::new("timestamp", DataType::UInt64, false),
        field("fc", Some("counter")),
        field("fg", Some("gauge")),
        field("fs", Some("summary")),
        field("fn", None),
    ]));
    let col = || Arc::new(Float64Array::from(vec![Some(0.5), None])) as ArrayRef;
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(UInt64Array::from(vec![BASE, BASE + S])) as ArrayRef,
            col(),
            col(),
            col(),
            col(),
        ],
    )
    .unwrap();
    let mut bytes = Vec::new();
    let props = metriken_storage::format::stamped_for(
        parquet::file::properties::WriterProperties::builder().build(),
        &schema,
    );
    let mut w = parquet::arrow::ArrowWriter::try_new(&mut bytes, schema, Some(props)).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();

    let source = metriken_storage::parquet::MultiParquetSource::open_bytes_with_pool(
        bytes.clone(),
        BufferPool::new(1 << 20),
    )
    .unwrap();
    assert_eq!(source.counter_names(), ["fc"]);
    assert_eq!(source.gauge_names(), ["fg"]);
    let (_, values) = counter(&source, "fc", None);
    assert_eq!(values, CounterValues::Float(vec![0.5]));
    let (_, values) = gauge(&source, "fg", None);
    assert_eq!(values, GaugeValues::Float(vec![0.5]));

    let table = metriken_storage::table::read_table_parquet("t".to_string(), bytes).unwrap();
    let kinds: Vec<(&str, &metriken_storage::table::Values)> = table
        .columns
        .iter()
        .map(|c| (c.name.as_str(), &c.values))
        .collect();
    assert_eq!(
        kinds,
        [
            (
                "fc",
                &metriken_storage::table::Values::FloatCounter(vec![Some(0.5), None])
            ),
            (
                "fg",
                &metriken_storage::table::Values::FloatGauge(vec![Some(0.5), None])
            ),
        ]
    );
}

/// A member that is an integer counter for the first ticks and a float
/// counter after (a schema change mid-recording) reads as one float series
/// with every value, whether the change falls inside a segment or at a
/// seal. Integers become `f64`.
#[test]
fn a_series_that_turns_float_reads_as_one_float_series() {
    for max_rows in [100, 4] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns.dendro");
        let config = WriterConfig {
            seal: SealPolicy {
                max_rows,
                ..SealPolicy::default()
            },
            ..WriterConfig::default()
        };
        let mut writer = ArchiveWriter::create(&path, config).unwrap();
        let mut source = writer
            .add_source(labels(&[("source", "s")]), BTreeMap::new(), BASE)
            .unwrap();
        let int_schema = Arc::new(GroupSchema {
            counters: vec![desc("0", &[("metric", "turns_total")])],
            ..GroupSchema::default()
        });
        let float_schema = Arc::new(GroupSchema {
            float_counters: vec![desc("0", &[("metric", "turns_total")])],
            ..GroupSchema::default()
        });
        for t in 0..TICKS {
            let ts = BASE + t * S;
            let g = if t < 6 {
                let mut g = group(NODE, &int_schema, t);
                g.counters = vec![Some(t * 10)];
                g
            } else {
                let mut g = group(NODE, &float_schema, t);
                g.float_counters = vec![Some(t as f64 * 10.0 + 0.5)];
                g
            };
            let snap = Snapshot::V3(SnapshotV3 {
                systemtime: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ts),
                duration: std::time::Duration::from_millis(1),
                metadata: Default::default(),
                groups: vec![g],
            });
            let staged = source.stage(&snap, ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
        }
        source.finalize((BASE + (TICKS - 1) * S, 0)).unwrap();
        writer.join().unwrap();

        let reader = open(&path);
        assert_eq!(reader.counter_labels("turns_total").len(), 1, "{max_rows}");
        let node = table_source(&reader, "turns_total");
        let want: Vec<f64> = (0..TICKS)
            .map(|t| {
                if t < 6 {
                    (t * 10) as f64
                } else {
                    t as f64 * 10.0 + 0.5
                }
            })
            .collect();
        assert_eq!(
            float_counter(node.as_ref(), "turns_total", None),
            (times(), bits(&want)),
            "max_rows {max_rows}"
        );
        let streams = node
            .counter_streams("turns_total", &Labels::default(), 0, u64::MAX)
            .unwrap();
        assert_eq!(streams.len(), 1);
        assert!(streams[0].float, "max_rows {max_rows}");
        assert_eq!(
            streams
                .into_iter()
                .next()
                .unwrap()
                .samples
                .map(|s| s.value.as_f64().to_bits())
                .collect::<Vec<_>>(),
            bits(&want),
            "max_rows {max_rows}"
        );
    }
}

/// A table built by hand whose float column's metadata names another kind
/// is written with the kind its values say.
#[test]
fn a_float_column_is_written_with_its_own_kind() {
    use metriken_storage::table::{read_table_parquet, write_table_parquet, Column, Table, Values};
    let column = |name: &str, kind: &str, values: Values| Column {
        name: name.to_string(),
        metadata: [
            ("metric".to_string(), name.to_string()),
            ("metric_type".to_string(), kind.to_string()),
        ]
        .into(),
        values,
        windows: Vec::new(),
    };
    let table = Table {
        sampler: "t/g".to_string(),
        timestamps: vec![BASE],
        wall_offsets: Vec::new(),
        columns: vec![
            column("fc", "gauge", Values::FloatCounter(vec![Some(1.5)])),
            column("fg", "histogram", Values::FloatGauge(vec![Some(-1.5)])),
        ],
        table_window: Some(vec![None]),
    };
    let read = read_table_parquet("t/g".to_string(), write_table_parquet(&table).unwrap()).unwrap();
    let got: Vec<(&str, Option<&str>, &Values)> = read
        .columns
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.metadata.get("metric_type").map(String::as_str),
                &c.values,
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (
                "fc",
                Some("counter"),
                &Values::FloatCounter(vec![Some(1.5)])
            ),
            ("fg", Some("gauge"), &Values::FloatGauge(vec![Some(-1.5)])),
        ]
    );
}
