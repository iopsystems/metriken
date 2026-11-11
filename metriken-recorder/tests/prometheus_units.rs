//! Units in Prometheus metric names: exposition text through
//! `PrometheusConverter::convert` and `SourceRecorder::stage` into an
//! archive, read back by `ArchiveReader` and queried through metriken-query,
//! and the same scrapes ingested into a live `MemoryStore`.
//!
//! What is checked, from `docs/journal/2026-11-09-prometheus-units.md`:
//! each token class is stored in its normalized unit under its stored name
//! with the exporter's name in `__prometheus_name__`; a `# UNIT` line takes
//! precedence over the name; `_sum`, `_count` and `_total` are renamed and
//! `_count` is not scaled; a histogram's buckets, `_sum`, overflow and
//! overflow bound agree; a summary's quantiles are scaled; bits are bytes;
//! a collision keeps the original names, in one scrape and from the scrape
//! it starts in, for good; a query by an exporter name reads the stored
//! series (D1a), decided per selector over the recordings it reaches (D2a),
//! preferring an exact name, in PromQL and in the histogram functions,
//! recorded and live; an older recording with the original names beside a
//! newer one; and `__prometheus_name__` is ignored by binary matching.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use dendro::seal::SealPolicy;
use metriken_model::Snapshot;
use metriken_query::{
    MatrixSample, MetricsSource, QueryOptions, QueryResult, Scope, ScopedSource, SourceRef,
};
use metriken_recorder::PrometheusConverter;
use metriken_storage::prometheus_units::normalize;
use metriken_storage::{
    is_internal_label, is_matched_label, ArchiveReader, ArchiveWriter, BufferPool, DendroCatalog,
    WriterConfig, OVERFLOW_BOUND_KEY, OVERFLOW_KEY, PROMETHEUS_NAME_LABEL, SCALE_KEY,
};

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;
const TICKS: u64 = 12;

// ─── What an exporter serves at tick `t` ────────────────────────────────────

/// One scrape of every token class. Per tick: `rpc_seconds` takes 5
/// observations at or below 1 ms, 4 in (1 ms, 10 ms] and 1 above, 20 ms
/// in all; `rx_bits_total` 800 bits; `wait_total` (`# UNIT wait seconds`)
/// 2 s; `up_us_total` 1000 µs.
fn scrape(t: u64) -> String {
    format!(
        "\
# TYPE rpc_seconds histogram
rpc_seconds_bucket{{le=\"0.001\"}} {a}
rpc_seconds_bucket{{le=\"0.01\"}} {b}
rpc_seconds_bucket{{le=\"+Inf\"}} {c}
rpc_seconds_sum {rpc_sum}
rpc_seconds_count {c}
# TYPE q_ms summary
q_ms{{quantile=\"0.5\"}} 2.5
q_ms{{quantile=\"0.99\"}} 7
q_ms_sum {q_sum}
q_ms_count {c}
# TYPE rx_bits_total counter
rx_bits_total {bits}
# TYPE mem_kib gauge
mem_kib 4
disk_mb 2
up_us_total {us}
# TYPE wait counter
# UNIT wait seconds
wait_total {wait}
# UNIT lag_ms seconds
lag_ms 0.5
cpu_percent 50
hit_ratio 0.5
temp_celsius 21.5
clock_hertz 2000
limit_cores 0.25
# TYPE size histogram
size_bucket{{le=\"0.5\"}} {a}
size_bucket{{le=\"5\"}} {b}
size_bucket{{le=\"+Inf\"}} {c}
plain_total {c}
",
        a = 5 * t,
        b = 9 * t,
        c = 10 * t,
        rpc_sum = 0.02 * t as f64,
        q_sum = 30 * t,
        bits = 800 * t,
        us = 1000 * t,
        wait = 2 * t,
    )
}

/// Two names that normalize to one, in every scrape.
fn same_scrape_collision(t: u64) -> String {
    format!("dup_seconds_total {t}\ndup_ms_total {}\n", 1000 * t)
}

/// `late_seconds_total` throughout; the exporter's own `late_nanoseconds_total`
/// at ticks 6 to 8 only.
fn mid_run_collision(t: u64) -> String {
    let mut text = format!("late_seconds_total {}\n", 2 * t);
    if (6..9).contains(&t) {
        text.push_str(&format!("late_nanoseconds_total {}\n", 7 * t));
    }
    text
}

// ─── Writing ────────────────────────────────────────────────────────────────

/// What the converter wrote before units were normalized, for the series
/// `scrape` serves: each renamed series under its exporter's name, without
/// `__prometheus_name__`, `unit` or `scale`, its scalars divided back by
/// the factor. A `_seconds` histogram's buckets were ×1e9 before too, so
/// its buckets are unchanged; its overflow series takes the old name.
fn as_before_units(mut snapshot: Snapshot) -> Snapshot {
    let Snapshot::V3(v3) = &mut snapshot else {
        panic!("a scrape is a V3 snapshot")
    };
    for group in &mut v3.groups {
        let schema = Arc::make_mut(group.schema.as_mut().unwrap());
        let restore = |meta: &mut BTreeMap<String, String>| -> f64 {
            meta.remove("unit");
            meta.remove(SCALE_KEY);
            let Some(original) = meta.remove(PROMETHEUS_NAME_LABEL) else {
                return 1.0;
            };
            let stored = meta["metric"].clone();
            if stored.ends_with("_overflow") && !original.ends_with("_overflow") {
                meta.insert("metric".into(), format!("{original}_overflow"));
                return 1.0;
            }
            if let Some(link) = meta.get_mut(OVERFLOW_KEY) {
                *link = format!("{original}_overflow");
            }
            meta.insert("metric".into(), original.clone());
            normalize(&original, None).map_or(1.0, |n| n.factor)
        };
        for (d, v) in schema
            .float_counters
            .iter_mut()
            .zip(&mut group.float_counters)
        {
            let factor = restore(&mut d.metadata);
            *v = v.map(|v| v / factor);
        }
        for (d, v) in schema.float_gauges.iter_mut().zip(&mut group.float_gauges) {
            let factor = restore(&mut d.metadata);
            *v = v.map(|v| v / factor);
        }
        for d in &mut schema.histograms {
            restore(&mut d.metadata);
        }
        group.schema_hash = schema.hash();
    }
    snapshot
}

/// One recording to write: its `source` label, the text at each tick, and
/// whether it is written as before units were normalized.
struct Rec<'a> {
    source: &'a str,
    text: &'a dyn Fn(u64) -> String,
    old: bool,
}

/// Each recording scraped once a second through its own converter into one
/// archive, sealing every 5 rows.
fn record(path: &Path, recs: &[Rec]) {
    let labelled: Vec<Labelled> = recs
        .iter()
        .map(|r| (vec![("source", r.source)], r.text, r.old))
        .collect();
    record_labelled(path, &labelled);
}

/// A recording to write: its labels, the text at each tick, and whether it
/// is written as before units were normalized.
type Labelled<'a> = (Vec<(&'a str, &'a str)>, &'a dyn Fn(u64) -> String, bool);

/// [`record`] with each recording's labels given whole.
fn record_labelled(path: &Path, recs: &[Labelled]) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    for (labels, text, old) in recs {
        let labels = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
        let mut converter = PrometheusConverter::new();
        for t in 0..TICKS {
            let ts = BASE + t * S;
            let mut snapshot = converter.convert(&text(t), ts - S / 2, ts);
            if *old {
                snapshot = as_before_units(snapshot);
            }
            let staged = source.stage(&snapshot, ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
            source.sync().unwrap();
        }
        source.finalize((BASE + (TICKS - 1) * S, 0)).unwrap();
    }
    writer.join().unwrap();
}

fn recorded(text: &dyn Fn(u64) -> String) -> (tempfile::TempDir, ArchiveReader) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    record(
        &path,
        &[Rec {
            source: "exporter",
            text,
            old: false,
        }],
    );
    let reader = open(&path);
    (dir, reader)
}

/// `text` scraped once a second into a live `MemoryStore`.
fn live(text: &dyn Fn(u64) -> String) -> metriken_query::MemoryStore {
    let store = metriken_query::MemoryStore::builder()
        .sampling_interval_ms(1000)
        .build();
    let mut converter = PrometheusConverter::new();
    for t in 0..TICKS {
        let ts = BASE + t * S;
        let mut snapshot = converter.convert(&text(t), ts - S / 2, ts);
        let Snapshot::V3(v3) = &mut snapshot else {
            panic!("a scrape is a V3 snapshot")
        };
        v3.systemtime = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ts);
        store.ingest_snapshot(snapshot);
    }
    store
}

// ─── Reading ────────────────────────────────────────────────────────────────

/// Every source of the archive as one reader.
fn open(path: &Path) -> ArchiveReader {
    let recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(path).unwrap()),
        None,
        BufferPool::new(64 << 20),
        None,
    )
    .unwrap();
    ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None)
}

/// `query`'s result series over the recorded ticks at a 1 s step.
fn matrix(source: &dyn MetricsSource, query: &str) -> Vec<MatrixSample> {
    let (lo, hi) = (BASE / S, BASE / S + TICKS - 1);
    let result = source
        .query_range(query, lo as f64, hi as f64, 1.0)
        .unwrap_or_else(|e| panic!("{query}: {e}"));
    let QueryResult::Matrix { result, .. } = result else {
        panic!("{query}: not a matrix")
    };
    result
}

/// The one series of `query`: its labels and its values.
fn one(source: &dyn MetricsSource, query: &str) -> (HashMap<String, String>, Vec<f64>) {
    let mut m = matrix(source, query);
    assert_eq!(m.len(), 1, "{query}: {m:?}");
    let m = m.remove(0);
    assert!(!m.values.is_empty(), "{query}: no points");
    (m.metric, m.values.into_iter().map(|(_, v)| v).collect())
}

/// Every value of `query`'s one series is within 1e-9 (relative) of `want`.
fn every(source: &dyn MetricsSource, query: &str, want: f64) -> HashMap<String, String> {
    let (labels, values) = one(source, query);
    for v in &values {
        assert!(
            (v - want).abs() <= want.abs() * 1e-9,
            "{query}: want {want}, got {values:?}"
        );
    }
    labels
}

/// The value of `query` is the stored name's, and the result is named by
/// the stored name with the exporter's name as `__prometheus_name__`.
fn reads_stored(source: &dyn MetricsSource, query: &str, want: f64, stored: &str, original: &str) {
    let labels = every(source, query, want);
    assert_eq!(
        labels.get("__name__").map(String::as_str),
        Some(stored),
        "{query}: {labels:?}"
    );
    assert_eq!(
        labels.get(PROMETHEUS_NAME_LABEL).map(String::as_str),
        Some(original),
        "{query}: {labels:?}"
    );
}

/// The converter's first scrape of `text`: each stored name's metadata.
fn converted(text: &str) -> HashMap<String, Vec<HashMap<String, String>>> {
    let mut snap = PrometheusConverter::new().convert(text, 0, 1);
    let mut out: HashMap<String, Vec<HashMap<String, String>>> = HashMap::new();
    let all = snap
        .float_counters()
        .into_iter()
        .map(|c| c.metadata)
        .chain(snap.float_gauges().into_iter().map(|g| g.metadata))
        .chain(snap.histograms().into_iter().map(|h| h.metadata));
    for meta in all {
        out.entry(meta["metric"].clone()).or_default().push(meta);
    }
    out
}

/// The first scrape's values by stored name and the `quantile` label.
fn values(text: &str) -> HashMap<(String, Option<String>), f64> {
    let mut snap = PrometheusConverter::new().convert(text, 0, 1);
    snap.float_counters()
        .into_iter()
        .map(|c| (c.metadata, c.value))
        .chain(
            snap.float_gauges()
                .into_iter()
                .map(|g| (g.metadata, g.value)),
        )
        .map(|(m, v)| ((m["metric"].clone(), m.get("quantile").cloned()), v))
        .collect()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

/// Each token class's stored name, unit, label and value, from one scrape.
#[test]
fn each_token_class_is_stored_in_its_normalized_unit() {
    let text = scrape(1);
    let meta = converted(&text);
    let v = values(&text);
    let value = |name: &str| v[&(name.to_string(), None)];
    let only = |name: &str| {
        let m = meta
            .get(name)
            .unwrap_or_else(|| panic!("{name}: {:?}", meta.keys()));
        assert_eq!(m.len(), 1, "{name}");
        m[0].clone()
    };
    // (stored, exporter name or None when unchanged, unit, value at tick 1)
    for (stored, original, unit, want) in [
        (
            "rpc_nanoseconds_sum",
            Some("rpc_seconds_sum"),
            Some("nanoseconds"),
            0.02e9,
        ),
        (
            "rpc_nanoseconds_count",
            Some("rpc_seconds_count"),
            None,
            10.0,
        ),
        (
            "q_nanoseconds_sum",
            Some("q_ms_sum"),
            Some("nanoseconds"),
            30e6,
        ),
        ("q_nanoseconds_count", Some("q_ms_count"), None, 10.0),
        (
            "rx_bytes_total",
            Some("rx_bits_total"),
            Some("bytes"),
            100.0,
        ),
        ("mem_bytes", Some("mem_kib"), Some("bytes"), 4096.0),
        ("disk_bytes", Some("disk_mb"), Some("bytes"), 2e6),
        (
            "up_nanoseconds_total",
            Some("up_us_total"),
            Some("nanoseconds"),
            1e6,
        ),
        (
            "wait_nanoseconds_total",
            Some("wait_total"),
            Some("nanoseconds"),
            2e9,
        ),
        (
            "lag_nanoseconds",
            Some("lag_ms"),
            Some("nanoseconds"),
            0.5e9,
        ),
        ("cpu_percent", None, Some("percentage"), 50.0),
        ("hit_ratio", None, Some("ratio"), 0.5),
        ("temp_celsius", None, Some("celsius"), 21.5),
        ("clock_hertz", None, Some("hertz"), 2000.0),
        ("limit_cores", None, Some("cores"), 0.25),
        ("plain_total", None, None, 10.0),
    ] {
        let m = only(stored);
        assert_eq!(
            m.get(PROMETHEUS_NAME_LABEL).map(String::as_str),
            original,
            "{stored}"
        );
        assert_eq!(m.get("unit").map(String::as_str), unit, "{stored}");
        assert_eq!(value(stored), want, "{stored}");
    }
    // A summary's quantiles, ×1e6.
    let q = |quantile: &str| v[&("q_nanoseconds".to_string(), Some(quantile.to_string()))];
    assert_eq!((q("0.5"), q("0.99")), (2.5e6, 7e6));
    for m in &meta["q_nanoseconds"] {
        assert_eq!(m[PROMETHEUS_NAME_LABEL], "q_ms");
        assert_eq!(m["unit"], "nanoseconds");
    }
    // The histogram: ×1e9 with no generic scale, its overflow named after
    // the stored name, carrying the exporter's name, its bound ×1e9.
    let h = only("rpc_nanoseconds");
    assert_eq!(h[PROMETHEUS_NAME_LABEL], "rpc_seconds");
    assert_eq!(h["unit"], "nanoseconds");
    assert!(!h.contains_key(SCALE_KEY), "{h:?}");
    assert_eq!(h[OVERFLOW_KEY], "rpc_nanoseconds_overflow");
    assert_eq!(h[OVERFLOW_BOUND_KEY], "10000000");
    let o = only("rpc_nanoseconds_overflow");
    assert_eq!(o[PROMETHEUS_NAME_LABEL], "rpc_seconds");
    assert!(!o.contains_key("unit"), "a count: {o:?}");
    assert_eq!(value("rpc_nanoseconds_overflow"), 1.0);
    // A histogram with no unit: the generic scale, recorded, the bound
    // scaled with the buckets.
    let size = only("size");
    assert_eq!(size[SCALE_KEY], "10");
    assert_eq!(size[OVERFLOW_BOUND_KEY], "50");
    assert!(!size.contains_key("unit") && !size.contains_key(PROMETHEUS_NAME_LABEL));
}

/// A `_ratio` histogram's bounds below 1 take the generic scale too, and
/// a `_bits` histogram's are divided by 8 first.
#[test]
fn a_unit_histogram_below_one_takes_the_generic_scale() {
    let text = "# TYPE r_ratio histogram\n\
                r_ratio_bucket{le=\"0.25\"} 1\nr_ratio_bucket{le=\"+Inf\"} 2\n\
                # TYPE p_bits histogram\n\
                p_bits_bucket{le=\"4\"} 1\np_bits_bucket{le=\"+Inf\"} 2\n";
    let meta = converted(text);
    let r = &meta["r_ratio"][0];
    assert_eq!((r["unit"].as_str(), r[SCALE_KEY].as_str()), ("ratio", "10"));
    assert_eq!(r[OVERFLOW_BOUND_KEY], "2.5");
    let p = &meta["p_bytes"][0];
    assert_eq!((p["unit"].as_str(), p[SCALE_KEY].as_str()), ("bytes", "10"));
    assert_eq!(p[OVERFLOW_BOUND_KEY], "5");
    assert_eq!(p[PROMETHEUS_NAME_LABEL], "p_bits");
}

/// Recorded and queried: the renamed series by their stored names, the
/// histogram's quantiles and band in nanoseconds agreeing with its `_sum`,
/// bits as a byte rate, and the mean from `_sum` over `_count` (whose
/// `__prometheus_name__` values differ, and matching ignores them).
#[test]
fn recorded_series_read_in_their_normalized_units() {
    let (_dir, reader) = recorded(&scrape);
    every(&reader, "rate(rx_bytes_total[3s])", 100.0);
    every(&reader, "rate(rpc_nanoseconds_sum[3s])", 0.02e9);
    every(&reader, "rate(wait_nanoseconds_total[3s])", 2e9);
    every(&reader, "disk_bytes", 2e6);
    // One result series: the operands' `__prometheus_name__` values differ.
    every(
        &reader,
        "rate(rpc_nanoseconds_sum[3s]) / rate(rpc_nanoseconds_count[3s])",
        2e6,
    );
    // `without` drops it as it drops every internal label.
    let summed = every(
        &reader,
        "sum without (source) (rate(rpc_nanoseconds_sum[3s]))",
        0.02e9,
    );
    assert!(!summed.contains_key(PROMETHEUS_NAME_LABEL), "{summed:?}");
    assert!(!is_matched_label(PROMETHEUS_NAME_LABEL));
    assert!(is_internal_label(PROMETHEUS_NAME_LABEL));

    // 5 of 10 a tick at or below 1 ms: the median's bucket ends within the
    // histogram's relative error of 1e6 ns.
    let (_, median) = one(&reader, "histogram_quantile(0.5, rpc_nanoseconds)");
    assert!(
        median.iter().all(|v| (*v / 1e6 - 1.0).abs() < 0.01),
        "{median:?}"
    );
    // 1 of 10 above 10 ms: the 0.95 quantile is +Inf, its band from the
    // bound in nanoseconds.
    let m = matrix(&reader, "histogram_quantile(0.95, rpc_nanoseconds)");
    let bands = m[0].intervals.clone().unwrap();
    assert!(
        m[0].values.iter().all(|(_, v)| *v == f64::INFINITY),
        "{m:?}"
    );
    assert!(
        bands.iter().all(|b| *b == (1e7, f64::INFINITY)),
        "{bands:?}"
    );
}

/// A query by the exporter's name, held by no series, reads the stored
/// series as stored and names it by the stored name (D1a): plain selectors,
/// `histogram_quantile`, and the histogram functions parsed by hand.
#[test]
fn a_query_by_the_exporter_name_reads_the_stored_series() {
    let (_dir, reader) = recorded(&scrape);
    reads_stored(
        &reader,
        "rate(rx_bits_total[3s])",
        100.0,
        "rx_bytes_total",
        "rx_bits_total",
    );
    reads_stored(
        &reader,
        "rate(rpc_seconds_sum[3s])",
        0.02e9,
        "rpc_nanoseconds_sum",
        "rpc_seconds_sum",
    );
    reads_stored(
        &reader,
        "{__name__=\"lag_ms\"}",
        0.5e9,
        "lag_nanoseconds",
        "lag_ms",
    );
    // Each side resolves on its own; matching ignores the two
    // `__prometheus_name__` values.
    every(
        &reader,
        "rate(rpc_seconds_sum[3s]) / rate(rpc_seconds_count[3s])",
        2e6,
    );
    // The PromQL histogram function and the hand-parsed ones.
    let m = matrix(&reader, "histogram_quantile(0.95, rpc_seconds)");
    assert_eq!(m.len(), 1);
    assert_eq!(
        m[0].metric.get("__name__").map(String::as_str),
        Some("rpc_nanoseconds")
    );
    assert!(m[0].values.iter().all(|(_, v)| *v == f64::INFINITY));
    let m = matrix(&reader, "histogram_quantiles([0.95], rpc_seconds)");
    assert_eq!(m.len(), 1, "{m:?}");
    assert!(
        m[0].values.iter().all(|(_, v)| *v == f64::INFINITY),
        "{m:?}"
    );
    let (lo, hi) = ((BASE / S) as f64, (BASE / S + TICKS - 1) as f64);
    reader
        .query_range("histogram_heatmap(rpc_seconds)", lo, hi, 1.0)
        .expect("the heatmap finds the stored histogram");
    // Instant queries take the same path.
    let QueryResult::Vector { result, .. } = reader.query("lag_ms", None).unwrap() else {
        panic!("not a vector")
    };
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].value.1, 0.5e9);
    // A name no series was renamed from is still not found.
    assert!(reader
        .query_range("nothing_seconds", 0.0, 1.0, 1.0)
        .is_err());
}

/// The live store answers the same queries as the recording.
#[test]
fn the_live_store_resolves_exporter_names_as_the_recording_does() {
    let (_dir, reader) = recorded(&scrape);
    let store = live(&scrape);
    for q in [
        "rate(rx_bits_total[3s])",
        "rate(rpc_seconds_sum[3s])",
        "rate(rpc_seconds_sum[3s]) / rate(rpc_seconds_count[3s])",
        "histogram_quantile(0.95, rpc_seconds)",
        "histogram_quantiles([0.5], rpc_seconds)",
        "q_ms{quantile=\"0.99\"}",
        "lag_ms",
    ] {
        let lived = matrix(&store, q);
        assert!(!lived.is_empty(), "{q}");
        let recorded = matrix(&reader, q);
        let strip = |m: Vec<MatrixSample>| -> Vec<(BTreeMap<String, String>, Vec<f64>)> {
            let mut out: Vec<_> = m
                .into_iter()
                .map(|s| {
                    let labels: BTreeMap<String, String> = s
                        .metric
                        .into_iter()
                        .filter(|(k, _)| k != "source" && k != "__recording__")
                        .collect();
                    (labels, s.values.into_iter().map(|(_, v)| v).collect())
                })
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        };
        assert_eq!(strip(lived), strip(recorded), "{q}: live, then recorded");
    }
}

/// An archive with an older recording of the endpoint (exporter names,
/// exporter units) and a newer one (stored names): an exact name is
/// preferred, so the exporter's name reads the older recording only, in
/// its units, and the stored name reads the newer only. A selector that
/// reaches only the newer recording falls back there (D2a), even beside a
/// selector of the same name that reads the older one.
#[test]
fn an_older_recording_with_exporter_names_beside_a_newer_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    record(
        &path,
        &[
            Rec {
                source: "old",
                text: &scrape,
                old: true,
            },
            Rec {
                source: "new",
                text: &scrape,
                old: false,
            },
        ],
    );
    let reader = open(&path);
    let (labels, _) = one(&reader, "rate(rpc_seconds_sum[3s])");
    assert_eq!(labels["source"], "old");
    every(&reader, "rate(rpc_seconds_sum[3s])", 0.02);
    let (labels, _) = one(&reader, "rate(rpc_nanoseconds_sum[3s])");
    assert_eq!(labels["source"], "new");
    every(&reader, "rate(rpc_nanoseconds_sum[3s])", 0.02e9);

    // Finite values tell the units apart: a summary quantile in ms in the
    // older recording, in ns in the newer one; and the older histogram's
    // own name (its buckets were ×1e9 before this change too) beside the
    // newer through `{source="new"}`.
    let (labels, _) = one(&reader, "q_ms{quantile=\"0.99\"}");
    assert_eq!(labels["source"], "old");
    every(&reader, "q_ms{quantile=\"0.99\"}", 7.0);
    reads_stored(
        &reader,
        "q_ms{quantile=\"0.99\", source=\"new\"}",
        7e6,
        "q_nanoseconds",
        "q_ms",
    );
    let (labels, median) = one(&reader, "histogram_quantile(0.5, rpc_seconds)");
    assert_eq!(
        labels.get("__name__").map(String::as_str),
        Some("rpc_seconds")
    );
    assert!(
        median.iter().all(|v| (*v / 1e6 - 1.0).abs() < 0.01),
        "{median:?}"
    );
    let (labels, median) = one(
        &reader,
        "histogram_quantile(0.5, rpc_seconds{source=\"new\"})",
    );
    assert_eq!(
        labels.get("__name__").map(String::as_str),
        Some("rpc_nanoseconds")
    );
    assert!(
        median.iter().all(|v| (*v / 1e6 - 1.0).abs() < 0.01),
        "{median:?}"
    );

    // Per selector: `{source="new"}` reaches only the newer recording.
    reads_stored(
        &reader,
        "rate(rpc_seconds_sum{source=\"new\"}[3s])",
        0.02e9,
        "rpc_nanoseconds_sum",
        "rpc_seconds_sum",
    );
    every(&reader, "rate(rpc_seconds_sum{source=\"old\"}[3s])", 0.02);
    let m = matrix(
        &reader,
        "rate(rpc_seconds_sum{source=\"old\"}[3s]) + ignoring(source) rate(rpc_seconds_sum{source=\"new\"}[3s])",
    );
    assert_eq!(m.len(), 1, "{m:?}");
    assert!(m[0]
        .values
        .iter()
        .all(|(_, v)| (v - (0.02 + 0.02e9)).abs() < 1e-3));
    // The histogram: each recording's quantile, the older under its own
    // name with its old overflow link, the newer through the fallback.
    let m = matrix(&reader, "histogram_quantile(0.95, rpc_seconds)");
    assert_eq!(m.len(), 1, "{m:?}");
    assert_eq!(
        m[0].metric.get("__name__").map(String::as_str),
        Some("rpc_seconds"),
        "the older recording's own name: {m:?}"
    );
    assert!(
        m[0].values.iter().all(|(_, v)| *v == f64::INFINITY),
        "{m:?}"
    );
    let m = matrix(
        &reader,
        "histogram_quantiles([0.95], rpc_seconds{source=\"new\"})",
    );
    assert_eq!(m.len(), 1, "{m:?}");
    assert!(
        m[0].values.iter().all(|(_, v)| *v == f64::INFINITY),
        "{m:?}"
    );
    // A scope with only the newer recording falls back too.
    let new = reader
        .sources()
        .iter()
        .position(|s| s.labels.get("source").map(String::as_str) == Some("new"))
        .expect("the newer source");
    let scope = Scope::new(vec![ScopedSource::new(SourceRef::Position(new), None)]).unwrap();
    let opts = QueryOptions::default().with_scope(Some(Arc::new(scope)));
    let (lo, hi) = ((BASE / S) as f64, (BASE / S + TICKS - 1) as f64);
    let QueryResult::Matrix { result, .. } = reader
        .query_range_opts("rate(rpc_seconds_sum[3s])", lo, hi, 1.0, &opts)
        .unwrap()
    else {
        panic!("not a matrix")
    };
    assert_eq!(result.len(), 1, "{result:?}");
    assert_eq!(result[0].metric["__name__"], "rpc_nanoseconds_sum");
}

/// Two exporter names that normalize to one keep their own names and
/// units in every scrape.
#[test]
fn a_collision_in_one_scrape_keeps_both_names() {
    let (_dir, reader) = recorded(&same_scrape_collision);
    every(&reader, "rate(dup_seconds_total[3s])", 1.0);
    every(&reader, "rate(dup_ms_total[3s])", 1000.0);
    assert!(!reader
        .counter_names()
        .contains(&"dup_nanoseconds_total".to_string()));
    let (labels, _) = one(&reader, "rate(dup_seconds_total[3s])");
    assert!(!labels.contains_key(PROMETHEUS_NAME_LABEL), "{labels:?}");
}

/// A collision that starts mid-run: `late_seconds_total` is stored as
/// `late_nanoseconds_total` until the exporter serves its own, and as
/// itself from that scrape on, also after the exporter's own goes away.
#[test]
fn a_collision_mid_run_renames_back_for_good() {
    let mut converter = PrometheusConverter::new();
    let names = |c: &mut PrometheusConverter, t: u64| {
        let mut snap = c.convert(&mid_run_collision(t), 0, 1);
        let mut out: Vec<(String, Option<String>, f64)> = snap
            .float_counters()
            .into_iter()
            .map(|c| {
                (
                    c.metadata["metric"].clone(),
                    c.metadata.get(PROMETHEUS_NAME_LABEL).cloned(),
                    c.value,
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    };
    let renamed = Some("late_seconds_total".to_string());
    assert_eq!(
        names(&mut converter, 5),
        vec![("late_nanoseconds_total".into(), renamed, 10e9)]
    );
    assert_eq!(
        names(&mut converter, 6),
        vec![
            ("late_nanoseconds_total".into(), None, 42.0),
            ("late_seconds_total".into(), None, 12.0),
        ]
    );
    assert_eq!(
        names(&mut converter, 10),
        vec![("late_seconds_total".into(), None, 20.0)]
    );

    // Recorded: both names, each with the rate of the rows it was written
    // under.
    let (_dir, reader) = recorded(&mid_run_collision);
    every(
        &reader,
        "rate(late_nanoseconds_total{__prometheus_name__=\"late_seconds_total\"}[2s])",
        2e9,
    );
    every(&reader, "rate(late_seconds_total[2s])", 2.0);
    every(
        &reader,
        "rate(late_nanoseconds_total{__prometheus_name__!~\".+\"}[2s])",
        7.0,
    );
}

/// A HELP text is kept under the exporter's name and the stored one.
#[test]
fn a_description_is_kept_under_both_names() {
    let mut converter = PrometheusConverter::new();
    converter.convert(
        "# HELP rpc_seconds Latency.\n# TYPE rpc_seconds gauge\nrpc_seconds 1\n",
        0,
        1,
    );
    let map: BTreeMap<String, String> =
        serde_json::from_str(&converter.pending_descriptions().unwrap()).unwrap();
    assert_eq!(map.get("rpc_seconds").map(String::as_str), Some("Latency."));
    assert_eq!(
        map.get("rpc_nanoseconds").map(String::as_str),
        Some("Latency.")
    );
}

/// A histogram whose name collides with another family's (`lat_seconds`
/// beside a gauge `lat_ms`, both `lat_nanoseconds`): the whole family is
/// kept as served, its `_sum` and `_count` with its buckets, so the
/// buckets and `_sum` stay in one unit.
#[test]
fn a_collision_keeps_the_whole_family() {
    let text = |t: u64| {
        format!(
            "# TYPE lat_seconds histogram\n\
             lat_seconds_bucket{{le=\"0.1\"}} {t}\n\
             lat_seconds_bucket{{le=\"+Inf\"}} {t}\n\
             lat_seconds_sum {}\n\
             lat_seconds_count {t}\n\
             # TYPE lat_ms gauge\n\
             lat_ms 3\n",
            0.05 * t as f64
        )
    };
    let meta = converted(&text(1));
    let mut names: Vec<&String> = meta.keys().collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "lat_ms",
            "lat_seconds",
            "lat_seconds_count",
            "lat_seconds_overflow",
            "lat_seconds_sum"
        ]
    );
    for (name, metas) in &meta {
        for m in metas {
            assert!(!m.contains_key(PROMETHEUS_NAME_LABEL), "{name}: {m:?}");
            assert!(!m.contains_key("unit"), "{name}: {m:?}");
        }
    }
    let v = values(&text(1));
    assert_eq!(v[&("lat_seconds_sum".to_string(), None)], 0.05);
    // The buckets are seconds × the generic scale, recorded.
    assert_eq!(meta["lat_seconds"][0][SCALE_KEY], "10");
    let (_dir, reader) = recorded(&text);
    every(&reader, "rate(lat_seconds_sum[3s])", 0.05);
}

/// An untyped `_bucket` sample (no `# TYPE ... histogram`) is kept as
/// served: its `le` label is in the exporter's unit, which a rename would
/// not change.
#[test]
fn an_untyped_bucket_is_kept_as_served() {
    let meta = converted("foo_seconds_bucket{le=\"0.1\"} 3\n");
    let m = &meta["foo_seconds_bucket"][0];
    assert_eq!(m["le"], "0.1");
    assert!(
        !m.contains_key(PROMETHEUS_NAME_LABEL) && !m.contains_key("unit"),
        "{m:?}"
    );
    assert_eq!(
        values("foo_seconds_bucket{le=\"0.1\"} 3\n")[&("foo_seconds_bucket".into(), None)],
        3.0
    );
}

/// One name stored under two names by two endpoints' `# UNIT` lines
/// (`x_total` as `x_nanoseconds_total` and as `x_bytes_total`): a selector
/// reaching both is an error naming both, live and recorded. Over the
/// archive a recording-label matcher picks one; on any source, live
/// included, the stored name with `__prometheus_name__="x_total"` does,
/// and the error names that form.
#[test]
fn an_exporter_name_stored_under_two_names_is_an_error() {
    let a = |t: u64| format!("# TYPE x counter\n# UNIT x seconds\nx_total{{job=\"a\"}} {t}\n");
    let b = |t: u64| format!("# TYPE x counter\n# UNIT x kilobytes\nx_total{{job=\"b\"}} {t}\n");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    record(
        &path,
        &[
            Rec {
                source: "a",
                text: &a,
                old: false,
            },
            Rec {
                source: "b",
                text: &b,
                old: false,
            },
        ],
    );
    let reader = open(&path);
    let (lo, hi) = ((BASE / S) as f64, (BASE / S + TICKS - 1) as f64);
    for q in ["rate(x_total[3s])", "rate(x_total{job=\"b\"}[3s])"] {
        let err = reader.query_range(q, lo, hi, 1.0).expect_err(q).to_string();
        assert!(
            err.contains("x_nanoseconds_total") && err.contains("x_bytes_total"),
            "{q}: {err}"
        );
    }
    reads_stored(
        &reader,
        "rate(x_total{source=\"b\"}[3s])",
        1000.0,
        "x_bytes_total",
        "x_total",
    );
    reads_stored(
        &reader,
        "rate(x_total{source=\"a\"}[3s])",
        1e9,
        "x_nanoseconds_total",
        "x_total",
    );
    // A selector with `or` reaches the recordings its branches match: one
    // holding one stored name reads it, and two holding both are an error.
    reads_stored(
        &reader,
        "rate(x_total{source=\"b\" or source=\"zzz\"}[3s])",
        1000.0,
        "x_bytes_total",
        "x_total",
    );
    for q in [
        "rate(x_total{source=\"a\" or source=\"b\"}[3s])",
        // Beside a selector of the same name reaching both recordings, the
        // `or` selector keeps its own reach, in either order.
        "rate(x_total{source=\"b\" or source=\"zzz\"}[3s]) + rate(x_total[3s])",
        "rate(x_total[3s]) + rate(x_total{source=\"b\" or source=\"zzz\"}[3s])",
    ] {
        let err = reader.query_range(q, lo, hi, 1.0).expect_err(q).to_string();
        assert!(
            err.contains("x_nanoseconds_total") && err.contains("x_bytes_total"),
            "{q}: {err}"
        );
    }
    // Two `or` selectors whose branches reach different recordings each
    // read their own stored name.
    every(
        &reader,
        "sum(rate(x_total{source=\"b\" or source=\"zzz\"}[3s])) \
         + sum(rate(x_total{source=\"a\" or source=\"zzz\"}[3s]))",
        1000.0 + 1e9,
    );

    // Live: one store over both endpoints.
    let store = metriken_query::MemoryStore::builder()
        .sampling_interval_ms(1000)
        .build();
    let (mut ca, mut cb) = (PrometheusConverter::new(), PrometheusConverter::new());
    for t in 0..TICKS {
        let ts = BASE + t * S;
        for (c, text) in [(&mut ca, a(t)), (&mut cb, b(t))] {
            let mut snapshot = c.convert(&text, ts - S / 2, ts);
            let Snapshot::V3(v3) = &mut snapshot else {
                panic!("a scrape is a V3 snapshot")
            };
            v3.systemtime = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ts);
            store.ingest_snapshot(snapshot);
        }
    }
    let err = store
        .query_range("rate(x_total[3s])", lo, hi, 1.0)
        .expect_err("live")
        .to_string();
    assert!(
        err.contains("x_nanoseconds_total") && err.contains("x_bytes_total"),
        "{err}"
    );
    // The error offers the form that works live; a recording-label matcher
    // does not, and is not offered.
    let escape = "x_nanoseconds_total{__prometheus_name__=\"x_total\"}";
    assert!(err.contains(escape), "{err}");
    assert!(!err.contains("recording-label"), "{err}");
    every(&store, &format!("rate({escape}[3s])"), 1e9);
    // Over the archive both forms are offered.
    let err = reader
        .query_range("rate(x_total[3s])", lo, hi, 1.0)
        .expect_err("recorded")
        .to_string();
    assert!(
        err.contains(escape) && err.contains("recording-label"),
        "{err}"
    );
    every(&reader, &format!("rate({escape}[3s])"), 1e9);
}

/// `{__name__="x"}` lists the same columns as `x` for an exporter name.
#[test]
fn columns_resolve_a_name_matcher_as_a_name() {
    let (_dir, reader) = recorded(&scrape);
    let bare = reader.columns("lag_ms").unwrap();
    assert!(!bare.is_empty());
    assert_eq!(reader.columns("{__name__=\"lag_ms\"}").unwrap(), bare);
}

/// The recording-label matcher an ambiguous-name error offers, if any.
fn offered_matcher(err: &str) -> Option<&str> {
    let from = err.find("recording-label matcher (")? + "recording-label matcher (".len();
    err[from..].strip_suffix(')')
}

/// The error for one name stored under two over an archive offers a
/// recording-label matcher on a key the recordings carry, whose value picks
/// the first stored name the error gives, and which resolves the query:
/// `alias`, the key rezolus records a user's `source=` as, and `source`.
/// When no key tells the recordings apart (one both carry with one value,
/// one only the first carries, or only the internal `__recording__` the
/// reader adds to recordings of equal labels), it offers none.
#[test]
fn the_ambiguous_name_error_offers_a_recording_label_the_recordings_carry() {
    let a = |t: u64| format!("# TYPE x counter\n# UNIT x seconds\nx_total{{job=\"a\"}} {t}\n");
    let b = |t: u64| format!("# TYPE x counter\n# UNIT x kilobytes\nx_total{{job=\"b\"}} {t}\n");
    let (lo, hi) = ((BASE / S) as f64, (BASE / S + TICKS - 1) as f64);
    let escape = "x_nanoseconds_total{__prometheus_name__=\"x_total\"}";
    type Case<'a> = (
        &'a str,
        Vec<(&'a str, &'a str)>,
        Vec<(&'a str, &'a str)>,
        Option<&'a str>,
    );
    let cases: [Case; 4] = [
        (
            "alias",
            vec![("alias", "first")],
            vec![("alias", "other")],
            Some("x_total{alias=\"first\"}"),
        ),
        (
            "source",
            vec![("source", "a"), ("host", "h")],
            vec![("source", "b"), ("host", "h")],
            Some("x_total{source=\"a\"}"),
        ),
        (
            "none",
            vec![("source", "s"), ("zone", "z")],
            vec![("source", "s")],
            None,
        ),
        (
            "only __recording__",
            vec![("source", "s")],
            vec![("source", "s")],
            None,
        ),
    ];
    for (case, la, lb, want) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.dendro");
        record_labelled(&path, &[(la, &a, false), (lb, &b, false)]);
        let reader = open(&path);
        let err = reader
            .query_range("rate(x_total[3s])", lo, hi, 1.0)
            .expect_err(case)
            .to_string();
        assert!(err.contains(escape), "{case}: {err}");
        assert!(
            err.find("x_nanoseconds_total") < err.find("x_bytes_total"),
            "{case}: the first stored name is the nanoseconds one: {err}"
        );
        assert_eq!(offered_matcher(&err), want, "{case}: {err}");
        if let Some(matcher) = want {
            every(&reader, &format!("rate({matcher}[3s])"), 1e9);
        }
        every(&reader, &format!("rate({escape}[3s])"), 1e9);
    }
}
