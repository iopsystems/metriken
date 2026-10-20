//! A reader of an archive another connection is changing: a seal that
//! commits while the reader opens, while a table's view is built, or while
//! a long table's occupant stream is read, and a renumbered or compacted
//! segment between a view's listing and a fetch.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use dendro::seal::SealPolicy;
use metriken_exposition::{GroupSchema, GroupSnapshot, MetricDesc, Snapshot, SnapshotV3};
use metriken_query::{MetricsSource, QueryResult};
use metriken_storage::catalog::{SegmentMeta, Source, Span, WalRow};
use metriken_storage::labels::Labels;
use metriken_storage::{
    ArchiveReader, ArchiveWriter, BufferPool, Catalog, DendroCatalog, SourceRecorder, WriterConfig,
};

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;
const STREAM: &str = "memory/meminfo";

/// One tick: a single gauge, `mem_free`, whose value names the tick.
fn snapshot(t: u64) -> Snapshot {
    let ts = BASE + t * S;
    let schema = GroupSchema {
        counters: Vec::new(),
        gauges: vec![MetricDesc {
            name: "0".to_string(),
            metadata: [("metric", "mem_free"), ("sampler", "memory")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }],
        histograms: Vec::new(),
    };
    let group = GroupSnapshot {
        name: STREAM.to_string(),
        schema_hash: schema.hash(),
        schema: (t == 0).then(|| Arc::new(schema)),
        window: Some(metriken::Window::new(ts - S / 2, ts)),
        counters: Vec::new(),
        gauges: vec![Some(1_000_000 - t as i64)],
        histograms: Vec::new(),
    };
    Snapshot::V3(SnapshotV3 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::from_millis(1),
        metadata: HashMap::new(),
        groups: vec![group],
    })
}

/// A writer whose seal policy seals every 5 rows.
fn writer(path: &Path) -> (ArchiveWriter, SourceRecorder) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    let labels = [("source".to_string(), "test".to_string())]
        .into_iter()
        .collect();
    let source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
    (writer, source)
}

/// Commit tick `t`, seal what is due, and wait for both to land.
fn tick(writer: &mut ArchiveWriter, source: &mut SourceRecorder, t: u64) {
    let staged = source.stage(&snapshot(t), BASE + t * S, 0).unwrap();
    writer.commit(vec![staged]).unwrap();
    source.maybe_seal().unwrap();
    source.sync().unwrap();
}

fn open(catalog: Box<dyn Catalog>) -> ArchiveReader {
    let mut recordings =
        ArchiveReader::from_catalog(catalog, None, BufferPool::new(64 * 1024 * 1024), None)
            .unwrap();
    assert_eq!(recordings.len(), 1);
    recordings.remove(0).1
}

/// The ticks of `mem_free`'s samples in `[from, to]`, in the order the
/// table returns them.
fn ticks(reader: &ArchiveReader, from: u64, to: u64) -> Vec<u64> {
    let owners = reader.owners_of(&["mem_free".to_string()]);
    assert_eq!(owners.len(), 1);
    let source = owners[0].reader().expect("the table opens").source();
    let gauges = source
        .gauges(
            "mem_free",
            &Labels::default(),
            BASE + from * S,
            BASE + to * S,
        )
        .expect("mem_free is a gauge of this table");
    assert_eq!(gauges.series.len(), 1);
    gauges.series[0]
        .timestamps
        .iter()
        .map(|ts| (ts - BASE) / S)
        .collect()
}

type Hook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

/// A dendro catalog that runs a hook just before the first `live_wal` read
/// of `stream` after the hook is armed: the point between a stream's
/// segment listing and its WAL read.
struct BeforeLiveWal {
    inner: DendroCatalog,
    stream: &'static str,
    hook: Hook,
}

impl Catalog for BeforeLiveWal {
    fn sources(&self) -> Result<Vec<Source>, String> {
        self.inner.sources()
    }
    fn tables(&self, source_id: i64) -> Result<Vec<String>, String> {
        self.inner.tables(source_id)
    }
    fn segment_meta(&self, source_id: i64, table: &str) -> Result<Vec<(u64, SegmentMeta)>, String> {
        self.inner.segment_meta(source_id, table)
    }
    fn segment_bytes(
        &self,
        source_id: i64,
        table: &str,
        seq: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        self.inner.segment_bytes(source_id, table, seq)
    }
    fn live_wal(&self, source_id: i64, table: &str) -> Result<Vec<WalRow>, String> {
        if table == self.stream {
            let hook = self.hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        self.inner.live_wal(source_id, table)
    }
    fn read_snapshot(&self, f: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        self.inner.read_snapshot(f)
    }
    fn segment_span(&self, source_id: i64, table: &str) -> Result<(u64, Span), String> {
        self.inner.segment_span(source_id, table)
    }
    fn live_wal_span(&self, source_id: i64, table: &str) -> Result<Span, String> {
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

/// A seal that commits between a table's segment listing and its WAL read
/// moves rows out of the WAL into a segment the listing does not have. Read
/// as two separate statements, those rows are in neither half: the view
/// must be the archive before the seal (ticks 0..=8) or after it (0..=9).
#[test]
fn seal_between_segment_listing_and_wal_read_loses_no_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.dendro");
    let (mut w, mut source) = writer(&path);
    // Ticks 0..=4 are sealed at tick 4; 5..=8 are in the WAL.
    for t in 0..=8 {
        tick(&mut w, &mut source, t);
    }

    let hook: Hook = Arc::new(Mutex::new(None));
    let reader = open(Box::new(BeforeLiveWal {
        inner: DendroCatalog::open(&path).unwrap(),
        stream: STREAM,
        hook: Arc::clone(&hook),
    }));

    // Armed after the open, so it fires when the table is first queried:
    // tick 9 is the fifth unsealed row, so this seals ticks 5..=9.
    *hook.lock().unwrap() = Some(Box::new(move || {
        tick(&mut w, &mut source, 9);
        std::mem::forget(source);
        w.join().unwrap();
    }));
    let got = ticks(&reader, 0, 100);
    assert!(hook.lock().unwrap().is_none(), "the hook did not run");
    assert_eq!(got, (0..=8).collect::<Vec<_>>());
}

/// A sequence number a view listed can name other bytes by the time the
/// view fetches them. Compaction cannot do it while a view is open (dendro's
/// `ArchiveMut::open` is refused while any connection holds the file), and a
/// running writer numbers each stream from a counter that only grows. A
/// writer reopened on the archive numbers from what the file holds, so a
/// stream whose every segment was evicted starts again at 0, and its next
/// seal is a new segment 0 holding rows the view already has in its live
/// tail. The view must not return those rows twice.
#[test]
fn renumbered_segment_after_listing_returns_no_row_twice() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("buffer.dendro");
    let (mut w, mut source) = writer(&path);
    // Segment 0 holds ticks 0..=4; 5..=8 are in the WAL.
    for t in 0..=8 {
        tick(&mut w, &mut source, t);
    }
    let source_id = source.source_id();

    // dendro's catalog, and the same through a wrapper that keeps the
    // trait's default `segment_bytes_matching`.
    let readers = [
        open(Box::new(DendroCatalog::open(&path).unwrap())),
        open(Box::new(BeforeLiveWal {
            inner: DendroCatalog::open(&path).unwrap(),
            stream: STREAM,
            hook: Hook::default(),
        })),
    ];
    // Builds each table's view (segment 0 and the tail) and reads only the
    // tail, so segment 0 is listed and not yet held decoded.
    for reader in &readers {
        assert_eq!(ticks(reader, 5, 8), (5..=8).collect::<Vec<_>>());
    }

    // Retention drops segment 0, and the writer stops.
    source.evict_before(BASE + 5 * S).unwrap();
    source.sync().unwrap();
    std::mem::forget(source);
    w.join().unwrap();
    drop(w);

    // A writer reopened on the file continues the source and seals its WAL:
    // the stream has no segment, so this one is numbered 0 again.
    let encoder = metriken_storage::Encoder::for_sources([(source_id, [STREAM])]);
    let mut reopened = dendro::writer::Writer::open(&path, Box::new(encoder)).unwrap();
    let (mut resumed, _) = reopened
        .resume_source(source_id, (BASE + 20 * S) as i64)
        .unwrap();
    resumed.seal(vec![STREAM.to_string()]).unwrap();
    resumed.sync().unwrap();
    let catalog = DendroCatalog::open(&path).unwrap();
    let seqs: Vec<(u64, u64, u64)> = catalog
        .segment_meta(source_id, STREAM)
        .unwrap()
        .into_iter()
        .map(|(seq, m)| (seq, (m.first_ts - BASE) / S, (m.last_ts - BASE) / S))
        .collect();
    assert_eq!(seqs, vec![(0, 5, 8)], "the renumbered segment");

    for reader in &readers {
        let got = ticks(reader, 0, 100);
        let mut seen = std::collections::BTreeSet::new();
        let repeated: Vec<u64> = got.iter().copied().filter(|t| !seen.insert(*t)).collect();
        assert!(
            repeated.is_empty(),
            "ticks read twice: {repeated:?} in {got:?}"
        );
        // Segment 0 as listed (ticks 0..=4) is gone; the tail is what is left.
        assert_eq!(got, (5..=8).collect::<Vec<_>>());
    }
}

/// Compaction gives a merged segment the number of the first segment it
/// replaced, and its span covers the listed one. It cannot run while a view
/// holds the file, so this asks the catalog directly: the number listed
/// before the merge reads as gone, through dendro's catalog and through the
/// trait's default.
#[test]
fn merged_segment_does_not_match_its_first_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("done.dendro");
    let (mut w, mut source) = writer(&path);
    // Three sealed segments: ticks 0..=4, 5..=9, 10..=14.
    for t in 0..=14 {
        tick(&mut w, &mut source, t);
    }
    let source_id = source.source_id();
    source.finalize((BASE + 14 * S, 0)).unwrap();
    w.join().unwrap();
    drop(w);

    let listed = DendroCatalog::open(&path)
        .unwrap()
        .segment_meta(source_id, STREAM)
        .unwrap();
    assert_eq!(listed.len(), 3);

    let mut db = dendro::archive::ArchiveMut::open(&path).unwrap();
    let done = dendro::rewrite::compact_stream(
        &mut db,
        source_id,
        STREAM,
        &dendro::rewrite::CompactSpec::to_rows(15),
    )
    .unwrap();
    assert_eq!((done.before, done.after), (3, 1), "{done:?}");
    drop(db);

    let catalogs: [Box<dyn Catalog>; 2] = [
        Box::new(DendroCatalog::open(&path).unwrap()),
        Box::new(BeforeLiveWal {
            inner: DendroCatalog::open(&path).unwrap(),
            stream: STREAM,
            hook: Hook::default(),
        }),
    ];
    for catalog in &catalogs {
        let merged = catalog.segment_meta(source_id, STREAM).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].0, listed[0].0, "the merge keeps the first number");
        let (seq, meta) = listed[0];
        assert!(catalog
            .segment_bytes(source_id, STREAM, seq)
            .unwrap()
            .is_some());
        assert_eq!(
            catalog
                .segment_bytes_matching(source_id, STREAM, seq, &meta)
                .unwrap(),
            None
        );
        let (seq, meta) = merged[0];
        assert!(catalog
            .segment_bytes_matching(source_id, STREAM, seq, &meta)
            .unwrap()
            .is_some());
    }
}

/// A table's first seal committing while the reader is opened, between the
/// table's segment listing (empty) and its WAL read (emptied by the seal),
/// leaves the open with no probe for the table. Read as two statements, the
/// reader does not list the table at all for as long as it is open.
#[test]
fn first_seal_while_the_reader_opens_keeps_the_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.dendro");
    let (mut w, mut source) = writer(&path);
    // Ticks 0..=3 are in the WAL; nothing is sealed.
    for t in 0..=3 {
        tick(&mut w, &mut source, t);
    }

    // Armed before the open, so it fires inside it: tick 4 is the fifth
    // unsealed row, so this seals ticks 0..=4.
    let hook: Hook = Arc::new(Mutex::new(Some(Box::new(move || {
        tick(&mut w, &mut source, 4);
        std::mem::forget(source);
        w.join().unwrap();
    }))));
    let reader = open(Box::new(BeforeLiveWal {
        inner: DendroCatalog::open(&path).unwrap(),
        stream: STREAM,
        hook: Arc::clone(&hook),
    }));
    assert!(hook.lock().unwrap().is_none(), "the hook did not run");
    // The view is built after the seal, so it holds the sealed segment.
    assert_eq!(ticks(&reader, 0, 100), (0..=4).collect::<Vec<_>>());
}

const TASKS: &str = "threads/tasks";
const OCCUPANTS: &str = "threads/tasks/occupants";

/// One tick of a long group: the gauge `rss` for slot 0, held by `a`
/// throughout, and from tick 1 for slot 1, held by a new occupant `b<t>`
/// each tick, so the occupant stream gains a row per tick.
fn long_snapshot(t: u64) -> Snapshot {
    let ts = BASE + t * S;
    let mut schema = GroupSchema::default();
    let mut gauges = Vec::new();
    let mut slots = vec![(0u64, "a".to_string())];
    if t >= 1 {
        slots.push((1, format!("b{t}")));
    }
    for (slot, comm) in slots {
        schema.gauges.push(MetricDesc {
            name: format!("0x{slot}"),
            metadata: [
                ("metric", "rss".to_string()),
                ("sampler", "threads".to_string()),
                ("id", slot.to_string()),
                ("comm", comm.to_string()),
                ("__uid__", format!("{comm}0")),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        });
        gauges.push(Some((t * 1000 + slot) as i64));
    }
    let group = GroupSnapshot {
        name: TASKS.to_string(),
        schema_hash: schema.hash(),
        schema: Some(Arc::new(schema)),
        window: Some(metriken::Window::new(ts - S / 2, ts)),
        counters: Vec::new(),
        gauges,
        histograms: Vec::new(),
    };
    Snapshot::V3(SnapshotV3 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::from_millis(1),
        metadata: HashMap::new(),
        groups: vec![group],
    })
}

fn long_tick(writer: &mut ArchiveWriter, source: &mut SourceRecorder, t: u64) {
    let staged = source.stage(&long_snapshot(t), BASE + t * S, 0).unwrap();
    writer.commit(vec![staged]).unwrap();
    source.maybe_seal().unwrap();
    source.sync().unwrap();
}

/// The `comm` labels `rss` answers with, sorted.
fn comms(reader: &ArchiveReader) -> Vec<String> {
    let result = reader
        .query_range("rss", BASE as f64 / 1e9, (BASE + 100 * S) as f64 / 1e9, 1.0)
        .unwrap();
    let QueryResult::Matrix { result } = result else {
        panic!("rss: not a matrix");
    };
    let mut comms: Vec<String> = result
        .into_iter()
        .filter_map(|m| m.metric.get("comm").cloned())
        .collect();
    comms.sort();
    comms.dedup();
    comms
}

/// A seal of a long table's occupant stream that commits between the
/// stream's segment listing and its WAL read moves the occupant rows into a
/// segment the listing does not have. Read as two statements, the table
/// opens with no occupant labels at all.
#[test]
fn seal_between_occupant_listing_and_wal_read_keeps_the_labels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.dendro");
    let (mut w, mut source) = writer(&path);
    // Ticks 0..=3 are in both streams' WALs; nothing is sealed.
    for t in 0..=3 {
        long_tick(&mut w, &mut source, t);
    }

    let hook: Hook = Arc::new(Mutex::new(None));
    let reader = open(Box::new(BeforeLiveWal {
        inner: DendroCatalog::open(&path).unwrap(),
        stream: OCCUPANTS,
        hook: Arc::clone(&hook),
    }));
    // Armed after the open, so it fires when the occupant stream is read for
    // the table's first query: tick 4 seals ticks 0..=4 of both streams.
    *hook.lock().unwrap() = Some(Box::new(move || {
        long_tick(&mut w, &mut source, 4);
        std::mem::forget(source);
        w.join().unwrap();
    }));
    let got = comms(&reader);
    assert!(hook.lock().unwrap().is_none(), "the hook did not run");
    assert_eq!(got, ["a", "b1", "b2", "b3"]);
}
