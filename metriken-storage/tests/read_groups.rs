//! `ArchiveReader::open_read_groups` and `open_read_groups_from_bytes`: a
//! dendro archive opened as one reader per read group. The cases are the
//! ones that decide a group (one producer epoch over overlapping or disjoint
//! spans, different epochs, a source and its copy, a snapshot beside its
//! original), the group's metadata (`process::merge_metadata`), its labels,
//! and its position. Fixtures are written with `ArchiveWriter`, copies
//! sharing a uuid with dendro's `copy_sources_into`. The legacy containers
//! (a parquet file and the committed `.rez` fixtures under `tests/fixtures`)
//! are refused.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use dendro::archive::{Archive, ArchiveMut};
use dendro::rewrite::{copy_sources_into, CopySpec};
use metriken_exposition::{Counter, Snapshot, SnapshotV2};
use metriken_query::MetricsSource;
use metriken_storage::process::EVENTS;
use metriken_storage::writer::Encoder;
use metriken_storage::{
    ArchiveReader, ArchiveWriter, BufferPool, DendroCatalog, LabeledRecordings, WriterConfig,
};
use serde_json::{json, Value};

const MS: u64 = 1_000_000;
const S: u64 = 1_000 * MS;
const BASE: u64 = 1_700_000_000 * S;

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// One source to write: labels, metadata, and rows at `BASE + t` seconds.
struct Spec {
    labels: BTreeMap<String, String>,
    metadata: BTreeMap<String, String>,
    ticks: std::ops::Range<u64>,
}

/// A source of `producer_epoch = epoch` labelled `source=rezolus`, with rows
/// at `ticks`.
fn epoch(epoch: &str, ticks: std::ops::Range<u64>) -> Spec {
    Spec {
        labels: map(&[("source", "rezolus")]),
        metadata: map(&[("producer_epoch", epoch)]),
        ticks,
    }
}

/// A counter `cpu_cycles` growing 10 a second, read 1 ms before each stamp.
fn snapshot(ts: u64) -> Snapshot {
    let metadata: HashMap<String, String> = [("metric", "cpu_cycles"), ("sampler", "cpu")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Snapshot::V2(SnapshotV2 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::ZERO,
        metadata: HashMap::new(),
        counters: vec![Counter::new("0".into(), 10 * (ts - BASE) / S, metadata)
            .with_window(Some(metriken::Window::new(ts - MS, ts)))],
        gauges: Vec::new(),
        histograms: Vec::new(),
        gauge_histograms: Vec::new(),
    })
}

/// Write `specs` as the sources of a new archive at `path`, each with a
/// fresh uuid.
fn write(path: &Path, specs: &[Spec]) {
    let mut writer = ArchiveWriter::create(path, WriterConfig::default()).unwrap();
    for spec in specs {
        let mut source = writer
            .add_source(spec.labels.clone(), spec.metadata.clone(), BASE)
            .unwrap();
        let mut last = 0;
        for t in spec.ticks.clone() {
            let ts = BASE + t * S;
            let staged = source.stage(&snapshot(ts), ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            last = ts;
        }
        source.finalize((last, 0)).unwrap();
    }
    writer.join().unwrap();
}

/// Copy every source of `from` into `into`, keeping each uuid; `range`
/// narrows the copy to the segments holding rows in it (a snapshot).
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

/// A new archive at `out` holding copies of every source of each of
/// `inputs`, in order, as `recording combine` assembles one.
fn combine(inputs: &[(&Path, Option<(u64, u64)>)], out: &Path) {
    let mut archive = ArchiveMut::create(out).unwrap();
    for (input, range) in inputs {
        copy(input, &mut archive, *range);
    }
}

fn pool() -> std::sync::Arc<BufferPool> {
    BufferPool::new(64 << 20)
}

/// The groups of `path`, opened by path and, checked to agree, by bytes.
/// One reader per source of the archive at `path`.
fn open_recordings(path: &Path) -> LabeledRecordings {
    let catalog = DendroCatalog::open(path).unwrap();
    ArchiveReader::from_catalog(Box::new(catalog), None, pool(), None).unwrap()
}

fn groups(path: &Path) -> LabeledRecordings {
    let by_path = ArchiveReader::open_read_groups(path, pool()).unwrap();
    let by_bytes =
        ArchiveReader::open_read_groups_from_bytes(std::fs::read(path).unwrap(), pool()).unwrap();
    assert_eq!(by_path.len(), by_bytes.len(), "{}", path.display());
    for ((pl, p), (bl, b)) in by_path.iter().zip(&by_bytes) {
        assert_eq!(pl, bl);
        assert_eq!(p.sources(), b.sources());
        assert_eq!(p.metadata(), b.metadata());
        assert_eq!(p.counter_names(), b.counter_names());
        assert_eq!(p.time_range_ns(), b.time_range_ns());
    }
    by_path
}

/// Each group's sources, by catalog index.
fn members(groups: &LabeledRecordings) -> Vec<Vec<usize>> {
    groups
        .iter()
        .map(|(_, r)| r.sources().iter().map(|s| s.catalog_index).collect())
        .collect()
}

/// The groups agree with `ArchiveReader::read_groups` over every source of
/// the archive joined, mapped to catalog indexes: the grouping is that one.
fn assert_same_as_joined(path: &Path, groups: &LabeledRecordings) {
    let joined = ArchiveReader::flatten(
        open_recordings(path).into_iter().map(|(_, r)| r).collect(),
        None,
    );
    let want: Vec<(Vec<usize>, BTreeMap<String, String>)> = joined
        .read_groups()
        .iter()
        .map(|g| {
            (
                g.sources
                    .iter()
                    .map(|&s| joined.sources()[s].catalog_index)
                    .collect(),
                g.labels.clone(),
            )
        })
        .collect();
    let got: Vec<(Vec<usize>, BTreeMap<String, String>)> = members(groups)
        .into_iter()
        .zip(groups.iter().map(|(l, _)| l.clone()))
        .collect();
    assert_eq!(got, want, "{}", path.display());
}

/// `sum(irate(cpu_cycles[3s]))` over the reader's span, as JSON.
fn answer(r: &dyn MetricsSource) -> Value {
    let (start, end) = r.time_range().unwrap();
    serde_json::to_value(
        r.query_range("sum(irate(cpu_cycles[3s]))", start, end, 1.0)
            .unwrap(),
    )
    .unwrap()
}

/// Two sources of one epoch whose spans overlap are one recording, read
/// once (the sum is not doubled); disjoint spans of one epoch are two;
/// different epochs are two (the control).
#[test]
fn producer_epoch_decides_which_sources_are_one_recording() {
    let dir = tempfile::tempdir().unwrap();
    let at = |name: &str, specs: &[Spec]| -> (PathBuf, LabeledRecordings) {
        let path = dir.path().join(name);
        write(&path, specs);
        let g = groups(&path);
        assert_same_as_joined(&path, &g);
        (path, g)
    };
    let (_, one) = at("one.dendro", &[epoch("E", 0..6)]);
    assert_eq!(members(&one), [vec![0]]);

    let (_, twice) = at("twice.dendro", &[epoch("E", 0..6), epoch("E", 0..6)]);
    assert_eq!(members(&twice), [vec![0, 1]], "one process recorded twice");
    assert_eq!(answer(&twice[0].1), answer(&one[0].1), "read once");
    let uuids: Vec<_> = twice[0].1.sources().iter().map(|s| &s.uuid).collect();
    assert!(uuids[0].is_some() && uuids[0] != uuids[1], "two uuids");

    let (_, apart) = at("apart.dendro", &[epoch("E", 0..4), epoch("E", 10..14)]);
    assert_eq!(members(&apart), [vec![0], vec![1]], "one epoch, disjoint");

    let (_, other) = at("other.dendro", &[epoch("E", 0..6), epoch("F", 0..6)]);
    assert_eq!(members(&other), [vec![0], vec![1]], "different epochs");
    // A source of no group is its own reader: what `open_recordings` opens.
    let alone = open_recordings(&dir.path().join("other.dendro"));
    for ((gl, g), (al, a)) in other.iter().zip(&alone) {
        assert_eq!(
            (gl, g.sources(), g.metadata()),
            (al, a.sources(), a.metadata())
        );
        assert_eq!(answer(g), answer(a));
    }
}

/// A recording and a copy of it (one uuid), and a recording beside a
/// snapshot of part of it (a ranged copy, one uuid), are one recording each,
/// answering as the recording alone.
#[test]
fn a_copy_or_a_snapshot_beside_its_original_is_one_recording() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.dendro");
    write(&a, &[epoch("E", 0..20)]);
    let alone = groups(&a);

    let whole = dir.path().join("with-copy.dendro");
    combine(&[(&a, None), (&a, None)], &whole);
    let snapshot = dir.path().join("with-snapshot.dendro");
    combine(
        &[(&a, Some((BASE + 5 * S, BASE + 9 * S))), (&a, None)],
        &snapshot,
    );
    for path in [&whole, &snapshot] {
        let g = groups(path);
        assert_same_as_joined(path, &g);
        assert_eq!(members(&g), [vec![0, 1]], "{}", path.display());
        let uuids: Vec<_> = g[0].1.sources().iter().map(|s| s.uuid.clone()).collect();
        assert_eq!(uuids[0], uuids[1], "copies keep the uuid");
        assert_eq!(g[0].0, alone[0].0, "the original's labels");
        assert_eq!(answer(&g[0].1), answer(&alone[0].1), "{}", path.display());
    }
}

/// A group's position is its first source's, as a listing shows it: an
/// original, its copy and a later recording are three sources and two
/// recordings, the original with its copy first.
#[test]
fn positions_name_recordings_not_sources() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.dendro");
    write(&a, &[epoch("E", 0..6)]);
    let later = dir.path().join("later.dendro");
    write(&later, &[epoch("F", 10..16)]);
    let three = dir.path().join("three.dendro");
    combine(&[(&a, None), (&later, None), (&a, None)], &three);
    let g = groups(&three);
    assert_same_as_joined(&three, &g);
    assert_eq!(members(&g), [vec![0, 2], vec![1]]);
}

/// Each metadata key of a group is the first source's that holds it, in
/// either order of the sources; a key one holds is the group's either way.
/// What `MetricsSource` reports is the group's.
#[test]
fn a_groups_metadata_is_the_first_source_holding_each_key() {
    let dir = tempfile::tempdir().unwrap();
    let source = |k: &str, only: Option<&str>| {
        let mut spec = epoch("E", 0..4);
        spec.metadata.insert("k".into(), k.into());
        if let Some(only) = only {
            spec.metadata.insert("only".into(), only.into());
        }
        spec
    };
    for (name, specs, want_k) in [
        (
            "first.dendro",
            [source("first", None), source("second", Some("x"))],
            "first",
        ),
        (
            "second.dendro",
            [source("second", Some("x")), source("first", None)],
            "second",
        ),
    ] {
        let path = dir.path().join(name);
        write(&path, &specs);
        let g = groups(&path);
        assert_eq!(members(&g), [vec![0, 1]], "{name}");
        let reader = &g[0].1;
        assert_eq!(reader.metadata()["k"], want_k, "{name}");
        assert_eq!(reader.metadata()["only"], "x", "{name}");
        assert_eq!(reader.metadata_get("only").as_deref(), Some("x"), "{name}");
        assert_eq!(reader.file_metadata()["k"], want_k, "{name}");
    }
}

/// A group's events are its sources' events, each id once (the first
/// source's copy), sorted by timestamp.
#[test]
fn a_groups_events_are_the_union_of_its_sources() {
    let dir = tempfile::tempdir().unwrap();
    let with_events = |list: Value| {
        let mut spec = epoch("E", 0..4);
        spec.metadata
            .insert(EVENTS.into(), json!({ EVENTS: list }).to_string());
        spec
    };
    let path = dir.path().join("events.dendro");
    write(
        &path,
        &[
            with_events(json!([
                {"id": "check", "timestamp": BASE + 3 * S, "kind": "check"},
                {"id": "deploy", "timestamp": BASE + 2 * S, "description": "first's"},
            ])),
            with_events(json!([
                {"id": "deploy", "timestamp": BASE + 2 * S, "description": "second's"},
                {"id": "start", "timestamp": BASE, "kind": "run_start"},
            ])),
        ],
    );
    let g = groups(&path);
    assert_eq!(members(&g), [vec![0, 1]]);
    let payload: Value = serde_json::from_str(&g[0].1.metadata()[EVENTS]).unwrap();
    assert_eq!(
        payload[EVENTS],
        json!([
            {"id": "start", "timestamp": BASE, "kind": "run_start"},
            {"id": "deploy", "timestamp": BASE + 2 * S, "description": "first's"},
            {"id": "check", "timestamp": BASE + 3 * S, "kind": "check"},
        ])
    );
}

/// A group's labels are every key some of its sources carry; two values of
/// one key make two groups.
#[test]
fn a_groups_labels_are_its_sources() {
    let dir = tempfile::tempdir().unwrap();
    let labelled = |pairs: &[(&str, &str)]| {
        let mut spec = epoch("E", 0..4);
        spec.labels.extend(map(pairs));
        spec
    };
    let path = dir.path().join("labels.dendro");
    write(&path, &[labelled(&[]), labelled(&[("arm", "x")])]);
    let g = groups(&path);
    assert_same_as_joined(&path, &g);
    assert_eq!(members(&g), [vec![0, 1]]);
    assert_eq!(g[0].0, map(&[("arm", "x"), ("source", "rezolus")]));
    // The series carry them.
    let labels = g[0].1.counter_labels("cpu_cycles");
    assert!(
        labels
            .iter()
            .all(|l| l.get("arm").map(String::as_str) == Some("x")),
        "{labels:?}"
    );

    let path = dir.path().join("arms.dendro");
    write(
        &path,
        &[labelled(&[("arm", "a")]), labelled(&[("arm", "b")])],
    );
    let g = groups(&path);
    assert_same_as_joined(&path, &g);
    assert_eq!(members(&g), [vec![0], vec![1]]);
    let arms: Vec<&str> = g.iter().map(|(l, _)| l["arm"].as_str()).collect();
    assert_eq!(arms, ["a", "b"]);
}

/// A group's series carry its labels, including a value filled from a copy
/// outside the group: a source and its copy (one uuid, `arm=a,host=1`), the
/// copy relabelled `host=2` without `arm`, and another source of the epoch
/// (`arm=b,host=2`) are three groups, and the copy's group takes `arm=a`
/// from its original. Its reader's series carry `arm=a` as the joined
/// reader's do, so `cpu_cycles{arm="a"}` matches in both.
#[test]
fn a_filled_label_is_on_the_groups_series() {
    // One relabelled copy (a group of one source), then two (a group of two
    // sources, neither carrying `arm`).
    for copies in [1usize, 2] {
        filled_label_case(copies);
    }
}

fn filled_label_case(copies: usize) {
    let dir = tempfile::tempdir().unwrap();
    let labelled = |pairs: &[(&str, &str)]| {
        let mut spec = epoch("E", 0..6);
        spec.labels.extend(map(pairs));
        spec
    };
    let a = dir.path().join("a.dendro");
    write(&a, &[labelled(&[("arm", "a"), ("host", "1")])]);
    let b = dir.path().join("b.dendro");
    write(&b, &[labelled(&[("arm", "b"), ("host", "2")])]);
    let out = dir.path().join("out.dendro");
    let mut inputs: Vec<(&Path, Option<(u64, u64)>)> = vec![(&a, None); copies + 1];
    inputs.push((&b, None));
    combine(&inputs, &out);
    {
        let mut archive = ArchiveMut::open(&out).unwrap();
        let ids: Vec<i64> = archive
            .read_sources()
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        for &copy in &ids[1..=copies] {
            archive
                .update_source_labels(copy, &map(&[("host", "2"), ("source", "rezolus")]))
                .unwrap();
        }
    }
    let g = groups(&out);
    assert_same_as_joined(&out, &g);
    let want = map(&[("arm", "a"), ("host", "2"), ("source", "rezolus")]);
    assert_eq!(
        members(&g),
        [vec![0], (1..=copies).collect(), vec![copies + 1]]
    );
    assert_eq!(g[1].0, want);
    let carried = |labels: &[BTreeMap<String, String>]| {
        labels
            .iter()
            .filter(|l| l.get("arm").map(String::as_str) == Some("a"))
            .filter(|l| l.get("host").map(String::as_str) == Some("2"))
            .count()
    };
    let in_group = g[1].1.counter_labels("cpu_cycles");
    assert!(!in_group.is_empty());
    assert_eq!(carried(&in_group), in_group.len(), "{in_group:?}");
    let joined = ArchiveReader::flatten(
        open_recordings(&out).into_iter().map(|(_, r)| r).collect(),
        None,
    );
    assert_eq!(carried(&joined.counter_labels("cpu_cycles")), 1);
    let selected = |r: &dyn MetricsSource| {
        let (_, end) = r.time_range().unwrap();
        match r
            .query(r#"irate(cpu_cycles{arm="a",host="2"}[3s])"#, Some(end))
            .unwrap()
        {
            metriken_query::QueryResult::Vector { result, .. } => result.len(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(selected(&g[1].1), 1);
    assert_eq!(selected(&joined), 1);
    // Readers a caller has already listed series from, grouped after.
    let listed = open_recordings(&out);
    for (_, r) in &listed {
        r.counter_labels("cpu_cycles");
    }
    let regrouped = ArchiveReader::flatten_read_groups(listed);
    let after = regrouped[1].1.counter_labels("cpu_cycles");
    assert_eq!(carried(&after), after.len(), "{after:?}");
}

/// A reader of several sources, or of none, is passed through at its
/// position in the input; readers of one source around it are grouped, each
/// group at its first reader's position.
#[test]
fn a_reader_of_several_sources_is_kept_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("four.dendro");
    write(
        &path,
        &[
            epoch("E", 0..4),
            epoch("F", 0..4),
            epoch("G", 0..4),
            epoch("E", 0..4),
        ],
    );
    let mut sources = open_recordings(&path).into_iter();
    let (labels, first) = sources.next().unwrap();
    let joined = ArchiveReader::flatten(
        vec![sources.next().unwrap().1, sources.next().unwrap().1],
        None,
    );
    let (_, last) = sources.next().unwrap();
    let empty = ArchiveReader::from_in_memory(Vec::new(), None, pool()).unwrap();
    // The joined reader first, so the group after it must be ordered behind
    // it.
    let got = ArchiveReader::flatten_read_groups(vec![
        (map(&[("kept", "joined")]), joined),
        (labels.clone(), first),
        (map(&[("kept", "empty")]), empty),
        (labels, last),
    ]);
    let shape: Vec<(Option<String>, Vec<usize>)> = got
        .iter()
        .map(|(l, r)| {
            (
                l.get("kept").cloned(),
                r.sources().iter().map(|s| s.catalog_index).collect(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            (Some("joined".into()), vec![1, 2]),
            (None, vec![0, 3]),
            (Some("empty".into()), vec![]),
        ]
    );
}

fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(path)
}

/// Each legacy container is refused by its first bytes, by path and by
/// bytes, with a message naming the conversion; a dendro archive beside
/// them opens.
#[test]
fn legacy_containers_are_refused() {
    for (name, kind) in [
        ("legacy/legacy-0.4.0.parquet", "a standalone parquet file"),
        ("rez/v3.rez", "a SQLite file without dendro's stamp"),
        ("rez/v3-multi.rez", "a SQLite file without dendro's stamp"),
        ("rez/v2.rez", "a tar archive"),
        ("rez/v2-partial.rez", "a tar archive"),
    ] {
        let path = fixture(name);
        let by_path = ArchiveReader::open_read_groups(&path, pool())
            .err()
            .unwrap_or_else(|| panic!("{name} opened"))
            .to_string();
        let by_bytes =
            ArchiveReader::open_read_groups_from_bytes(std::fs::read(&path).unwrap(), pool())
                .err()
                .unwrap_or_else(|| panic!("{name} opened from bytes"))
                .to_string();
        for err in [&by_path, &by_bytes] {
            assert!(err.contains(kind), "{name}: {err}");
            assert!(err.contains("rezolus recording upgrade"), "{name}: {err}");
            assert!(err.contains("to_dendro::convert_path"), "{name}: {err}");
        }
        assert!(by_path.contains(&path.display().to_string()), "{by_path}");
    }
    // The control: a committed dendro archive opens (copied, since opening
    // one creates SQLite sidecars beside it).
    let dir = tempfile::tempdir().unwrap();
    let dendro = dir.path().join("legacy-0.4.0.dendro");
    std::fs::copy(fixture("legacy/legacy-0.4.0.dendro"), &dendro).unwrap();
    assert_eq!(groups(&dendro).len(), 1);
}

/// A reader opened by path reads its tables from the path on first query,
/// and refuses to once another file has been renamed over it. The check
/// compares inodes, so it runs only where files have them.
#[cfg(unix)]
#[test]
fn a_file_replaced_after_the_open_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    write(&path, &[epoch("E", 0..6)]);
    let other = dir.path().join("b.dendro");
    write(&other, &[epoch("F", 0..6)]);
    let opened = ArchiveReader::open_read_groups(&path, pool()).unwrap();
    std::fs::rename(&other, &path).unwrap();
    let (start, end) = opened[0].1.time_range().unwrap();
    let got = opened[0]
        .1
        .query_range("sum(irate(cpu_cycles[3s]))", start, end, 1.0);
    assert!(got.is_err(), "the replacement was read: {got:?}");
    // The control: a reader whose file stays in place answers.
    let kept = ArchiveReader::open_read_groups(&path, pool()).unwrap();
    let (start, end) = kept[0].1.time_range().unwrap();
    kept[0]
        .1
        .query_range("sum(irate(cpu_cycles[3s]))", start, end, 1.0)
        .unwrap();
}

/// The timestamps, in seconds after `BASE`, at which `sum(irate(...))` has a
/// value over `[0 s, 30 s]` under `scope`.
fn scoped_points(r: &ArchiveReader, scope: Option<metriken_query::Scope>) -> Vec<u64> {
    let opts = metriken_query::QueryOptions::default().with_scope(scope.map(std::sync::Arc::new));
    let start = BASE as f64 / 1e9;
    let result = r
        .query_range_opts(
            "sum(irate(cpu_cycles[3s]))",
            start,
            start + 30.0,
            1.0,
            &opts,
        )
        .unwrap();
    let metriken_query::QueryResult::Matrix { result, .. } = result else {
        panic!("not a matrix");
    };
    result
        .iter()
        .flat_map(|m| m.values.iter().map(|(t, _)| (*t - start).round() as u64))
        .collect()
}

/// A scope reads only the sources it names: within a process group of two
/// sources it unions only the named one (S1), by position or by uuid, and a
/// span keeps a source's rows within it. A scope naming no source holding
/// the metric reads nothing.
#[test]
fn a_scope_reads_only_the_sources_it_names() {
    use metriken_query::{Scope, ScopedSource, SourceRef};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group.dendro");
    write(&path, &[epoch("e1", 0..20), epoch("e1", 10..30)]);
    let mut opened = groups(&path);
    assert_eq!(opened.len(), 1, "one process group");
    let (_, reader) = opened.remove(0);
    assert_eq!(reader.sources().len(), 2);
    let scope = |entries: Vec<(SourceRef, Option<(u64, u64)>)>| {
        Some(
            Scope::new(
                entries
                    .into_iter()
                    .map(|(source, span)| ScopedSource::new(source, span))
                    .collect(),
            )
            .unwrap(),
        )
    };

    let all = scoped_points(&reader, None);
    assert!(
        all.first() <= Some(&1) && all.last() >= Some(&29),
        "{all:?}"
    );
    let first = scoped_points(&reader, scope(vec![(SourceRef::Position(0), None)]));
    assert!(first.last() <= Some(&19) && !first.is_empty(), "{first:?}");
    let uuid = reader.sources()[1]
        .uuid
        .clone()
        .expect("a dendro source has a uuid");
    let second = scoped_points(&reader, scope(vec![(SourceRef::Uuid(uuid), None)]));
    assert!(second.first() >= Some(&10), "{second:?}");
    assert_eq!(second.last(), all.last());

    let span = (BASE + 5 * S, BASE + 12 * S);
    let clipped = scoped_points(&reader, scope(vec![(SourceRef::Position(0), Some(span))]));
    assert!(
        !clipped.is_empty() && clipped.iter().all(|t| (5..=12).contains(t)),
        "{clipped:?}"
    );

    let none = scoped_points(&reader, scope(vec![(SourceRef::Position(7), None)]));
    assert!(none.is_empty());
}
