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
//! does not divide by.

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
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    let labels = BTreeMap::from([("source".to_string(), "exporter".to_string())]);
    let mut source = writer.add_source(labels, BTreeMap::new(), BASE).unwrap();
    let mut converter = PrometheusConverter::new();
    for t in 0..TICKS {
        let ts = BASE + t * S;
        let snapshot = converter.convert(&scrape(t), ts - S / 2, ts);
        let staged = source.stage(&snapshot, ts, 0).unwrap();
        writer.commit(vec![staged]).unwrap();
        source.maybe_seal().unwrap();
        source.sync().unwrap();
    }
    source.finalize((BASE + (TICKS - 1) * S, 0)).unwrap();
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
    let store = metriken_query::MemoryStore::builder()
        .sampling_interval_ms(1000)
        .build();
    let mut converter = PrometheusConverter::new();
    for t in 0..TICKS {
        let ts = BASE + t * S;
        let mut snapshot = converter.convert(&scrape(t), ts - S / 2, ts);
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
/// stored ×1000. Its quantiles read ×1000 too, since metriken-query does not
/// divide by `scale`; divided, they are in seconds. Its `_sum` is not
/// scaled.
#[test]
fn a_histogram_below_one_records_its_scale() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.dendro");
    let reader = recorded(&path);
    let columns = sealed_columns(&path);
    let h = &columns[&("fast_seconds".to_string(), None)];
    assert_eq!(h["unit"], "seconds");
    assert_eq!(h[SCALE_KEY], "1000");
    assert_eq!(h[OVERFLOW_BOUND_KEY], "10");
    let scale: f64 = h[SCALE_KEY].parse().unwrap();
    let q = matrix(&reader, "histogram_quantile(0.9, fast_seconds)");
    assert_eq!(q.len(), 1, "{q:?}");
    for v in &q[0].1 {
        assert!((v / scale - 0.01).abs() < 1e-12, "{q:?}");
    }
    every(
        &reader,
        "rate(fast_seconds_sum[3s])",
        Some("fast_seconds_sum"),
        0.02,
    );
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
