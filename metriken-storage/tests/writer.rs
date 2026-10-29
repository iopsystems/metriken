//! The writer's oracle: the same snapshots written long and forced wide
//! read back the same through `ArchiveReader`, apart from `__occupant__`.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use dendro::seal::SealPolicy;
use metriken_exposition::{GroupSchema, GroupSnapshot, MetricDesc, Snapshot, SnapshotV3};
use metriken_query::{MetricsSource, QueryResult};
use metriken_storage::BufferPool;
use metriken_storage::{
    ArchiveReader, ArchiveWriter, Catalog, DendroCatalog, SourceRecorder, WriterConfig,
};

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
        gauge_histograms: Vec::new(),
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
        gauge_histograms: Vec::new(),
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
        gauge_histograms: Vec::new(),
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

/// Each metric's kind, name and label sets.
type Listings = Vec<(&'static str, String, Vec<BTreeMap<String, String>>)>;

/// Every metric's label sets, by kind and name, each list sorted.
fn listings(reader: &ArchiveReader) -> Listings {
    let mut out = Vec::new();
    let sorted = |mut sets: Vec<BTreeMap<String, String>>| {
        sets.sort();
        sets
    };
    for n in reader.counter_names() {
        out.push(("counter", n.clone(), sorted(reader.counter_labels(&n))));
    }
    for n in reader.gauge_names() {
        out.push(("gauge", n.clone(), sorted(reader.gauge_labels(&n))));
    }
    for n in reader.histogram_names() {
        out.push(("histogram", n.clone(), sorted(reader.histogram_labels(&n))));
    }
    for n in reader.gauge_histogram_names() {
        out.push((
            "gauge histogram",
            n.clone(),
            sorted(reader.gauge_histogram_labels(&n)),
        ));
    }
    out
}

/// `reader`'s listings, and the tables they built a reader for, against
/// those of a second open of `path` that builds every table's reader first.
fn listings_and_readers_built(reader: &ArchiveReader, path: &Path) -> (Listings, Vec<String>) {
    let listed = listings(reader);
    let built = reader.opened_tables();
    let read = open(path);
    let names: Vec<String> = listed.iter().map(|(_, n, _)| n.clone()).collect();
    for t in read.owners_of(&names) {
        assert!(t.reader().is_some(), "{}", t.sampler());
    }
    assert_eq!(listed, listings(&read), "{}", path.display());
    assert!(listed.iter().all(|(_, _, sets)| !sets.is_empty()));
    (listed, built)
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
    // Listed from the names indexes without a table's reader when the
    // recording is complete; a live one lists through every table's reader.
    for path in [&long, &wide] {
        let (_, mut built) = listings_and_readers_built(&open(path), path);
        built.sort();
        let expected: Vec<String> = match finalize {
            true => Vec::new(),
            false => {
                let mut tables: Vec<String> = streams(path)
                    .into_iter()
                    .filter(|s| !s.ends_with("/occupants"))
                    .collect();
                tables.sort();
                tables
            }
        };
        assert_eq!(built, expected, "{}", path.display());
    }
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

/// A complete recording whose long table still has WAL rows, as a copy
/// marked complete while its source was being written: the long table lists
/// through its reader, because a long table's materialized tail need not
/// name every occupant; the wide tables list from the names indexes.
#[test]
fn a_complete_long_table_with_a_live_tail_lists_through_its_reader() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.dendro");
    record(&path, true, false, None);
    {
        let mut db = dendro::archive::ArchiveMut::open(&path).unwrap();
        let id = db.read_sources().unwrap()[0].id;
        db.mark_complete(id).unwrap();
    }
    let (_, built) = listings_and_readers_built(&open(&path), &path);
    assert_eq!(built, vec!["threads/tasks".to_string()]);
}

/// A complete recording with a damaged segment lists its table from the
/// names indexes, and lists it the same after a query fails to build the
/// table's reader.
#[cfg(feature = "rez")]
#[test]
fn a_damaged_segment_lists_the_same_before_and_after_a_query() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wide.dendro");
    record(&path, false, true, None);
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        let changed = db
            .execute(
                "UPDATE segments SET bytes = x'00010203' WHERE stream = 'memory/meminfo' \
                 AND seq = (SELECT max(seq) FROM segments WHERE stream = 'memory/meminfo')",
                [],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }
    let reader = open(&path);
    let before = listings(&reader);
    assert!(before.iter().any(|(_, n, _)| n == "mem_free"));
    let names: Vec<String> = before.iter().map(|(_, n, _)| n.clone()).collect();
    let failed = reader
        .owners_of(&names)
        .into_iter()
        .filter(|t| t.reader().is_none())
        .count();
    assert_eq!(failed, 1, "the damaged table's reader fails to build");
    assert_eq!(listings(&reader), before);
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
            gauge_histograms: Vec::new(),
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
    use metriken_storage::Catalog;
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
    use metriken_storage::writer::Encoder;

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("live.dendro");
    record(&src, true, false, None);

    let archive = Archive::open(&src).unwrap();
    let encoder = Encoder::for_archive(&archive).unwrap();
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
    use metriken_storage::writer::Compression;
    use metriken_storage::Catalog;

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
    use metriken_storage::writer::Encoder;
    use metriken_storage::{default_compression, segment_props, KeepMetrics};

    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("src.dendro");
    record(&src_path, true, true, None);
    let src = Archive::open(&src_path).unwrap();
    let copy = |metrics: &[&str], dest: &Path| {
        let metrics: BTreeSet<String> = metrics.iter().map(|m| m.to_string()).collect();
        let keep = KeepMetrics::new(&metrics);
        let spec = CopySpec {
            keep_columns: Some(&keep),
            writer_props: Some(segment_props(default_compression())),
            ..CopySpec::everything()
        };
        let encoder = Encoder::for_archive(&src).unwrap();
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
    use metriken_storage::format::FORMAT_KEY;
    use metriken_storage::Catalog;

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
        Some(metriken_storage::ENCODER_VERSION)
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
            // Format 1: this recording holds no gauge histogram, the one
            // column kind that needs format 2.
            assert_eq!(version, Some("1".to_string()), "{table} #{seq}");
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
        gauge_histograms: Vec::new(),
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
/// not the first segment's. The names come from the segments' names
/// indexes, so the open fetches no sealed segment.
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

        let pool = BufferPool::new(1 << 20);
        let (counted, fetched) = open_counting(&path, &pool);
        assert_eq!(
            fetched.get(),
            (0, 0),
            "finalize={finalize}: fetched at open"
        );
        let mut names = counted.gauge_names();
        names.sort();
        assert_eq!(names, vec!["dirty", "mem_free", "swap_free"]);

        // A complete recording lists every metric from the names indexes'
        // columns, building no reader and fetching no table segment; a live
        // one lists through each table's reader.
        let (listed, built) = listings_and_readers_built(&counted, &path);
        assert!(listed.iter().any(|(_, n, _)| n == "ctx_switches"));
        assert!(listed.iter().any(|(_, n, _)| n == "dirty"));
        if finalize {
            assert_eq!(built, Vec::<String>::new());
            assert_eq!(fetched.get().0, 0, "finalize={finalize}");
        } else {
            assert_eq!(built.len(), 2, "{built:?}");
        }
        // An index with no columns frame, as one written before them: each
        // table's reader is built for the listing, which is the same.
        let (counted, _) = open_counting_with(&path, &pool, Indexes::NamesOnly);
        let (_, built) = listings_and_readers_built(&counted, &path);
        assert_eq!(built.len(), 2, "finalize={finalize}: {built:?}");

        // Segments sealed before names indexes: with a fingerprint each,
        // every segment whose fingerprint is new is fetched, and the names
        // are complete; with none, only each table's first.
        let catalog = DendroCatalog::open(&path).unwrap();
        let id = catalog.sources().unwrap()[0].id;
        let sealed: Vec<usize> = catalog
            .tables(id)
            .unwrap()
            .iter()
            .filter(|t| !t.ends_with("/occupants"))
            .map(|t| catalog.segment_meta(id, t).unwrap().len())
            .filter(|n| *n > 0)
            .collect();
        let (counted, fetched) = open_counting_with(&path, &pool, Indexes::Fingerprints);
        assert_eq!(fetched.get().0, sealed.iter().sum::<usize>());
        let mut names = counted.gauge_names();
        names.sort();
        assert_eq!(names, vec!["dirty", "mem_free", "swap_free"]);
        let (_, fetched) = open_counting_with(&path, &pool, Indexes::None);
        assert_eq!(fetched.get().0, sealed.len());

        // An index that does not decode, or that names a segment format
        // this reader refuses, is not taken: its segment is fetched and
        // probed (here the footers are of a format it reads, so the names
        // stay complete).
        for indexes in [Indexes::Unreadable, Indexes::Newer, Indexes::LaterVersion] {
            let (counted, fetched) = open_counting_with(&path, &pool, indexes);
            assert_eq!(fetched.get().0, sealed.iter().sum::<usize>());
            let mut names = counted.gauge_names();
            names.sort();
            assert_eq!(names, vec!["dirty", "mem_free", "swap_free"]);
        }

        // A later segment without an index, in a table whose first segment
        // has one, is fetched; the first segment holds only `mem_free`.
        let (counted, fetched) = open_counting_with(&path, &pool, Indexes::FirstOnly);
        assert_eq!(fetched.get().0, sealed.iter().map(|n| n - 1).sum::<usize>());
        let mut names = counted.gauge_names();
        names.sort();
        assert_eq!(names, vec!["dirty", "mem_free", "swap_free"]);
        let (counted, fetched) = open_counting_with(&path, &pool, Indexes::FingerprintFirstOnly);
        assert_eq!(fetched.get().0, sealed.iter().sum::<usize>());
        let mut names = counted.gauge_names();
        names.sort();
        assert_eq!(names, vec!["dirty", "mem_free", "swap_free"]);

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
    indexes: Indexes,
}

/// The segment indexes a [`Counting`] catalog gives.
#[derive(Clone, Copy)]
enum Indexes {
    /// As stored: the writer's names indexes.
    Stored,
    /// A names fingerprint (`MNS1`) per segment, each distinct, as an
    /// archive sealed before names indexes would give for segments whose
    /// names all differ.
    Fingerprints,
    /// None, as a `.rez` or an archive sealed before fingerprints.
    None,
    /// A names index (`MNI1`) per segment that does not decode, as one a
    /// later version wrote with a field of another type would.
    Unreadable,
    /// A names index per segment naming a segment format newer than this
    /// reader reads (the footers are not changed).
    Newer,
    /// A names index of a version this reader does not know (`MNI2`).
    LaterVersion,
    /// The stored indexes with every segment's but the first removed, as
    /// dendro's compaction leaves merged segments.
    FirstOnly,
    /// A names fingerprint on the first segment and no index on the rest,
    /// as compaction leaves a table sealed before names indexes.
    FingerprintFirstOnly,
    /// The stored indexes without their columns frame.
    NamesOnly,
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
    fn sources(&self) -> Result<Vec<metriken_storage::catalog::Source>, String> {
        self.inner.sources()
    }
    fn sources_with_uuids(
        &self,
    ) -> Result<Vec<(metriken_storage::catalog::Source, Option<String>)>, String> {
        self.inner.sources_with_uuids()
    }
    fn tables(&self, source_id: i64) -> Result<Vec<String>, String> {
        self.inner.tables(source_id)
    }
    fn segment_meta(
        &self,
        source_id: i64,
        table: &str,
    ) -> Result<Vec<(u64, metriken_storage::catalog::SegmentMeta)>, String> {
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
    ) -> Result<Vec<metriken_storage::catalog::WalRow>, String> {
        self.inner.live_wal(source_id, table)
    }
    fn segment_indexes(
        &self,
        source_id: i64,
        table: &str,
    ) -> Result<Vec<metriken_storage::SegmentIndex>, String> {
        let stored = self.inner.segment_indexes(source_id, table)?;
        Ok(match self.indexes {
            Indexes::Stored => stored,
            Indexes::Fingerprints => stored
                .into_iter()
                .map(|(seq, _)| {
                    let mut index = b"MNS1".to_vec();
                    index.extend_from_slice(&seq.to_le_bytes());
                    (seq, Some(index))
                })
                .collect(),
            Indexes::None => Vec::new(),
            Indexes::LaterVersion => stored
                .into_iter()
                .map(|(seq, _)| (seq, Some(b"MNI2a later index".to_vec())))
                .collect(),
            Indexes::FingerprintFirstOnly => stored
                .into_iter()
                .map(|(seq, _)| (seq, (seq == 0).then(|| b"MNS1\0\0\0\0\0\0\0\0".to_vec())))
                .collect(),
            Indexes::NamesOnly => stored
                .into_iter()
                .map(|(seq, index)| {
                    let index = index.map(|index| {
                        let names = zstd::zstd_safe::find_frame_compressed_size(&index[4..])
                            .expect("a names frame");
                        assert!(4 + names < index.len(), "no columns frame");
                        index[..4 + names].to_vec()
                    });
                    (seq, index)
                })
                .collect(),
            Indexes::FirstOnly => stored
                .into_iter()
                .map(|(seq, index)| (seq, index.filter(|_| seq == 0)))
                .collect(),
            Indexes::Unreadable => stored
                .into_iter()
                .map(|(seq, _)| (seq, Some(b"MNI1not an index".to_vec())))
                .collect(),
            Indexes::Newer => stored
                .into_iter()
                .map(|(seq, _)| {
                    #[derive(serde::Serialize)]
                    struct Names {
                        format: String,
                    }
                    let packed = rmp_serde::to_vec_named(&Names {
                        format: "99".into(),
                    })
                    .unwrap();
                    let mut index = b"MNI1".to_vec();
                    index.extend(zstd::stream::encode_all(packed.as_slice(), 3).unwrap());
                    (seq, Some(index))
                })
                .collect(),
        })
    }
    fn segment_span(
        &self,
        source_id: i64,
        table: &str,
    ) -> Result<(u64, metriken_storage::catalog::Span), String> {
        self.inner.segment_span(source_id, table)
    }
    fn live_wal_span(
        &self,
        source_id: i64,
        table: &str,
    ) -> Result<metriken_storage::catalog::Span, String> {
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
    open_counting_with(path, pool, Indexes::Stored)
}

/// [`open_counting`] with the catalog giving `indexes`.
fn open_counting_with(
    path: &Path,
    pool: &Arc<BufferPool>,
    indexes: Indexes,
) -> (ArchiveReader, Arc<Fetched>) {
    let fetched = Arc::new(Fetched::default());
    let catalog = Counting {
        inner: DendroCatalog::open(path).unwrap(),
        fetched: Arc::clone(&fetched),
        indexes,
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

/// Compacting a long stream with dendro's default merge does not change what
/// it answers, its interval, or its catalog row counts. dendro (0.4.1 and
/// later) carries the segments' key-value metadata into the merged segment,
/// so it stays long; before that it lost `metriken.layout` and
/// `metriken.occupants` and read as a wide table.
#[test]
fn a_compacted_long_stream_answers_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compact-long.dendro");
    record(&path, true, true, None);
    let pool = BufferPool::new(64 * 1024 * 1024);
    let (before, _) = open_counting(&path, &pool);
    let expected = answers(&before, 0);
    let interval = before.interval();
    drop(before);
    let rows = catalog_rows(&path);
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
    let (after, _) = open_counting(&path, &pool);
    assert_eq!(answers(&after, 0), expected);
    assert_eq!(after.interval(), interval);
    assert_eq!(catalog_rows(&path), rows, "catalog rows count WAL rows");
}

/// Each stream's catalog rows, summed over its segments: the WAL rows it
/// covers, which compaction must not change.
fn catalog_rows(path: &Path) -> BTreeMap<String, u64> {
    let db = dendro::archive::Archive::open(path).unwrap();
    let mut out = BTreeMap::new();
    for source in db.read_sources().unwrap() {
        for stream in db.all_streams(source.id).unwrap() {
            let rows = db
                .read_segment_meta(source.id, &stream)
                .unwrap()
                .iter()
                .map(|(_, m)| m.rows)
                .sum();
            out.insert(format!("{}/{stream}", source.id), rows);
        }
    }
    out
}

/// Compacting a long stream with [`OccupantUnion`] merges across the
/// segments' occupant-list changes, which the default merge stops at, and
/// the merged stream answers and lists as before.
///
/// [`OccupantUnion`]: metriken_storage::long::OccupantUnion
#[test]
fn a_long_stream_compacted_by_union_answers_as_before() {
    use metriken_storage::long::OccupantUnion;

    let dir = tempfile::tempdir().unwrap();
    let wide = dir.path().join("wide.dendro");
    record(&wide, false, true, None);
    let wide = open(&wide);
    let pool = BufferPool::new(64 * 1024 * 1024);
    let mut tasks = Vec::new();
    for by_union in [false, true] {
        let path = dir.path().join(format!("compact-{by_union}.dendro"));
        record(&path, true, true, None);
        let (before, _) = open_counting(&path, &pool);
        let expected = answers(&before, 0);
        let interval = before.interval();
        drop(before);
        let rows = catalog_rows(&path);
        let id = DendroCatalog::open(&path).unwrap().sources().unwrap()[0].id;
        let segments = |path: &Path| {
            dendro::archive::Archive::open(path)
                .unwrap()
                .read_segment_meta(id, "threads/tasks")
                .unwrap()
                .len()
        };
        let sealed = segments(&path);
        {
            let tables = streams(&path);
            let mut db = dendro::archive::ArchiveMut::open(&path).unwrap();
            let spec = dendro::rewrite::CompactSpec::to_rows(100);
            for stream in tables {
                if by_union {
                    dendro::rewrite::compact_stream_with(
                        &mut db,
                        id,
                        &stream,
                        &spec,
                        &OccupantUnion,
                    )
                } else {
                    dendro::rewrite::compact_stream(&mut db, id, &stream, &spec)
                }
                .unwrap();
            }
        }
        let (after, _) = open_counting(&path, &pool);
        assert_eq!(answers(&after, 0), expected, "union: {by_union}");
        assert_eq!(after.interval(), interval, "union: {by_union}");
        assert_eq!(catalog_rows(&path), rows, "union: {by_union}");
        listings_match_queries(&after, &wide);
        tasks.push((sealed, segments(&path)));
    }
    eprintln!(
        "threads/tasks segments (sealed, compacted): stop {:?}, union {:?}",
        tasks[0], tasks[1]
    );
    assert_eq!(tasks[0].0, tasks[1].0);
    assert!(
        tasks[1].1 < tasks[0].1,
        "the union merge leaves fewer segments: {tasks:?}"
    );
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

type LabelSets = BTreeSet<Vec<(String, String)>>;

/// The label sets `labels` lists, without `__occupant__` and `__name__`.
fn label_sets(labels: Vec<BTreeMap<String, String>>) -> LabelSets {
    labels
        .into_iter()
        .map(|l| {
            l.into_iter()
                .filter(|(k, _)| k != "__occupant__" && k != "__name__")
                .collect()
        })
        .collect()
}

/// The label sets of the series `query` returns with at least one sample.
fn answered(reader: &ArchiveReader, query: &str) -> LabelSets {
    let (series, _) = answer(reader, query, 0).unwrap();
    series
        .into_iter()
        .filter(|(_, values)| !values.is_empty())
        .map(|(l, _)| l.into_iter().filter(|(k, _)| k != "__name__").collect())
        .collect()
}

/// Each metric of the long table lists the series a query of it returns,
/// and no occupant it has no value for. `threads/tasks` mixes a slotted
/// member set (`cpu_time`, `syscalls`, `latency`) with an unslotted gauge
/// (`threads_total`, the occupant of slot `""`) over five-row segments, so
/// every column is sparse over the segment's occupants. The engine answers
/// a histogram query aggregated, so `latency` is checked against the same
/// recording written wide, where each slot is its own column.
fn listings_match_queries(reader: &ArchiveReader, wide: &ArchiveReader) {
    let total = label_sets(reader.gauge_labels("threads_total"));
    assert_eq!(total, answered(reader, "threads_total"));
    assert_eq!(total.len(), 1, "one unslotted member: {total:?}");
    for (metric, query) in [
        ("cpu_time", "irate(cpu_time[3s])"),
        ("syscalls", "irate(syscalls[3s])"),
    ] {
        let listed = label_sets(reader.counter_labels(metric));
        assert_eq!(listed, answered(reader, query), "{metric}");
        assert!(
            listed.iter().all(|l| l.iter().any(|(k, _)| k == "comm")),
            "{metric}: a series without a slot's labels: {listed:?}"
        );
    }
    // `syscalls` is one column per op, each with every slot: 4 occupants
    // held the three slots, times two ops.
    assert_eq!(label_sets(reader.counter_labels("syscalls")).len(), 8);
    let latency = label_sets(reader.histogram_labels("latency"));
    assert_eq!(latency, label_sets(wide.histogram_labels("latency")));
    assert_eq!(latency.len(), 4);
}

/// A long table lists only the occupants each column has values for:
/// sealed, from the live tail, and after a copy trimmed to some metrics.
#[test]
fn a_long_table_lists_only_the_series_it_has() {
    use dendro::archive::{Archive, ArchiveMut};
    use dendro::rewrite::{copy_sources_into, CopySpec};
    use metriken_storage::writer::Encoder;
    use metriken_storage::{default_compression, segment_props, KeepMetrics};

    let dir = tempfile::tempdir().unwrap();
    let wide = dir.path().join("wide.dendro");
    record(&wide, false, true, None);
    let wide = open(&wide);
    let sealed = dir.path().join("sealed.dendro");
    record(&sealed, true, true, None);
    listings_match_queries(&open(&sealed), &wide);

    let live = dir.path().join("live.dendro");
    record(&live, true, false, None);
    listings_match_queries(&open(&live), &wide);

    // A projection keeps every row, so each kept column's list holds.
    let src = Archive::open(&sealed).unwrap();
    let metrics: BTreeSet<String> = ["threads_total", "latency"]
        .iter()
        .map(|m| m.to_string())
        .collect();
    let keep = KeepMetrics::new(&metrics);
    let spec = CopySpec {
        keep_columns: Some(&keep),
        writer_props: Some(segment_props(default_compression())),
        ..CopySpec::everything()
    };
    let encoder = Encoder::for_archive(&src).unwrap();
    let trimmed = dir.path().join("trimmed.dendro");
    let mut dst = ArchiveMut::create(&trimmed).unwrap();
    dst.transaction(|tx| copy_sources_into(&src, tx, &spec, &encoder))
        .unwrap();
    drop(dst);
    let reader = open(&trimmed);
    let total = label_sets(reader.gauge_labels("threads_total"));
    assert_eq!(total, answered(&reader, "threads_total"));
    assert_eq!(total.len(), 1);
    let latency = label_sets(reader.histogram_labels("latency"));
    assert_eq!(latency, label_sets(wide.histogram_labels("latency")));
}

/// Record `ticks` snapshots of `snap`, committing `backlog` passes at a time
/// and checking the seal after each commit, as a recorder does when frames
/// queued behind a slow seal. `max_rows` is the seal policy's.
fn record_in_backlogs(
    path: &Path,
    long_groups: bool,
    max_rows: usize,
    backlog: u64,
    ticks: u64,
    snap: &dyn Fn(u64) -> Snapshot,
) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows,
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
    let mut t = 0;
    while t < ticks {
        let staged = (t..ticks.min(t + backlog))
            .map(|t| source.stage(&snap(t), BASE + t * S, 0).unwrap())
            .collect();
        writer.commit(staged).unwrap();
        source.maybe_seal().unwrap();
        t += backlog;
    }
    source.finalize((BASE + (ticks - 1) * S, 0)).unwrap();
    writer.join().unwrap();
}

/// Each stream's segment row counts, in `seq` order.
fn segment_rows(path: &Path) -> BTreeMap<String, Vec<u64>> {
    let db = dendro::archive::Archive::open(path).unwrap();
    let mut out = BTreeMap::new();
    for source in db.read_sources().unwrap() {
        for stream in db.all_streams(source.id).unwrap() {
            let rows = db
                .read_segment_meta(source.id, &stream)
                .unwrap()
                .iter()
                .map(|(_, m)| m.rows)
                .collect();
            out.insert(stream, rows);
        }
    }
    out
}

/// Committing three passes per seal check at a cap of two rows writes no
/// segment past the cap, and the archive answers as one committed a pass
/// at a time. A part of a split seal starts after the row that carried
/// the group's schema (or the long table's columns); without the encoder
/// anchoring it from the rows before, its rows would be skipped.
fn backlog_seals_within_the_cap(long_groups: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (split, single) = (
        dir.path().join("split.dendro"),
        dir.path().join("single.dendro"),
    );
    record_in_backlogs(&split, long_groups, 2, 3, TICKS, &snapshot);
    record_in_backlogs(&single, long_groups, 2, 1, TICKS, &snapshot);
    let rows = segment_rows(&split);
    for (stream, segments) in &rows {
        assert!(
            segments.iter().all(|r| *r <= 2),
            "{stream}: a segment past the cap: {segments:?}"
        );
    }
    let group = rows["threads/tasks"].clone();
    assert_eq!(
        group.iter().sum::<u64>(),
        TICKS - 1,
        "tick 3 has no members"
    );
    assert!(
        group.windows(2).any(|w| w == [2, 1]),
        "a three-pass backlog was split: {group:?}"
    );
    let (split, single) = (open(&split), open(&single));
    for q in QUERIES {
        let (a, b) = (answer(&split, q, 0), answer(&single, q, 0));
        assert!(a.as_ref().is_ok_and(|(a, _)| !a.is_empty()), "{q}: {a:?}");
        assert_eq!(a, b, "{q}");
    }
}

#[test]
fn a_wide_backlog_seals_within_the_row_cap() {
    backlog_seals_within_the_cap(false);
}

#[test]
fn a_long_backlog_seals_within_the_row_cap() {
    backlog_seals_within_the_cap(true);
}

/// [`backlog_seals_within_the_cap`] for a V2 sampler table, whose cells
/// carry their metadata only the first time a metric appears in a segment:
/// a later part of a split seal must still read back with its labels.
#[test]
fn a_v2_backlog_seals_within_the_row_cap_with_its_labels() {
    use metriken_exposition::{Counter, Gauge, SnapshotV2};

    let meta = |metric: &str, op: &str| -> HashMap<String, String> {
        [("metric", metric), ("sampler", "fake"), ("op", op)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let v2 = |t: u64| {
        let ts = BASE + t * S;
        let w = Some(metriken::Window::new(ts - S / 2, ts));
        Snapshot::V2(SnapshotV2 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
            duration: Duration::ZERO,
            metadata: HashMap::new(),
            counters: vec![
                Counter::new("0".into(), t * 10, meta("ops", "read")).with_window(w),
                Counter::new("1".into(), t * 20, meta("ops", "write")).with_window(w),
            ],
            gauges: vec![Gauge::new("2".into(), t as i64, meta("free", "none")).with_window(w)],
            histograms: Vec::new(),
            gauge_histograms: Vec::new(),
        })
    };
    let dir = tempfile::tempdir().unwrap();
    let (split, single) = (
        dir.path().join("split.dendro"),
        dir.path().join("single.dendro"),
    );
    record_in_backlogs(&split, false, 2, 3, 12, &v2);
    record_in_backlogs(&single, false, 2, 1, 12, &v2);
    let rows = segment_rows(&split);
    assert!(rows["fake"].iter().all(|r| *r <= 2), "{rows:?}");
    assert!(rows["fake"].windows(2).any(|w| w == [2, 1]), "{rows:?}");
    let (split, single) = (open(&split), open(&single));
    for q in ["sum by (op) (rate(ops[2s]))", "free"] {
        let (a, b) = (answer(&split, q, 0), answer(&single, q, 0));
        assert_eq!(a, b, "{q}");
    }
    let by_op = answer(&split, "sum by (op) (rate(ops[2s]))", 0).unwrap().0;
    let ops: Vec<_> = by_op.iter().map(|(l, _)| l.clone()).collect();
    assert_eq!(
        ops,
        [
            vec![("op".to_string(), "read".to_string())],
            vec![("op".to_string(), "write".to_string())]
        ]
    );
}

/// A group named `flip/g` whose members at tick `t` are `members(t)`, each
/// a gauge of its own metric with a value unique to the member and tick.
fn members_snapshot(t: u64, members: &[&str]) -> Snapshot {
    let ts = BASE + t * S;
    let mut schema = GroupSchema::default();
    let mut gauges = Vec::new();
    for (i, m) in members.iter().enumerate() {
        // Named by the metric, so a member keeps its column whichever
        // members are present beside it.
        schema.gauges.push(MetricDesc {
            name: m.to_string(),
            metadata: [("metric", *m), ("sampler", "flip")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        });
        gauges.push(Some((t * 1000 + i as u64) as i64));
    }
    Snapshot::V3(SnapshotV3 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::from_millis(1),
        metadata: HashMap::new(),
        groups: vec![GroupSnapshot {
            name: "flip/g".to_string(),
            schema_hash: schema.hash(),
            schema: Some(Arc::new(schema)),
            window: Some(metriken::Window::new(ts - S / 2, ts)),
            counters: Vec::new(),
            gauges,
            histograms: Vec::new(),
            gauge_histograms: Vec::new(),
        }],
    })
}

/// How many distinct values `metric` reads back over `ticks` ticks: one
/// per tick the member was present, since every value is unique.
fn samples(reader: &ArchiveReader, metric: &str, ticks: u64) -> usize {
    let (start, end) = (BASE as f64 / 1e9, (BASE + (ticks - 1) * S) as f64 / 1e9);
    let QueryResult::Matrix { result } = reader.query_range(metric, start, end, 1.0).unwrap()
    else {
        panic!("{metric}: not a matrix");
    };
    let values: BTreeSet<String> = result
        .iter()
        .flat_map(|m| m.values.iter().map(|(_, v)| v.to_string()))
        .collect();
    values.len()
}

/// A group whose schema returns to an earlier one within a segment keeps
/// every sample, sealed whole or split at a cap of 2, and answers as one
/// pass per commit does. The writer used to state a schema once per hash
/// per segment, and the WAL decoder resolves a row against the last schema
/// stated, so the second run of rows under an earlier schema was skipped.
fn flip_back_keeps_every_sample(members: fn(u64) -> Vec<&'static str>) {
    const TICKS: u64 = 12;
    let snap = move |t: u64| members_snapshot(t, &members(t));
    let metrics: BTreeSet<&str> = (0..TICKS).flat_map(members).collect();
    let dir = tempfile::tempdir().unwrap();
    let reference = dir.path().join("reference.dendro");
    record_in_backlogs(&reference, false, 2, 1, TICKS, &snap);
    let reference = open(&reference);
    for (cap, backlog) in [(100, 1), (100, 6), (2, 6), (2, 3)] {
        let path = dir.path().join(format!("{cap}-{backlog}.dendro"));
        record_in_backlogs(&path, false, cap, backlog, TICKS, &snap);
        let rows = &segment_rows(&path)["flip/g"];
        assert_eq!(rows.iter().sum::<u64>(), TICKS, "{cap}/{backlog}: {rows:?}");
        assert!(
            rows.iter().all(|r| *r as usize <= cap),
            "{cap}/{backlog}: a segment past the cap: {rows:?}"
        );
        let reader = open(&path);
        for m in &metrics {
            let present = (0..TICKS).filter(|t| members(*t).contains(m)).count();
            assert_eq!(
                samples(&reader, m, TICKS),
                present,
                "{cap}/{backlog}: {m} lost samples ({rows:?})"
            );
            assert_eq!(
                answer(&reader, m, 0),
                answer(&reference, m, 0),
                "{cap}/{backlog}: {m}"
            );
        }
    }
}

#[test]
fn a_schema_that_alternates_keeps_every_sample() {
    flip_back_keeps_every_sample(|t| {
        if t % 2 == 0 {
            vec!["x", "y"]
        } else {
            vec!["x"]
        }
    });
}

#[test]
fn a_schema_that_returns_after_two_ticks_keeps_every_sample() {
    flip_back_keeps_every_sample(|t| {
        if (t / 2) % 2 == 0 {
            vec!["x", "y"]
        } else {
            vec!["x"]
        }
    });
}

/// A scrape-shaped group: series that come and go between scrapes, so the
/// group's schema changes most ticks and revisits earlier member sets.
#[test]
fn intermittent_series_keep_every_sample() {
    flip_back_keeps_every_sample(|t| {
        let mut m = vec!["up"];
        if t % 3 != 1 {
            m.push("requests");
        }
        if t % 4 == 0 {
            m.push("errors");
        }
        m
    });
}

/// Record `ticks` snapshots of `snap` one pass per commit with segments
/// that never fill (`max_rows` 1000), evicting everything before tick
/// `cutoff` after tick `at`, as hindsight with a retention window shorter
/// than the seal policy's `max_age` does. `None` records without eviction.
fn record_evicting(
    path: &Path,
    long_groups: bool,
    evict: Option<(u64, u64)>,
    finalize: bool,
    ticks: u64,
    snap: &dyn Fn(u64) -> Snapshot,
) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 1000,
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
    for t in 0..ticks {
        let staged = source.stage(&snap(t), BASE + t * S, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
        source.maybe_seal().unwrap();
        if let Some((at, cutoff)) = evict {
            if t == at {
                source.evict_before(BASE + cutoff * S).unwrap();
            }
        }
    }
    if finalize {
        source.finalize((BASE + (ticks - 1) * S, 0)).unwrap();
    } else {
        source.sync().unwrap();
        std::mem::forget(source);
    }
    writer.join().unwrap();
}

/// Values `metric` reads back from tick `from` to tick `ticks - 1`.
fn samples_from(reader: &ArchiveReader, metric: &str, from: u64, ticks: u64) -> usize {
    let (start, end) = (
        (BASE + from * S) as f64 / 1e9,
        (BASE + (ticks - 1) * S) as f64 / 1e9,
    );
    let QueryResult::Matrix { result } = reader.query_range(metric, start, end, 1.0).unwrap()
    else {
        panic!("{metric}: not a matrix");
    };
    let values: BTreeSet<String> = result
        .iter()
        .flat_map(|m| m.values.iter().map(|(_, v)| v.to_string()))
        .collect();
    values.len()
}

/// Retention that cuts into an open segment, below the row carrying the
/// group's schema, keeps every row at or after the cutoff: read live and
/// after finalize. The cutoff used to delete the anchor, and the open
/// segment's later rows were skipped as unanchored when read or sealed.
fn eviction_keeps_rows_after_the_cutoff(finalize: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evicted.dendro");
    let snap = |t: u64| members_snapshot(t, &["x", "y"]);
    record_evicting(&path, false, Some((10, 5)), finalize, 20, &snap);
    let reader = open(&path);
    for m in ["x", "y"] {
        assert_eq!(samples_from(&reader, m, 5, 20), 15, "{m}");
    }
}

#[test]
fn eviction_keeps_rows_after_the_cutoff_live() {
    eviction_keeps_rows_after_the_cutoff(false);
}

#[test]
fn eviction_keeps_rows_after_the_cutoff_finalized() {
    eviction_keeps_rows_after_the_cutoff(true);
}

/// The same for long and wide tables of the slotted fixture, and a V2
/// sampler table whose cells carry their labels on first mention: after
/// an eviction into the open segment, every query answers from tick 8 as
/// a recording that evicted nothing.
#[test]
fn eviction_into_an_open_segment_answers_as_without_it() {
    let dir = tempfile::tempdir().unwrap();
    for long in [false, true] {
        let (evicted, kept) = (
            dir.path().join(format!("evicted-{long}.dendro")),
            dir.path().join(format!("kept-{long}.dendro")),
        );
        record_evicting(&evicted, long, Some((10, 5)), true, TICKS, &snapshot);
        record_evicting(&kept, long, None, true, TICKS, &snapshot);
        let (evicted, kept) = (open(&evicted), open(&kept));
        for q in QUERIES {
            let (a, b) = (answer(&evicted, q, 8), answer(&kept, q, 8));
            assert!(
                a.as_ref().is_ok_and(|(a, _)| !a.is_empty()),
                "{long} {q}: {a:?}"
            );
            assert_eq!(a, b, "long={long} {q}");
        }
    }

    use metriken_exposition::{Counter, SnapshotV2};
    let v2 = |t: u64| {
        let ts = BASE + t * S;
        let meta = |op: &str| -> HashMap<String, String> {
            [("metric", "ops"), ("sampler", "fake"), ("op", op)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        Snapshot::V2(SnapshotV2 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
            duration: Duration::ZERO,
            metadata: HashMap::new(),
            counters: vec![
                Counter::new("0".into(), t * 10, meta("read")),
                Counter::new("1".into(), t * 20, meta("write")),
            ],
            gauges: Vec::new(),
            histograms: Vec::new(),
            gauge_histograms: Vec::new(),
        })
    };
    let (evicted, kept) = (
        dir.path().join("evicted-v2.dendro"),
        dir.path().join("kept-v2.dendro"),
    );
    record_evicting(&evicted, false, Some((10, 5)), true, 20, &v2);
    record_evicting(&kept, false, None, true, 20, &v2);
    let q = "sum by (op) (rate(ops[2s]))";
    let a = answer(&open(&evicted), q, 8);
    assert_eq!(a.as_ref().map(|a| a.0.len()).ok(), Some(2), "{a:?}");
    assert_eq!(a, answer(&open(&kept), q, 8));
}

/// A source's name and the snapshot it records at each tick.
type Named = (&'static str, fn(u64) -> Snapshot);

/// `snapshot(t)` with `threads/tasks` reduced to its unslotted gauge, which
/// a writer records wide.
fn unslotted(t: u64) -> Snapshot {
    let Snapshot::V3(mut s) = snapshot(t) else {
        unreachable!()
    };
    for g in &mut s.groups {
        if g.name == "threads/tasks" {
            let schema = GroupSchema {
                counters: Vec::new(),
                gauges: vec![MetricDesc {
                    name: "8".to_string(),
                    metadata: [("metric", "threads_total"), ("sampler", "threads")]
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                }],
                histograms: Vec::new(),
                gauge_histograms: Vec::new(),
            };
            g.schema_hash = schema.hash();
            g.schema = Some(Arc::new(schema));
            g.counters = Vec::new();
            g.histograms = Vec::new();
            g.gauges = vec![Some(7_000 + t as i64)];
        }
    }
    Snapshot::V3(s)
}

/// `snapshot(t)` with every `comm` prefixed `b-`, so labels that cross
/// from one source to another show.
fn renamed(t: u64) -> Snapshot {
    let Snapshot::V3(mut s) = snapshot(t) else {
        unreachable!()
    };
    for g in &mut s.groups {
        if let Some(schema) = &g.schema {
            let mut schema = (**schema).clone();
            for d in schema
                .counters
                .iter_mut()
                .chain(&mut schema.gauges)
                .chain(&mut schema.histograms)
            {
                if let Some(c) = d.metadata.get_mut("comm") {
                    *c = format!("b-{c}");
                }
            }
            g.schema_hash = schema.hash();
            g.schema = Some(Arc::new(schema));
        }
    }
    Snapshot::V3(s)
}

/// Record one source per entry of `snapshots`, labelled `source=<name>`,
/// every tick committed together, as [`record`] records one. With
/// `seal_each_tick` each source seals when its policy says so after every
/// tick; without it nothing seals before `finalize`, so a stream's rows
/// are sealed then as several capped parts, each after the first through
/// `encode_after`.
fn record_sources(path: &Path, snapshots: &[Named], finalize: bool, seal_each_tick: bool) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        restate_every_ns: 6 * S,
        long_groups: true,
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    let mut sources: Vec<_> = snapshots
        .iter()
        .map(|(name, _)| {
            let labels = [("source".to_string(), name.to_string())]
                .into_iter()
                .collect();
            writer.add_source(labels, BTreeMap::new(), BASE).unwrap()
        })
        .collect();
    for t in 0..TICKS {
        let staged = sources
            .iter_mut()
            .zip(snapshots)
            .map(|(source, (_, f))| source.stage(&f(t), BASE + t * S, 0).unwrap())
            .collect();
        writer.commit(staged).unwrap();
        for source in &mut sources {
            if seal_each_tick {
                source.maybe_seal().unwrap();
            }
            source.sync().unwrap();
        }
    }
    for mut source in sources {
        if finalize {
            source.finalize((BASE + (TICKS - 1) * S, 0)).unwrap();
        } else {
            source.sync().unwrap();
            std::mem::forget(source);
        }
    }
    writer.join().unwrap();
}

/// Each source of the archive at `path`, by its `source` label.
fn open_sources(path: &Path) -> BTreeMap<String, ArchiveReader> {
    let catalog = DendroCatalog::open(path).unwrap();
    ArchiveReader::from_catalog(
        Box::new(catalog),
        None,
        BufferPool::new(64 * 1024 * 1024),
        None,
    )
    .unwrap()
    .into_iter()
    .map(|(labels, reader)| {
        let source = labels
            .into_iter()
            .find(|(k, _)| k == "source")
            .map(|(_, v)| v)
            .unwrap();
        (source, reader)
    })
    .collect()
}

/// Every source of `together` answers each query as the same snapshots
/// recorded alone do.
fn assert_sources_answer_as_alone(
    together: &Path,
    snapshots: &[Named],
    finalize: bool,
    seal_each_tick: bool,
) {
    let readers = open_sources(together);
    assert_eq!(readers.len(), snapshots.len());
    for (name, f) in snapshots {
        let dir = tempfile::tempdir().unwrap();
        let alone = dir.path().join("alone.dendro");
        record_sources(&alone, &[(*name, *f)], finalize, seal_each_tick);
        let alone = open_sources(&alone).remove(*name).unwrap();
        let reader = &readers[*name];
        for q in QUERIES {
            let (x, y) = (answer(reader, q, 0), answer(&alone, q, 0));
            assert_eq!(
                x.as_ref().map(|a| &a.0),
                y.as_ref().map(|a| &a.0),
                "source {name}, finalize {finalize}, seal_each_tick {seal_each_tick}: {q}"
            );
        }
    }
}

/// Two sources recording a group of the same name, one long (slotted
/// members) and one wide (no slots), each seal and read back as when
/// recorded alone. Whether a stream is long is each source's decision: a
/// set keyed by stream name alone sealed the wide source's rows as long,
/// and its `sync` failed to decode them.
#[test]
fn a_stream_long_in_one_source_and_wide_in_another_seals_both() {
    let snapshots: [Named; 2] = [("a", snapshot), ("b", unslotted)];
    for (finalize, seal_each_tick) in [(true, true), (false, true), (true, false)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.dendro");
        record_sources(&path, &snapshots, finalize, seal_each_tick);
        let catalog = DendroCatalog::open(&path).unwrap();
        let tables: BTreeMap<String, Vec<String>> = catalog
            .sources()
            .unwrap()
            .into_iter()
            .map(|s| {
                let name = s.labels["source"].clone();
                (name, catalog.tables(s.id).unwrap())
            })
            .collect();
        assert!(tables["a"].contains(&"threads/tasks/occupants".to_string()));
        assert!(!tables["b"].iter().any(|t| t.ends_with("/occupants")));
        assert_sources_answer_as_alone(&path, &snapshots, finalize, seal_each_tick);
    }
}

/// Two sources recording the same long group keep their occupants'
/// labels apart.
#[test]
fn two_sources_of_one_long_stream_keep_their_labels_apart() {
    let snapshots: [Named; 2] = [("a", snapshot), ("b", renamed)];
    for finalize in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twins.dendro");
        record_sources(&path, &snapshots, finalize, true);
        assert_sources_answer_as_alone(&path, &snapshots, finalize, true);
    }
}

/// A copy of a live archive whose sources record one stream long and wide
/// encodes each source's unsealed rows as that source wrote them, with the
/// encoder `Encoder::for_archive` builds from the archive's per-source
/// stream lists.
#[test]
fn a_copy_encodes_each_sources_tail_as_that_source_wrote_it() {
    use dendro::archive::{Archive, ArchiveMut};
    use dendro::rewrite::{copy_sources_into, CopySpec};
    use metriken_storage::writer::Encoder;

    let snapshots: [Named; 2] = [("a", snapshot), ("b", unslotted)];
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("live.dendro");
    record_sources(&src, &snapshots, false, true);

    let archive = Archive::open(&src).unwrap();
    let encoder = Encoder::for_archive(&archive).unwrap();
    let dst = dir.path().join("copy.dendro");
    let mut out = ArchiveMut::create(&dst).unwrap();
    out.transaction(|tx| copy_sources_into(&archive, tx, &CopySpec::everything(), &encoder))
        .unwrap();
    drop(out);

    let (a, b) = (open_sources(&src), open_sources(&dst));
    assert_eq!(b.len(), 2);
    for (name, reader) in &a {
        for q in QUERIES {
            let (x, y) = (answer(reader, q, 0), answer(&b[name], q, 0));
            assert_eq!(
                x.as_ref().map(|r| &r.0),
                y.as_ref().map(|r| &r.0),
                "{name}: {q}"
            );
        }
    }
}

/// A copy of a live archive builds its encoder in the snapshot the copy
/// reads. Built outside it, an encoder misses a long source that appears
/// before the copy takes its own snapshot, encodes that source's unsealed
/// rows as wide, and the copy fails; built inside it, the copy holds what
/// the snapshot saw and succeeds.
#[test]
fn a_live_copy_builds_its_encoder_in_the_copys_snapshot() {
    use dendro::archive::{Archive, ArchiveMut};
    use dendro::rewrite::{copy_sources_into, CopySpec};
    use metriken_storage::writer::Encoder;

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("live.dendro");
    let mut writer = ArchiveWriter::create(
        &src,
        WriterConfig {
            long_groups: true,
            ..WriterConfig::default()
        },
    )
    .unwrap();
    let labels = |v: &str| {
        [("source".to_string(), v.to_string())]
            .into_iter()
            .collect()
    };
    let mut ticks = 0..TICKS;
    let mut record = |writer: &mut ArchiveWriter, source: &mut SourceRecorder, n: usize| {
        for t in ticks.by_ref().take(n) {
            let staged = source.stage(&snapshot(t), BASE + t * S, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            source.sync().unwrap();
        }
    };
    let mut a = writer
        .add_source(labels("a"), BTreeMap::new(), BASE)
        .unwrap();
    record(&mut writer, &mut a, 4);
    let archive = Archive::open(&src).unwrap();

    // Outside: `b` arrives after the encoder was built.
    let encoder = Encoder::for_archive(&archive).unwrap();
    let mut b = writer
        .add_source(labels("b"), BTreeMap::new(), BASE)
        .unwrap();
    record(&mut writer, &mut b, 4);
    let mut out = ArchiveMut::create(&dir.path().join("outside.dendro")).unwrap();
    let outside =
        out.transaction(|tx| copy_sources_into(&archive, tx, &CopySpec::everything(), &encoder));
    assert!(
        matches!(&outside, Err(dendro::Error::Encoder { stream, .. }) if stream == "threads/tasks"),
        "the hazard did not occur: {outside:?}"
    );
    drop(out);

    // Inside: `c` arrives after the snapshot, so neither the encoder nor
    // the copy sees it.
    let inside = dir.path().join("inside.dendro");
    let mut out = ArchiveMut::create(&inside).unwrap();
    let mut c = None;
    archive
        .read_snapshot(|archive| {
            let encoder = Encoder::for_archive(archive)?;
            let mut source = writer
                .add_source(labels("c"), BTreeMap::new(), BASE)
                .unwrap();
            record(&mut writer, &mut source, 4);
            c = Some(source);
            out.transaction(|tx| copy_sources_into(archive, tx, &CopySpec::everything(), &encoder))
        })
        .unwrap();
    drop(out);
    let copied = open_sources(&inside);
    assert_eq!(
        copied.keys().map(String::as_str).collect::<Vec<_>>(),
        ["a", "b"]
    );
    // Left unfinalized so the archive keeps its live tails.
    for source in [a, b, c.unwrap()] {
        std::mem::forget(source);
    }
    writer.join().unwrap();
}
