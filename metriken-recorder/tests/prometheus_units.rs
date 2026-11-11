//! Units in Prometheus metric names: exposition text through
//! `PrometheusConverter::convert` and `SourceRecorder::stage` into an
//! archive, read back by `ArchiveReader` and queried through metriken-query,
//! and the same scrapes ingested into a live `MemoryStore`.
//!
//! Every series is stored under the exporter's name with the exporter's
//! values, and the `unit` key of its sealed column is the unit its name's
//! token or `# UNIT` line names (`metriken_storage::prometheus_units`). A
//! `_count` has no unit. A histogram's bounds are multiplied by a power of
//! 10 when the smallest is below 1, recorded as `scale`, which metriken-query
//! divides every value it takes from a bucket by.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use dendro::seal::SealPolicy;
use metriken_model::Snapshot;
use metriken_query::{MetricsSource, QueryResult};
use metriken_recorder::{PrometheusConverter, PROMETHEUS_GROUP};
use metriken_storage::{
    ArchiveReader, ArchiveWriter, BufferPool, Catalog, DendroCatalog, WriterConfig,
    OVERFLOW_BOUND_KEY, OVERFLOW_KEY, SCALE_KEY,
};

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;
const TICKS: u64 = 12;

// ─── What an exporter serves at tick `t` ────────────────────────────────────

/// One scrape. Per tick: `rpc_seconds` takes 5 observations at or below
/// 1 s, 4 in (1 s, 10 s] and 1 above, 20 s in all; `fast_seconds` the same
/// counts at 1 ms and 10 ms, 0.02 s in all; `busy_seconds_total` 1.5 s;
/// `q_ms_sum` 30 ms over 10 observations.
fn scrape(t: u64) -> String {
    format!(
        "\
# TYPE rpc_seconds histogram
rpc_seconds_bucket{{le=\"1\"}} {a}
rpc_seconds_bucket{{le=\"10\"}} {b}
rpc_seconds_bucket{{le=\"+Inf\"}} {c}
rpc_seconds_sum {rpc_sum}
rpc_seconds_count {c}
# TYPE fast_seconds histogram
fast_seconds_bucket{{le=\"0.001\"}} {a}
fast_seconds_bucket{{le=\"0.01\"}} {b}
fast_seconds_bucket{{le=\"+Inf\"}} {c}
fast_seconds_sum {fast_sum}
fast_seconds_count {c}
# TYPE busy_seconds_total counter
busy_seconds_total {busy}
# TYPE lag_seconds gauge
lag_seconds 0.25
# TYPE wait_ms gauge
wait_ms 12.5
# TYPE mem_bytes gauge
mem_bytes 4096
# TYPE cpu_percent gauge
cpu_percent 50
# TYPE queue gauge
# UNIT queue seconds
queue 0.75
# TYPE q_ms summary
q_ms{{quantile=\"0.5\"}} 2.5
q_ms_sum {q_sum}
q_ms_count {c}
# TYPE plain_total counter
plain_total {c}
",
        a = 5 * t,
        b = 9 * t,
        c = 10 * t,
        rpc_sum = 20 * t,
        fast_sum = 0.02 * t as f64,
        busy = 1.5 * t as f64,
        q_sum = 30 * t,
    )
}

// ─── Writing ────────────────────────────────────────────────────────────────

/// `scrape` once a second through one converter into one archive, sealing
/// every 5 rows, and opened.
fn recorded(path: &Path) -> ArchiveReader {
    record(path, &[("exporter", &scrape)])
}

/// Each `(source, text)` scraped once a second through its own converter
/// into one archive as a recording labelled `source`, sealing every 5 rows,
/// and the archive opened as one reader.
fn record(path: &Path, recordings: &[(&str, &dyn Fn(u64) -> String)]) -> ArchiveReader {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    for (name, text) in recordings {
        let labels = BTreeMap::from([("source".to_string(), name.to_string())]);
        let mut source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
        let mut converter = PrometheusConverter::new();
        for t in 0..TICKS {
            let ts = BASE + t * S;
            let snapshot = converter.convert(&text(t), ts - S / 2, ts);
            let staged = source.stage(&snapshot, ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
            source.sync().unwrap();
        }
        source.finalize((BASE + (TICKS - 1) * S, 0)).unwrap();
    }
    writer.join().unwrap();

    let recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(path).unwrap()),
        None,
        BufferPool::new(64 << 20),
        None,
    )
    .unwrap();
    ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None)
}

/// `scrape` once a second into a live `MemoryStore`.
fn live() -> metriken_query::MemoryStore {
    live_of(&scrape)
}

/// `text` once a second into a live `MemoryStore`.
fn live_of(text: &dyn Fn(u64) -> String) -> metriken_query::MemoryStore {
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

/// The column metadata of the archive's first sealed segment, keyed by
/// metric name and `quantile` label.
type Columns = HashMap<(String, Option<String>), HashMap<String, String>>;

fn sealed_columns(path: &Path) -> Columns {
    use parquet::file::reader::FileReader;
    let catalog = DendroCatalog::open(path).unwrap();
    let source = catalog.sources().unwrap().remove(0);
    let (seq, _) = catalog.segment_meta(source.id, PROMETHEUS_GROUP).unwrap()[0];
    let bytes = catalog
        .segment_bytes(source.id, PROMETHEUS_GROUP, seq)
        .unwrap()
        .unwrap();
    let reader =
        parquet::file::reader::SerializedFileReader::new(bytes::Bytes::from(bytes)).unwrap();
    let schema = parquet::arrow::parquet_to_arrow_schema(
        reader.metadata().file_metadata().schema_descr(),
        reader.metadata().file_metadata().key_value_metadata(),
    )
    .unwrap();
    let mut out = Columns::new();
    for field in schema.fields() {
        let meta = field.metadata();
        let Some(metric) = meta.get("metric") else {
            continue;
        };
        let key = (metric.clone(), meta.get("quantile").cloned());
        let meta: HashMap<String, String> =
            meta.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        if let Some(previous) = out.insert(key.clone(), meta.clone()) {
            assert_eq!(previous, meta, "{key:?}: two columns disagree");
        }
    }
    out
}

// ─── Reading ────────────────────────────────────────────────────────────────

/// `query`'s result series over the recorded ticks at a 1 s step, each as
/// its labels and values; empty when the query names no metric the source
/// holds.
fn matrix(source: &dyn MetricsSource, query: &str) -> Vec<(HashMap<String, String>, Vec<f64>)> {
    let (lo, hi) = (BASE / S + 2, BASE / S + TICKS - 1);
    let result = match source.query_range(query, lo as f64, hi as f64, 1.0) {
        Ok(result) => result,
        Err(e) if is_absent(&e.to_string()) => return Vec::new(),
        Err(e) => panic!("{query}: {e}"),
    };
    let QueryResult::Matrix { result, .. } = result else {
        panic!("{query}: not a matrix")
    };
    result
        .into_iter()
        .map(|m| (m.metric, m.values.into_iter().map(|(_, v)| v).collect()))
        .collect()
}

/// Whether a query error says the query names no metric the source holds.
fn is_absent(err: &str) -> bool {
    err.contains("not found") || err.contains("no metric present")
}

/// Every value of `query`'s one series is within 1e-9 (relative) of
/// `want`, and the series is named `name` (`None` for a result without a
/// name).
fn every(source: &dyn MetricsSource, query: &str, name: Option<&str>, want: f64) {
    let m = matrix(source, query);
    assert_eq!(m.len(), 1, "{query}: {m:?}");
    let (labels, values) = &m[0];
    assert!(!values.is_empty(), "{query}: no points");
    for v in values {
        assert!(
            (v - want).abs() <= want.abs() * 1e-9,
            "{query}: want {want}, got {values:?}"
        );
    }
    assert_eq!(
        labels.get("__name__").map(String::as_str),
        name,
        "{query}: {labels:?}"
    );
    assert!(
        labels.keys().all(|k| !k.starts_with("__prometheus")),
        "{query}: {labels:?}"
    );
}

/// The bucket band of each point of `query`'s one series.
fn bands(source: &dyn MetricsSource, query: &str) -> Vec<(f64, f64)> {
    let (lo, hi) = (BASE / S + 2, BASE / S + TICKS - 1);
    let QueryResult::Matrix { mut result, .. } = source
        .query_range(query, lo as f64, hi as f64, 1.0)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
    else {
        panic!("{query}: not a matrix")
    };
    assert_eq!(result.len(), 1, "{query}");
    result
        .remove(0)
        .intervals
        .unwrap_or_else(|| panic!("{query}: no bands"))
}

/// The bucket bounds of `query`'s heatmap.
fn heatmap_bounds(source: &dyn MetricsSource, query: &str) -> Vec<f64> {
    let (lo, hi) = (BASE / S + 2, BASE / S + TICKS - 1);
    match source
        .query_range(query, lo as f64, hi as f64, 1.0)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
    {
        QueryResult::HistogramHeatmap { result, .. } => result.bucket_bounds,
        other => panic!("{query}: not a heatmap: {other:?}"),
    }
}

/// The archive and the live store over the same scrapes.
fn sources(dir: &Path) -> (ArchiveReader, metriken_query::MemoryStore) {
    (recorded(&dir.join("a.dendro")), live())
}

// ─── Tests ──────────────────────────────────────────────────────────────────

/// Each scalar reads back under the exporter's name with the exporter's
/// value, recorded and live: seconds in seconds, milliseconds in
/// milliseconds, a percentage in 0 to 100, a `# UNIT` line's series as
/// served.
#[test]
fn scalars_read_back_as_served() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, store) = sources(dir.path());
    for source in [&reader as &dyn MetricsSource, &store] {
        let rate = |name: &str| format!("rate({name}[3s])");
        every(
            source,
            &rate("busy_seconds_total"),
            Some("busy_seconds_total"),
            1.5,
        );
        every(
            source,
            &rate("rpc_seconds_sum"),
            Some("rpc_seconds_sum"),
            20.0,
        );
        every(
            source,
            &rate("rpc_seconds_count"),
            Some("rpc_seconds_count"),
            10.0,
        );
        every(
            source,
            &rate("fast_seconds_sum"),
            Some("fast_seconds_sum"),
            0.02,
        );
        every(source, &rate("q_ms_sum"), Some("q_ms_sum"), 30.0);
        every(source, "lag_seconds", Some("lag_seconds"), 0.25);
        every(source, "wait_ms", Some("wait_ms"), 12.5);
        every(source, "mem_bytes", Some("mem_bytes"), 4096.0);
        every(source, "cpu_percent", Some("cpu_percent"), 50.0);
        every(source, "queue", Some("queue"), 0.75);
        every(source, "q_ms{quantile=\"0.5\"}", Some("q_ms"), 2.5);
        // The mean from `_sum` over `_count`, in seconds.
        let mean = "rate(rpc_seconds_sum[3s]) / rate(rpc_seconds_count[3s])";
        every(source, mean, None, 2.0);
        // No series is stored under a converted name.
        for name in [
            "busy_nanoseconds_total",
            "lag_nanoseconds",
            "wait_nanoseconds",
            "rpc_nanoseconds_sum",
            "queue_nanoseconds",
        ] {
            assert!(matrix(source, name).is_empty(), "{name}");
        }
    }
}

/// Each sealed column's `unit` is the exporter's unit by its canonical
/// name; a `_count`, a histogram's overflow series and a name without a
/// token or `# UNIT` line have none; no column carries a
/// `__prometheus_name__`.
#[test]
fn the_unit_key_is_the_exporters_unit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    recorded(&path);
    let columns = sealed_columns(&path);
    let unit = |name: &str, quantile: Option<&str>| {
        let key = (name.to_string(), quantile.map(str::to_string));
        let meta = columns
            .get(&key)
            .unwrap_or_else(|| panic!("{key:?}: {:?}", columns.keys()));
        meta.get("unit").cloned()
    };
    for (name, quantile, want) in [
        ("rpc_seconds", None, Some("seconds")),
        ("rpc_seconds_sum", None, Some("seconds")),
        ("rpc_seconds_count", None, None),
        ("rpc_seconds_overflow", None, None),
        ("fast_seconds", None, Some("seconds")),
        ("fast_seconds_sum", None, Some("seconds")),
        ("fast_seconds_count", None, None),
        ("fast_seconds_overflow", None, None),
        ("busy_seconds_total", None, Some("seconds")),
        ("lag_seconds", None, Some("seconds")),
        ("wait_ms", None, Some("milliseconds")),
        ("mem_bytes", None, Some("bytes")),
        ("cpu_percent", None, Some("percent")),
        ("queue", None, Some("seconds")),
        ("q_ms", Some("0.5"), Some("milliseconds")),
        ("q_ms_sum", None, Some("milliseconds")),
        ("q_ms_count", None, None),
        ("plain_total", None, None),
    ] {
        assert_eq!(unit(name, quantile).as_deref(), want, "{name}");
    }
    assert_eq!(columns.len(), 18, "{:?}", columns.keys());
    for (key, meta) in &columns {
        assert!(
            meta.keys().all(|k| !k.starts_with("__")),
            "{key:?}: {meta:?}"
        );
    }
}

/// A `_seconds` histogram whose bounds are at least 1 is stored with its
/// bounds as served and no `scale`, so its quantiles read in seconds.
#[test]
fn a_seconds_histogram_reads_in_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    let reader = recorded(&path);
    let columns = sealed_columns(&path);
    let h = &columns[&("rpc_seconds".to_string(), None)];
    assert!(!h.contains_key(SCALE_KEY), "{h:?}");
    assert_eq!(h[OVERFLOW_BOUND_KEY], "10");
    let store = live();
    for source in [&reader as &dyn MetricsSource, &store] {
        every(
            source,
            "histogram_quantile(0.5, rpc_seconds)",
            Some("rpc_seconds"),
            1.0,
        );
        every(
            source,
            "histogram_quantile(0.9, rpc_seconds)",
            Some("rpc_seconds"),
            10.0,
        );
    }
}

/// A histogram whose smallest bound is below 1 has its bounds and overflow
/// bound multiplied by the power of 10 that brings it to 1, recorded as
/// `scale`, in the exporter's unit: `fast_seconds` (1 ms and 10 ms) is
/// stored ×1000 with `scale` 1000. metriken-query divides every value it
/// takes from a bucket by it, recorded and live: the quantiles, their
/// bands, the mean, the sum and the heatmap's bounds are in seconds. The
/// count is not scaled, nor is the `_sum` series.
#[test]
fn a_histogram_below_one_reads_in_its_unit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    let reader = recorded(&path);
    let columns = sealed_columns(&path);
    let h = &columns[&("fast_seconds".to_string(), None)];
    assert_eq!(h["unit"], "seconds");
    assert_eq!(h[SCALE_KEY], "1000");
    assert_eq!(h[OVERFLOW_BOUND_KEY], "10");
    let store = live();
    for source in [&reader as &dyn MetricsSource, &store] {
        let p50 = "histogram_quantile(0.5, fast_seconds)";
        every(source, p50, Some("fast_seconds"), 0.001);
        let p90 = "histogram_quantile(0.9, fast_seconds)";
        every(source, p90, Some("fast_seconds"), 0.01);
        let quantiles = "histogram_quantiles([0.5, 0.9], fast_seconds)";
        let m = matrix(source, quantiles);
        assert_eq!(m.len(), 2, "{m:?}");
        for (labels, values) in &m {
            let want = match labels["quantile"].as_str() {
                "0.5" => 0.001,
                _ => 0.01,
            };
            assert!(
                values.iter().all(|v| (v - want).abs() <= want * 1e-9),
                "{quantiles}: {m:?}"
            );
        }
        // The bucket band of each quantile point, in seconds.
        for band in bands(source, p90) {
            assert!(
                band.0 <= 0.01 + 1e-12 && band.1 >= 0.01 - 1e-12 && band.1 < 0.02,
                "{p90}: {band:?}"
            );
        }
        // 5 observations of 1 ms and 4 of 10 ms a tick: a mean of 5 ms and
        // a sum of 45 ms. The overflow is in neither.
        every(
            source,
            "histogram_mean(fast_seconds)",
            Some("fast_seconds"),
            0.005,
        );
        every(
            source,
            "histogram_sum(fast_seconds)",
            Some("fast_seconds"),
            0.045,
        );
        every(
            source,
            "histogram_count(fast_seconds)",
            Some("fast_seconds"),
            9.0,
        );
        let bounds = heatmap_bounds(source, "histogram_heatmap(fast_seconds)");
        assert!(!bounds.is_empty());
        assert!(
            bounds
                .iter()
                .all(|b| (0.001 - 1e-12..=0.01 + 1e-12).contains(b)),
            "{bounds:?}"
        );
        assert!(
            bounds.iter().any(|b| (b - 0.01).abs() < 1e-12),
            "{bounds:?}"
        );
        every(
            source,
            "rate(fast_seconds_sum[3s])",
            Some("fast_seconds_sum"),
            0.02,
        );
    }
}

/// `fast_seconds` with buckets at 1 ms and 10 ms (stored ×1000).
fn fast_ms(t: u64) -> String {
    format!(
        "# TYPE fast_seconds histogram\n\
         fast_seconds_bucket{{le=\"0.001\"}} {a}\n\
         fast_seconds_bucket{{le=\"0.01\"}} {b}\n\
         fast_seconds_bucket{{le=\"+Inf\"}} {b}\n",
        a = 5 * t,
        b = 10 * t,
    )
}

/// `fast_seconds` with buckets at 0.1 s and 1 s (stored ×10).
fn fast_tenths(t: u64) -> String {
    format!(
        "# TYPE fast_seconds histogram\n\
         fast_seconds_bucket{{le=\"0.1\"}} {a}\n\
         fast_seconds_bucket{{le=\"1\"}} {b}\n\
         fast_seconds_bucket{{le=\"+Inf\"}} {b}\n",
        a = 5 * t,
        b = 10 * t,
    )
}

/// Two recordings of one histogram at one scale pool as before and read in
/// seconds. At two scales, a function that pools them is refused, naming
/// the scales; a label matcher selecting one reads it in seconds; the
/// count, which no scale changes, is not refused.
#[test]
fn recordings_at_different_scales_are_refused_when_pooled() {
    let dir = tempfile::tempdir().unwrap();
    let same = record(
        &dir.path().join("same.dendro"),
        &[("a", &fast_ms), ("b", &fast_ms)],
    );
    every(
        &same,
        "histogram_quantile(0.5, fast_seconds)",
        Some("fast_seconds"),
        0.001,
    );

    let mixed = record(
        &dir.path().join("mixed.dendro"),
        &[("a", &fast_ms), ("b", &fast_tenths)],
    );
    for query in [
        "histogram_quantile(0.5, fast_seconds)",
        "histogram_quantiles([0.5], fast_seconds)",
        "histogram_mean(fast_seconds)",
        "histogram_sum(fast_seconds)",
        "histogram_heatmap(fast_seconds)",
    ] {
        let err = mixed
            .query_range(
                query,
                (BASE / S + 2) as f64,
                (BASE / S + TICKS - 1) as f64,
                1.0,
            )
            .err()
            .unwrap_or_else(|| panic!("{query}: not refused"))
            .to_string();
        assert!(err.contains("different scales"), "{query}: {err}");
        assert!(err.contains("1000") && err.contains("10"), "{query}: {err}");
    }
    let one =
        |source: &str| format!("histogram_quantile(0.5, fast_seconds{{source=\"{source}\"}})");
    every(&mixed, &one("a"), Some("fast_seconds"), 0.001);
    every(&mixed, &one("b"), Some("fast_seconds"), 0.1);
    every(
        &mixed,
        "histogram_count(fast_seconds)",
        Some("fast_seconds"),
        20.0,
    );
}

/// A live store fed one exporter whose buckets change scale mid-run:
/// before the change and after it each read in seconds, and a function
/// pooling rows of both is refused.
#[test]
fn a_live_series_whose_scale_changes_is_refused_when_pooled() {
    let text = |t: u64| {
        if t < 6 {
            fast_ms(t)
        } else {
            fast_tenths(t)
        }
    };
    let store = live_of(&text);
    let read = |lo: u64, hi: u64| {
        store.query_range(
            "histogram_quantile(0.5, fast_seconds)",
            (BASE / S + lo) as f64,
            (BASE / S + hi) as f64,
            1.0,
        )
    };
    let values = |r: Result<QueryResult, metriken_query::QueryError>| -> Vec<f64> {
        let QueryResult::Matrix { result, .. } = r.unwrap() else {
            panic!("not a matrix")
        };
        result
            .into_iter()
            .flat_map(|m| m.values)
            .map(|(_, v)| v)
            .collect()
    };
    let before = values(read(2, 4));
    assert!(
        !before.is_empty() && before.iter().all(|v| (v - 0.001).abs() < 1e-12),
        "{before:?}"
    );
    let after = values(read(8, 11));
    assert!(
        !after.is_empty() && after.iter().all(|v| (v - 0.1).abs() < 1e-12),
        "{after:?}"
    );
    let err = read(2, 11)
        .expect_err("pooled across the change")
        .to_string();
    assert!(err.contains("different scales"), "{err}");
}

/// A histogram's overflow series is `<exporter name>_overflow`, which its
/// column names, and a quantile in the overflow is +Inf.
#[test]
fn the_overflow_series_is_named_after_the_exporter_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    let reader = recorded(&path);
    let columns = sealed_columns(&path);
    for name in ["rpc_seconds", "fast_seconds"] {
        let h = &columns[&(name.to_string(), None)];
        assert_eq!(h[OVERFLOW_KEY], format!("{name}_overflow"));
    }
    let store = live();
    for source in [&reader as &dyn MetricsSource, &store] {
        every(
            source,
            "rate(rpc_seconds_overflow[3s])",
            Some("rpc_seconds_overflow"),
            1.0,
        );
        let q = matrix(source, "histogram_quantile(0.99, rpc_seconds)");
        assert_eq!(q.len(), 1, "{q:?}");
        assert!(q[0].1.iter().all(|v| *v == f64::INFINITY), "{:?}", q[0].1);
    }
}

/// A HELP text is kept under the exporter's name only.
#[test]
fn a_description_is_kept_under_the_exporters_name() {
    let mut converter = PrometheusConverter::new();
    converter.convert(
        "# HELP rpc_ms Latency.\n# TYPE rpc_ms gauge\nrpc_ms 1\n",
        0,
        1,
    );
    let descriptions: BTreeMap<String, String> =
        serde_json::from_str(&converter.pending_descriptions().unwrap()).unwrap();
    assert_eq!(
        descriptions,
        BTreeMap::from([("rpc_ms".to_string(), "Latency.".to_string())])
    );
}
