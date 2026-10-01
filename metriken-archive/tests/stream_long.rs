//! A slot group recorded over the long stream answers the same as one
//! recorded from wide snapshots: `GroupBuilder::build_stream`,
//! `FrameProducer`, `StreamDecoder` and `SourceRecorder::stage_streamed`,
//! against `build_groups` and `SourceRecorder::stage`, over the same ticks.
#![cfg(all(feature = "stream", feature = "write"))]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use dendro::replicate::Frame;
use dendro::seal::SealPolicy;
use metriken::group::SlotIdentity;
use metriken::{metric, MetricEntry, Window};
use metriken_archive::stream::{EncodedStreamGroup, FrameProducer, SchemaCache};
use metriken_archive::{
    ArchiveReader, ArchiveWriter, DendroCatalog, SourceRecorder, StreamDecoder, WriterConfig,
};
use metriken_exposition::group_builder::{
    Acquisition, GroupBuilder, GroupId, Membership, NoGuard, Route, Router, Stamp,
};
use metriken_exposition::Snapshot;
use metriken_query::{BufferPool, MetricsSource, QueryResult};

#[metric(name = "long_tasks_cpu", metadata = { acq_group = "tasks" })]
static TASK_CPU: metriken::CounterGroup = metriken::CounterGroup::new(16);
#[metric(name = "long_tasks_ctx", metadata = { acq_group = "tasks" })]
static TASK_CTX: metriken::CounterGroup = metriken::CounterGroup::new(16);
#[metric(name = "long_tasks_rss", metadata = { acq_group = "tasks" })]
static TASK_RSS: metriken::GaugeGroup = metriken::GaugeGroup::new(16);
static TASKS: SlotIdentity = SlotIdentity::new(&[&TASK_CPU, &TASK_CTX, &TASK_RSS]);

#[metric(name = "long_cpus_busy", metadata = { acq_group = "cpus" })]
static CPU_BUSY: metriken::CounterGroup = metriken::CounterGroup::new(4);

#[metric(name = "long_main_requests", metadata = { acq_group = "main" })]
static REQUESTS: metriken::Counter = metriken::Counter::new();
#[metric(name = "long_main_queues", metadata = { acq_group = "main" })]
static QUEUES: metriken::CounterGroup = metriken::CounterGroup::new(2);

/// `tasks` by slot metadata with a stamped window, `cpus` every slot,
/// `main` (a counter beside a counter group) by value.
struct TestRouter {
    window: Mutex<Option<Window>>,
}

impl Router for TestRouter {
    type Guard = NoGuard;

    fn route<'a>(&'a self, metric: &'a MetricEntry) -> Option<Route<'a>> {
        let group = metric.metadata().get("acq_group")?;
        let membership = match group {
            "tasks" => Membership::Slots,
            "cpus" => Membership::All,
            _ => Membership::Present,
        };
        Some(Route {
            group: GroupId::new("t", group),
            membership,
        })
    }

    fn acquire(&self, group: GroupId<'_>) -> Acquisition<NoGuard> {
        if group.name == "tasks" {
            Acquisition::Stamped(*self.window.lock().unwrap())
        } else {
            Acquisition::Windowless
        }
    }

    fn window(&self, group: GroupId<'_>) -> Option<Window> {
        (group.name == "tasks")
            .then(|| *self.window.lock().unwrap())
            .flatten()
    }
}

const S: u64 = 1_000_000_000;
const TICKS: u64 = 30;
/// Where the long recording's subscription is replaced, as a reconnect.
const RECONNECT: u64 = 15;

fn base() -> u64 {
    metriken::epoch::clock_anchor_wall_ns() as u64 + 10 * S
}

fn writer(path: &Path) -> (ArchiveWriter, SourceRecorder) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        restate_every_ns: 6 * S,
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    let source = writer
        .add_source(
            [("source".to_string(), "t".to_string())].into(),
            BTreeMap::new(),
            base(),
        )
        .unwrap();
    (writer, source)
}

fn labels(comm: String) -> BTreeMap<String, String> {
    [("comm".to_string(), comm)].into()
}

/// Per tick: every value moves, and one slot changes hands (twice in three
/// ticks to an occupant with the labels its predecessor had).
fn advance(tick: u64) {
    if tick == 0 {
        for slot in 0..6 {
            TASKS.assign(slot, labels(format!("c{slot}")));
        }
    } else {
        let slot = (tick % 8) as usize;
        TASKS.release(slot);
        TASKS.assign(slot, labels(format!("c{}", tick % 3)));
    }
    for slot in 0..8 {
        if TASKS.uid(slot).is_some() {
            TASK_CPU.add(slot, 10 + slot as u64);
            TASK_CTX.add(slot, 1);
            TASK_RSS.set(slot, (tick * 100 + slot as u64) as i64);
        }
    }
    for cpu in 0..4 {
        CPU_BUSY.add(cpu, cpu as u64 + 1);
    }
    REQUESTS.add(7);
    QUEUES.add(1, 3);
}

/// Records both archives from the same ticks; returns, per tick, how many
/// occupants the long stream described.
fn record(wide: &Path, long: &Path, finalize: bool) -> Vec<usize> {
    let router = || TestRouter {
        window: Mutex::new(None),
    };
    let (mut wide_builder, mut long_builder) =
        (GroupBuilder::new(router()), GroupBuilder::new(router()));
    let (mut wide_writer, mut wide_source) = writer(wide);
    let (mut long_writer, mut long_source) = writer(long);
    let mut schemas = SchemaCache::new();
    let subscribe = || FrameProducer::new(BTreeMap::new(), BTreeMap::new());
    let (mut producer, mut decoder) = (subscribe(), StreamDecoder::new());
    let mut described = Vec::new();

    for tick in 0..TICKS {
        advance(tick);
        let ts = base() + tick * S;
        let window = Some(Window::new(ts - S / 2, ts));
        for b in [&wide_builder, &long_builder] {
            *b.router().window.lock().unwrap() = window;
        }
        let stamp = Stamp {
            ts: ts as i64,
            wall_offset: 0,
        };

        let snapshot = Snapshot::V3(wide_builder.snapshot(
            stamp,
            std::time::Duration::ZERO,
            Vec::new(),
            Default::default(),
        ));
        let staged = wide_source.stage(&snapshot, ts, 0).unwrap();
        wide_writer.commit(vec![staged]).unwrap();
        wide_source.maybe_seal().unwrap();

        if tick == RECONNECT {
            (producer, decoder) = (subscribe(), StreamDecoder::new());
        }
        let groups: Vec<EncodedStreamGroup> = long_builder
            .build_stream(Vec::new())
            .iter()
            .map(|g| EncodedStreamGroup::encode(g, &mut schemas).unwrap())
            .collect();
        let Frame::Rows { rows, .. } = producer.interval(&groups, stamp.ts, 0, tick) else {
            panic!("a rows frame");
        };
        described.push(
            rows.iter()
                .filter(|r| r.stream == "t/tasks/occupants")
                .map(|r| {
                    metriken_segment::occupants::decode_wal_row(&r.row)
                        .unwrap()
                        .len()
                })
                .sum(),
        );
        let streamed = decoder.decode(rows).unwrap();
        let staged = long_source.stage_streamed(streamed, ts, 0).unwrap();
        long_writer.commit(vec![staged]).unwrap();
        long_source.maybe_seal().unwrap();
    }
    assert_eq!(decoder.unresolved, 0);

    let last = (base() + (TICKS - 1) * S, 0);
    for (mut w, s) in [(wide_writer, wide_source), (long_writer, long_source)] {
        if finalize {
            s.finalize(last).unwrap();
        } else {
            let mut s = s;
            s.sync().unwrap();
            std::mem::forget(s);
        }
        w.join().unwrap();
    }
    described
}

fn open(path: &Path) -> ArchiveReader {
    let mut recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(path).unwrap()),
        None,
        BufferPool::new(64 * 1024 * 1024),
        None,
    )
    .unwrap();
    assert_eq!(recordings.len(), 1);
    recordings.remove(0).1
}

type Answer = Vec<(Vec<(String, String)>, Vec<(i64, String)>)>;

/// Sorted `(labels, values)` without `__occupant__`, whose numbers the two
/// paths assign independently.
fn answer(reader: &ArchiveReader, query: &str) -> Answer {
    let (start, end) = (base() as f64 / 1e9, (base() + (TICKS - 1) * S) as f64 / 1e9);
    let QueryResult::Matrix { result } = reader
        .query_range(query, start, end, 1.0)
        .unwrap_or_else(|e| panic!("{query}: {e:?}"))
    else {
        panic!("{query}: not a matrix");
    };
    let mut out: Answer = result
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
    out
}

const QUERIES: &[&str] = &[
    "rate(long_tasks_cpu[3s])",
    "irate(long_tasks_ctx[3s])",
    "sum by (comm) (rate(long_tasks_cpu[3s]))",
    "long_tasks_rss",
    "count by (comm) (long_tasks_rss)",
    "rate(long_cpus_busy[3s])",
    "rate(long_main_requests[3s])",
    "rate(long_main_queues[3s])",
];

/// One test, since the two recordings share the registry's statics.
#[test]
fn the_long_stream_records_what_wide_snapshots_record() {
    for finalize in [true, false] {
        TASKS.retain(|_, _| false);
        let dir = tempfile::tempdir().unwrap();
        let (wide, long) = (
            dir.path().join("wide.dendro"),
            dir.path().join("long.dendro"),
        );
        let described = record(&wide, &long, finalize);

        // Every live occupant at the start and again at the reconnect, then
        // only the occupant that changed hands.
        assert_eq!(described[0], 6, "{described:?}");
        assert!(described[RECONNECT as usize] >= 6, "{described:?}");
        for (tick, n) in described.iter().enumerate() {
            if tick != 0 && tick != RECONNECT as usize {
                assert_eq!(*n, 1, "tick {tick}: {described:?}");
            }
        }

        let (wide, long) = (open(&wide), open(&long));
        for q in QUERIES {
            let (w, l) = (answer(&wide, q), answer(&long, q));
            assert!(!w.is_empty(), "{q}: no answer");
            assert_eq!(l, w, "{q} (finalized: {finalize})");
        }
        // A reconnect re-describes live occupants; they keep their series.
        let cpu = answer(&long, "rate(long_tasks_cpu[3s])");
        let uids: std::collections::HashSet<_> = cpu
            .iter()
            .map(|(l, _)| l.iter().find(|(k, _)| k == "__uid__").cloned())
            .collect();
        assert_eq!(uids.len(), cpu.len(), "one series per occupant");
    }
}
