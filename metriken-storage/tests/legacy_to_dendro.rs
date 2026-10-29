//! Legacy recordings converted to `.dendro` answer as they did: every
//! committed legacy fixture (the v2 tar `.rez` fixtures, a v1 tar made from
//! one, truncated tars, the parquet files under `tests/fixtures/legacy` of
//! this crate and of metriken-query) and synthetic parquet files (combined
//! metadata, per-metric windows, many row groups) is converted, and the
//! original, read through the existing readers, and the archive, read
//! through `ArchiveReader`, are asked the same questions: the metric and
//! series listings, the metadata, and range and instant queries of `rate`,
//! `irate`, gauge reads, `avg_over_time`, `count`, `sum`, `sum by`,
//! `histogram_quantile`, `histogram_count` and `histogram_heatmap`, on the
//! grid, raw and per-series paths. Answers must be equal; a float may
//! differ only in its last places ([`REL`]), and none does in these
//! fixtures.
//!
//! `LEGACY_TO_DENDRO=path` with `-- --ignored converts_a_real_recording`
//! runs the same comparison over any parquet file or tar `.rez`.
#![cfg(all(feature = "rez", feature = "write"))]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array, ListBuilder, UInt64Array, UInt64Builder};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use metriken_query::{MetricsSource, ParquetReader, QueryOptions, QueryResult, RateMode};
use metriken_storage::rez::open::{open_recordings, open_recordings_from_bytes};
use metriken_storage::rez::to_dendro::{convert_tar_into, convert_tar_to_dendro};
use metriken_storage::to_dendro::{ParquetToArchive, Written, PARQUET_TABLE};
use metriken_storage::{ArchiveReader, BufferPool, DendroCatalog, WriterConfig};
use parquet::file::metadata::KeyValue;
use serde_json::Value;

/// The relative difference two floats of one answer may have.
const REL: f64 = 1e-12;

const BASE_NS: u64 = 1_700_000_000_000_000_000;

fn storage_fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(path)
}

fn query_fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../metriken-query/tests/fixtures")
        .join(path)
}

fn pool() -> Arc<BufferPool> {
    BufferPool::new(64 << 20)
}

fn open_archive(path: &Path) -> Vec<(BTreeMap<String, String>, ArchiveReader)> {
    ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(path).unwrap()),
        None,
        pool(),
        None,
    )
    .unwrap()
}

// ─── Answers ────────────────────────────────────────────────────────────────

/// One question and its answer, as JSON (an error as its message).
type Answer = (String, Result<Value, String>);

fn result_json(r: Result<QueryResult, impl std::fmt::Display>) -> Result<Value, String> {
    let mut v = serde_json::to_value(r.map_err(|e| e.to_string())?).unwrap();
    // Series in a fixed order: readers list them in their own.
    if let Some(series) = v.get_mut("result").and_then(Value::as_array_mut) {
        series.sort_by_key(|s| s.get("metric").map(Value::to_string).unwrap_or_default());
    }
    Ok(v)
}

fn sorted_labels(mut labels: Vec<BTreeMap<String, String>>) -> Value {
    labels.sort();
    serde_json::to_value(labels).unwrap()
}

/// The queries asked of every metric, by kind.
fn queries<S: MetricsSource + ?Sized>(src: &S) -> Vec<String> {
    let keys = |labels: Vec<BTreeMap<String, String>>| -> BTreeSet<String> {
        labels.into_iter().flat_map(|l| l.into_keys()).collect()
    };
    let mut out = Vec::new();
    for m in src.counter_names() {
        for q in [
            m.to_string(),
            format!("rate({m}[5s])"),
            format!("irate({m}[5s])"),
            format!("sum(rate({m}[5s]))"),
            format!("sum(irate({m}[5s]))"),
            format!("count({m})"),
        ] {
            out.push(q);
        }
        for k in keys(src.counter_labels(&m)) {
            out.push(format!("sum by ({k}) (rate({m}[5s]))"));
        }
    }
    for m in src.gauge_names() {
        for q in [
            m.to_string(),
            format!("sum({m})"),
            format!("max({m})"),
            format!("count({m})"),
            format!("avg_over_time({m}[5s])"),
        ] {
            out.push(q);
        }
        for k in keys(src.gauge_labels(&m)) {
            out.push(format!("sum by ({k}) ({m})"));
        }
    }
    for m in src.histogram_names() {
        for q in [
            format!("histogram_quantile(0.5, {m})"),
            format!("histogram_quantile(0.99, {m})"),
            format!("histogram_count({m})"),
            format!("histogram_heatmap({m})"),
        ] {
            out.push(q);
        }
    }
    out
}

/// Everything `src` answers over `(start, end)` seconds.
fn answers<S: MetricsSource + ?Sized>(src: &S, (start, end): (f64, f64)) -> Vec<Answer> {
    let mut out: Vec<Answer> = Vec::new();
    let mut listing = |what: &str, v: Value| out.push((what.to_string(), Ok(v)));
    listing(
        "counters",
        serde_json::to_value(src.counter_names()).unwrap(),
    );
    listing("gauges", serde_json::to_value(src.gauge_names()).unwrap());
    listing(
        "histograms",
        serde_json::to_value(src.histogram_names()).unwrap(),
    );
    listing(
        "gauge histograms",
        serde_json::to_value(src.gauge_histogram_names()).unwrap(),
    );
    for m in src.counter_names() {
        listing(
            &format!("labels of {m}"),
            sorted_labels(src.counter_labels(&m)),
        );
    }
    for m in src.gauge_names() {
        listing(
            &format!("labels of {m}"),
            sorted_labels(src.gauge_labels(&m)),
        );
    }
    for m in src.histogram_names() {
        listing(
            &format!("labels of {m}"),
            sorted_labels(src.histogram_labels(&m)),
        );
    }
    listing(
        "time range",
        serde_json::to_value(src.time_range()).unwrap(),
    );
    listing(
        "sample timestamps",
        serde_json::to_value(src.sample_timestamps()).unwrap(),
    );

    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    let per_series = QueryOptions::default().with_per_series_rates(true);
    let mid = (start + end) / 2.0;
    for q in queries(src) {
        for step in [1.0, 0.5, 3.0] {
            out.push((
                format!("{q} @ step {step}"),
                result_json(src.query_range(&q, start, end, step)),
            ));
        }
        out.push((
            format!("{q} raw"),
            result_json(src.query_range_opts(&q, start, end, 1.0, &raw)),
        ));
        out.push((
            format!("{q} per series"),
            result_json(src.query_range_opts(&q, start, end, 1.0, &per_series)),
        ));
        out.push((
            format!("{q} inner range"),
            result_json(src.query_range(&q, start + 2.0, mid, 1.0)),
        ));
        for at in [Some(start + 2.0), Some(mid), Some(end), None] {
            out.push((format!("{q} at {at:?}"), result_json(src.query(&q, at))));
        }
    }
    out
}

/// How two answer sets compared.
#[derive(Debug, Default)]
struct Compared {
    answers: usize,
    numbers: usize,
    /// Numbers that differ within [`REL`].
    noise: usize,
    max_rel: f64,
}

fn same_value(a: &Value, b: &Value, at: &str, c: &mut Compared) -> Result<(), String> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            c.numbers += 1;
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            if x == y {
                return Ok(());
            }
            let rel = (x - y).abs() / x.abs().max(y.abs());
            if rel <= REL {
                c.noise += 1;
                c.max_rel = c.max_rel.max(rel);
                Ok(())
            } else {
                Err(format!("{at}: {x} against {y}"))
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Err(format!("{at}: {} items against {}", x.len(), y.len()));
            }
            for (i, (x, y)) in x.iter().zip(y).enumerate() {
                same_value(x, y, &format!("{at}[{i}]"), c)?;
            }
            Ok(())
        }
        (Value::Object(x), Value::Object(y)) => {
            let xk: Vec<_> = x.keys().collect();
            let yk: Vec<_> = y.keys().collect();
            if xk != yk {
                return Err(format!("{at}: keys {xk:?} against {yk:?}"));
            }
            for (k, v) in x {
                same_value(v, &y[k], &format!("{at}.{k}"), c)?;
            }
            Ok(())
        }
        _ if a == b => Ok(()),
        _ => Err(format!("{at}: {a} against {b}")),
    }
}

/// Panics naming the first answer that differs.
fn assert_same_answers(want: &[Answer], got: &[Answer]) -> Compared {
    let (c, differ) = compare_answers(want, got);
    if !differ.is_empty() {
        let mut shown: Vec<String> = differ
            .iter()
            .take(8)
            .map(|d| d.chars().take(700).collect())
            .collect();
        shown.push(format!("{} of {} answers differ", differ.len(), c.answers));
        panic!("{}", shown.join("\n"));
    }
    c
}

/// The comparison, and each answer that differs.
fn compare_answers(want: &[Answer], got: &[Answer]) -> (Compared, Vec<String>) {
    let mut c = Compared::default();
    assert_eq!(
        want.iter().map(|(q, _)| q).collect::<Vec<_>>(),
        got.iter().map(|(q, _)| q).collect::<Vec<_>>(),
        "the same questions"
    );
    let mut differ = Vec::new();
    for ((q, want), (_, got)) in want.iter().zip(got) {
        c.answers += 1;
        match (want, got) {
            (Ok(w), Ok(g)) => {
                if let Err(e) = same_value(w, g, q, &mut c) {
                    differ.push(format!("{e}\n  want {w}\n   got {g}"));
                }
            }
            (Err(w), Err(g)) if w == g => {}
            _ => differ.push(format!("{q}: {want:?} against {got:?}")),
        }
    }
    (c, differ)
}

/// The converted metadata holds the original's (apart from parquet's own
/// keys), and adds only `producer_version` beside a `version`.
fn assert_same_metadata(want: &HashMap<String, String>, got: &HashMap<String, String>) {
    for (k, v) in want {
        if k == "ARROW:schema" || k == "metriken.format" {
            continue;
        }
        assert_eq!(got.get(k), Some(v), "metadata `{k}`");
    }
    for k in got.keys() {
        let added = k == "producer_version" && want.contains_key("version");
        assert!(want.contains_key(k) || added, "metadata `{k}` was added");
    }
    if let Some(version) = want.get("version") {
        assert_eq!(got.get("producer_version"), Some(version));
    }
}

/// Every answer `want` gives, `got` gives, each recording alone and the
/// recordings together.
fn assert_answers_as<A: MetricsSource + ?Sized, B: MetricsSource + ?Sized>(
    want: &A,
    got: &B,
) -> Compared {
    assert_same_metadata(&want.file_metadata(), &got.file_metadata());
    // A recording with no rows (a tar cut right after its first manifest)
    // has listings and no range to query.
    let range = want.time_range().unwrap_or((0.0, 0.0));
    assert_same_answers(&answers(want, range), &answers(got, range))
}

// ─── Tar `.rez` ─────────────────────────────────────────────────────────────

/// Convert the tar at `input` and compare the archive with it.
fn check_tar_bytes(bytes: Vec<u8>, config: &WriterConfig) -> (Written, Compared) {
    let original = open_recordings_from_bytes(bytes.clone(), pool()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.dendro");
    let mut out = dendro::archive::ArchiveMut::create(&dest).unwrap();
    let written = convert_tar_into(bytes.as_slice(), &mut out, config).unwrap();
    drop(out);
    let report = dendro::archive::Archive::open(&dest)
        .unwrap()
        .verify(dendro::archive::Depth::Full)
        .unwrap();
    assert!(report.is_sound(), "{:?}", report.problems);

    let converted = open_recordings(&dest, pool()).unwrap();
    assert_eq!(original.len(), converted.len(), "one source per recording");
    let mut total = Compared::default();
    for ((wl, want), (gl, got)) in original.iter().zip(&converted) {
        assert_eq!(wl, gl, "recording labels");
        assert_eq!(want.complete(), got.complete(), "complete");
        let c = assert_answers_as(want, got);
        total.answers += c.answers;
        total.numbers += c.numbers;
        total.noise += c.noise;
        total.max_rel = total.max_rel.max(c.max_rel);
    }
    // And flattened, as `view` reads a multi-recording archive.
    let flat = |r: Vec<(BTreeMap<String, String>, ArchiveReader)>| {
        ArchiveReader::flatten(r.into_iter().map(|(_, r)| r).collect(), None)
    };
    let (want, got) = (flat(original), flat(converted));
    assert_answers_as(&want, &got);
    (written, total)
}

fn check_tar(path: &Path) -> (Written, Compared) {
    check_tar_bytes(std::fs::read(path).unwrap(), &WriterConfig::default())
}

/// A small seal cap, so a 12-row table splits into several segments.
fn tiny_segments(rows: usize) -> WriterConfig {
    let mut config = WriterConfig::default();
    config.seal.max_rows = rows;
    config
}

#[test]
fn a_v2_tar_converts() {
    let (written, c) = check_tar(&storage_fixture("rez/v2.rez"));
    assert_eq!(
        (
            written.sources,
            written.tables,
            written.segments,
            written.rows
        ),
        (1, 2, 2, 24)
    );
    assert_eq!(c.noise, 0, "{c:?}");
    println!("v2.rez: {c:?}");
}

#[test]
fn a_streamed_v2_tar_converts_and_stays_incomplete() {
    let (written, c) = check_tar(&storage_fixture("rez/v2-partial.rez"));
    assert_eq!((written.sources, written.tables), (1, 2));
    assert!(written.segments >= 4, "{written:?}");
    assert_eq!(c.noise, 0, "{c:?}");
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.dendro");
    convert_tar_to_dendro(&storage_fixture("rez/v2-partial.rez"), &dest).unwrap();
    let (_, reader) = open_archive(&dest).remove(0);
    assert!(
        !reader.complete(),
        "a recovered recording reads as recovered"
    );
    println!("v2-partial.rez: {c:?}");
}

#[test]
fn split_segments_answer_as_one() {
    for name in ["rez/v2.rez", "rez/v2-partial.rez"] {
        for rows in [1, 2, 5] {
            let bytes = std::fs::read(storage_fixture(name)).unwrap();
            let (written, c) = check_tar_bytes(bytes, &tiny_segments(rows));
            assert!(
                written.segments >= 24 / rows,
                "{name} at {rows}: {written:?}"
            );
            assert_eq!(c.noise, 0, "{name} at {rows}: {c:?}");
        }
    }
}

/// Every prefix of a streamed tar that reads converts to what it reads.
/// Prefixes that read the same manifest read the same tables, so each
/// distinct reading is compared in full once.
#[test]
fn a_truncated_tar_converts_its_readable_prefix() {
    let bytes = std::fs::read(storage_fixture("rez/v2-partial.rez")).unwrap();
    let mut readable = 0;
    let mut seen = BTreeSet::new();
    for cut in (512..bytes.len()).step_by(512) {
        let prefix = bytes[..cut].to_vec();
        let Ok((manifest, _)) = metriken_storage::rez::rez::read_archive_reader(prefix.as_slice())
        else {
            continue;
        };
        readable += 1;
        if !seen.insert(serde_json::to_string(&manifest).unwrap()) {
            continue;
        }
        let (_, c) = check_tar_bytes(prefix, &WriterConfig::default());
        assert_eq!(c.noise, 0, "cut at {cut}");
    }
    assert!(readable >= 3, "{readable} readable prefixes");
    assert!(seen.len() >= 3, "{} distinct readings", seen.len());
}

/// The members of a tar, in order.
fn tar_members(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut archive = tar::Archive::new(bytes);
    archive
        .entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            let name = e.path().unwrap().to_string_lossy().into_owned();
            let mut buf = Vec::new();
            e.read_to_end(&mut buf).unwrap();
            (name, buf)
        })
        .collect()
}

fn tar_of(members: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, bytes) in members {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, bytes.as_slice())
            .unwrap();
    }
    builder.into_inner().unwrap()
}

/// `v2.rez` with a v1 manifest: `file` only, no `files`, no `complete`,
/// which v1 recordings read as complete.
#[test]
fn a_v1_tar_converts_complete() {
    let members = tar_members(&std::fs::read(storage_fixture("rez/v2.rez")).unwrap());
    let members: Vec<(String, Vec<u8>)> = members
        .into_iter()
        .map(|(name, bytes)| {
            if name != "manifest.json" {
                return (name, bytes);
            }
            let mut m: Value = serde_json::from_slice(&bytes).unwrap();
            m["version"] = 1.into();
            for rec in m["recordings"].as_array_mut().unwrap() {
                let rec = rec.as_object_mut().unwrap();
                rec.remove("complete");
                for t in rec["tables"].as_array_mut().unwrap() {
                    t.as_object_mut().unwrap().remove("files");
                }
            }
            (name, serde_json::to_vec(&m).unwrap())
        })
        .collect();
    let bytes = tar_of(&members);
    let (written, c) = check_tar_bytes(bytes.clone(), &WriterConfig::default());
    assert_eq!((written.sources, written.rows), (1, 24));
    assert_eq!(c.noise, 0, "{c:?}");
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.dendro");
    let mut out = dendro::archive::ArchiveMut::create(&dest).unwrap();
    convert_tar_into(bytes.as_slice(), &mut out, &WriterConfig::default()).unwrap();
    drop(out);
    let (_, reader) = open_archive(&dest).remove(0);
    assert!(reader.complete());
}

/// Two recordings in one tar become two sources, each with its labels.
#[test]
fn a_tar_of_two_recordings_converts_to_two_sources() {
    let members = tar_members(&std::fs::read(storage_fixture("rez/v2.rez")).unwrap());
    let mut out = Vec::new();
    for (name, bytes) in members {
        if name == "manifest.json" {
            let mut m: Value = serde_json::from_slice(&bytes).unwrap();
            let mut second = m["recordings"][0].clone();
            second["dir"] = "rezolus-web-02".into();
            second["labels"]["host"] = "web-02".into();
            second["metadata"]["version"] = "5.9.0".into();
            m["recordings"].as_array_mut().unwrap().push(second);
            out.push((name, serde_json::to_vec(&m).unwrap()));
        } else {
            out.push((name.replace("web-01", "web-02"), bytes.clone()));
            out.push((name, bytes));
        }
    }
    let (written, c) = check_tar_bytes(tar_of(&out), &WriterConfig::default());
    assert_eq!((written.sources, written.tables), (2, 4));
    assert_eq!(c.noise, 0, "{c:?}");
}

#[test]
fn a_tar_carries_clock_anchor_and_offsets() {
    let members = tar_members(&std::fs::read(storage_fixture("rez/v2.rez")).unwrap());
    let members: Vec<(String, Vec<u8>)> = members
        .into_iter()
        .map(|(name, bytes)| {
            if name != "manifest.json" {
                return (name, bytes);
            }
            let mut m: Value = serde_json::from_slice(&bytes).unwrap();
            m["recordings"][0]["clock_anchor_wall_ns"] = 1_699_999_999_000_000_000u64.into();
            m["recordings"][0]["clock_offsets"] = serde_json::json!([
                [1_700_000_003_000_000_000u64, 12],
                [1_700_000_007_000_000_000u64, -4]
            ]);
            (name, serde_json::to_vec(&m).unwrap())
        })
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("in.rez");
    std::fs::write(&src, tar_of(&members)).unwrap();
    let dest = dir.path().join("out.dendro");
    let written = convert_tar_to_dendro(&src, &dest).unwrap();
    assert_eq!(written.clock_offsets, 2);
    let archive = dendro::archive::Archive::open(&dest).unwrap();
    let source = &archive.read_sources().unwrap()[0];
    assert_eq!(source.meta.clock_anchor_wall_ns, 1_699_999_999_000_000_000);
    assert_eq!(
        archive.read_clock_offsets(source.id).unwrap(),
        [
            (1_700_000_003_000_000_000, 12),
            (1_700_000_007_000_000_000, -4)
        ]
    );
    assert!(source.complete);

    // Without an anchor in the manifest, the first row.
    let dest = dir.path().join("plain.dendro");
    convert_tar_to_dendro(&storage_fixture("rez/v2.rez"), &dest).unwrap();
    let archive = dendro::archive::Archive::open(&dest).unwrap();
    assert_eq!(
        archive.read_sources().unwrap()[0].meta.clock_anchor_wall_ns,
        BASE_NS as i64
    );
}

// ─── Parquet ────────────────────────────────────────────────────────────────

fn check_parquet_with(path: &Path, converter: &ParquetToArchive) -> (Written, Compared) {
    let original = ParquetReader::open(path).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.dendro");
    let written = converter.convert_file_path(path, &dest).unwrap();
    let report = dendro::archive::Archive::open(&dest)
        .unwrap()
        .verify(dendro::archive::Depth::Full)
        .unwrap();
    assert!(report.is_sound(), "{:?}", report.problems);
    let mut recordings = open_archive(&dest);
    assert_eq!(recordings.len(), 1);
    let (_, converted) = recordings.remove(0);
    assert!(converted.complete());
    let c = assert_answers_as(&original, &converted);
    (written, c)
}

fn check_parquet(path: &Path) -> (Written, Compared) {
    check_parquet_with(path, &ParquetToArchive::new())
}

#[test]
fn the_legacy_parquet_fixtures_convert() {
    for path in [
        storage_fixture("legacy/legacy-0.4.0.parquet"),
        query_fixture("legacy/legacy-0.4.0.parquet"),
    ] {
        let (written, c) = check_parquet(&path);
        assert_eq!((written.sources, written.tables, written.rows), (1, 1, 4));
        assert_eq!(c.noise, 0, "{c:?}");
        println!("{}: {c:?}", path.display());
    }
}

/// A rezolus-shaped parquet file: `ops` (a counter of two series, `id` 0
/// and 1, +10 and +20 a tick), `mem_free` (a gauge, 1000 − t), `lat` (a
/// counting histogram, `metric_type = histogram`), each with a per-metric
/// acquisition window, and a `duration` column; the metadata of a file
/// rezolus's `combine` wrote.
fn rezolus_parquet(path: &Path, ticks: u64, windows: bool) {
    rezolus_parquet_at(path, &(0..ticks).collect::<Vec<_>>(), windows)
}

/// [`rezolus_parquet`] holding a row at each of `t` seconds only.
fn rezolus_parquet_at(path: &Path, t: &[u64], windows: bool) {
    let ticks = t.len() as u64;
    let md = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let config = histogram::Config::new(2, 8).unwrap();
    let buckets = config.total_buckets();
    let list = DataType::List(Arc::new(Field::new("item", DataType::UInt64, true)));
    let mut fields = vec![
        Field::new("timestamp", DataType::UInt64, false)
            .with_metadata(md(&[("metric_type", "timestamp"), ("unit", "nanoseconds")])),
        Field::new("duration", DataType::UInt64, true)
            .with_metadata(md(&[("metric_type", "duration"), ("unit", "nanoseconds")])),
        Field::new("0", DataType::UInt64, true).with_metadata(md(&[
            ("metric", "ops"),
            ("metric_type", "counter"),
            ("id", "0"),
            ("source", "rezolus"),
        ])),
        Field::new("1", DataType::UInt64, true).with_metadata(md(&[
            ("metric", "ops"),
            ("metric_type", "counter"),
            ("id", "1"),
            ("source", "rezolus"),
        ])),
        Field::new("2", DataType::Int64, true).with_metadata(md(&[
            ("metric", "mem_free"),
            ("metric_type", "gauge"),
            ("source", "rezolus"),
        ])),
        Field::new("3:buckets", list, true).with_metadata(md(&[
            ("metric", "lat"),
            ("metric_type", "histogram"),
            ("grouping_power", "2"),
            ("max_value_power", "8"),
            ("source", "llm-perf"),
        ])),
    ];
    let t: Vec<u64> = t.to_vec();
    let ts: Vec<u64> = t.iter().map(|t| BASE_NS + t * 1_000_000_000).collect();
    let mut lat = ListBuilder::new(UInt64Builder::new());
    for t in &t {
        for b in 0..buckets {
            lat.values().append_value(if b == 2 { t + 1 } else { 0 });
        }
        lat.append(true);
    }
    let mut arrays: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(ts.clone())),
        Arc::new(UInt64Array::from(vec![3_000_000u64; ticks as usize])),
        Arc::new(UInt64Array::from_iter_values(t.iter().map(|t| 10 * t))),
        Arc::new(UInt64Array::from_iter_values(t.iter().map(|t| 20 * t + 7))),
        Arc::new(Int64Array::from_iter_values(
            t.iter().map(|t| 1000 - *t as i64),
        )),
        Arc::new(lat.finish()),
    ];
    if windows {
        // Each read began up to 2 ms before the row's timestamp and took
        // 1 to 3 ms, varying so the bands are not all alike.
        for col in ["0", "1", "3"] {
            fields.push(Field::new(
                format!("{col}:window_begin"),
                DataType::Int64,
                true,
            ));
            fields.push(Field::new(
                format!("{col}:window_width"),
                DataType::UInt64,
                true,
            ));
            arrays.push(Arc::new(Int64Array::from_iter_values(
                t.iter().map(|t| -2_000_000 + (*t as i64 % 3) * 500_000),
            )));
            arrays.push(Arc::new(UInt64Array::from_iter_values(
                t.iter().map(|t| 1_000_000 + (t % 5) * 500_000),
            )));
        }
    }
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(Arc::clone(&schema), arrays).unwrap();
    let kv = [
        ("source", r#"["rezolus","llm-perf"]"#),
        ("sampling_interval_ms", "1000"),
        ("version", "5.11.0"),
        ("producer_epoch", "6b0f3c1e-7a8e-4f43-9b6a-2c1d9e0f4a11"),
        ("systeminfo", r#"{"os":"linux","arch":"x86_64"}"#),
        (
            "descriptions",
            r#"{"ops":"Operations","mem_free":"Free memory"}"#,
        ),
        (
            "per_source_metadata",
            r#"{"rezolus":{"version":"5.11.0"},"llm-perf":{"version":"0.1.11","role":"loadgen"}}"#,
        ),
        ("report", "trimmed"),
    ]
    .map(|(k, v)| KeyValue::new(k.to_string(), v.to_string()))
    .to_vec();
    let props = parquet::file::properties::WriterProperties::builder()
        .set_key_value_metadata(Some(kv))
        .set_max_row_group_row_count(Some(7))
        // Compressed, so the encoded bytes the byte cap meters differ from
        // the uncompressed size.
        .set_compression(parquet::basic::Compression::ZSTD(Default::default()))
        .build();
    let file = std::fs::File::create(path).unwrap();
    let mut w = parquet::arrow::ArrowWriter::try_new(file, schema, Some(props)).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

#[test]
fn a_combined_parquet_with_windows_converts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("combined.parquet");
    rezolus_parquet(&path, 40, true);
    for converter in [
        ParquetToArchive::new(),
        ParquetToArchive::new().writer_config(tiny_segments(6)),
    ] {
        let (written, c) = check_parquet_with(&path, &converter);
        assert_eq!(written.rows, 40);
        assert_eq!(c.noise, 0, "{c:?}");
        println!("combined.parquet in {} segments: {c:?}", written.segments);
    }
}

/// The bands a parquet file's per-metric windows give are the archive's.
#[test]
fn per_metric_windows_keep_their_bands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("windows.parquet");
    rezolus_parquet(&path, 20, true);
    let dest = dir.path().join("out.dendro");
    ParquetToArchive::new()
        .convert_file_path(&path, &dest)
        .unwrap();
    let original = ParquetReader::open(&path).unwrap();
    let (_, converted) = open_archive(&dest).remove(0);
    let (lo, hi) = (BASE_NS as f64 / 1e9 + 2.0, BASE_NS as f64 / 1e9 + 18.0);
    let bands = |r: QueryResult| -> Vec<Vec<Option<(f64, f64)>>> {
        let QueryResult::Matrix { result, .. } = r else {
            panic!("{r:?}")
        };
        let mut result: Vec<_> = result
            .into_iter()
            .map(|s| (format!("{:?}", s.metric.get("id")), s.bands))
            .collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
            .into_iter()
            .map(|(_, b)| b.expect("a windowed rate has bands"))
            .collect()
    };
    let want = bands(original.query_range("irate(ops[5s])", lo, hi, 1.0).unwrap());
    let got = bands(
        converted
            .query_range("irate(ops[5s])", lo, hi, 1.0)
            .unwrap(),
    );
    assert_eq!(want, got);
    // The windows vary, so the bands do: not the duration fallback, and not
    // degenerate.
    let widths: BTreeSet<u64> = got[0]
        .iter()
        .flatten()
        .map(|(l, h)| ((h - l) * 1e6) as u64)
        .collect();
    assert!(widths.len() > 1, "{widths:?}");
}

/// The windowless file reads its bands from `duration`, before and after.
#[test]
fn a_windowless_parquet_converts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.parquet");
    rezolus_parquet(&path, 30, false);
    let (_, c) = check_parquet(&path);
    assert_eq!(c.noise, 0, "{c:?}");
}

/// Many row groups and many samples, from metriken-query's fixture builder,
/// cut into segments of 128 rows.
#[test]
fn a_long_parquet_converts_in_segments() {
    let fixture = metriken_query::fixtures::FixtureBuilder::new()
        .samples(2000)
        .row_group_size(300)
        .monotonic_counter("cpu_cycles", &[("id", "0")], 1000)
        .monotonic_counter("cpu_cycles", &[("id", "1")], 3000)
        .resetting_counter("resets", &[], 50, 1200)
        .gauge("load", &[("cpu", "0")], |t| (t % 17) as i64)
        .point_histogram("latency", &[], 3, 10, 9, 4)
        .metadata("source", "rezolus")
        .build()
        .unwrap();
    let path = fixture.path().to_path_buf();
    let (written, c) = check_parquet_with(
        &path,
        &ParquetToArchive::new().writer_config(tiny_segments(128)),
    );
    assert_eq!((written.rows, written.segments), (2000, 16));
    assert_eq!(c.noise, 0, "{c:?}");
    println!("2000 samples: {c:?}");
}

#[test]
fn a_label_tags_every_series() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.parquet");
    rezolus_parquet(&path, 10, true);
    let dest = dir.path().join("out.dendro");
    ParquetToArchive::new()
        .label("host", "web-01")
        .metadata("version", "5.12.0")
        .convert_file_path(&path, &dest)
        .unwrap();
    let (labels, reader) = open_archive(&dest).remove(0);
    assert_eq!(labels["host"], "web-01");
    assert!(reader
        .counter_labels("ops")
        .iter()
        .all(|l| l.get("host").map(String::as_str) == Some("web-01")));
    assert_eq!(reader.metadata()["version"], "5.12.0", "the caller's wins");
    assert_eq!(reader.metadata()["producer_version"], "5.12.0");
}

/// A counting histogram written now says `counter_histogram`.
#[test]
fn counting_histograms_are_written_as_counter_histogram() {
    use metriken_storage::Catalog;
    use parquet::file::reader::FileReader;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.dendro");
    ParquetToArchive::new()
        .convert_file_path(storage_fixture("legacy/legacy-0.4.0.parquet"), &dest)
        .unwrap();
    let catalog = DendroCatalog::open(&dest).unwrap();
    let source = catalog.sources().unwrap().remove(0);
    assert_eq!(catalog.tables(source.id).unwrap(), [PARQUET_TABLE]);
    let bytes = catalog
        .segment_bytes(source.id, PARQUET_TABLE, 0)
        .unwrap()
        .unwrap();
    let file = parquet::file::reader::SerializedFileReader::new(bytes::Bytes::from(bytes)).unwrap();
    let fm = file.metadata().file_metadata();
    let schema =
        parquet::arrow::parquet_to_arrow_schema(fm.schema_descr(), fm.key_value_metadata())
            .unwrap();
    assert_eq!(
        schema.field_with_name("2:buckets").unwrap().metadata()["metric_type"],
        "counter_histogram"
    );
    let kv: Vec<_> = fm
        .key_value_metadata()
        .unwrap()
        .iter()
        .map(|e| e.key.as_str())
        .collect();
    assert!(kv.contains(&"metriken.format"), "{kv:?}");
}

// ─── Errors and cleanup ─────────────────────────────────────────────────────

#[test]
fn an_existing_output_is_refused_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.dendro");
    std::fs::write(&dest, b"keep me").unwrap();
    let err = convert_tar_to_dendro(&storage_fixture("rez/v2.rez"), &dest).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    let err = ParquetToArchive::new()
        .convert_file_path(storage_fixture("legacy/legacy-0.4.0.parquet"), &dest)
        .unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(std::fs::read(&dest).unwrap(), b"keep me");
}

#[test]
fn a_failed_conversion_removes_what_it_created() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.dendro");
    // A tar whose segment is not parquet.
    let members = tar_members(&std::fs::read(storage_fixture("rez/v2.rez")).unwrap());
    let broken: Vec<(String, Vec<u8>)> = members
        .into_iter()
        .map(|(n, b)| {
            if n.ends_with("b.parquet") {
                (n, b"not parquet".to_vec())
            } else {
                (n, b)
            }
        })
        .collect();
    let src = dir.path().join("broken.rez");
    std::fs::write(&src, tar_of(&broken)).unwrap();
    let err = convert_tar_to_dendro(&src, &dest).unwrap_err();
    assert!(err.to_string().contains("table b"), "{err}");
    let left = || -> BTreeSet<String> {
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(left(), BTreeSet::from(["broken.rez".to_string()]));

    // A parquet file with no rows.
    let empty = dir.path().join("empty.parquet");
    rezolus_parquet(&empty, 0, false);
    let err = ParquetToArchive::new()
        .convert_file_path(&empty, &dest)
        .unwrap_err();
    assert!(err.to_string().contains("no rows"), "{err}");
    assert_eq!(
        left(),
        BTreeSet::from(["broken.rez".to_string(), "empty.parquet".to_string()])
    );
}

/// Into an archive in memory, as the browser would, and read from its bytes.
#[test]
fn converts_into_an_archive_in_memory() {
    let mut out = dendro::archive::ArchiveMut::create_in_memory().unwrap();
    let bytes =
        bytes::Bytes::from(std::fs::read(storage_fixture("legacy/legacy-0.4.0.parquet")).unwrap());
    ParquetToArchive::new()
        .convert_into(bytes, &mut out)
        .unwrap();
    let tar = std::fs::read(storage_fixture("rez/v2.rez")).unwrap();
    convert_tar_into(tar.as_slice(), &mut out, &WriterConfig::default()).unwrap();
    let image = out.serialize().unwrap();
    let recordings = open_recordings_from_bytes(image, pool()).unwrap();
    assert_eq!(recordings.len(), 2);
    assert_eq!(recordings[0].1.histogram_names(), ["lat"]);
    assert_eq!(recordings[1].0["host"], "web-01");
}

// ─── v3 `.rez` ──────────────────────────────────────────────────────────────

/// Every answer of every recording in `want` and the flattened set, given by
/// `got` as well.
fn assert_recordings_answer_as(
    want: &[(BTreeMap<String, String>, ArchiveReader)],
    got: &[(BTreeMap<String, String>, ArchiveReader)],
) -> Compared {
    assert_eq!(want.len(), got.len(), "one source per recording");
    let mut total = Compared::default();
    for ((wl, w), (gl, g)) in want.iter().zip(got) {
        assert_eq!(wl, gl, "recording labels");
        assert_eq!(w.complete(), g.complete(), "complete");
        let c = assert_answers_as(w, g);
        total.answers += c.answers;
        total.numbers += c.numbers;
    }
    total
}

/// Each committed v3 fixture, read from bytes and converted into an archive
/// in memory with `convert_v3_into`, answers as the `.rez` does and as the
/// path form's archive does. `v3-live.rez` is unfinished with a live WAL
/// tail, which is carried as WAL rows rather than sealed.
#[test]
fn v3_fixtures_convert_from_bytes_into_memory() {
    use metriken_storage::rez::rez_sqlite::RezDb;
    use metriken_storage::rez::to_dendro::convert_v3_into;
    use metriken_storage::to_dendro::{convert_path, ConvertedFrom};

    let dir = tempfile::tempdir().unwrap();
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(storage_fixture("rez"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.starts_with("v3") && name.ends_with(".rez")
        })
        .collect();
    fixtures.sort();
    assert_eq!(fixtures.len(), 3, "{fixtures:?}");

    for fixture in &fixtures {
        let name = fixture.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(fixture).unwrap();

        let db = RezDb::open_bytes(bytes).unwrap();
        let mut out = dendro::archive::ArchiveMut::create_in_memory().unwrap();
        let in_memory = convert_v3_into(&db, &mut out).unwrap();
        let image = out.serialize().unwrap();
        drop(out);

        let dest = dir.path().join(format!("{name}.dendro"));
        let ConvertedFrom::RezV3(by_path) = convert_path(fixture, &dest).unwrap() else {
            panic!("{name} is a v3 .rez");
        };
        assert_eq!(in_memory, by_path, "{name}: what each form wrote");
        if name == "v3-live.rez" {
            assert_eq!(in_memory.wal_rows, 4, "{name}: the live tail is copied");
        }

        let original = open_recordings(fixture, pool()).unwrap();
        let from_path = open_recordings(&dest, pool()).unwrap();
        let from_memory = open_recordings_from_bytes(image, pool()).unwrap();
        let c = assert_recordings_answer_as(&original, &from_memory);
        assert!(c.numbers > 0, "{name}: compared no numbers");
        assert_recordings_answer_as(&from_path, &from_memory);
        if name == "v3-live.rez" {
            assert!(
                original.iter().all(|(_, r)| !r.complete()),
                "{name} is unfinished"
            );
        }
    }
}

/// `v` without uncertainty bands and the interpolation flags beside them.
fn strip_bands(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for k in ["bands", "intervals", "interval", "interpolated"] {
                m.remove(k);
            }
            m.values_mut().for_each(strip_bands);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_bands),
        _ => {}
    }
}

/// Converts any parquet file or tar `.rez` and compares the answers. A tar
/// is compared as the fixtures are, and fails on a difference. A parquet
/// file is compared against two readers of the original, and the answers
/// that differ are counted rather than failed on: `ParquetReader`, and an
/// `ArchiveReader` holding the file as its one in-memory segment, which
/// separates what the conversion changed from what the two readers answer
/// differently for the same bytes.
///
/// ```text
/// LEGACY_TO_DENDRO=/path/to/recording.parquet \
///   cargo test -p metriken-storage --all-features --release --test \
///   legacy_to_dendro converts_a_real_recording -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs LEGACY_TO_DENDRO=<path to a parquet file or tar .rez>"]
fn converts_a_real_recording() {
    let path = PathBuf::from(std::env::var_os("LEGACY_TO_DENDRO").expect("set LEGACY_TO_DENDRO"));
    let start = std::time::Instant::now();
    if metriken_storage::rez::rez::is_rez_path(&path).unwrap_or(false) {
        let (written, c) = check_tar(&path);
        println!(
            "{}: {written:?}, {c:?} in {:.1?}",
            path.display(),
            start.elapsed()
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.dendro");
    // `LEGACY_MAX_BYTES` overrides the byte cap, to compare segment sizes.
    let mut config = WriterConfig::default();
    if let Some(bytes) = std::env::var_os("LEGACY_MAX_BYTES") {
        config.seal.max_bytes = bytes.to_str().unwrap().parse().unwrap();
    }
    let written = ParquetToArchive::new()
        .writer_config(config)
        .convert_file_path(&path, &dest)
        .unwrap();
    let took = start.elapsed();
    let parquet = ParquetReader::open(&path).unwrap();
    let metadata: BTreeMap<String, String> = parquet
        .file_metadata()
        .into_iter()
        .filter(|(k, _)| k != "ARROW:schema")
        .collect();
    let in_memory = ArchiveReader::from_in_memory(
        vec![metriken_storage::InMemorySource {
            name: "original".to_string(),
            labels: BTreeMap::new(),
            metadata,
            complete: true,
            tables: vec![(
                PARQUET_TABLE.to_string(),
                vec![std::fs::read(&path).unwrap()],
            )],
        }],
        None,
        pool(),
    )
    .unwrap();
    let (_, converted) = open_archive(&dest).remove(0);
    assert_same_metadata(&parquet.file_metadata(), &converted.file_metadata());
    let range = parquet.time_range().unwrap();
    let start = std::time::Instant::now();
    let got = answers(&converted, range);
    println!(
        "  the converted archive's answers took {:.1?}",
        start.elapsed()
    );
    println!(
        "{}: {written:?} in {took:.1?}, {} -> {} bytes; intervals: parquet {}, in-memory {}, converted {}",
        path.display(),
        std::fs::metadata(&path).unwrap().len(),
        std::fs::metadata(&dest).unwrap().len(),
        MetricsSource::interval(&parquet),
        MetricsSource::interval(&in_memory),
        MetricsSource::interval(&converted),
    );
    let unbanded = |a: Vec<Answer>| -> Vec<Answer> {
        a.into_iter()
            .map(|(q, r)| {
                (
                    q,
                    r.map(|mut v| {
                        strip_bands(&mut v);
                        v
                    }),
                )
            })
            .collect()
    };
    for (name, want, got) in [
        ("ParquetReader", answers(&parquet, range), got.clone()),
        (
            "in-memory ArchiveReader",
            answers(&in_memory, range),
            got.clone(),
        ),
        (
            "in-memory ArchiveReader, bands left out",
            unbanded(answers(&in_memory, range)),
            unbanded(got),
        ),
    ] {
        let (c, differ) = compare_answers(&want, &got);
        println!(
            "  against {name}: {} of {} answers differ; {c:?}",
            differ.len(),
            c.answers
        );
        for d in differ.iter().take(4) {
            println!(
                "    {}",
                d.lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect::<String>()
            );
        }
    }
}

// ─── Intervals ──────────────────────────────────────────────────────────────

/// A recording with dropped rows measures a longer interval than it
/// declares. The parquet reader reads at the declared one, and so does the
/// archive's, from the `sampling_interval_ms` each segment carries.
#[test]
fn a_parquet_with_dropped_rows_reads_at_its_declared_interval() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gaps.parquet");
    // 40 ticks, 1 s apart, with 7 dropped: measured, 39 s over 32 gaps.
    let t: Vec<u64> = (0..40)
        .filter(|t| ![3, 4, 11, 17, 18, 19, 30].contains(t))
        .collect();
    rezolus_parquet_at(&path, &t, true);
    let (_, c) = check_parquet(&path);
    assert_eq!(c.noise, 0, "{c:?}");
    let dest = dir.path().join("out.dendro");
    ParquetToArchive::new()
        .convert_file_path(&path, &dest)
        .unwrap();
    let (_, converted) = open_archive(&dest).remove(0);
    assert_eq!(MetricsSource::interval(&converted), 1.0);
}

/// Each converted segment declares the input's interval and carries a
/// names index.
#[test]
fn converted_segments_declare_the_interval() {
    use metriken_storage::Catalog;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.parquet");
    rezolus_parquet(&path, 30, true);
    let dest = dir.path().join("out.dendro");
    ParquetToArchive::new()
        .writer_config(tiny_segments(7))
        .convert_file_path(&path, &dest)
        .unwrap();
    let catalog = DendroCatalog::open(&dest).unwrap();
    let source = catalog.sources().unwrap().remove(0);
    let segments = catalog.segment_meta(source.id, PARQUET_TABLE).unwrap();
    assert_eq!(segments.len(), 5);
    for (seq, _) in segments {
        let bytes = catalog
            .segment_bytes(source.id, PARQUET_TABLE, seq)
            .unwrap()
            .unwrap();
        assert_eq!(
            footer(&bytes)
                .get("sampling_interval_ms")
                .map(String::as_str),
            Some("1000"),
            "segment {seq}"
        );
    }
    for (seq, index) in catalog.segment_indexes(source.id, PARQUET_TABLE).unwrap() {
        assert!(
            index.as_deref().is_some_and(|i| i.starts_with(b"MNI1")),
            "segment {seq}: {index:?}"
        );
    }
}

/// A segment's footer key-value metadata.
fn footer(bytes: &[u8]) -> BTreeMap<String, String> {
    use parquet::file::reader::FileReader;
    let file =
        parquet::file::reader::SerializedFileReader::new(bytes::Bytes::copy_from_slice(bytes))
            .unwrap();
    file.metadata()
        .file_metadata()
        .key_value_metadata()
        .into_iter()
        .flatten()
        .map(|e| (e.key.clone(), e.value.clone().unwrap_or_default()))
        .collect()
}

/// `bytes`, a parquet segment, without the rows at `drop`.
fn drop_rows(bytes: &[u8], drop: &[usize]) -> Vec<u8> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let b = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(bytes)).unwrap();
    let kv = b.metadata().file_metadata().key_value_metadata().cloned();
    let schema = b.schema().clone();
    let batches: Vec<RecordBatch> = b.build().unwrap().map(|x| x.unwrap()).collect();
    let batch = arrow::compute::concat_batches(&schema, &batches).unwrap();
    let keep: Vec<bool> = (0..batch.num_rows()).map(|i| !drop.contains(&i)).collect();
    let batch =
        arrow::compute::filter_record_batch(&batch, &arrow::array::BooleanArray::from(keep))
            .unwrap();
    let props = parquet::file::properties::WriterProperties::builder()
        .set_key_value_metadata(kv)
        .build();
    let mut buf = Vec::new();
    let mut w = parquet::arrow::ArrowWriter::try_new(&mut buf, schema, Some(props)).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    buf
}

/// `v2.rez` with the rows at `drop` taken out of table `a` and table `b`
/// (each 12 rows), and `manifest` applied to its manifest.
fn v2_with(drop_a: &[usize], drop_b: &[usize], manifest: impl Fn(&mut Value)) -> Vec<u8> {
    let members = tar_members(&std::fs::read(storage_fixture("rez/v2.rez")).unwrap());
    let members: Vec<(String, Vec<u8>)> = members
        .into_iter()
        .map(|(name, bytes)| {
            if name == "manifest.json" {
                let mut m: Value = serde_json::from_slice(&bytes).unwrap();
                manifest(&mut m);
                (name, serde_json::to_vec(&m).unwrap())
            } else if name.ends_with("/a.parquet") {
                (name, drop_rows(&bytes, drop_a))
            } else {
                (name, drop_rows(&bytes, drop_b))
            }
        })
        .collect();
    tar_of(&members)
}

/// A tar with dropped rows reads at the interval its reader measured from
/// the first rows, which the converted segments declare.
#[test]
fn a_tar_with_dropped_rows_reads_at_the_tar_readers_interval() {
    let bytes = v2_with(&[4, 8], &[4, 8], |_| {});
    let (_, c) = check_tar_bytes(bytes.clone(), &WriterConfig::default());
    assert_eq!(c.noise, 0, "{c:?}");
    for rows in [1, 3] {
        let (_, c) = check_tar_bytes(bytes.clone(), &tiny_segments(rows));
        assert_eq!(c.noise, 0, "{c:?}");
    }
}

/// Without an anchor in the manifest, the source starts at the recording's
/// first row, whichever table holds it.
#[test]
fn a_v1_anchor_is_the_earliest_row_of_any_table() {
    let v1 = |m: &mut Value| {
        m["version"] = 1.into();
        for rec in m["recordings"].as_array_mut().unwrap() {
            let rec = rec.as_object_mut().unwrap();
            rec.remove("complete");
            rec.remove("clock_anchor_wall_ns");
        }
    };
    // Table `a` starts 3 s in, `b` at the first tick.
    let bytes = v2_with(&[0, 1, 2], &[], v1);
    let (_, c) = check_tar_bytes(bytes.clone(), &WriterConfig::default());
    assert_eq!(c.noise, 0, "{c:?}");
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.dendro");
    let mut out = dendro::archive::ArchiveMut::create(&dest).unwrap();
    convert_tar_into(bytes.as_slice(), &mut out, &WriterConfig::default()).unwrap();
    drop(out);
    let archive = dendro::archive::Archive::open(&dest).unwrap();
    assert_eq!(
        archive.read_sources().unwrap()[0].meta.clock_anchor_wall_ns,
        BASE_NS as i64
    );
}

// ─── Segment caps, metadata, refusals ───────────────────────────────────────

/// A byte cap smaller than a row seals each row alone, whatever the row cap.
#[test]
fn the_byte_cap_splits_segments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.parquet");
    rezolus_parquet(&path, 12, true);
    let mut config = WriterConfig::default();
    config.seal.max_rows = 1000;
    config.seal.max_bytes = 1;
    let (written, c) = check_parquet_with(&path, &ParquetToArchive::new().writer_config(config));
    assert_eq!((written.rows, written.segments), (12, 12));
    assert_eq!(c.noise, 0, "{c:?}");

    // The cap meters a row as the input's average encoded bytes per row (its
    // row groups' compressed size over its rows): a cap of three rows' worth
    // seals after every third row, a byte more after every fourth, and the
    // default after the last.
    let per_row = {
        use parquet::file::reader::FileReader;
        let file =
            parquet::file::reader::SerializedFileReader::new(std::fs::File::open(&path).unwrap())
                .unwrap();
        let groups = file.metadata().row_groups();
        let bytes: i64 = groups.iter().map(|g| g.compressed_size()).sum();
        let rows: i64 = groups.iter().map(|g| g.num_rows()).sum();
        (bytes / rows) as usize
    };
    let capped = |bytes: usize| {
        let mut config = WriterConfig::default();
        config.seal.max_rows = 1000;
        config.seal.max_bytes = bytes;
        ParquetToArchive::new().writer_config(config)
    };
    let (written, _) = check_parquet_with(&path, &capped(3 * per_row));
    assert_eq!(written.segments, 4, "{written:?}");
    let (written, _) = check_parquet_with(&path, &capped(3 * per_row + 1));
    assert_eq!(
        written.segments, 3,
        "a fourth row reaches the cap: {written:?}"
    );
    let (written, _) = check_parquet(&path);
    assert_eq!(written.segments, 1);
}

/// The source's metadata is the file's, without the parquet format's keys.
#[test]
fn the_source_metadata_has_no_format_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.parquet");
    rezolus_parquet(&path, 5, true);
    let dest = dir.path().join("out.dendro");
    ParquetToArchive::new()
        .convert_file_path(&path, &dest)
        .unwrap();
    let archive = dendro::archive::Archive::open(&dest).unwrap();
    let metadata = &archive.read_sources().unwrap()[0].meta.metadata;
    assert!(
        !metadata.contains_key("ARROW:schema"),
        "{:?}",
        metadata.keys()
    );
    assert!(!metadata.contains_key("metriken.format"));
    assert_eq!(metadata["sampling_interval_ms"], "1000");
    assert_eq!(metadata["report"], "trimmed");
}

/// A long segment in a tar is refused rather than split.
#[test]
fn a_long_segment_in_a_tar_is_refused() {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let members = tar_members(&std::fs::read(storage_fixture("rez/v2.rez")).unwrap());
    let members: Vec<(String, Vec<u8>)> = members
        .into_iter()
        .map(|(name, bytes)| {
            if !name.ends_with("/b.parquet") {
                return (name, bytes);
            }
            let b = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes)).unwrap();
            let schema = b.schema().clone();
            let batches: Vec<RecordBatch> = b.build().unwrap().map(|x| x.unwrap()).collect();
            let props = parquet::file::properties::WriterProperties::builder()
                .set_key_value_metadata(Some(vec![KeyValue::new(
                    "metriken.layout".to_string(),
                    "long".to_string(),
                )]))
                .build();
            let mut buf = Vec::new();
            let mut w =
                parquet::arrow::ArrowWriter::try_new(&mut buf, schema, Some(props)).unwrap();
            for batch in batches {
                w.write(&batch).unwrap();
            }
            w.close().unwrap();
            (name, buf)
        })
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("long.rez");
    std::fs::write(&src, tar_of(&members)).unwrap();
    let dest = dir.path().join("out.dendro");
    let err = convert_tar_to_dendro(&src, &dest).unwrap_err();
    assert!(
        err.to_string().contains("table b has a long segment"),
        "{err}"
    );
    assert!(!dest.exists());
}

/// A table whose segments hold no rows is not written, so its names are
/// not listed.
#[test]
fn a_tar_table_with_no_rows_is_not_written() {
    let bytes = v2_with(&[], &(0..12).collect::<Vec<_>>(), |m| {
        m["recordings"][0]["tables"][1]["rows"] = 0.into();
    });
    let original = open_recordings_from_bytes(bytes.clone(), pool()).unwrap();
    assert_eq!(
        original[0].1.gauge_names(),
        ["mem_free"],
        "the tar lists it"
    );
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.dendro");
    let mut out = dendro::archive::ArchiveMut::create(&dest).unwrap();
    let written = convert_tar_into(bytes.as_slice(), &mut out, &WriterConfig::default()).unwrap();
    drop(out);
    assert_eq!(written.tables, 1);
    let (_, converted) = open_archive(&dest).remove(0);
    assert!(converted.gauge_names().is_empty());
    assert_eq!(converted.counter_names(), ["ops"]);
}

/// An output with a stale sidecar beside it is refused, and nothing is
/// touched.
#[test]
fn an_output_with_a_sidecar_beside_it_is_refused() {
    for suffix in ["-wal", "-shm", "-journal"] {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.dendro");
        let sidecar = dir.path().join(format!("out.dendro{suffix}"));
        std::fs::write(&sidecar, b"stale").unwrap();
        let err = convert_tar_to_dendro(&storage_fixture("rez/v2.rez"), &dest).unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("out.dendro{suffix} already exists")),
            "{err}"
        );
        let err = ParquetToArchive::new()
            .convert_file_path(storage_fixture("legacy/legacy-0.4.0.parquet"), &dest)
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert!(!dest.exists());
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"stale");
    }
}

// ─── convert_path ───────────────────────────────────────────────────────────

#[test]
fn convert_path_chooses_by_content() {
    use metriken_storage::to_dendro::{convert_path, ConvertedFrom};
    let dir = tempfile::tempdir().unwrap();
    // Names that say nothing, so only content can decide.
    let cases = [
        ("rez/v3.rez", "a"),
        ("rez/v2.rez", "b"),
        ("legacy/legacy-0.4.0.parquet", "c"),
    ];
    for (fixture, name) in cases {
        let src = dir.path().join(name);
        std::fs::copy(storage_fixture(fixture), &src).unwrap();
        let dest = dir.path().join(format!("{name}.dendro"));
        let got = convert_path(&src, &dest).unwrap();
        match (fixture, got) {
            ("rez/v3.rez", ConvertedFrom::RezV3(c)) => assert_eq!(c.sources, 1),
            ("rez/v2.rez", ConvertedFrom::RezTar(w)) => assert_eq!(w.rows, 24),
            ("legacy/legacy-0.4.0.parquet", ConvertedFrom::Parquet(w)) => assert_eq!(w.rows, 4),
            other => panic!("{other:?}"),
        }
        assert_eq!(open_archive(&dest).len(), 1, "{fixture}");
        // A dendro archive is refused as one already.
        let err = convert_path(&dest, dir.path().join(format!("{name}-again.dendro"))).unwrap_err();
        assert!(
            err.to_string().contains("already a dendro archive"),
            "{err}"
        );
    }
    let junk = dir.path().join("junk");
    std::fs::write(&junk, b"neither parquet nor a tar").unwrap();
    let out = dir.path().join("junk.dendro");
    let err = convert_path(&junk, &out).unwrap_err();
    assert!(
        err.to_string()
            .contains("junk: not a recording metriken reads"),
        "{err}"
    );
    assert!(!out.exists());
    // An existing output is refused for a v3 input too.
    let v3 = dir.path().join("a");
    let err = convert_path(&v3, dir.path().join("b.dendro")).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    // Before the input is opened: a v3 header over bytes SQLite cannot
    // read is not reported when the output exists.
    let broken = dir.path().join("broken");
    let mut bytes = b"SQLite format 3\0".to_vec();
    bytes.resize(4096, 0xAB);
    std::fs::write(&broken, &bytes).unwrap();
    let err = convert_path(&broken, dir.path().join("b.dendro")).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    // An archive that cannot be created is named by the output, not the
    // staging path beside it.
    let missing = dir.path().join("no-such-dir").join("out.dendro");
    let err = convert_path(&v3, &missing).unwrap_err().to_string();
    let named = format!("failed to create {}", missing.display());
    assert!(
        err.matches(&named).count() == 1 && !err.contains(".partial"),
        "{err}"
    );
}

/// A parquet file with no `sampling_interval_ms` reads at 1000 ms in the
/// parquet reader, and its segments declare that.
#[test]
fn a_parquet_without_an_interval_reads_at_one_second() {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let dir = tempfile::tempdir().unwrap();
    let with = dir.path().join("with.parquet");
    let t: Vec<u64> = (0..40)
        .filter(|t| ![3, 4, 11, 17, 18, 19, 30].contains(t))
        .collect();
    rezolus_parquet_at(&with, &t, true);
    // The same file without the key.
    let b = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&with).unwrap()).unwrap();
    let kv: Vec<KeyValue> = b
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.key != "sampling_interval_ms" && e.key != "ARROW:schema")
        .collect();
    let schema = b.schema().clone();
    let batches: Vec<RecordBatch> = b.build().unwrap().map(|x| x.unwrap()).collect();
    let without = dir.path().join("without.parquet");
    let props = parquet::file::properties::WriterProperties::builder()
        .set_key_value_metadata(Some(kv))
        .build();
    let schema = Arc::new(Schema::new(schema.fields().clone()));
    let mut w = parquet::arrow::ArrowWriter::try_new(
        std::fs::File::create(&without).unwrap(),
        schema,
        Some(props),
    )
    .unwrap();
    for batch in batches {
        w.write(&batch).unwrap();
    }
    w.close().unwrap();
    assert_eq!(
        MetricsSource::interval(&ParquetReader::open(&without).unwrap()),
        1.0
    );
    let (_, c) = check_parquet(&without);
    assert_eq!(c.noise, 0, "{c:?}");
}

/// A file of wide histograms, decoded a few rows at a time (the batch size
/// is `to_dendro`'s unit test), converts and reads as it did.
#[test]
fn a_parquet_of_wide_histograms_converts() {
    let list = DataType::List(Arc::new(Field::new("item", DataType::UInt64, true)));
    // A smaller file of the same shape, converted a few rows at a time,
    // reads as it did: 2 columns of 11,264 buckets is 176 KiB a row.
    let (gp, mvp) = (10u32, 20u32);
    let buckets = ((mvp - gp + 1) as usize) << gp;
    let md = |name: &str| -> HashMap<String, String> {
        [
            ("metric", name),
            ("metric_type", "histogram"),
            ("grouping_power", "10"),
            ("max_value_power", "20"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    };
    let schema = Arc::new(Schema::new(vec![
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("duration", DataType::UInt64, true),
        Field::new("h0", list.clone(), true).with_metadata(md("h0")),
        Field::new("h1", list.clone(), true).with_metadata(md("h1")),
    ]));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sparse.parquet");
    let props = parquet::file::properties::WriterProperties::builder()
        .set_compression(parquet::basic::Compression::ZSTD(Default::default()))
        .build();
    let mut w = parquet::arrow::ArrowWriter::try_new(
        std::fs::File::create(&path).unwrap(),
        Arc::clone(&schema),
        Some(props),
    )
    .unwrap();
    let rows = 12u64;
    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from_iter_values(
            (0..rows).map(|t| BASE_NS + t * 1_000_000_000),
        )),
        Arc::new(UInt64Array::from(vec![1000u64; rows as usize])),
    ];
    for _ in 0..2 {
        let mut b = ListBuilder::new(UInt64Builder::new());
        for t in 0..rows {
            for i in 0..buckets {
                b.values().append_value(if i == 100 { t } else { 0 });
            }
            b.append(true);
        }
        columns.push(Arc::new(b.finish()));
    }
    w.write(&RecordBatch::try_new(Arc::clone(&schema), columns).unwrap())
        .unwrap();
    w.close().unwrap();
    let dest = dir.path().join("out.dendro");
    let written = ParquetToArchive::new()
        .convert_file_path(&path, &dest)
        .unwrap();
    assert_eq!(written.rows, rows);
    let original = ParquetReader::open(&path).unwrap();
    let (_, converted) = open_archive(&dest).remove(0);
    let (lo, hi) = original.time_range().unwrap();
    for q in ["histogram_count(h0)", "histogram_quantile(0.5, h1)"] {
        assert_eq!(
            result_json(original.query_range(q, lo, hi, 1.0)),
            result_json(converted.query_range(q, lo, hi, 1.0)),
            "{q}"
        );
    }
}
