//! `MetricsSource::sample_timestamps` over a dendro archive
//! (`metriken-query`'s `ArchiveReader` impl): the instants at which the
//! first source with rows, or a copy of it, holds a row, ascending, each
//! once. The cases are tables of different cadence, a live archive's WAL
//! tail, a source and its copy, a copy holding more tables than the source
//! before it, two hosts of which only the first is listed, a second
//! recorder of one agent, a first source that recorded nothing or lost its
//! rows to retention after open, and archives with no rows. Parity with
//! `ParquetReader` on the same data is checked by `legacy_to_dendro.rs`,
//! whose answers include the list.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use dendro::archive::{Archive, ArchiveMut};
use dendro::rewrite::{copy_sources_into, CopySpec};
use dendro::seal::SealPolicy;
use metriken_exposition::{GroupSchema, GroupSnapshot, MetricDesc, Snapshot, SnapshotV3};
use metriken_query::MetricsSource;
use metriken_storage::writer::Encoder;
use metriken_storage::{
    ArchiveReader, ArchiveWriter, BufferPool, Catalog, DendroCatalog, LabeledRecordings,
    SourceRecorder, WriterConfig,
};

const MS: u64 = 1_000_000;
const S: u64 = 1_000 * MS;
const BASE: u64 = 1_700_000_000 * S;
const FAST: &str = "cpu/usage";
const SLOW: &str = "memory/meminfo";

fn pool() -> Arc<BufferPool> {
    BufferPool::new(64 << 20)
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Group `stream`'s row at `ts`: one gauge, named after the stream, with
/// its schema when `first`.
fn group(stream: &str, ts: u64, first: bool) -> GroupSnapshot {
    let (sampler, _) = stream.split_once('/').unwrap();
    let metric = stream.replace('/', "_");
    let schema = GroupSchema {
        counters: Vec::new(),
        gauges: vec![MetricDesc {
            name: "0".to_string(),
            metadata: [("metric", metric.as_str()), ("sampler", sampler)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }],
        histograms: Vec::new(),
        gauge_histograms: Vec::new(),
    };
    GroupSnapshot {
        name: stream.to_string(),
        schema_hash: schema.hash(),
        schema: first.then(|| Arc::new(schema)),
        window: Some(metriken::Window::new(ts - MS, ts)),
        counters: Vec::new(),
        gauges: vec![Some((ts / MS) as i64)],
        histograms: Vec::new(),
        gauge_histograms: Vec::new(),
    }
}

fn snapshot(ts: u64, groups: Vec<GroupSnapshot>) -> Snapshot {
    Snapshot::V3(SnapshotV3 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::from_millis(1),
        metadata: HashMap::new(),
        groups,
    })
}

/// Which groups a source holds a row of at offset `at` (ns past `BASE`).
type Plan = fn(u64) -> Vec<&'static str>;

/// Record into `source` a snapshot at `BASE + at` for each of `offsets`,
/// holding the groups `plan` names, sealing what is due.
fn record(
    writer: &mut ArchiveWriter,
    source: &mut SourceRecorder,
    offsets: impl IntoIterator<Item = u64>,
    plan: Plan,
) -> u64 {
    let mut seen: Vec<&str> = Vec::new();
    let mut last = BASE;
    for at in offsets {
        let ts = BASE + at;
        let groups = plan(at)
            .into_iter()
            .map(|g| {
                let first = !seen.contains(&g);
                if first {
                    seen.push(g);
                }
                group(g, ts, first)
            })
            .collect();
        let staged = source.stage(&snapshot(ts, groups), ts, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
        source.maybe_seal().unwrap();
        last = ts;
    }
    source.sync().unwrap();
    last
}

/// `FAST` every second except second 6; `SLOW` every third second. Second
/// 6 is held by `SLOW` alone.
fn two_cadences(at: u64) -> Vec<&'static str> {
    let t = at / S;
    let mut out = Vec::new();
    if t != 6 {
        out.push(FAST);
    }
    if t.is_multiple_of(3) {
        out.push(SLOW);
    }
    out
}

fn fast_only(_: u64) -> Vec<&'static str> {
    vec![FAST]
}

fn config(max_rows: usize) -> WriterConfig {
    WriterConfig {
        seal: SealPolicy {
            max_rows,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    }
}

/// One source per entry of `sources` (labels, offsets, plan), finalized.
fn write(path: &Path, sources: &[(BTreeMap<String, String>, Vec<u64>, Plan)]) {
    let mut writer = ArchiveWriter::create(path, config(4)).unwrap();
    for (labels, offsets, plan) in sources {
        let mut source = writer
            .add_source(labels.clone(), BTreeMap::new(), BASE)
            .unwrap();
        let last = record(&mut writer, &mut source, offsets.clone(), *plan);
        source.finalize((last, 0)).unwrap();
    }
    writer.join().unwrap();
}

fn open_recordings(path: &Path) -> LabeledRecordings {
    let catalog = DendroCatalog::open(path).unwrap();
    ArchiveReader::from_catalog(Box::new(catalog), None, pool(), None).unwrap()
}

fn joined(path: &Path) -> ArchiveReader {
    ArchiveReader::flatten(
        open_recordings(path).into_iter().map(|(_, r)| r).collect(),
        None,
    )
}

fn seconds(range: std::ops::RangeInclusive<u64>) -> Vec<u64> {
    range.map(|t| BASE + t * S).collect()
}

/// `S` seconds after `BASE` for each of `ts`, for messages.
fn offsets(ts: &[u64]) -> Vec<f64> {
    ts.iter().map(|t| (t - BASE) as f64 / S as f64).collect()
}

/// The union of the tables' rows: second 6 comes from the slow table, the
/// seconds both tables hold are listed once, and the result is the same
/// whether the reader is opened by path, by bytes, or as a read group.
#[test]
fn tables_of_different_cadence_list_the_union_of_their_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.dendro");
    let labels = map(&[("source", "rezolus")]);
    write(
        &path,
        &[(labels, (0..=10).map(|t| t * S).collect(), two_cadences)],
    );

    let mut recordings = open_recordings(&path);
    assert_eq!(recordings.len(), 1);
    let reader = recordings.remove(0).1;
    let table = |name: &str| -> Vec<u64> {
        let owners = reader.owners_of(&[name.to_string()]);
        assert_eq!(owners.len(), 1, "{name}");
        owners[0].row_timestamps().to_vec()
    };
    // The fixture is what the test needs: neither table alone is the answer.
    let mut fast = seconds(0..=10);
    fast.retain(|&t| t != BASE + 6 * S);
    assert_eq!(table("cpu_usage"), fast);
    assert_eq!(
        table("memory_meminfo"),
        seconds(0..=10).into_iter().step_by(3).collect::<Vec<_>>()
    );

    let got = reader.sample_timestamps();
    assert_eq!(got, seconds(0..=10), "{:?}", offsets(&got));

    let by_bytes =
        ArchiveReader::open_read_groups_from_bytes(std::fs::read(&path).unwrap(), pool()).unwrap();
    assert_eq!(by_bytes.len(), 1);
    assert_eq!(by_bytes[0].1.sample_timestamps(), got, "opened from bytes");
    let groups = ArchiveReader::open_read_groups(&path, pool()).unwrap();
    assert_eq!(
        groups[0].1.sample_timestamps(),
        got,
        "opened as read groups"
    );
}

/// A live archive lists the rows in its WAL tail: ticks 0..=7 are sealed in
/// segments of four rows, 8..=9 are in the WAL, and the writer is still open.
#[test]
fn a_live_archive_lists_its_wal_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.dendro");
    let mut writer = ArchiveWriter::create(&path, config(4)).unwrap();
    let mut source = writer
        .add_source(map(&[("source", "rezolus")]), BTreeMap::new(), BASE)
        .unwrap();
    record(&mut writer, &mut source, (0..=9).map(|t| t * S), fast_only);

    let catalog = DendroCatalog::open(&path).unwrap();
    let wal = catalog
        .live_wal_span(catalog_source(&catalog), FAST)
        .unwrap();
    assert_eq!(wal.rows, 2, "two rows in the WAL");
    let reader = open_recordings(&path).remove(0).1;
    assert!(!reader.complete(), "the archive is live");
    let got = reader.sample_timestamps();
    assert_eq!(got, seconds(0..=9), "{:?}", offsets(&got));
    drop(source);
    writer.join().unwrap();
}

/// The id of the catalog's only source.
fn catalog_source(catalog: &DendroCatalog) -> i64 {
    let sources = catalog.sources().unwrap();
    assert_eq!(sources.len(), 1);
    sources[0].id
}

/// Copy every source of `from` into `into`, keeping each uuid.
fn copy(from: &Path, into: &mut ArchiveMut) {
    let src = Archive::open(from).unwrap();
    let encoder = Encoder::for_archive(&src).unwrap();
    into.transaction(|tx| copy_sources_into(&src, tx, &CopySpec::everything(), &encoder))
        .unwrap();
}

/// A source and its copy are one read group, holding the same instants:
/// each is listed once, as for the source alone.
#[test]
fn a_read_group_of_copies_lists_each_instant_once() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.dendro");
    let labels = map(&[("source", "rezolus")]);
    write(
        &a,
        &[(labels, (0..=10).map(|t| t * S).collect(), two_cadences)],
    );
    let alone = joined(&a).sample_timestamps();
    assert_eq!(alone, seconds(0..=10));

    let both = dir.path().join("with-copy.dendro");
    {
        let mut out = ArchiveMut::create(&both).unwrap();
        copy(&a, &mut out);
        copy(&a, &mut out);
    }
    let reader = joined(&both);
    assert_eq!(reader.sources().len(), 2);
    assert_eq!(reader.read_groups().len(), 1, "one read group");
    assert_eq!(reader.sample_timestamps(), alone, "joined");
    let groups = ArchiveReader::open_read_groups(&both, pool()).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].1.sample_timestamps(), alone, "as a read group");
}

/// Two hosts sample on their own clocks, here half a second apart. A reader
/// of both lists the first source's instants; each read group opened on
/// its own lists its own.
#[test]
fn only_the_first_source_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hosts.dendro");
    let a: Vec<u64> = (0..=5).map(|t| t * S).collect();
    let b: Vec<u64> = (0..=5).map(|t| t * S + 500 * MS).collect();
    write(
        &path,
        &[
            (map(&[("host", "a")]), a.clone(), fast_only),
            (map(&[("host", "b")]), b.clone(), fast_only),
        ],
    );
    let at = |o: &[u64]| o.iter().map(|t| BASE + t).collect::<Vec<_>>();
    let reader = joined(&path);
    assert_eq!(reader.read_groups().len(), 2);
    assert_eq!(reader.sample_timestamps(), at(&a), "the first host's");

    let groups = ArchiveReader::open_read_groups(&path, pool()).unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].1.sample_timestamps(), at(&a));
    assert_eq!(groups[1].1.sample_timestamps(), at(&b));

    // The other order: the first source is host b's.
    let reversed = ArchiveReader::flatten(
        open_recordings(&path)
            .into_iter()
            .rev()
            .map(|(_, r)| r)
            .collect(),
        None,
    );
    assert_eq!(reversed.sample_timestamps(), at(&b));
}

/// No rows, no instants: an archive with no source, and one whose only
/// source recorded nothing.
#[test]
fn an_archive_without_rows_lists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let none = dir.path().join("none.dendro");
    drop(ArchiveMut::create(&none).unwrap());
    assert!(joined(&none).sample_timestamps().is_empty());

    let idle = dir.path().join("idle.dendro");
    write(
        &idle,
        &[(map(&[("source", "rezolus")]), Vec::new(), fast_only)],
    );
    let recordings = open_recordings(&idle);
    for (_, r) in &recordings {
        assert!(r.sample_timestamps().is_empty());
    }
    assert!(joined(&idle).sample_timestamps().is_empty());
}

/// Two recorders of one agent are one read group but sample on their own
/// schedules, here 50 ms apart. The list is the first recorder's, not the
/// two interleaved.
#[test]
fn a_second_recorder_of_the_agent_is_not_listed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two-recorders.dendro");
    let mut writer = ArchiveWriter::create(&path, config(4)).unwrap();
    for offset in [0, 50 * MS] {
        let mut source = writer
            .add_source(
                map(&[("source", "rezolus")]),
                map(&[("producer_epoch", "e1")]),
                BASE,
            )
            .unwrap();
        let offsets = (0..=5).map(|t| t * S + offset);
        let last = record(&mut writer, &mut source, offsets, fast_only);
        source.finalize((last, 0)).unwrap();
    }
    writer.join().unwrap();

    let reader = joined(&path);
    assert_eq!(reader.sources().len(), 2);
    assert_eq!(reader.read_groups().len(), 1, "one read group");
    let got = reader.sample_timestamps();
    assert_eq!(got, seconds(0..=5), "{:?}", offsets(&got));
    let groups = ArchiveReader::open_read_groups(&path, pool()).unwrap();
    assert_eq!(groups[0].1.sample_timestamps(), got, "as a read group");
}

/// A first source that recorded nothing owns no table and is passed over:
/// the list is the instants of the first source with rows.
#[test]
fn a_first_source_that_recorded_nothing_is_passed_over() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idle-first.dendro");
    write(
        &path,
        &[
            (map(&[("host", "a")]), Vec::new(), fast_only),
            (
                map(&[("host", "b")]),
                (0..=5).map(|t| t * S).collect(),
                fast_only,
            ),
        ],
    );
    let got = joined(&path).sample_timestamps();
    assert_eq!(got, seconds(0..=5), "{:?}", offsets(&got));
}

/// A copy holding only the fast table, followed by a full copy: the full
/// copy contributes second 6, which only the slow table holds.
#[test]
fn a_copy_holding_more_tables_contributes_its_instants() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.dendro");
    let labels = map(&[("source", "rezolus")]);
    write(
        &a,
        &[(labels, (0..=10).map(|t| t * S).collect(), two_cadences)],
    );
    let both = dir.path().join("partial-copy.dendro");
    {
        let mut out = ArchiveMut::create(&both).unwrap();
        let src = Archive::open(&a).unwrap();
        let encoder = Encoder::for_archive(&src).unwrap();
        let fast = |s: &str| s == FAST;
        let spec = CopySpec {
            keep_streams: Some(&fast),
            ..CopySpec::everything()
        };
        out.transaction(|tx| copy_sources_into(&src, tx, &spec, &encoder))
            .unwrap();
        copy(&a, &mut out);
    }
    let reader = joined(&both);
    assert_eq!(reader.read_groups().len(), 1);
    let got = reader.sample_timestamps();
    assert_eq!(got, seconds(0..=10), "{:?}", offsets(&got));
}

/// A live archive's first source loses every row to retention after the
/// reader opened: it still owns its tables, which now read nothing, and the
/// list is the next source's.
#[test]
fn a_first_source_evicted_after_open_is_passed_over() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evicted.dendro");
    let mut writer = ArchiveWriter::create(&path, config(4)).unwrap();
    let mut a = writer
        .add_source(map(&[("host", "a")]), BTreeMap::new(), BASE)
        .unwrap();
    record(&mut writer, &mut a, (0..=9).map(|t| t * S), fast_only);
    let mut b = writer
        .add_source(map(&[("host", "b")]), BTreeMap::new(), BASE)
        .unwrap();
    let b_offsets: Vec<u64> = (0..=9).map(|t| t * S + 500 * MS).collect();
    record(&mut writer, &mut b, b_offsets.clone(), fast_only);

    let reader = joined(&path);
    a.evict_before(BASE + 1_000 * S).unwrap();
    a.sync().unwrap();
    let want: Vec<u64> = b_offsets.iter().map(|t| BASE + t).collect();
    let got = reader.sample_timestamps();
    assert_eq!(got, want, "{:?}", offsets(&got));
    drop((a, b));
    writer.join().unwrap();
}
