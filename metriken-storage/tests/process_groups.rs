//! Process groups over `.dendro` archives, opened as a consumer opens them:
//! `DendroCatalog` → `ArchiveReader::from_catalog` → `flatten`. The fixtures
//! are the spec's (`docs/journal/2026-10-19-recording-identity-and-views.md`,
//! case 3, case 4's bridges, case 6 and Q39), written with `ArchiveWriter`;
//! copies sharing a uuid are made with dendro's `copy_sources_into`, as
//! hindsight's dumps and rezolus's `combine` make them.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, SystemTime};

use dendro::archive::{Archive, ArchiveMut};
use dendro::rewrite::{copy_sources_into, CopySpec};
use dendro::seal::SealPolicy;
use metriken_exposition::{Counter, Snapshot, SnapshotV2};
use metriken_storage::catalog::{SegmentMeta, Source, Span, WalRow};
use metriken_storage::process::{Notice, ProcessGroups};
use metriken_storage::writer::Encoder;
use metriken_storage::{
    ArchiveReader, ArchiveWriter, BufferPool, Catalog, DendroCatalog, WriterConfig,
};

const MS: u64 = 1_000_000;
const S: u64 = 1_000 * MS;
const MIN: u64 = 60 * S;
/// 2023-11-14 22:13:20 UTC; the fixtures' clock times are minutes after it.
const BASE: u64 = 1_700_000_000 * S;

/// `hh:mm` as a timestamp.
fn at(h: u64, m: u64) -> u64 {
    BASE + (h * 60 + m) * MIN
}

/// One source to write: its labels, its producer epochs (each with the
/// first row it covers), and its row timestamps.
struct Spec {
    labels: Vec<(&'static str, &'static str)>,
    epochs: Vec<(&'static str, u64)>,
    rows: Vec<u64>,
}

fn rows(from: u64, to: u64, every: u64) -> Vec<u64> {
    (0..)
        .map(|k| from + k * every)
        .take_while(|t| *t <= to)
        .collect()
}

fn spec(
    labels: &[(&'static str, &'static str)],
    epochs: &[(&'static str, u64)],
    rows: Vec<u64>,
) -> Spec {
    Spec {
        labels: labels.to_vec(),
        epochs: epochs.to_vec(),
        rows,
    }
}

/// A counter `cpu_cycles` growing 1000/s, read 1 ms before each stamp.
fn snapshot(ts: u64) -> Snapshot {
    let metadata: HashMap<String, String> = [("metric", "cpu_cycles"), ("sampler", "cpu")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let value = (ts - BASE) / MS;
    Snapshot::V2(SnapshotV2 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::ZERO,
        metadata: HashMap::new(),
        counters: vec![Counter::new("0".into(), value, metadata)
            .with_window(Some(metriken::Window::new(ts - MS, ts)))],
        gauges: Vec::new(),
        histograms: Vec::new(),
    })
}

/// Write `specs` as the sources of one new archive at `path`, each with a
/// fresh uuid. `max_rows` bounds a sealed segment.
fn write(path: &Path, specs: &[Spec], max_rows: usize) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    for spec in specs {
        let labels: BTreeMap<String, String> = spec
            .labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut metadata = BTreeMap::new();
        match spec.epochs.as_slice() {
            [] => {}
            [(epoch, _)] => {
                metadata.insert("producer_epoch".to_string(), epoch.to_string());
            }
            several => {
                let list: Vec<String> = several
                    .iter()
                    .map(|(e, from)| format!(r#"{{"epoch":"{e}","from_ts":{from}}}"#))
                    .collect();
                metadata.insert(
                    "producer_epochs".to_string(),
                    format!("[{}]", list.join(",")),
                );
                metadata.insert(
                    "producer_epoch".to_string(),
                    several.last().unwrap().0.to_string(),
                );
            }
        }
        let mut source = writer.add_source(labels, metadata, BASE).unwrap();
        for &ts in &spec.rows {
            let staged = source.stage(&snapshot(ts), ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
        }
        source.finalize((*spec.rows.last().unwrap(), 0)).unwrap();
    }
    writer.join().unwrap();
}

/// Copy every source of `from` into `into`, keeping each uuid; `range`
/// narrows the copy to rows in it (whole segments, as a hindsight dump).
fn copy(from: &Path, into: &mut ArchiveMut, range: Option<(u64, u64)>) {
    let src = Archive::open(from).unwrap();
    let encoder = Encoder::for_archive(&src).unwrap();
    let spec = match range {
        Some((start, end)) => CopySpec {
            start: start as i64,
            end: end as i64,
            ..CopySpec::everything()
        },
        None => CopySpec::everything(),
    };
    into.transaction(|tx| copy_sources_into(&src, tx, &spec, &encoder))
        .unwrap();
}

/// The archive at `path` as one reader over all its sources.
fn open(path: &Path) -> ArchiveReader {
    let catalog = DendroCatalog::open(path).unwrap();
    let recordings =
        ArchiveReader::from_catalog(Box::new(catalog), None, BufferPool::new(64 << 20), None)
            .unwrap();
    ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None)
}

/// Each process group's sources, by position.
fn groups(g: &ProcessGroups) -> Vec<Vec<usize>> {
    g.groups.iter().map(|g| g.sources.clone()).collect()
}

/// Each bridge's sources, by position.
fn bridges(g: &ProcessGroups) -> Vec<Vec<usize>> {
    g.bridges
        .iter()
        .map(|b| g.uuid_groups[b.uuid_group].sources.clone())
        .collect()
}

/// Case 3: a recording of agent A and a hindsight buffer's recording of A
/// over part of it — two uuids, one epoch, overlapping — are one process.
#[test]
fn a_record_and_a_buffer_of_one_agent_are_one_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    write(
        &path,
        &[
            spec(
                &[("host", "a")],
                &[("e", at(10, 0))],
                rows(at(10, 0), at(10, 30), 10 * S),
            ),
            spec(
                &[("host", "a")],
                &[("e", at(10, 5))],
                rows(at(10, 5), at(10, 10), S),
            ),
        ],
        20,
    );
    let reader = open(&path);
    let g = reader.process_groups();
    assert_eq!(groups(g), vec![vec![0, 1]]);
    let join = &g.groups[0].joins[0];
    assert_eq!(join.epoch, "e");
    assert_eq!(join.overlap_ns, (at(10, 5), at(10, 10)));
    assert!(g.bridges.is_empty() && g.notices.is_empty());
    // Two fresh sources, two uuids.
    let uuids: Vec<_> = reader.sources().iter().map(|s| s.uuid.clone()).collect();
    assert!(uuids[0].is_some() && uuids[0] != uuids[1]);
}

/// Case 3: two dumps of one hindsight buffer, 10:00–10:05 and 10:20–10:25,
/// share the buffer's uuid and join through it, gap and all.
#[test]
fn two_disjoint_dumps_of_one_buffer_are_one_process() {
    let dir = tempfile::tempdir().unwrap();
    let buffer = dir.path().join("buffer.dendro");
    write(
        &buffer,
        &[spec(
            &[],
            &[("e", at(10, 0))],
            rows(at(10, 0), at(10, 30), 10 * S),
        )],
        6,
    );
    let combined = dir.path().join("combined.dendro");
    let mut out = ArchiveMut::create(&combined).unwrap();
    copy(&buffer, &mut out, Some((at(10, 0), at(10, 5))));
    copy(&buffer, &mut out, Some((at(10, 20), at(10, 25))));
    drop(out);

    let reader = open(&combined);
    let sources = reader.sources();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].uuid, sources[1].uuid);
    // The dumps hold whole segments around what was asked, and no more.
    assert!(sources[0].span_ns.unwrap().1 < at(10, 20));
    let g = reader.process_groups();
    assert_eq!(groups(g), vec![vec![0, 1]]);
    assert_eq!(g.uuid_groups.len(), 1);
    assert!(g.groups[0].joins.is_empty(), "joined by uuid, not by epoch");
    assert!(g.bridges.is_empty() && g.notices.is_empty());
}

/// Case 3: two recorders of one agent at once, at 1 s and 100 ms.
#[test]
fn two_recorders_at_different_intervals_are_one_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.dendro");
    write(
        &path,
        &[
            spec(&[], &[("e", at(10, 0))], rows(at(10, 0), at(10, 2), S)),
            spec(
                &[],
                &[("e", at(10, 0))],
                rows(at(10, 0), at(10, 2), 100 * MS),
            ),
        ],
        200,
    );
    assert_eq!(groups(open(&path).process_groups()), vec![vec![0, 1]]);
}

/// Case 3 (and W4): one agent, one epoch, recorded continuously 09:00–18:00
/// and by two invocations 09:00–10:00 and 17:00–18:00. The continuous
/// recording is a bridge and is set aside, so the attempts stay apart.
#[test]
fn a_continuous_recording_beside_two_attempts_is_a_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w4.dendro");
    let e = &[("e", at(9, 0))];
    write(
        &path,
        &[
            spec(&[], e, rows(at(9, 0), at(18, 0), MIN)),
            spec(&[], e, rows(at(9, 0), at(10, 0), 10 * S)),
            spec(&[], e, rows(at(17, 0), at(18, 0), 10 * S)),
        ],
        100,
    );
    let g = open(&path).process_groups().clone();
    assert_eq!(bridges(&g), vec![vec![0]]);
    assert_eq!(g.bridges[0].between, [1, 2]);
    assert_eq!(groups(&g), vec![vec![1], vec![2]]);
    assert_eq!(g.group_of(0), None);
    // Same epoch, disjoint spans: apart, and no notice (O2).
    assert!(g.notices.is_empty(), "{:?}", g.notices);
}

/// Case 3: dumps D1 10:00–10:05 and D2 10:20–10:25 and a copy L of the live
/// buffer 09:50–10:30, one uuid: one uuid group, which is not a bridge.
/// Adding a record R 09:00–11:00 of the same agent (another uuid, one
/// epoch): two uuid groups, neither a bridge, one process group.
#[test]
fn copies_of_one_buffer_with_a_record_are_one_process() {
    let dir = tempfile::tempdir().unwrap();
    let buffer = dir.path().join("buffer.dendro");
    let e = &[("e", at(9, 0))];
    write(
        &buffer,
        &[spec(&[], e, rows(at(9, 50), at(10, 30), 10 * S))],
        6,
    );
    let record = dir.path().join("record.dendro");
    write(
        &record,
        &[spec(&[], e, rows(at(9, 0), at(11, 0), 30 * S))],
        50,
    );

    let combined = dir.path().join("combined.dendro");
    let mut out = ArchiveMut::create(&combined).unwrap();
    copy(&buffer, &mut out, Some((at(10, 0), at(10, 5))));
    copy(&buffer, &mut out, Some((at(10, 20), at(10, 25))));
    copy(&buffer, &mut out, None);
    drop(out);
    let g = open(&combined).process_groups().clone();
    assert_eq!(g.uuid_groups.len(), 1);
    assert_eq!(groups(&g), vec![vec![0, 1, 2]]);
    assert!(g.bridges.is_empty());

    let mut out = ArchiveMut::open(&combined).unwrap();
    copy(&record, &mut out, None);
    drop(out);
    let g = open(&combined).process_groups().clone();
    assert_eq!(g.uuid_groups.len(), 2);
    assert!(g.bridges.is_empty());
    assert_eq!(groups(&g), vec![vec![0, 1, 2, 3]]);
    assert_eq!(g.groups[0].joins.len(), 1);
    assert_eq!(g.groups[0].joins[0].uuid_groups, [0, 1]);
}

/// Case 3's epoch chain: A holds e1 then e2 (a restart), B holds e2 only and
/// overlaps A in e2. One process group through e2.
#[test]
fn an_epoch_chain_joins_through_the_shared_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chain.dendro");
    write(
        &path,
        &[
            spec(
                &[],
                &[("e1", at(9, 0)), ("e2", at(9, 30))],
                rows(at(9, 0), at(10, 0), 10 * S),
            ),
            spec(
                &[],
                &[("e2", at(9, 40))],
                rows(at(9, 40), at(10, 10), 10 * S),
            ),
        ],
        30,
    );
    let reader = open(&path);
    let epochs: Vec<&str> = reader.sources()[0]
        .epochs
        .iter()
        .map(|e| e.epoch.as_str())
        .collect();
    assert_eq!(epochs, ["e1", "e2"]);
    assert_eq!(reader.sources()[0].epochs[0].last_ns, at(9, 30) - 1);
    let g = reader.process_groups();
    assert_eq!(groups(g), vec![vec![0, 1]]);
    assert_eq!(g.groups[0].joins[0].epoch, "e2");
    assert_eq!(g.groups[0].joins[0].overlap_ns, (at(9, 40), at(10, 0)));
}

/// A restart that is the only thing two sources have in common: A holds e1
/// then e2, B holds e1 but only after A's restart (a wrong clock, or a
/// different process reusing an id). The spans intersect, the epoch spans
/// do not, so nothing joins.
#[test]
fn an_epoch_shared_outside_its_span_does_not_join() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("outside.dendro");
    write(
        &path,
        &[
            spec(
                &[],
                &[("e1", at(9, 0)), ("e2", at(9, 30))],
                rows(at(9, 0), at(10, 0), 10 * S),
            ),
            spec(
                &[],
                &[("e1", at(9, 40))],
                rows(at(9, 40), at(10, 10), 10 * S),
            ),
        ],
        30,
    );
    let g = open(&path).process_groups().clone();
    assert_eq!(groups(&g), vec![vec![0], vec![1]]);
    assert!(g.notices.is_empty());
}

/// Case 3, R3: one epoch, a 100 ms source whose rows all lie between two
/// rows of a 1 s source. The spans intersect without a row of each, so they
/// stay apart with a notice. The 1 s source's rows there are in one sealed
/// segment that starts before and ends after the intersection, which is
/// where `has_row_in` reads a timestamp column.
#[test]
fn spans_that_intersect_without_a_row_of_each_stay_apart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r3.dendro");
    let start = at(10, 0);
    write(
        &path,
        &[
            spec(&[], &[("e", start)], rows(start, start + 60 * S, S)),
            spec(
                &[],
                &[("e", start + 4050 * MS)],
                rows(start + 4050 * MS, start + 4950 * MS, 100 * MS),
            ),
        ],
        1000,
    );
    let g = open(&path).process_groups().clone();
    assert_eq!(groups(&g), vec![vec![0], vec![1]]);
    assert_eq!(
        g.notices,
        vec![Notice::EpochWithoutSharedRow {
            epoch: "e".into(),
            uuid_groups: [0, 1],
            intersection_ns: (start + 4050 * MS, start + 4950 * MS),
        }]
    );

    // The same, with the short source reaching the long one's row at 5 s:
    // the row is found inside the straddling segment, and they join.
    let path = dir.path().join("r3-row.dendro");
    write(
        &path,
        &[
            spec(&[], &[("e", start)], rows(start, start + 60 * S, S)),
            spec(
                &[],
                &[("e", start + 4050 * MS)],
                rows(start + 4050 * MS, start + 5050 * MS, 100 * MS),
            ),
        ],
        1000,
    );
    let g = open(&path).process_groups().clone();
    assert_eq!(groups(&g), vec![vec![0, 1]]);
    assert!(g.notices.is_empty());
}

/// The same R3 check against rows still in the live WAL: a source left
/// unfinalized, nothing sealed.
#[test]
fn rows_in_the_live_wal_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.dendro");
    let start = at(10, 0);
    let mut writer = ArchiveWriter::create(&path, WriterConfig::default()).unwrap();
    let mut add = |rows: &[u64]| {
        let metadata = [("producer_epoch".to_string(), "e".to_string())].into();
        let mut source = writer.add_source(BTreeMap::new(), metadata, BASE).unwrap();
        let mut staged = Vec::new();
        for &ts in rows {
            staged.push(source.stage(&snapshot(ts), ts, 0).unwrap());
        }
        (source, staged)
    };
    let (mut long, a) = add(&rows(start, start + 60 * S, S));
    let (mut short, b) = add(&rows(start + 4050 * MS, start + 4950 * MS, 100 * MS));
    for s in a.into_iter().chain(b) {
        writer.commit(vec![s]).unwrap();
    }
    long.sync().unwrap();
    short.sync().unwrap();
    writer.join().unwrap();
    std::mem::forget(long);
    std::mem::forget(short);

    let g = open(&path).process_groups().clone();
    assert_eq!(groups(&g), vec![vec![0], vec![1]]);
    assert_eq!(g.notices.len(), 1);
}

/// Case 4: agent A 09:00–18:00, B 09:00–10:00, C 17:00–18:00, no epochs, no
/// labels: A is a bridge by span overlap.
#[test]
fn an_epochless_continuous_recording_is_a_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("abc.dendro");
    write(
        &path,
        &[
            spec(&[], &[], rows(at(9, 0), at(18, 0), MIN)),
            spec(&[], &[], rows(at(9, 0), at(10, 0), 10 * S)),
            spec(&[], &[], rows(at(17, 0), at(18, 0), 10 * S)),
        ],
        100,
    );
    let g = open(&path).process_groups().clone();
    assert_eq!(bridges(&g), vec![vec![0]]);
    assert_eq!(groups(&g), vec![vec![1], vec![2]]);
}

/// Case 4: D1 and D2 09:00–18:00 beside B 09:00–10:00 and C 17:00–18:00.
/// Both are bridges. Variant: D1 and D2 record one agent (one epoch); they
/// are still both bridges, and not joined to each other here — their pieces
/// join later, per cluster.
#[test]
fn two_continuous_recordings_are_both_bridges() {
    let dir = tempfile::tempdir().unwrap();
    for shared in [None, Some("d")] {
        let path = dir.path().join(format!("d1d2-{}.dendro", shared.is_some()));
        let d: Vec<(&'static str, u64)> = shared.map(|e| (e, at(9, 0))).into_iter().collect();
        write(
            &path,
            &[
                spec(&[], &d, rows(at(9, 0), at(18, 0), MIN)),
                spec(&[], &d, rows(at(9, 0), at(18, 0), MIN)),
                spec(&[], &[], rows(at(9, 0), at(10, 0), 10 * S)),
                spec(&[], &[], rows(at(17, 0), at(18, 0), 10 * S)),
            ],
            100,
        );
        let g = open(&path).process_groups().clone();
        assert_eq!(bridges(&g), vec![vec![0], vec![1]], "{shared:?}");
        assert_eq!(groups(&g), vec![vec![2], vec![3]], "{shared:?}");
    }
}

/// Case 4's chain: X 09:00–11:00, Y 10:00–12:00, Z 11:30–13:00. Y is a
/// bridge.
#[test]
fn the_middle_of_a_chain_is_a_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("xyz.dendro");
    write(
        &path,
        &[
            spec(&[], &[], rows(at(9, 0), at(11, 0), MIN)),
            spec(&[], &[], rows(at(10, 0), at(12, 0), MIN)),
            spec(&[], &[], rows(at(11, 30), at(13, 0), MIN)),
        ],
        100,
    );
    let g = open(&path).process_groups().clone();
    assert_eq!(bridges(&g), vec![vec![1]]);
    assert_eq!(g.bridges[0].between, [0, 2]);
}

/// P6 with arms: an unarmed continuous recording of one agent beside its
/// armed attempts is kept out of a join between them (owner decision on the
/// Q39 scenario) and reported as a bridge. An armed one is a bridge within
/// its arm; a pair it overlaps in another arm is not bridged.
#[test]
fn bridges_are_found_within_an_arm() {
    let dir = tempfile::tempdir().unwrap();
    let e = &[("e", at(9, 0))];
    let path = dir.path().join("unarmed.dendro");
    write(
        &path,
        &[
            spec(&[], e, rows(at(9, 0), at(18, 0), MIN)),
            spec(&[("arm", "a")], e, rows(at(9, 0), at(10, 0), 10 * S)),
            spec(&[("arm", "a")], e, rows(at(17, 0), at(18, 0), 10 * S)),
        ],
        100,
    );
    let g = open(&path).process_groups().clone();
    assert_eq!(bridges(&g), vec![vec![0]]);
    assert_eq!(groups(&g), vec![vec![1], vec![2]]);

    let path = dir.path().join("armed.dendro");
    write(
        &path,
        &[
            spec(&[("arm", "a")], &[], rows(at(9, 0), at(18, 0), MIN)),
            spec(&[("arm", "a")], &[], rows(at(9, 0), at(10, 0), 10 * S)),
            spec(&[("arm", "a")], &[], rows(at(17, 0), at(18, 0), 10 * S)),
            spec(&[("arm", "b")], &[], rows(at(9, 0), at(18, 0), MIN)),
        ],
        100,
    );
    let g = open(&path).process_groups().clone();
    assert_eq!(bridges(&g), vec![vec![0]]);
    assert_eq!(groups(&g), vec![vec![1], vec![2], vec![3]]);
}

/// Case 6 / Q39: three `arm=a` recordings at 10:00, 10:00 and 11:00 from
/// three agents (three epochs): nothing joins. Variant: the two 10:00
/// recordings share an epoch, which joins them.
#[test]
fn three_armed_recordings_join_only_by_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let arm = &[("arm", "a")];
    for shared in [false, true] {
        let path = dir.path().join(format!("arms-{shared}.dendro"));
        let second = if shared { "e1" } else { "e2" };
        write(
            &path,
            &[
                spec(
                    arm,
                    &[("e1", at(10, 0))],
                    rows(at(10, 0), at(10, 10), 10 * S),
                ),
                spec(
                    arm,
                    &[(second, at(10, 0))],
                    rows(at(10, 0), at(10, 10), 10 * S),
                ),
                spec(
                    arm,
                    &[("e3", at(11, 0))],
                    rows(at(11, 0), at(11, 10), 10 * S),
                ),
            ],
            20,
        );
        let g = open(&path).process_groups().clone();
        assert!(g.bridges.is_empty());
        let expected = match shared {
            false => vec![vec![0], vec![1], vec![2]],
            true => vec![vec![0, 1], vec![2]],
        };
        assert_eq!(groups(&g), expected);
    }
}

/// Readers of two archives joined by `flatten` group across them: a copy in
/// another file is still a copy.
#[test]
fn copies_in_two_archives_are_one_process() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original.dendro");
    write(
        &original,
        &[spec(&[], &[], rows(at(10, 0), at(10, 5), 10 * S))],
        20,
    );
    let copied = dir.path().join("copied.dendro");
    let mut out = ArchiveMut::create(&copied).unwrap();
    copy(&original, &mut out, None);
    drop(out);

    let readers: Vec<ArchiveReader> = [&original, &copied].iter().map(|p| open(p)).collect();
    let reader = ArchiveReader::flatten(readers, None);
    let sources = reader.sources();
    assert_eq!((sources[0].catalog_index, sources[1].catalog_index), (0, 0));
    assert_eq!(groups(reader.process_groups()), vec![vec![0, 1]]);
}

/// A catalog that counts segment fetches.
struct Counting {
    inner: DendroCatalog,
    fetched: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Catalog for Counting {
    fn sources(&self) -> Result<Vec<Source>, String> {
        self.inner.sources()
    }
    fn sources_with_uuids(&self) -> Result<Vec<(Source, Option<String>)>, String> {
        self.inner.sources_with_uuids()
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
        self.fetched
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.segment_bytes(source_id, table, seq)
    }
    fn live_wal(&self, source_id: i64, table: &str) -> Result<Vec<WalRow>, String> {
        self.inner.live_wal(source_id, table)
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

/// How many segments `process_groups` fetches for the archive at `path`,
/// beyond what opening it fetched.
fn fetches_to_group(path: &Path) -> (usize, ProcessGroups) {
    let fetched = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let catalog = Counting {
        inner: DendroCatalog::open(path).unwrap(),
        fetched: std::sync::Arc::clone(&fetched),
    };
    let recordings =
        ArchiveReader::from_catalog(Box::new(catalog), None, BufferPool::new(64 << 20), None)
            .unwrap();
    let reader = ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None);
    let before = fetched.load(std::sync::atomic::Ordering::Relaxed);
    let groups = reader.process_groups().clone();
    (
        fetched.load(std::sync::atomic::Ordering::Relaxed) - before,
        groups,
    )
}

/// Grouping reads the catalog, not segments, unless a segment straddles a
/// whole intersection: two sources of one epoch whose segment edges fall
/// inside each other's spans need no fetch; a short source inside one
/// segment of a long one needs that one segment.
#[test]
fn grouping_fetches_a_segment_only_when_one_straddles_the_intersection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edges.dendro");
    let e = &[("e", at(10, 0))];
    write(
        &path,
        &[
            spec(&[], e, rows(at(10, 0), at(10, 30), 10 * S)),
            spec(&[], e, rows(at(10, 5), at(10, 10), S)),
        ],
        20,
    );
    let (fetched, g) = fetches_to_group(&path);
    assert_eq!(groups(&g), vec![vec![0, 1]]);
    assert_eq!(fetched, 0);

    let path = dir.path().join("straddle.dendro");
    let start = at(10, 0);
    write(
        &path,
        &[
            spec(&[], e, rows(start, start + 60 * S, S)),
            spec(&[], e, rows(start + 4050 * MS, start + 5050 * MS, 100 * MS)),
        ],
        1000,
    );
    let (fetched, g) = fetches_to_group(&path);
    assert_eq!(groups(&g), vec![vec![0, 1]]);
    assert_eq!(fetched, 1);
}

/// What a [`Tampered`] catalog does to reads after open.
#[derive(Clone, Copy, PartialEq)]
enum Tamper {
    None,
    /// Segment reads fail.
    Fail,
    /// Segments are gone (retention took them since open).
    Gone,
    /// The WAL's oldest row is gone from it (sealed since open).
    Sealed,
}

/// A catalog that answers as dendro's does until `tamper` is set.
struct Tampered(DendroCatalog, std::sync::Arc<std::sync::Mutex<Tamper>>);

impl Tampered {
    fn mode(&self) -> Tamper {
        *self.1.lock().unwrap()
    }
}

impl Catalog for Tampered {
    fn sources(&self) -> Result<Vec<Source>, String> {
        self.0.sources()
    }
    fn sources_with_uuids(&self) -> Result<Vec<(Source, Option<String>)>, String> {
        self.0.sources_with_uuids()
    }
    fn tables(&self, source_id: i64) -> Result<Vec<String>, String> {
        self.0.tables(source_id)
    }
    fn segment_meta(&self, source_id: i64, table: &str) -> Result<Vec<(u64, SegmentMeta)>, String> {
        self.0.segment_meta(source_id, table)
    }
    fn segment_bytes(
        &self,
        source_id: i64,
        table: &str,
        seq: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        match self.mode() {
            Tamper::Fail => Err("injected read failure".to_string()),
            Tamper::Gone => Ok(None),
            _ => self.0.segment_bytes(source_id, table, seq),
        }
    }
    fn live_wal(&self, source_id: i64, table: &str) -> Result<Vec<WalRow>, String> {
        let mut rows = self.0.live_wal(source_id, table)?;
        if self.mode() == Tamper::Sealed && !rows.is_empty() {
            rows.remove(0);
        }
        Ok(rows)
    }
    fn segment_span(&self, source_id: i64, table: &str) -> Result<(u64, Span), String> {
        self.0.segment_span(source_id, table)
    }
    fn live_wal_span(&self, source_id: i64, table: &str) -> Result<Span, String> {
        self.0.live_wal_span(source_id, table)
    }
    fn caller_row_streams(&self, source_id: i64) -> Result<Vec<String>, String> {
        self.0.caller_row_streams(source_id)
    }
    fn caller_rows(
        &self,
        source_id: i64,
        stream: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, String> {
        self.0.caller_rows(source_id, stream, from, to)
    }
    fn last_caller_row_at_or_before(
        &self,
        source_id: i64,
        stream: &str,
        upto: u64,
        pred: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<Option<u64>, String> {
        self.0
            .last_caller_row_at_or_before(source_id, stream, upto, pred)
    }
}

/// Group the archive at `path` with reads tampered with after open.
fn group_tampered(path: &Path, tamper: Tamper) -> ProcessGroups {
    let mode = std::sync::Arc::new(std::sync::Mutex::new(Tamper::None));
    let recordings = ArchiveReader::from_catalog(
        Box::new(Tampered(
            DendroCatalog::open(path).unwrap(),
            std::sync::Arc::clone(&mode),
        )),
        None,
        BufferPool::new(64 << 20),
        None,
    )
    .unwrap();
    let reader = ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None);
    *mode.lock().unwrap() = tamper;
    reader.process_groups().clone()
}

/// The straddle fixture: a 1 s source whose rows are all in one sealed
/// segment, and a 100 ms source of the same epoch that reaches its row at
/// 5 s, so joining them reads that segment.
fn straddle(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("straddle.dendro");
    let start = at(10, 0);
    let e = &[("e", start)];
    write(
        &path,
        &[
            spec(&[], e, rows(start, start + 60 * S, S)),
            spec(&[], e, rows(start + 4050 * MS, start + 5050 * MS, 100 * MS)),
        ],
        1000,
    );
    path
}

/// The notice for the fixtures' two sources when their rows cannot be told.
fn unread() -> Vec<Notice> {
    vec![Notice::RowsUnread {
        epoch: "e".into(),
        uuid_groups: [0, 1],
    }]
}

/// A row check that cannot read the segment it needs leaves the pair
/// unjoined and says so, rather than reporting that the sources share no
/// row. Reads fail only after open, so only the grouping's read of the
/// straddling segment fails.
#[test]
fn an_unreadable_segment_is_unknown_not_absent() {
    let dir = tempfile::tempdir().unwrap();
    let g = group_tampered(&straddle(dir.path()), Tamper::Fail);
    assert_eq!(groups(&g), vec![vec![0], vec![1]]);
    assert_eq!(g.notices, unread());
}

/// The same when the straddling segment is gone since open (retention).
#[test]
fn a_segment_gone_since_open_is_unknown_not_absent() {
    let dir = tempfile::tempdir().unwrap();
    let g = group_tampered(&straddle(dir.path()), Tamper::Gone);
    assert_eq!(groups(&g), vec![vec![0], vec![1]]);
    assert_eq!(g.notices, unread());
}

/// WAL rows sealed since open are no longer in the WAL, so not finding one
/// there says nothing: the live fixture, whose short source lies between two
/// rows of the long one, with the long one's oldest WAL row moved out.
/// Untampered it gets the shared-no-row notice (`rows_in_the_live_wal_count`).
#[test]
fn wal_rows_sealed_since_open_are_unknown_not_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.dendro");
    let start = at(10, 0);
    let mut writer = ArchiveWriter::create(&path, WriterConfig::default()).unwrap();
    let mut add = |rows: &[u64]| {
        let metadata = [("producer_epoch".to_string(), "e".to_string())].into();
        let mut source = writer.add_source(BTreeMap::new(), metadata, BASE).unwrap();
        let staged: Vec<_> = rows
            .iter()
            .map(|&ts| source.stage(&snapshot(ts), ts, 0).unwrap())
            .collect();
        (source, staged)
    };
    let (mut long, a) = add(&rows(start, start + 60 * S, S));
    let (mut short, b) = add(&rows(start + 4050 * MS, start + 4950 * MS, 100 * MS));
    for s in a.into_iter().chain(b) {
        writer.commit(vec![s]).unwrap();
    }
    long.sync().unwrap();
    short.sync().unwrap();
    writer.join().unwrap();
    std::mem::forget(long);
    std::mem::forget(short);

    let g = group_tampered(&path, Tamper::Sealed);
    assert_eq!(groups(&g), vec![vec![0], vec![1]]);
    assert_eq!(g.notices, unread());
}
