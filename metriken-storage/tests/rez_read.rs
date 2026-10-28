//! `.rez` archives read through the published reader: each container shape
//! (v2 tar written whole, v2 tar streamed and never finalized, v3 SQLite
//! finalized, v3 live with a WAL tail, v3 with two recordings) opened by
//! path and by bytes, and queried through metriken-query.
//!
//! Nothing in this crate writes a `.rez` any more, so `tests/fixtures/rez/`
//! was written once by the last writers this crate had (`RezRecorder`,
//! `rez_stream::StreamRecorder` and `rez_v3_writer::StreamRecorderV3`) at
//! 0c39b2d (after 0.4.0; output kept at 0.4.0's shape). Every recording but
//! `v3-multi.rez` holds 12 ticks, 1 s apart from 1700000000 s, labelled
//! `source=rezolus,host=web-01`: `ops` (a windowed counter, +10 a tick,
//! sampler `a`), `mem_free` (a gauge, 1000 − t, sampler `b`) and `lat` (a
//! windowed counting histogram at grouping power 2, max value power 8, one
//! more observation of 2 each tick, sampler `a`). `v2.rez`, `v2-partial.rez` and `v3.rez` were written from V2
//! snapshots (one table per sampler), `v3-live.rez` from V3 snapshots (group
//! tables `a/g` and `b/g`).
//!
//! - `v2.rez`: `RezRecorder::finalize`, one segment per table, complete.
//! - `v2-partial.rez`: the streamed tar writer at four rows a segment,
//!   dropped without finalize (the `.partial` file), so it ends at its last
//!   checkpoint manifest and is incomplete.
//! - `v3.rez`: `StreamRecorderV3` at four rows a segment, finalized.
//! - `v3-live.rez`: `StreamRecorderV3` at five rows a segment, not
//!   finalized; both tables have sealed segments and live WAL rows (copied
//!   with `VACUUM INTO` while the writer held it).
//! - `v3-multi.rez`: two recordings, `host=web-01` and `host=web-02`, three
//!   ticks of `mem_free` each, valued `(i + 1) * 1000 + t` for recording
//!   `i`.
#![cfg(feature = "rez")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use metriken_query::{MetricsSource, QueryResult};
use metriken_storage::rez::catalog::Container;
use metriken_storage::rez::open::{open_recordings, open_recordings_from_bytes, open_with_pool};
use metriken_storage::rez::rez::{detect_rez_format, is_rez_path, RezFormat};
use metriken_storage::rez::rez_sqlite::RezDb;
use metriken_storage::{ArchiveReader, BufferPool, LabeledRecordings};

const BASE_S: f64 = 1_700_000_000.0;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rez")
        .join(name)
}

/// A copy of the fixture: opening a v3 archive read-write may create SQLite
/// sidecars beside it.
fn copy(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::copy(fixture(name), &path).unwrap();
    (dir, path)
}

fn pool() -> std::sync::Arc<BufferPool> {
    BufferPool::new(64 << 20)
}

fn series(result: QueryResult) -> Vec<(f64, f64)> {
    match result {
        QueryResult::Matrix { result } => {
            assert_eq!(result.len(), 1, "{result:?}");
            result.into_iter().next().unwrap().values
        }
        other => panic!("{other:?}"),
    }
}

fn query(reader: &ArchiveReader, q: &str, from: u64, to: u64) -> Vec<(f64, f64)> {
    series(
        reader
            .query_range(q, BASE_S + from as f64, BASE_S + to as f64, 1.0)
            .unwrap(),
    )
}

fn at(from: u64, to: u64, f: impl Fn(u64) -> f64) -> Vec<(f64, f64)> {
    (from..=to).map(|t| (BASE_S + t as f64, f(t))).collect()
}

fn web_01() -> BTreeMap<String, String> {
    [("source", "rezolus"), ("host", "web-01")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The three metrics of a 12-tick fixture, through `last` (the newest tick
/// the recording holds).
fn assert_holds_the_ticks(reader: &ArchiveReader, last: u64) {
    assert_eq!(reader.counter_names(), ["ops"]);
    assert_eq!(reader.gauge_names(), ["mem_free"]);
    assert_eq!(reader.histogram_names(), ["lat"]);
    assert_eq!(
        query(reader, "irate(ops[5s])", 1, last),
        at(1, last, |_| 10.0)
    );
    assert_eq!(
        query(reader, "mem_free", 0, last),
        at(0, last, |t| 1000.0 - t as f64)
    );
    assert_eq!(
        query(reader, "histogram_count(lat)", 1, last),
        at(1, last, |_| 1.0)
    );
}

fn only(mut recordings: LabeledRecordings) -> ArchiveReader {
    assert_eq!(recordings.len(), 1);
    let (labels, reader) = recordings.remove(0);
    assert_eq!(labels, web_01());
    reader
}

#[test]
fn a_v2_tar_reads() {
    let path = fixture("v2.rez");
    assert_eq!(detect_rez_format(&path).unwrap(), RezFormat::V2Tar);
    assert!(is_rez_path(&path).unwrap());
    assert!(Container::of_path(&path).unwrap().is_none());

    let reader = only(open_recordings(&path, pool()).unwrap());
    assert!(reader.complete());
    assert_holds_the_ticks(&reader, 11);

    let bytes = std::fs::read(&path).unwrap();
    let reader = only(open_recordings_from_bytes(bytes, pool()).unwrap());
    assert_holds_the_ticks(&reader, 11);
}

/// An unfinalized streamed tar reads to its last checkpoint manifest:
/// incomplete, with the rows of the segments that manifest names.
#[test]
fn a_v2_tar_that_was_never_finalized_reads_to_its_last_checkpoint() {
    let path = fixture("v2-partial.rez");
    assert_eq!(detect_rez_format(&path).unwrap(), RezFormat::V2Tar);
    let (manifest, recordings) = metriken_storage::rez::rez::read_archive_bytes(&path).unwrap();
    assert!(!manifest.recordings[0].complete);
    assert!(
        recordings[0].tables.iter().any(|(_, s)| s.len() > 1),
        "the fixture has a table of several segments"
    );

    let reader = only(open_recordings(&path, pool()).unwrap());
    assert!(!reader.complete());
    let ops = query(&reader, "irate(ops[5s])", 1, 11);
    assert!(ops.len() >= 4, "{ops:?}");
    let last = ops.last().unwrap().0 - BASE_S;
    assert_holds_the_ticks(&reader, last as u64);
}

/// Kill geometry on the bytes the streamed tar writer produced: every prefix
/// of `v2-partial.rez` cut at a coarse sweep and at each entry's header,
/// data start and mid-data either fails to open (before the first manifest
/// is whole) or opens incomplete with exactly the segments its newest
/// surviving manifest names. The hand-built tars in `rez.rs`'s
/// `recovery_tests` cover the reader's rules; this covers the writer's
/// output.
#[test]
fn truncated_writer_bytes_recover_the_sealed_prefix() {
    let bytes = std::fs::read(fixture("v2-partial.rez")).unwrap();

    let mut cuts: Vec<usize> = (0..bytes.len()).step_by(64).collect();
    let mut archive = tar::Archive::new(std::io::Cursor::new(bytes.clone()));
    for entry in archive.entries().unwrap() {
        let entry = entry.unwrap();
        let head = entry.raw_header_position() as usize;
        cuts.push(head);
        cuts.push(head + 512);
        cuts.push(head + 512 + entry.size() as usize / 2);
    }
    cuts.push(bytes.len());
    cuts.retain(|&c| c <= bytes.len());
    cuts.sort_unstable();
    cuts.dedup();

    let mut first_ok: Option<usize> = None;
    let mut recovered_counts: Vec<usize> = Vec::new();
    for cut in cuts {
        let read =
            metriken_storage::rez::rez::read_archive_reader(std::io::Cursor::new(&bytes[..cut]));
        match read {
            Ok((manifest, recordings)) => {
                first_ok.get_or_insert(cut);
                let rec = &manifest.recordings[0];
                assert!(
                    !rec.complete,
                    "a truncated archive is never complete (cut {cut})"
                );
                let named: usize = rec.tables.iter().map(|t| t.files.len()).sum();
                let recovered: usize = recordings[0].tables.iter().map(|(_, s)| s.len()).sum();
                assert_eq!(
                    named, recovered,
                    "the resolved manifest names exactly the segments recovered (cut {cut})"
                );
                assert!(!recordings[0].complete, "cut {cut}");
                recovered_counts.push(recovered);
            }
            Err(e) => assert!(
                first_ok.is_none(),
                "cut {cut} failed after the archive first opened at {first_ok:?}: {e}"
            ),
        }
    }
    // The initial empty manifest makes the archive readable almost at once.
    assert_eq!(first_ok, Some(896));
    // One rung per checkpoint: the initial (empty) manifest, then each seal
    // batch, which seals both samplers' segments together.
    recovered_counts.sort_unstable();
    recovered_counts.dedup();
    assert_eq!(recovered_counts, vec![0, 2, 4, 6]);
}

#[test]
fn a_finalized_v3_reads_across_its_segments() {
    let (_dir, path) = copy("v3.rez");
    assert_eq!(detect_rez_format(&path).unwrap(), RezFormat::V3Sqlite);
    assert!(matches!(
        Container::of_path(&path).unwrap(),
        Some(Container::Rez)
    ));
    let db = RezDb::open(&path).unwrap();
    let rec = &db.read_recordings().unwrap()[0];
    assert!(rec.complete);
    assert!(db.read_segment_meta(rec.id, "a").unwrap().len() > 1);
    drop(db);

    let reader = only(open_recordings(&path, pool()).unwrap());
    assert!(reader.complete());
    assert_holds_the_ticks(&reader, 11);

    let reader = open_with_pool(&path, pool()).unwrap();
    assert_holds_the_ticks(&reader, 11);
}

/// A live archive's WAL tail joins its sealed segments with no tick lost or
/// repeated at the seam.
#[test]
fn a_live_v3_reads_its_wal_tail_after_its_segments() {
    let (_dir, path) = copy("v3-live.rez");
    let db = RezDb::open(&path).unwrap();
    let rec = &db.read_recordings().unwrap()[0];
    assert!(!rec.complete);
    for table in ["a/g", "b/g"] {
        assert!(!db.read_segment_meta(rec.id, table).unwrap().is_empty());
        assert!(!db.live_wal(rec.id, table).unwrap().is_empty());
    }
    drop(db);

    let reader = only(open_recordings(&path, pool()).unwrap());
    assert!(!reader.complete());
    assert_holds_the_ticks(&reader, 11);

    let bytes = std::fs::read(&path).unwrap();
    let reader = only(open_recordings_from_bytes(bytes, pool()).unwrap());
    assert_holds_the_ticks(&reader, 11);
}

#[test]
fn a_two_recording_v3_reads_each_recording_under_its_labels() {
    let (_dir, path) = copy("v3-multi.rez");
    let recordings = open_recordings(&path, pool()).unwrap();
    assert_eq!(recordings.len(), 2);
    for (i, (labels, reader)) in recordings.iter().enumerate() {
        assert_eq!(
            labels.get("host").map(String::as_str),
            Some(["web-01", "web-02"][i])
        );
        let want = ((i as f64) + 1.0) * 1000.0;
        assert_eq!(
            query(reader, "mem_free", 0, 2),
            at(0, 2, |t| want + t as f64)
        );
    }
}

/// `.rez` to dendro keeps every row: the converted live archive reads the
/// same as the `.rez` did, its WAL tail included. Each sealed segment it
/// copies carries a names index.
#[cfg(feature = "write")]
#[test]
fn a_live_v3_converts_to_dendro() {
    use metriken_storage::Catalog;
    let (dir, path) = copy("v3-live.rez");
    let dest = dir.path().join("out.dendro");
    metriken_storage::rez::to_dendro::convert_v3_to_dendro(&path, &dest).unwrap();
    let catalog = metriken_storage::DendroCatalog::open(&dest).unwrap();
    let source = catalog.sources().unwrap().remove(0).id;
    let mut sealed = 0;
    for table in catalog.tables(source).unwrap() {
        for (seq, index) in catalog.segment_indexes(source, &table).unwrap() {
            assert!(
                index.as_deref().is_some_and(|i| i.starts_with(b"MNI1")),
                "{table}#{seq}: {index:?}"
            );
            sealed += 1;
        }
    }
    assert!(sealed > 0, "the fixture has sealed segments");
    let mut recordings =
        ArchiveReader::from_catalog(Box::new(catalog), None, pool(), None).unwrap();
    assert_eq!(recordings.len(), 1);
    let (labels, reader) = recordings.remove(0);
    assert_eq!(labels, web_01());
    assert_holds_the_ticks(&reader, 11);
}

/// dendro does not open a `.rez`: an archive the `.rez` writer made carries
/// no dendro header stamp, so it sniffs as not an archive.
#[test]
fn dendro_does_not_read_a_rez() {
    let (_dir, path) = copy("v3.rez");
    assert!(matches!(
        dendro::archive::sniff(&path).unwrap(),
        dendro::archive::Sniff::NotAnArchive
    ));
    assert!(dendro::archive::Archive::open(&path).is_err());
}
