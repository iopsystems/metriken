//! The writer's oracle: the same snapshots written long and forced wide
//! read back the same through `ArchiveReader`, apart from `__occupant__`.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use dendro::seal::SealPolicy;
use metriken_archive::{
    ArchiveReader, ArchiveWriter, Catalog, DendroCatalog, SourceRecorder, WriterConfig,
};
use metriken_exposition::{GroupSchema, GroupSnapshot, MetricDesc, Snapshot, SnapshotV3};
use metriken_query::{BufferPool, MetricsSource, QueryResult};

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;
const TICKS: u64 = 24;

/// Who holds each slot at tick `t`: `(slot, comm, pid, uid)`. Slot 0 changes
/// hands at tick 10, slot 2 exists for ticks 5..15, and at tick 3 the group
/// has no members at all.
fn occupants(t: u64) -> Vec<(u64, &'static str, u64, &'static str)> {
    let mut v = Vec::new();
    if t == 3 {
        return v;
    }
    v.push(if t < 10 {
        (0, "nginx", 100, "a0")
    } else {
        (0, "redis", 300, "c0")
    });
    v.push((1, "sshd", 200, "b0"));
    if (5..15).contains(&t) {
        v.push((2, "cron", 400, "d0"));
    }
    v
}

fn desc(
    name: String,
    metric: &str,
    extra: &[(&str, &str)],
    occ: (u64, &str, u64, &str),
) -> MetricDesc {
    let mut metadata: BTreeMap<String, String> = [
        ("metric", metric.to_string()),
        ("sampler", "threads".to_string()),
        ("id", occ.0.to_string()),
        ("comm", occ.1.to_string()),
        ("pid", occ.2.to_string()),
        ("__uid__", occ.3.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for (k, v) in extra {
        metadata.insert(k.to_string(), v.to_string());
    }
    MetricDesc { name, metadata }
}

fn snapshot(t: u64) -> Snapshot {
    let ts = BASE + t * S;
    let window = Some(metriken::Window::new(ts - S / 2, ts));
    let mut schema = GroupSchema::default();
    let (mut counters, mut histograms) = (Vec::new(), Vec::new());
    for occ @ (slot, _, pid, _) in occupants(t) {
        schema
            .counters
            .push(desc(format!("0x{slot}"), "cpu_time", &[], occ));
        counters.push(Some(t * 1000 + pid));
        schema.counters.push(desc(
            format!("1x{slot}"),
            "syscalls",
            &[("op", "read")],
            occ,
        ));
        counters.push(Some(t * 10 + pid));
        schema.counters.push(desc(
            format!("2x{slot}"),
            "syscalls",
            &[("op", "write")],
            occ,
        ));
        counters.push(Some(t * 3 + pid));
        schema.histograms.push(desc(
            format!("3x{slot}"),
            "latency",
            &[("grouping_power", "2"), ("max_value_power", "10")],
            occ,
        ));
        let mut h = histogram::Histogram::new(2, 10).unwrap();
        h.add(pid, t + 1).unwrap();
        histograms.push(Some(h));
    }
    // A member with no slot, beside the slotted ones: in a long group it
    // is the occupant of slot "".
    let mut gauges = Vec::new();
    if t != 3 {
        schema.gauges.push(MetricDesc {
            name: "8".to_string(),
            metadata: [("metric", "threads_total"), ("sampler", "threads")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        });
        gauges.push(Some(occupants(t).len() as i64));
    }
    let hash = schema.hash();
    let threads = GroupSnapshot {
        name: "threads/tasks".to_string(),
        schema_hash: hash,
        schema: Some(Arc::new(schema)),
        window,
        counters,
        gauges,
        histograms,
    };
    let fixed_schema = GroupSchema {
        counters: Vec::new(),
        gauges: vec![MetricDesc {
            name: "9".to_string(),
            metadata: [("metric", "mem_free"), ("sampler", "memory")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }],
        histograms: Vec::new(),
    };
    let fixed = GroupSnapshot {
        name: "memory/meminfo".to_string(),
        schema_hash: fixed_schema.hash(),
        // After the first tick the schema is only named by its hash.
        schema: (t == 0).then(|| Arc::new(fixed_schema)),
        window,
        counters: Vec::new(),
        gauges: vec![Some(1_000_000 - t as i64)],
        histograms: Vec::new(),
    };
    Snapshot::V3(SnapshotV3 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::from_millis(1),
        metadata: HashMap::new(),
        groups: vec![threads, fixed],
    })
}

/// Record `TICKS` snapshots; finalize when `finalize`, else leave the tail
/// in the WAL. With `evict`, drop everything before that tick at the end.
fn record(path: &Path, long_groups: bool, finalize: bool, evict: Option<u64>) {
    record_with(path, long_groups, finalize, evict, &mut |_, _| {});
}

/// [`record`], running `on_tick` after each tick has landed.
fn record_with(
    path: &Path,
    long_groups: bool,
    finalize: bool,
    evict: Option<u64>,
    on_tick: &mut dyn FnMut(&mut SourceRecorder, u64),
) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        restate_every_ns: 6 * S,
        long_groups,
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    let labels = [("source".to_string(), "test".to_string())]
        .into_iter()
        .collect();
    let mut source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
    for t in 0..TICKS {
        let snap = snapshot(t);
        let staged = source.stage(&snap, BASE + t * S, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
        source.maybe_seal().unwrap();
        source.sync().unwrap();
        on_tick(&mut source, t);
    }
    if let Some(t) = evict {
        source.evict_before(BASE + t * S).unwrap();
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
    assert_eq!(recordings.len(), 1);
    recordings.remove(0).1
}

type Answer = Vec<(Vec<(String, String)>, Vec<(i64, String)>)>;

/// A query's answer from tick `from` on, as sorted `(labels, values)`
/// without `__occupant__`, and whether any series carried `__occupant__`.
fn answer(reader: &ArchiveReader, query: &str, from: u64) -> Result<(Answer, bool), String> {
    let (start, end) = (
        (BASE + from * S) as f64 / 1e9,
        (BASE + (TICKS - 1) * S) as f64 / 1e9,
    );
    let result = reader
        .query_range(query, start, end, 1.0)
        .map_err(|e| format!("{e:?}"))?;
    let QueryResult::Matrix { result } = result else {
        panic!("{query}: not a matrix");
    };
    let occupant = result.iter().any(|m| m.metric.contains_key("__occupant__"));
    let mut out: Vec<_> = result
        .into_iter()
        .map(|m| {
            let mut labels: Vec<_> = m
                .metric
                .into_iter()
                .filter(|(k, _)| k != "__occupant__")
                .collect();
            labels.sort();
            let values = m
                .values
                .into_iter()
                .map(|(t, v)| ((t * 1000.0).round() as i64, format!("{v:.6}")))
                .collect();
            (labels, values)
        })
        .collect();
    out.sort();
    Ok((out, occupant))
}

const QUERIES: &[&str] = &[
    "rate(cpu_time[3s])",
    "irate(cpu_time[3s])",
    "sum by (comm) (rate(cpu_time[3s]))",
    "sum by (op) (rate(syscalls[3s]))",
    "rate(syscalls{op=\"write\", comm=\"cron\"}[3s])",
    "irate(cpu_time{__uid__=\"c0\"}[3s])",
    "mem_free",
    "threads_total",
    "histogram_quantile(0.5, latency)",
];

fn streams(path: &Path) -> Vec<String> {
    let catalog = DendroCatalog::open(path).unwrap();
    let id = catalog.sources().unwrap()[0].id;
    catalog.tables(id).unwrap()
}

fn compare(finalize: bool, evict: Option<u64>) {
    let dir = tempfile::tempdir().unwrap();
    let (long, wide) = (
        dir.path().join("long.dendro"),
        dir.path().join("wide.dendro"),
    );
    record(&long, true, finalize, evict);
    record(&wide, false, finalize, evict);
    assert!(streams(&long).contains(&"threads/tasks/occupants".to_string()));
    assert!(!streams(&wide).iter().any(|s| s.ends_with("/occupants")));
    let from = evict.unwrap_or(0);
    let (long, wide) = (open(&long), open(&wide));
    for q in QUERIES {
        let (l, w) = (answer(&long, q, from), answer(&wide, q, from));
        assert_eq!(l.as_ref().map(|a| &a.0), w.as_ref().map(|a| &a.0), "{q}");
        // `cron` is gone by tick 15, so after eviction it has no series.
        if evict.is_some() && q.contains("cron") {
            assert!(
                w.as_ref().is_ok_and(|(a, _)| a.is_empty()),
                "{q}: an evicted occupant still answers: {w:?}"
            );
            continue;
        }
        let ((w, wide_occupants), (_, long_occupants)) = (w.unwrap(), l.unwrap());
        assert!(!w.is_empty(), "{q}: the wide archive has no answer");
        assert!(!wide_occupants, "{q}: a wide series carries __occupant__");
        if q.contains("cpu_time") && !q.contains("sum") {
            assert!(long_occupants, "{q}: no long series carries __occupant__");
        }
    }
}

#[test]
fn long_and_wide_answer_the_same_when_finalized() {
    compare(true, None);
}

#[test]
fn long_and_wide_answer_the_same_from_the_live_tail() {
    compare(false, None);
}

/// Occupant rows are kept one restatement period (6 ticks here) behind the
/// cutoff. Restatements land at ticks 6, 12 and 18, so with a cutoff at 19
/// the rows for ticks 19..24 name occupants whose labels were last written
/// at tick 18; evicting occupant rows at the cutoff itself loses them.
#[test]
fn long_and_wide_answer_the_same_after_eviction() {
    compare(true, Some(19));
}

/// Metadata patched during a recording reads back with the source, merged
/// into what it started with: live after a sync, and after finalize.
#[test]
fn metadata_patched_during_a_recording_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.dendro");
    let mut writer = ArchiveWriter::create(&path, WriterConfig::default()).unwrap();
    let labels = [("source".to_string(), "test".to_string())]
        .into_iter()
        .collect();
    let metadata = [("version".to_string(), "1.0".to_string())]
        .into_iter()
        .collect();
    let mut source = writer.add_source(labels, metadata, BASE).unwrap();
    let events = |n: usize| {
        (
            "events".to_string(),
            format!(
                r#"{{"events":[{}]}}"#,
                vec![r#"{"kind":"run_start"}"#; n].join(",")
            ),
        )
    };
    for t in 0..4 {
        let staged = source.stage(&snapshot(t), BASE + t * S, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
        if t == 1 {
            source
                .update_metadata([events(1)].into_iter().collect())
                .unwrap();
        }
        source.maybe_seal().unwrap();
    }
    source.sync().unwrap();
    let live = open(&path);
    assert_eq!(live.metadata_get("events"), Some(events(1).1));
    assert_eq!(live.metadata_get("version").as_deref(), Some("1.0"));

    // The last patch before finalize replaces the key, and is kept.
    source
        .update_metadata([events(2)].into_iter().collect())
        .unwrap();
    source.finalize((BASE + 3 * S, 0)).unwrap();
    writer.join().unwrap();
    let done = open(&path);
    assert_eq!(done.metadata_get("events"), Some(events(2).1));
    assert_eq!(done.metadata_get("version").as_deref(), Some("1.0"));
}

/// A V2 snapshot (no acquisition groups) is written one table per sampler
/// and reads back: values, labels, a metric that first appears mid-segment,
/// and a tick whose window did not advance, which is skipped.
#[test]
fn a_v2_snapshot_is_written_one_table_per_sampler() {
    use metriken_exposition::{Counter, Gauge, SnapshotV2};

    let meta = |metric: &str, sampler: &str, extra: &[(&str, &str)]| {
        let mut m: HashMap<String, String> = [("metric", metric), ("sampler", sampler)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        for (k, v) in extra {
            m.insert(k.to_string(), v.to_string());
        }
        m
    };
    let v2 = |t: u64, window_tick: u64| {
        let ts = BASE + t * S;
        let w = Some(metriken::Window::new(
            BASE + window_tick * S - S / 2,
            BASE + window_tick * S,
        ));
        let mut counters = vec![
            Counter::new(
                "0".into(),
                window_tick * 10,
                meta("ops", "fake", &[("op", "read")]),
            )
            .with_window(w),
            Counter::new(
                "1".into(),
                window_tick * 20,
                meta("ops", "fake", &[("op", "write")]),
            )
            .with_window(w),
        ];
        // First seen mid-segment, inside the first one: the reader routes a
        // table by one segment's footer, so a metric first seen in a later
        // segment is not found (the same for a `.rez`).
        if window_tick >= 2 {
            counters.push(
                Counter::new("2".into(), window_tick, meta("late", "fake", &[])).with_window(w),
            );
        }
        Snapshot::V2(SnapshotV2 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
            duration: Duration::ZERO,
            metadata: HashMap::new(),
            counters,
            gauges: vec![
                Gauge::new("3".into(), window_tick as i64, meta("free", "mem", &[])).with_window(w),
            ],
            histograms: Vec::new(),
        })
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v2.dendro");
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 4,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(&path, config).unwrap();
    let labels = [("source".to_string(), "test".to_string())]
        .into_iter()
        .collect();
    let mut source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
    // Tick 7 repeats tick 6's window: the producer did not re-read, so the
    // row is skipped rather than written twice.
    let windows = [0, 1, 2, 3, 4, 5, 6, 6, 8, 9, 10, 11];
    for (t, &w) in windows.iter().enumerate() {
        let staged = source
            .stage(&v2(t as u64, w), BASE + t as u64 * S, 0)
            .unwrap();
        writer.commit(vec![staged]).unwrap();
        source.maybe_seal().unwrap();
    }
    source.finalize((BASE + 11 * S, 0)).unwrap();
    writer.join().unwrap();

    assert_eq!(
        {
            let mut s = streams(&path);
            s.sort();
            s
        },
        vec!["fake".to_string(), "mem".to_string()]
    );
    let reader = open(&path);
    let rows = |q: &str| answer(&reader, q, 0).unwrap().0;
    let by_op = rows("sum by (op) (rate(ops[2s]))");
    assert_eq!(by_op.len(), 2, "{by_op:?}");
    for (labels, values) in &by_op {
        let want = if labels.contains(&("op".to_string(), "read".to_string())) {
            10.0
        } else {
            20.0
        };
        assert!(!values.is_empty());
        for (t, v) in values {
            assert!(
                (v.parse::<f64>().unwrap() - want).abs() < 1e-6,
                "{labels:?} at {t}: {v}"
            );
        }
    }
    let late = rows("rate(late[2s])");
    assert_eq!(late.len(), 1);
    assert!(late[0]
        .1
        .iter()
        .all(|(_, v)| (v.parse::<f64>().unwrap() - 1.0).abs() < 1e-6));
    // The gauge reads back its last value, and the skipped tick did not add
    // a row: twelve ticks, eleven distinct windows.
    let free = rows("free");
    assert_eq!(free.len(), 1);
    assert_eq!(free[0].1.last().unwrap().1.parse::<f64>().unwrap(), 11.0);
    use metriken_archive::Catalog;
    let catalog = DendroCatalog::open(&path).unwrap();
    let id = catalog.sources().unwrap()[0].id;
    let rows_in = |t: &str| -> u64 {
        catalog
            .segment_meta(id, t)
            .unwrap()
            .iter()
            .map(|(_, m)| m.rows)
            .sum()
    };
    assert_eq!(rows_in("mem"), 11);
    assert!(
        catalog.segment_meta(id, "fake").unwrap().len() > 1,
        "the table sealed more than once, so a later segment had to carry its metadata again"
    );
}

/// An encoder built from an archive's stream list re-encodes a long table's
/// unsealed rows as long, and its version matches the writer's, so dendro
/// accepts it for a ranged copy of a live archive.
#[test]
fn an_encoder_for_an_archives_streams_copies_a_live_long_table() {
    use dendro::archive::{Archive, ArchiveMut};
    use dendro::rewrite::{copy_sources_into, CopySpec};
    use metriken_archive::writer::Encoder;

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("live.dendro");
    record(&src, true, false, None);

    let archive = Archive::open(&src).unwrap();
    let mut streams = Vec::new();
    for s in archive.read_sources().unwrap() {
        streams.extend(archive.all_streams(s.id).unwrap());
    }
    let encoder = Encoder::for_streams(streams.iter().map(String::as_str));
    let dst = dir.path().join("copy.dendro");
    let mut out = ArchiveMut::create(&dst).unwrap();
    out.transaction(|tx| copy_sources_into(&archive, tx, &CopySpec::everything(), &encoder))
        .unwrap();
    drop(out);

    let (a, b) = (open(&src), open(&dst));
    for q in QUERIES {
        let (x, y) = (answer(&a, q, 0), answer(&b, q, 0));
        assert_eq!(x.as_ref().map(|r| &r.0), y.as_ref().map(|r| &r.0), "{q}");
    }
}

/// Sealed segments are written with the configured codec: zstd level 3 by
/// default, and LZ4 when asked.
#[test]
fn sealed_segments_use_the_configured_codec() {
    use metriken_archive::writer::Compression;
    use metriken_archive::Catalog;

    let codecs = |path: &Path| -> BTreeSet<String> {
        let catalog = DendroCatalog::open(path).unwrap();
        let id = catalog.sources().unwrap()[0].id;
        let mut out = BTreeSet::new();
        for table in catalog.tables(id).unwrap() {
            for (seq, _) in catalog.segment_meta(id, &table).unwrap() {
                let bytes = catalog.segment_bytes(id, &table, seq).unwrap().unwrap();
                let meta =
                    parquet::file::reader::SerializedFileReader::new(bytes::Bytes::from(bytes))
                        .map(|r| {
                            use parquet::file::reader::FileReader;
                            r.metadata().clone()
                        })
                        .unwrap();
                for rg in meta.row_groups() {
                    for c in rg.columns() {
                        // The footer names the codec, not its level.
                        let codec = format!("{:?}", c.compression());
                        out.insert(codec.split('(').next().unwrap().to_string());
                    }
                }
            }
        }
        out
    };

    let dir = tempfile::tempdir().unwrap();
    let zstd = dir.path().join("zstd.dendro");
    record(&zstd, true, true, None);
    assert_eq!(
        codecs(&zstd),
        BTreeSet::from(["ZSTD".to_string()]),
        "every column of every sealed segment, occupant streams included"
    );

    let lz4 = dir.path().join("lz4.dendro");
    let mut writer = ArchiveWriter::create(
        &lz4,
        WriterConfig {
            compression: Compression::LZ4_RAW,
            ..WriterConfig::default()
        },
    )
    .unwrap();
    let labels = [("source".to_string(), "test".to_string())]
        .into_iter()
        .collect();
    let mut source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
    for t in 0..4 {
        let staged = source.stage(&snapshot(t), BASE + t * S, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
    }
    source.finalize((BASE + 3 * S, 0)).unwrap();
    writer.join().unwrap();
    assert_eq!(codecs(&lz4), BTreeSet::from(["LZ4_RAW".to_string()]));
}

/// A copy trimmed to some metrics keeps a long table long: its `occupant`
/// column, its layout markers, and its occupant stream whole. The kept
/// metric answers the same; the others are gone. A table left with no kept
/// metric is dropped by the copy, and its occupant stream is not.
#[test]
fn keep_metrics_trims_a_long_table_and_keeps_it_long() {
    use dendro::archive::{Archive, ArchiveMut};
    use dendro::rewrite::{copy_sources_into, CopySpec};
    use metriken_archive::writer::Encoder;
    use metriken_archive::{default_compression, segment_props, KeepMetrics};

    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("src.dendro");
    record(&src_path, true, true, None);
    let src = Archive::open(&src_path).unwrap();
    let mut names = Vec::new();
    for s in src.read_sources().unwrap() {
        names.extend(src.all_streams(s.id).unwrap());
    }
    let copy = |metrics: &[&str], dest: &Path| {
        let metrics: BTreeSet<String> = metrics.iter().map(|m| m.to_string()).collect();
        let keep = KeepMetrics::new(&metrics);
        let spec = CopySpec {
            keep_columns: Some(&keep),
            writer_props: Some(segment_props(default_compression())),
            ..CopySpec::everything()
        };
        let encoder = Encoder::for_streams(names.iter().map(String::as_str));
        let mut dst = ArchiveMut::create(dest).unwrap();
        dst.transaction(|tx| copy_sources_into(&src, tx, &spec, &encoder))
            .unwrap();
    };

    let cpu = dir.path().join("cpu.dendro");
    copy(&["cpu_time"], &cpu);
    let mut kept = streams(&cpu);
    kept.sort();
    assert_eq!(
        kept,
        vec![
            "threads/tasks".to_string(),
            "threads/tasks/occupants".to_string()
        ]
    );
    let (a, b) = (open(&src_path), open(&cpu));
    for q in [
        "rate(cpu_time[3s])",
        "sum by (comm) (rate(cpu_time[3s]))",
        "irate(cpu_time{__uid__=\"c0\"}[3s])",
    ] {
        let (x, y) = (answer(&a, q, 0).unwrap(), answer(&b, q, 0).unwrap());
        assert_eq!(x.0, y.0, "{q}");
        if !q.starts_with("sum") {
            assert!(
                y.1,
                "{q}: still a long table, its series carry __occupant__"
            );
        }
    }
    assert!(
        answer(&b, "rate(syscalls[3s])", 0).is_err(),
        "syscalls is gone"
    );

    let mem = dir.path().join("mem.dendro");
    copy(&["mem_free"], &mem);
    let mut kept = streams(&mem);
    kept.sort();
    assert_eq!(
        kept,
        vec![
            "memory/meminfo".to_string(),
            "threads/tasks/occupants".to_string()
        ],
        "the long table had no kept metric; its occupant stream is the caller's to remove"
    );
}

/// Every sealed segment carries the segment format version, and a source
/// written by an encoder this reader does not decode is refused at open
/// rather than misread.
#[test]
fn segments_carry_the_format_and_an_unknown_encoder_is_refused() {
    use metriken_archive::Catalog;
    use metriken_storage::format::{FORMAT_KEY, FORMAT_VERSION};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rec.dendro");
    record(&path, true, true, None);

    let catalog = DendroCatalog::open(&path).unwrap();
    let source = catalog.sources().unwrap().remove(0);
    assert_eq!(
        source
            .metadata
            .get(dendro::keys::ENCODER)
            .map(String::as_str),
        Some(metriken_archive::ENCODER_VERSION)
    );
    let mut segments = 0;
    for table in catalog.tables(source.id).unwrap() {
        for (seq, _) in catalog.segment_meta(source.id, &table).unwrap() {
            let bytes = catalog
                .segment_bytes(source.id, &table, seq)
                .unwrap()
                .unwrap();
            let reader =
                parquet::file::reader::SerializedFileReader::new(bytes::Bytes::from(bytes))
                    .unwrap();
            use parquet::file::reader::FileReader;
            let kv = reader
                .metadata()
                .file_metadata()
                .key_value_metadata()
                .cloned();
            let version = kv
                .unwrap_or_default()
                .into_iter()
                .find(|e| e.key == FORMAT_KEY)
                .and_then(|e| e.value);
            assert_eq!(version, Some(FORMAT_VERSION.to_string()), "{table} #{seq}");
            segments += 1;
        }
    }
    assert!(segments > 0);
    drop(catalog);
    let _ = open(&path);

    {
        let mut db = dendro::archive::ArchiveMut::open(&path).unwrap();
        let id = db.read_sources().unwrap()[0].id;
        let mut md = db.read_sources().unwrap()[0].meta.metadata.clone();
        md.insert(
            dendro::keys::ENCODER.to_string(),
            "metriken-archive/2".to_string(),
        );
        db.update_source_metadata(id, &md).unwrap();
    }
    let err = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(&path).unwrap()),
        None,
        BufferPool::new(64 * 1024 * 1024),
        None,
    )
    .err()
    .expect("an unknown encoder is refused");
    assert!(err.to_string().contains("\"metriken-archive/2\""), "{err}");
}

/// A tick of two groups whose metric sets grow: `memory/meminfo` (wide)
/// gains `swap_free` at tick 7, `threads/tasks` (long) gains `ctx_switches`
/// at tick 12, and `memory/meminfo` gains `dirty` at tick 21, after the
/// last seal of a live recording.
fn growing(t: u64) -> Snapshot {
    let ts = BASE + t * S;
    let window = Some(metriken::Window::new(ts - S / 2, ts));
    let gauge = |name: &str, metric: &str| MetricDesc {
        name: name.to_string(),
        metadata: [("metric", metric), ("sampler", "memory")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    };
    let mut mem = GroupSchema::default();
    let mut gauges = vec![Some(1_000 - t as i64)];
    mem.gauges.push(gauge("9", "mem_free"));
    if t >= 7 {
        mem.gauges.push(gauge("10", "swap_free"));
        gauges.push(Some(500 + t as i64));
    }
    if t >= 21 {
        mem.gauges.push(gauge("11", "dirty"));
        gauges.push(Some(t as i64));
    }
    let mut tasks = GroupSchema::default();
    let mut counters = Vec::new();
    for occ @ (slot, _, pid, _) in occupants(t) {
        tasks
            .counters
            .push(desc(format!("0x{slot}"), "cpu_time", &[], occ));
        counters.push(Some(t * 1000 + pid));
        if t >= 12 {
            tasks
                .counters
                .push(desc(format!("4x{slot}"), "ctx_switches", &[], occ));
            counters.push(Some(t * 7 + pid));
        }
    }
    let group = |name: &str, schema: GroupSchema, counters, gauges| GroupSnapshot {
        name: name.to_string(),
        schema_hash: schema.hash(),
        schema: Some(Arc::new(schema)),
        window,
        counters,
        gauges,
        histograms: Vec::new(),
    };
    Snapshot::V3(SnapshotV3 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::from_millis(1),
        metadata: HashMap::new(),
        groups: vec![
            group("threads/tasks", tasks, counters, Vec::new()),
            group("memory/meminfo", mem, Vec::new(), gauges),
        ],
    })
}

/// A metric that first appears in a later segment of a table, or only in its
/// live tail, is found: the reader routes a query by every segment's names,
/// not the first segment's.
#[test]
fn a_metric_that_first_appears_in_a_later_segment_is_found() {
    for finalize in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grow.dendro");
        let config = WriterConfig {
            seal: SealPolicy {
                max_rows: 5,
                ..SealPolicy::default()
            },
            ..WriterConfig::default()
        };
        let mut writer = ArchiveWriter::create(&path, config).unwrap();
        let labels = [("source".to_string(), "test".to_string())]
            .into_iter()
            .collect();
        let mut source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
        for t in 0..TICKS {
            let staged = source.stage(&growing(t), BASE + t * S, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
        }
        if finalize {
            source.finalize((BASE + (TICKS - 1) * S, 0)).unwrap();
        } else {
            source.sync().unwrap();
            std::mem::forget(source);
        }
        writer.join().unwrap();

        let reader = open(&path);
        let points = |q: &str| -> usize {
            let (a, _) = answer(&reader, q, 0).unwrap();
            a.iter().map(|(_, v)| v.len()).sum()
        };
        assert!(
            points("sum(swap_free)") > 0,
            "finalize={finalize}: swap_free"
        );
        assert!(
            points("sum(rate(ctx_switches[3s]))") > 0,
            "finalize={finalize}: ctx_switches"
        );
        assert!(points("sum(dirty)") > 0, "finalize={finalize}: dirty");
        assert!(points("sum(mem_free)") > 0);
        let mut names = reader.gauge_names();
        names.sort();
        assert_eq!(
            names,
            vec!["dirty", "mem_free", "swap_free"],
            "finalize={finalize}"
        );
    }
}

/// A catalog counting the sealed segments read through it.
struct Counting {
    inner: DendroCatalog,
    fetched: Arc<Fetched>,
}

/// Sealed segments read: of data tables, and of occupant streams.
#[derive(Default)]
struct Fetched {
    tables: std::sync::atomic::AtomicUsize,
    occupants: std::sync::atomic::AtomicUsize,
}

impl Fetched {
    fn get(&self) -> (usize, usize) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.tables.load(Relaxed), self.occupants.load(Relaxed))
    }
}

impl Catalog for Counting {
    fn sources(&self) -> Result<Vec<metriken_archive::catalog::Source>, String> {
        self.inner.sources()
    }
    fn tables(&self, source_id: i64) -> Result<Vec<String>, String> {
        self.inner.tables(source_id)
    }
    fn segment_meta(
        &self,
        source_id: i64,
        table: &str,
    ) -> Result<Vec<(u64, metriken_archive::catalog::SegmentMeta)>, String> {
        self.inner.segment_meta(source_id, table)
    }
    fn segment_bytes(
        &self,
        source_id: i64,
        table: &str,
        seq: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        let counter = if table.ends_with("/occupants") {
            &self.fetched.occupants
        } else {
            &self.fetched.tables
        };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.segment_bytes(source_id, table, seq)
    }
    fn live_wal(
        &self,
        source_id: i64,
        table: &str,
    ) -> Result<Vec<metriken_archive::catalog::WalRow>, String> {
        self.inner.live_wal(source_id, table)
    }
    fn segment_indexes(
        &self,
        source_id: i64,
        table: &str,
    ) -> Result<Vec<metriken_archive::SegmentIndex>, String> {
        self.inner.segment_indexes(source_id, table)
    }
    fn segment_span(
        &self,
        source_id: i64,
        table: &str,
    ) -> Result<(u64, metriken_archive::catalog::Span), String> {
        self.inner.segment_span(source_id, table)
    }
    fn live_wal_span(
        &self,
        source_id: i64,
        table: &str,
    ) -> Result<metriken_archive::catalog::Span, String> {
        self.inner.live_wal_span(source_id, table)
    }
    fn caller_row_streams(&self, source_id: i64) -> Result<Vec<String>, String> {
        self.inner.caller_row_streams(source_id)
    }
    fn caller_rows(
        &self,
        source_id: i64,
        stream: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, String> {
        self.inner.caller_rows(source_id, stream, from, to)
    }
    fn last_caller_row_at_or_before(
        &self,
        source_id: i64,
        stream: &str,
        upto: u64,
        pred: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<Option<u64>, String> {
        self.inner
            .last_caller_row_at_or_before(source_id, stream, upto, pred)
    }
}

/// Opens `path` on `pool` through a [`Counting`] catalog; returns the reader
/// and its fetch counts.
fn open_counting(path: &Path, pool: &Arc<BufferPool>) -> (ArchiveReader, Arc<Fetched>) {
    let fetched = Arc::new(Fetched::default());
    let catalog = Counting {
        inner: DendroCatalog::open(path).unwrap(),
        fetched: Arc::clone(&fetched),
    };
    let mut recordings =
        ArchiveReader::from_catalog(Box::new(catalog), None, Arc::clone(pool), None).unwrap();
    (recordings.remove(0).1, fetched)
}

fn answers(reader: &ArchiveReader, from: u64) -> Vec<Result<Answer, String>> {
    QUERIES
        .iter()
        .map(|q| answer(reader, q, from).map(|a| a.0))
        .collect()
}

/// While an archive is written, and through an eviction, a reader reopened
/// with `reuse_from` answers as a fresh open does, and reads fewer
/// segments doing it.
fn follow(long_groups: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("follow.dendro");
    let pool = BufferPool::new(64 * 1024 * 1024);
    let mut previous: Option<ArchiveReader> = None;
    let (mut reused, mut fresh_total) = ((0, 0), (0, 0));
    let mut from = 0;
    record_with(&path, long_groups, false, None, &mut |source, t| {
        if t == 17 {
            from = 12;
            source.evict_before(BASE + from * S).unwrap();
            source.sync().unwrap();
        }
        if t % 2 == 0 && t != 18 {
            return;
        }
        let (fresh, fresh_count) = open_counting(&path, &pool);
        let (after, after_count) = open_counting(&path, &pool);
        match &previous {
            Some(previous) => after.reuse_from(previous),
            None => after.keep_handover(),
        }
        assert_eq!(answers(&after, from), answers(&fresh, from), "tick {t}");
        if previous.is_some() {
            let (a, f) = (after_count.get(), fresh_count.get());
            reused = (reused.0 + a.0, reused.1 + a.1);
            fresh_total = (fresh_total.0 + f.0, fresh_total.1 + f.1);
        }
        previous = Some(after);
    });
    assert!(
        reused.0 * 2 < fresh_total.0,
        "reuse read {} table segments, fresh opens {}",
        reused.0,
        fresh_total.0
    );
    if long_groups {
        assert!(
            reused.1 * 2 < fresh_total.1,
            "reuse read {} occupant segments, fresh opens {}",
            reused.1,
            fresh_total.1
        );
    }
}

#[test]
fn a_reused_long_reader_answers_as_a_fresh_open() {
    follow(true);
}

#[test]
fn a_reused_wide_reader_answers_as_a_fresh_open() {
    follow(false);
}

/// Compaction gives a merged segment the sequence number of the first
/// segment it replaced. A reader that saw that first segment alone is not
/// reused for the merged one.
#[test]
fn a_compacted_segment_is_not_taken_for_the_one_it_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compact.dendro");
    let pool = BufferPool::new(64 * 1024 * 1024);
    let mut early: Option<ArchiveReader> = None;
    record_with(&path, true, true, None, &mut |_, t| {
        // One segment of five rows sealed, the rest in the tail. Read from
        // a copy, since compaction needs the archive to itself; the copy's
        // sequence numbers are the original's.
        if t == 6 {
            let copy = dir.path().join("early.dendro");
            for suffix in ["", "-wal"] {
                let from = format!("{}{suffix}", path.display());
                if std::path::Path::new(&from).exists() {
                    std::fs::copy(&from, format!("{}{suffix}", copy.display())).unwrap();
                }
            }
            let (reader, _) = open_counting(&copy, &pool);
            reader.keep_handover();
            answers(&reader, 0);
            early = Some(reader);
        }
    });
    {
        let id = DendroCatalog::open(&path).unwrap().sources().unwrap()[0].id;
        let tables = streams(&path);
        let mut db = dendro::archive::ArchiveMut::open(&path).unwrap();
        for stream in tables {
            dendro::rewrite::compact_stream(
                &mut db,
                id,
                &stream,
                &dendro::rewrite::CompactSpec::to_rows(10),
            )
            .unwrap();
        }
    }
    let (fresh, _) = open_counting(&path, &pool);
    let (after, _) = open_counting(&path, &pool);
    after.reuse_from(early.as_ref().unwrap());
    assert_eq!(answers(&after, 0), answers(&fresh, 0));
}

/// A reader whose tables were not queried passes on the state it was given,
/// and a first reader without `keep_handover` saves none.
#[test]
fn state_passes_through_a_reader_that_was_not_queried() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pass.dendro");
    record(&path, true, true, None);
    let pool = BufferPool::new(64 * 1024 * 1024);
    let (fresh, fresh_count) = open_counting(&path, &pool);
    let expected = answers(&fresh, 0);

    let (first, _) = open_counting(&path, &pool);
    first.keep_handover();
    answers(&first, 0);
    let (second, second_count) = open_counting(&path, &pool);
    second.reuse_from(&first);
    drop(first);
    let (third, third_count) = open_counting(&path, &pool);
    third.reuse_from(&second);
    drop(second);
    assert_eq!(answers(&third, 0), expected);
    // The second reader read only what opening reads. The third read the
    // segments its query decodes, since open segments are not passed on
    // past a reader that was not queried, but no footer to build its state
    // and no occupant segment.
    let (_unqueried, probe_count) = open_counting(&path, &pool);
    assert_eq!(second_count.get(), probe_count.get());
    let (third, fresh, probe) = (third_count.get(), fresh_count.get(), probe_count.get());
    assert!(
        third.0 > probe.0 && third.0 < fresh.0 && third.1 == 0 && fresh.1 > 0,
        "{third:?} {fresh:?}"
    );

    let (plain, _) = open_counting(&path, &pool);
    answers(&plain, 0);
    let (after, after_count) = open_counting(&path, &pool);
    after.reuse_from(&plain);
    assert_eq!(answers(&after, 0), expected);
    assert_eq!(after_count.get(), fresh_count.get());
}
