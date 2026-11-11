//! Prometheus scrapes as float counters and gauges: exposition text through
//! `PrometheusConverter::convert` and `SourceRecorder::stage` (the calls the
//! recorder's scrape makes) into an archive, read back by `ArchiveReader`
//! and queried through metriken-query.
//!
//! What is checked, from `docs/journal/2026-11-06-float-scalars.md`: every
//! scalar an endpoint serves is stored as an `f64` and read back with its
//! fraction; typed counters and untyped `_total`, `_sum` and `_count`
//! samples are counters, gauges and other untyped samples are gauges, a
//! summary's quantiles are gauges and a histogram's `_sum` and `_count` are
//! counters; a counter's values are stored as served, decreases included
//! (the query reads a decrease as a reset); NaN and infinite samples are
//! dropped; an endpoint whose samples carry an `id` is written long
//! (decision 2).
//!
//! The fixtures' names carry no unit token (`cpu_time_total`, not
//! `cpu_seconds_total`), so no series has a `unit` key; units are
//! `tests/prometheus_units.rs`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use dendro::seal::SealPolicy;
use metriken_query::{MetricsSource, QueryOptions, QueryResult};
use metriken_recorder::PrometheusConverter;
use metriken_storage::format::FORMAT_KEY;
use metriken_storage::{
    ArchiveReader, ArchiveWriter, BufferPool, Catalog, CounterValues, DataSource, DendroCatalog,
    GaugeValues, Labels, WriterConfig,
};

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;
const TICKS: u64 = 12;

// ─── What an exporter serves at tick `t` ────────────────────────────────────

/// A counter in seconds, 0.1 a second: truncated, it would move in whole
/// seconds.
fn cpu_seconds(t: u64) -> f64 {
    0.1 * t as f64 + 0.125
}

/// An untyped `_total`, 1.5 a second.
fn requests(t: u64) -> f64 {
    1.5 * t as f64
}

/// A histogram's `_sum`, 0.05 a second.
fn lat_sum(t: u64) -> f64 {
    0.05 * t as f64 + 0.01
}

/// A counter served as is, with a decrease at tick 6.
fn resets(t: u64) -> f64 {
    if t < 6 {
        2.5 + t as f64
    } else {
        0.5 + (t - 6) as f64
    }
}

/// A gauge NaN at tick 4 and `+Inf` at tick 7.
fn flaky(t: u64) -> String {
    match t {
        4 => "NaN".into(),
        7 => "+Inf".into(),
        _ => format!("{}", 0.5 + 0.25 * t as f64),
    }
}

/// A counter NaN at tick 5 and `-Inf` at tick 8.
fn errs(t: u64) -> String {
    match t {
        5 => "NaN".into(),
        8 => "-Inf".into(),
        _ => format!("{}", 0.75 * t as f64),
    }
}

/// An endpoint whose samples carry no `id`.
fn node_text(t: u64) -> String {
    format!(
        "\
# TYPE cpu_limit gauge
cpu_limit 0.25
# TYPE cpu_time_total counter
cpu_time_total {cpu}
requests_total {req}
temperature_celsius 21.5
# TYPE resets_total counter
resets_total {resets}
# TYPE rpc_time summary
rpc_time{{quantile=\"0.5\"}} 0.0125
rpc_time{{quantile=\"0.99\"}} 0.25
rpc_time_sum {rpc_sum}
rpc_time_count {rpc_count}
# TYPE lat_time histogram
lat_time_bucket{{le=\"0.1\"}} {low}
lat_time_bucket{{le=\"1\"}} {count}
lat_time_bucket{{le=\"+Inf\"}} {count}
lat_time_sum {lat_sum}
lat_time_count {count}
# TYPE flaky gauge
flaky {flaky}
# TYPE errs_total counter
errs_total {errs}
# TYPE always_nan gauge
always_nan NaN
always_inf_total +Inf
",
        cpu = cpu_seconds(t),
        req = requests(t),
        resets = resets(t),
        rpc_sum = 0.5 * t as f64 + 0.25,
        rpc_count = 3 * t,
        low = t,
        count = 2 * t,
        lat_sum = lat_sum(t),
        flaky = flaky(t),
        errs = errs(t),
    )
}

const IDS: [&str; 2] = ["/a", "/b"];

fn container_cpu(i: usize, t: u64) -> f64 {
    (i as f64 + 1.0) * 1.5 + 0.3 * t as f64
}

fn container_quota(i: usize) -> f64 {
    0.25 * (i as f64 + 1.0)
}

/// A per-container exporter: every sample but one carries an `id`.
fn containers_text(t: u64) -> String {
    let mut text = String::from("# TYPE container_cpu_usage_time_total counter\n");
    for (i, id) in IDS.iter().enumerate() {
        text.push_str(&format!(
            "container_cpu_usage_time_total{{id=\"{id}\"}} {}\n",
            container_cpu(i, t)
        ));
    }
    text.push_str("# TYPE container_spec_cpu_quota gauge\n");
    for (i, id) in IDS.iter().enumerate() {
        text.push_str(&format!(
            "container_spec_cpu_quota{{id=\"{id}\"}} {}\n",
            container_quota(i)
        ));
    }
    text.push_str("# TYPE machine_cpu_cores gauge\nmachine_cpu_cores 2.5\n");
    text
}

// ─── Writing ────────────────────────────────────────────────────────────────

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Scrape `text` once a second for `TICKS` ticks through one converter
/// into one recording, sealing every 5 rows.
fn record(path: &Path, text: impl Fn(u64) -> String) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    let mut source = writer
        .add_source(labels(&[("source", "exporter")]), BTreeMap::new(), BASE)
        .unwrap();
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
    writer.join().unwrap();
}

// ─── Reading ────────────────────────────────────────────────────────────────

fn open(path: &Path) -> ArchiveReader {
    ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(path).unwrap()),
        None,
        BufferPool::new(64 << 20),
        None,
    )
    .unwrap()
    .remove(0)
    .1
}

fn table_source(reader: &ArchiveReader, name: &str) -> Arc<dyn DataSource> {
    let owners = reader.owners_of(&[name.to_string()]);
    assert_eq!(owners.len(), 1, "one table holds {name}");
    owners[0].reader().expect("the table opens").source()
}

fn filter(pairs: &[(&str, &str)]) -> Labels {
    Labels::from(labels(pairs))
}

/// The one stored series of counter `name`: its ticks and values, which
/// must be floats.
fn float_counter(reader: &ArchiveReader, name: &str, by: &[(&str, &str)]) -> (Vec<u64>, Vec<f64>) {
    let source = table_source(reader, name);
    let mut found = source
        .counters(name, &filter(by), 0, u64::MAX)
        .unwrap_or_else(|| panic!("counter {name}"))
        .series;
    assert_eq!(found.len(), 1, "{name} {by:?}");
    let c = found.remove(0);
    let CounterValues::Float(v) = c.values else {
        panic!("{name}: an integer series: {:?}", c.values)
    };
    (ticks(&c.timestamps), v)
}

/// The one stored series of gauge `name`, as [`float_counter`].
fn float_gauge(reader: &ArchiveReader, name: &str, by: &[(&str, &str)]) -> (Vec<u64>, Vec<f64>) {
    let source = table_source(reader, name);
    let mut found = source
        .gauges(name, &filter(by), 0, u64::MAX)
        .unwrap_or_else(|| panic!("gauge {name}"))
        .series;
    assert_eq!(found.len(), 1, "{name} {by:?}");
    let g = found.remove(0);
    let GaugeValues::Float(v) = g.values else {
        panic!("{name}: an integer series: {:?}", g.values)
    };
    (ticks(&g.timestamps), v)
}

fn ticks(timestamps: &[u64]) -> Vec<u64> {
    timestamps.iter().map(|ts| (ts - BASE) / S).collect()
}

fn all_ticks() -> Vec<u64> {
    (0..TICKS).collect()
}

/// A result series: its labels (no `__name__`) and points as
/// `(tick, value)`.
type Series = (BTreeMap<String, String>, Vec<(u64, f64)>);

/// `query`'s series over the recorded ticks at a 1 s step, by labels, on
/// the batched rate path or per series.
fn query(reader: &ArchiveReader, query: &str, per_series: bool) -> Vec<Series> {
    let opts = QueryOptions::default().with_per_series_rates(per_series);
    let (lo, hi) = (BASE / S, BASE / S + TICKS - 1);
    let result = reader
        .query_range_opts(query, lo as f64, hi as f64, 1.0, &opts)
        .unwrap_or_else(|e| panic!("{query}: {e}"));
    let QueryResult::Matrix { result, .. } = result else {
        panic!("{query}: not a matrix")
    };
    let mut out: Vec<Series> = result
        .into_iter()
        .map(|m| {
            let labels = m
                .metric
                .into_iter()
                .filter(|(k, _)| k != "__name__")
                .collect();
            let points = m
                .values
                .into_iter()
                .map(|(t, v)| ((t.round() as u64) - BASE / S, v))
                .collect();
            (labels, points)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn one(reader: &ArchiveReader, q: &str, per_series: bool) -> Vec<(u64, f64)> {
    let mut got = query(reader, q, per_series);
    assert_eq!(got.len(), 1, "{q}: {got:?}");
    got.remove(0).1
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9
}

/// Every point of `points` is `want`, and there are some.
fn all_close(q: &str, points: &[(u64, f64)], want: f64) {
    assert!(!points.is_empty(), "{q}: no points");
    assert!(
        points.iter().all(|(_, v)| close(*v, want)),
        "{q}: want {want}: {points:?}"
    );
}

/// Each segment of the recording's one table: its `metriken.format` and
/// whether it is long.
fn segments(path: &Path) -> Vec<(Option<String>, bool)> {
    use parquet::file::reader::FileReader;
    let catalog = DendroCatalog::open(path).unwrap();
    let source = catalog.sources().unwrap().remove(0);
    catalog
        .segment_meta(source.id, metriken_recorder::PROMETHEUS_GROUP)
        .unwrap()
        .into_iter()
        .map(|(seq, _)| {
            let bytes = catalog
                .segment_bytes(source.id, metriken_recorder::PROMETHEUS_GROUP, seq)
                .unwrap()
                .unwrap();
            let reader =
                parquet::file::reader::SerializedFileReader::new(bytes::Bytes::from(bytes))
                    .unwrap();
            let kv = reader
                .metadata()
                .file_metadata()
                .key_value_metadata()
                .cloned()
                .unwrap_or_default();
            let get = |key: &str| {
                kv.iter()
                    .find(|e| e.key == key)
                    .and_then(|e| e.value.clone())
            };
            (
                get(FORMAT_KEY),
                get(metriken_storage::long::LAYOUT_KEY).as_deref()
                    == Some(metriken_storage::long::LAYOUT_LONG),
            )
        })
        .collect()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

/// Every scalar is a float counter or float gauge of the kind the
/// exposition gives it, with its fraction.
#[test]
fn every_scalar_is_stored_as_a_float_of_its_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.dendro");
    record(&path, node_text);
    let reader = open(&path);

    for name in [
        "cpu_time_total",
        "requests_total",
        "resets_total",
        "rpc_time_sum",
        "rpc_time_count",
        "lat_time_sum",
        "lat_time_count",
        "errs_total",
    ] {
        assert!(reader.has_counter(name), "{name} is a counter");
        assert!(!reader.has_gauge(name), "{name} is not a gauge");
    }
    for name in ["cpu_limit", "temperature_celsius", "rpc_time", "flaky"] {
        assert!(reader.has_gauge(name), "{name} is a gauge");
        assert!(!reader.has_counter(name), "{name} is not a counter");
    }

    assert_eq!(
        float_gauge(&reader, "cpu_limit", &[]),
        (all_ticks(), vec![0.25; TICKS as usize])
    );
    assert_eq!(
        float_gauge(&reader, "temperature_celsius", &[]),
        (all_ticks(), vec![21.5; TICKS as usize])
    );
    assert_eq!(
        float_counter(&reader, "cpu_time_total", &[]),
        (
            all_ticks(),
            all_ticks().into_iter().map(cpu_seconds).collect()
        )
    );
    assert_eq!(
        float_counter(&reader, "lat_time_sum", &[]),
        (all_ticks(), all_ticks().into_iter().map(lat_sum).collect())
    );
    // Whole numbers are floats too: one endpoint, one kind per scalar.
    assert_eq!(
        float_counter(&reader, "lat_time_count", &[]),
        (
            all_ticks(),
            all_ticks().iter().map(|t| 2.0 * *t as f64).collect()
        )
    );
    for (q, want) in [("0.5", 0.0125), ("0.99", 0.25)] {
        assert_eq!(
            float_gauge(&reader, "rpc_time", &[("quantile", q)]),
            (all_ticks(), vec![want; TICKS as usize]),
            "quantile {q}"
        );
    }
    // The buckets are still a histogram.
    assert!(reader.histogram_names().iter().any(|n| n == "lat_time"));

    // Stored as served: the decrease at tick 6 is not accumulated over.
    assert_eq!(
        float_counter(&reader, "resets_total", &[]),
        (all_ticks(), all_ticks().into_iter().map(resets).collect())
    );

    // Every segment of the table is format 3, and wide.
    let found = segments(&path);
    assert!(!found.is_empty(), "sealed segments");
    assert!(
        found
            .iter()
            .all(|(f, long)| f.as_deref() == Some("3") && !*long),
        "{found:?}"
    );
}

/// A NaN or infinite sample is dropped: its tick has no value, and a
/// series that is never finite is not recorded.
#[test]
fn non_finite_samples_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.dendro");
    record(&path, node_text);
    let reader = open(&path);

    let (ticks, values) = float_gauge(&reader, "flaky", &[]);
    let want: Vec<u64> = all_ticks()
        .into_iter()
        .filter(|t| ![4, 7].contains(t))
        .collect();
    assert_eq!(ticks, want);
    assert!(values.iter().all(|v| v.is_finite()), "{values:?}");

    let (ticks, values) = float_counter(&reader, "errs_total", &[]);
    let want: Vec<u64> = all_ticks()
        .into_iter()
        .filter(|t| ![5, 8].contains(t))
        .collect();
    assert_eq!(ticks, want);
    assert!(values.iter().all(|v| v.is_finite()), "{values:?}");

    assert!(!reader.has_gauge("always_nan"));
    assert!(!reader.has_counter("always_inf_total"));
}

/// The query engine reads the floats: a gauge's fraction, a fractional
/// counter's rate, an untyped `_total`'s rate, a histogram `_sum`'s rate,
/// on the batched rate path and per series.
#[test]
fn scrapes_query_with_their_fractions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("node.dendro");
    record(&path, node_text);
    let reader = open(&path);

    for q in ["cpu_limit", "rpc_time{quantile=\"0.99\"}"] {
        all_close(q, &one(&reader, q, false), 0.25);
    }
    all_close(
        "temperature_celsius",
        &one(&reader, "temperature_celsius", false),
        21.5,
    );
    for per_series in [false, true] {
        for (q, want) in [
            ("rate(cpu_time_total[3s])", 0.1),
            ("irate(cpu_time_total[2s])", 0.1),
            ("rate(requests_total[3s])", 1.5),
            ("rate(lat_time_sum[3s])", 0.05),
            ("rate(lat_time_count[3s])", 2.0),
        ] {
            all_close(q, &one(&reader, q, per_series), want);
        }
        // The decrease at tick 6 is a reset: the increase across it is the
        // value after it, 0.5, never negative.
        let resets = one(&reader, "irate(resets_total[2s])", per_series);
        let at = |tick| resets.iter().find(|(t, _)| *t == tick).map(|(_, v)| *v);
        assert_eq!(at(5), Some(1.0), "{resets:?}");
        assert_eq!(at(6), Some(0.5), "{resets:?}");
        assert_eq!(at(7), Some(1.0), "{resets:?}");
    }
}

/// An endpoint whose samples carry an `id` is written long, with float
/// columns, and reads back per occupant (decision 2).
#[test]
fn an_id_labelled_endpoint_stays_long_with_float_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("containers.dendro");
    record(&path, containers_text);

    let found = segments(&path);
    assert!(!found.is_empty(), "sealed segments");
    assert!(
        found
            .iter()
            .all(|(f, long)| f.as_deref() == Some("3") && *long),
        "{found:?}"
    );

    let reader = open(&path);
    for (i, id) in IDS.iter().enumerate() {
        assert_eq!(
            float_counter(&reader, "container_cpu_usage_time_total", &[("id", id)]),
            (
                all_ticks(),
                all_ticks()
                    .into_iter()
                    .map(|t| container_cpu(i, t))
                    .collect()
            ),
            "{id}"
        );
        assert_eq!(
            float_gauge(&reader, "container_spec_cpu_quota", &[("id", id)]),
            (all_ticks(), vec![container_quota(i); TICKS as usize]),
            "{id}"
        );
    }
    all_close(
        "machine_cpu_cores",
        &one(&reader, "machine_cpu_cores", false),
        2.5,
    );
    for per_series in [false, true] {
        let rates = query(
            &reader,
            "rate(container_cpu_usage_time_total[3s])",
            per_series,
        );
        assert_eq!(rates.len(), IDS.len(), "{rates:?}");
        for (labels, points) in &rates {
            all_close(&format!("{labels:?}"), points, 0.3);
        }
    }
}
