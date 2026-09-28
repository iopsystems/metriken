//! The writer's oracle: the same snapshots written long and forced wide
//! read back the same through `ArchiveReader`, apart from `__occupant__`.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use dendro::seal::SealPolicy;
use metriken_archive::{ArchiveReader, ArchiveWriter, DendroCatalog, WriterConfig};
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
    use metriken_archive::Catalog;
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
            assert!(w.is_err(), "{q}: an evicted occupant still answers");
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
