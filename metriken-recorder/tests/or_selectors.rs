//! Selectors with `or` branches (`x{job="a" or job="b"}`), from exposition
//! text through `PrometheusConverter::convert` and `SourceRecorder::stage`
//! into an archive, read back by `ArchiveReader` and queried through
//! metriken-query, and ingested into a live `MemoryStore`.
//!
//! promql-parser puts every matcher of such a selector into its branches
//! (its `or` between label matchers). A series is selected when it
//! satisfies every matcher of at least one branch; each matcher is applied
//! as Prometheus applies it, a label a series does not carry taken as the
//! empty string.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use dendro::seal::SealPolicy;
use metriken_query::{MatrixSample, MetricsSource, QueryOptions, QueryResult};
use metriken_recorder::PrometheusConverter;
use metriken_storage::{ArchiveReader, ArchiveWriter, BufferPool, DendroCatalog, WriterConfig};

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;
const TICKS: u64 = 12;

/// What the exporter serves at tick `t`: a counter of three series (one
/// with `job`), a gauge of the same three, a histogram of two jobs whose
/// observations fall in different buckets, and a counter in bits, which is
/// stored in bytes under another name with `__prometheus_name__`.
fn text(t: u64) -> String {
    format!(
        "\
# TYPE x_total counter
x_total{{unit=\"a\"}} {t}
x_total{{unit=\"b\"}} {t2}
x_total{{unit=\"c\",job=\"j\"}} {t3}
# TYPE g gauge
g{{unit=\"a\"}} 1
g{{unit=\"b\"}} 2
g{{unit=\"c\",job=\"j\"}} 3
# TYPE h histogram
h_bucket{{job=\"j\",le=\"1\"}} {t}
h_bucket{{job=\"j\",le=\"+Inf\"}} {t}
h_sum{{job=\"j\"}} {t}
h_count{{job=\"j\"}} {t}
h_bucket{{job=\"k\",le=\"1\"}} 0
h_bucket{{job=\"k\",le=\"100\"}} {t2}
h_bucket{{job=\"k\",le=\"+Inf\"}} {t2}
h_sum{{job=\"k\"}} {t50}
h_count{{job=\"k\"}} {t2}
# TYPE rx_bits_total counter
rx_bits_total{{job=\"j\"}} {bits}
rx_bits_total{{job=\"k\"}} {bits2}
",
        t2 = 2 * t,
        t3 = 3 * t,
        t50 = 100 * t,
        bits = 800 * t,
        bits2 = 1600 * t,
    )
}

/// The scrapes, once a second, recorded into one archive as source
/// `source="e"`, sealing every 5 rows, and opened.
fn recorded(path: &Path) -> ArchiveReader {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    let labels = BTreeMap::from([("source".to_string(), "e".to_string())]);
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

/// The scrapes ingested into a live store.
fn live() -> metriken_query::MemoryStore {
    let store = metriken_query::MemoryStore::builder()
        .sampling_interval_ms(1000)
        .build();
    let mut converter = PrometheusConverter::new();
    for t in 0..TICKS {
        let ts = BASE + t * S;
        let mut snapshot = converter.convert(&text(t), ts - S / 2, ts);
        let metriken_model::Snapshot::V3(v3) = &mut snapshot else {
            panic!("a scrape is a V3 snapshot")
        };
        v3.systemtime = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ts);
        store.ingest_snapshot(snapshot);
    }
    store
}

fn range(source: &dyn MetricsSource, query: &str, per_series: bool) -> QueryResult {
    let (lo, hi) = ((BASE / S) as f64, (BASE / S + TICKS - 1) as f64);
    let opts = QueryOptions::default().with_per_series_rates(per_series);
    source
        .query_range_opts(query, lo, hi, 1.0, &opts)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
}

fn matrix(source: &dyn MetricsSource, query: &str, per_series: bool) -> Vec<MatrixSample> {
    match range(source, query, per_series) {
        QueryResult::Matrix { result, .. } => result,
        other => panic!("{query}: {other:?}"),
    }
}

/// `query`'s result, written out with each series' labels in order, so two
/// equal results compare equal.
fn written(source: &dyn MetricsSource, query: &str) -> String {
    match range(source, query, false) {
        QueryResult::Matrix { result, .. } => {
            let mut series: Vec<_> = result
                .into_iter()
                .map(|m| {
                    let labels: BTreeMap<String, String> = m.metric.into_iter().collect();
                    (labels, m.values, m.bands)
                })
                .collect();
            series.sort_by(|a, b| a.0.cmp(&b.0));
            format!("{series:?}")
        }
        other => format!("{other:?}"),
    }
}

/// The `exported_unit` of each series of `query` (empty for one without),
/// each series once: a series listed twice is listed twice.
fn units(source: &dyn MetricsSource, query: &str, per_series: bool) -> Vec<String> {
    let mut units: Vec<String> = matrix(source, query, per_series)
        .into_iter()
        .map(|m| {
            assert!(!m.values.is_empty(), "{query}: no points");
            m.metric.get("exported_unit").cloned().unwrap_or_default()
        })
        .collect();
    units.sort();
    units
}

/// Both sources, the archive and a live store, with their names.
fn sources(dir: &Path) -> (ArchiveReader, metriken_query::MemoryStore) {
    (recorded(&dir.join("a.dendro")), live())
}

/// A counter selector with `or`, on both rate paths, recorded and live:
/// no branch matching selects nothing, one branch its series, a series two
/// branches match appears once, and `job=""` selects the series without
/// `job`. Regex and negative matchers decide as they do without `or`.
#[test]
fn a_counter_selector_with_or_selects_the_union_of_its_branches() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, store) = sources(dir.path());
    let cases: &[(&str, &[&str])] = &[
        (r#"job="zzz" or job="yyy""#, &[]),
        (r#"job="zzz" or job="j""#, &["c"]),
        (r#"exported_unit="a" or exported_unit="b""#, &["a", "b"]),
        (r#"job="j" or exported_unit="c""#, &["c"]),
        (r#"job="j" or exported_unit=~"a|c""#, &["a", "c"]),
        (r#"job="" or job="zzz""#, &["a", "b"]),
        (r#"exported_unit=~"a|c" or job="zzz""#, &["a", "c"]),
        (r#"exported_unit!~"a|b" or job="zzz""#, &["c"]),
        (r#"exported_unit!="a", job="" or job="zzz""#, &["b"]),
        (
            r#"job="", exported_unit="a" or job="", exported_unit="b""#,
            &["a", "b"],
        ),
    ];
    for source in [&reader as &dyn MetricsSource, &store] {
        for per_series in [false, true] {
            for (matchers, want) in cases {
                for query in [
                    format!("rate(x_total{{{matchers}}}[3s])"),
                    format!("irate(x_total{{{matchers}}}[3s])"),
                ] {
                    assert_eq!(
                        units(source, &query, per_series),
                        *want,
                        "per_series={per_series} {query}"
                    );
                }
            }
            // An aggregate over the selector sums the selected series once.
            let sum = matrix(
                source,
                r#"sum(rate(x_total{job="j" or exported_unit=~"a|c"}[3s]))"#,
                per_series,
            );
            assert_eq!(sum.len(), 1, "{sum:?}");
            assert!(
                sum[0].values.iter().all(|(_, v)| (v - 4.0).abs() < 1e-9),
                "per_series={per_series}: {sum:?}"
            );
        }
    }
}

/// A gauge selector with `or`, bare and in a range function.
#[test]
fn a_gauge_selector_with_or_selects_the_union_of_its_branches() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, store) = sources(dir.path());
    for source in [&reader as &dyn MetricsSource, &store] {
        for (query, want) in [
            (r#"g{job="zzz" or job="yyy"}"#, &[][..]),
            (r#"g{job="zzz" or exported_unit="b"}"#, &["b"][..]),
            (r#"g{job="j" or exported_unit="c"}"#, &["c"][..]),
            (r#"g{job="" or job="zzz"}"#, &["a", "b"][..]),
            (
                r#"avg_over_time(g{job="" or job="zzz"}[3s])"#,
                &["a", "b"][..],
            ),
            (r#"idelta(g{job="" or job="zzz"}[3s])"#, &["a", "b"][..]),
            (
                r#"deriv(g{exported_unit=~"b|c" or job="zzz"}[3s])"#,
                &["b", "c"][..],
            ),
        ] {
            assert_eq!(units(source, query, false), want, "{query}");
        }
    }
}

/// Each histogram function over a selector with `or` reads the series its
/// branches select: the result is that of the selector naming the one
/// series a branch matches, and differs from the result over every series.
#[test]
fn a_histogram_function_with_or_reads_the_selected_series() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, store) = sources(dir.path());
    for source in [&reader as &dyn MetricsSource, &store] {
        for function in [
            "histogram_quantile(0.5, {})",
            "histogram_quantiles([0.5, 0.9], {})",
            "histogram_heatmap({})",
            "histogram_irate({})",
            "histogram_mean({})",
            "histogram_count({})",
            "histogram_sum({})",
        ] {
            let query = |selector: &str| function.replace("{}", selector);
            let all = written(source, &query("h"));
            let one = written(source, &query(r#"h{job="j"}"#));
            assert_ne!(all, one, "{function}: the k series changes the result");
            for selector in [
                r#"h{job="j" or job="zzz"}"#,
                r#"h{job="zzz" or job=~"j|x"}"#,
            ] {
                let or = written(source, &query(selector));
                assert_eq!(or, one, "{}", query(selector));
            }
            let none = written(source, &query(r#"h{job="zzz" or job="yyy"}"#));
            assert_ne!(none, one, "{function}: no branch matches");
        }
    }
}

/// A selector naming a series by the exporter's name, held by no series,
/// reads the stored series renamed from it, with its branches applied.
#[test]
fn an_exporter_name_with_or_reads_the_stored_series_its_branches_select() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, store) = sources(dir.path());
    for source in [&reader as &dyn MetricsSource, &store] {
        for per_series in [false, true] {
            let query = r#"rate(rx_bits_total{job="zzz" or job="k"}[3s])"#;
            let m = matrix(source, query, per_series);
            let jobs: Vec<&str> = m.iter().map(|m| m.metric["job"].as_str()).collect();
            assert_eq!(jobs, ["k"], "per_series={per_series} {query}: {m:?}");
            // 1600 bits a second, stored as 200 bytes.
            assert!(
                m[0].values.iter().all(|(_, v)| (v - 200.0).abs() < 1e-9),
                "per_series={per_series} {query}: {m:?}"
            );
            let none = r#"rate(rx_bits_total{job="zzz" or job="yyy"}[3s])"#;
            assert!(matrix(source, none, per_series).is_empty(), "{none}");
        }
    }
}

/// `columns` of a selector with `or` lists the columns of the series its
/// branches select, each once.
#[test]
fn columns_of_a_selector_with_or_are_the_selected_series() {
    let dir = tempfile::tempdir().unwrap();
    let (reader, store) = sources(dir.path());
    for source in [&reader as &dyn MetricsSource, &store] {
        let columns = |query: &str| -> BTreeSet<String> {
            source
                .columns(query)
                .unwrap_or_else(|e| panic!("{query}: {e}"))
                .into_iter()
                .collect()
        };
        let a = columns(r#"rate(x_total{exported_unit="a"}[3s])"#);
        let c = columns(r#"rate(x_total{exported_unit="c"}[3s])"#);
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(c.len(), 1, "{c:?}");
        assert!(columns(r#"rate(x_total{job="zzz" or job="yyy"}[3s])"#).is_empty());
        assert_eq!(
            columns(r#"rate(x_total{job="zzz" or exported_unit="a"}[3s])"#),
            a
        );
        assert_eq!(
            columns(r#"rate(x_total{job="j" or exported_unit="c"}[3s])"#),
            c
        );
        assert_eq!(
            columns(r#"rate(x_total{job="j" or exported_unit="a"}[3s])"#),
            a.union(&c).cloned().collect()
        );
    }
}
