//! Exporter labels whose key is a storage key (`unit`, `scale`, …), from
//! exposition text through `PrometheusConverter::convert` and
//! `SourceRecorder::stage` into an archive, read back by `ArchiveReader` and
//! queried through metriken-query, and ingested into a live `MemoryStore`
//! (#394).
//!
//! The converter stores such a label as `exported_<key>`, so two series
//! that differ only by it stay two series. A recording made before that,
//! whose two columns of one metric have one label set once storage keys are
//! removed, is refused by a selector matching that label set, on both rate
//! paths and for gauges and histograms, rather than read as one series.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use dendro::seal::SealPolicy;
use metriken_model::Snapshot;
use metriken_query::{MatrixSample, MetricsSource, QueryOptions, QueryResult};
use metriken_recorder::PrometheusConverter;
use metriken_storage::{ArchiveReader, ArchiveWriter, BufferPool, DendroCatalog, WriterConfig};

const S: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000 * S;
const TICKS: u64 = 12;

/// What the exporter serves at tick `t`: a counter and a gauge whose two
/// series differ only by `unit`, a histogram whose two differ only by
/// `scale`, and a counter series that differs from them by `job` as well.
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
# TYPE h histogram
h_bucket{{scale=\"a\",le=\"1\"}} {t}
h_bucket{{scale=\"a\",le=\"+Inf\"}} {t}
h_bucket{{scale=\"b\",le=\"10\"}} {t2}
h_bucket{{scale=\"b\",le=\"+Inf\"}} {t2}
",
        t2 = 2 * t,
        t3 = 3 * t
    )
}

/// The snapshot as metriken-recorder 0.12.0 wrote it: each `exported_<key>`
/// back under `<key>`.
fn as_before(mut snapshot: Snapshot) -> Snapshot {
    let Snapshot::V3(v3) = &mut snapshot else {
        panic!("a scrape is a V3 snapshot")
    };
    for group in &mut v3.groups {
        let schema = Arc::make_mut(group.schema.as_mut().unwrap());
        for list in [
            &mut schema.float_counters,
            &mut schema.float_gauges,
            &mut schema.histograms,
        ] {
            for d in list.iter_mut() {
                for key in ["unit", "scale"] {
                    if let Some(v) = d.metadata.remove(&format!("exported_{key}")) {
                        d.metadata.insert(key.to_string(), v);
                    }
                }
            }
        }
        group.schema_hash = schema.hash();
    }
    snapshot
}

/// The scrapes, once a second, as written today or (`old`) as before.
fn scrapes(old: bool) -> Vec<(u64, Snapshot)> {
    scrapes_of(&text, old)
}

/// `text`'s scrapes, once a second, as written today or (`old`) as before.
fn scrapes_of(text: &dyn Fn(u64) -> String, old: bool) -> Vec<(u64, Snapshot)> {
    let mut converter = PrometheusConverter::new();
    (0..TICKS)
        .map(|t| {
            let ts = BASE + t * S;
            let snapshot = converter.convert(&text(t), ts - S / 2, ts);
            (ts, if old { as_before(snapshot) } else { snapshot })
        })
        .collect()
}

/// The scrapes recorded into one archive as source `source="e"`, sealing
/// every 5 rows, and opened.
fn recorded(path: &Path, old: bool) -> ArchiveReader {
    record(path, scrapes(old)).unwrap()
}

/// `snapshots` recorded into one archive as source `source="e"`, sealing
/// every 5 rows, and opened; the first error writing them.
fn record(path: &Path, snapshots: Vec<(u64, Snapshot)>) -> Result<ArchiveReader, String> {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 5,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let e = |e: Box<dyn std::error::Error + Send + Sync>| e.to_string();
    let mut writer = ArchiveWriter::create(path, config).map_err(e)?;
    let labels = BTreeMap::from([("source".to_string(), "e".to_string())]);
    let mut source = writer
        .add_source(labels, BTreeMap::new(), BASE)
        .map_err(e)?;
    for (ts, snapshot) in snapshots {
        let staged = source.stage(&snapshot, ts, 0).map_err(e)?;
        writer.commit(vec![staged]).map_err(e)?;
        source.maybe_seal().map_err(e)?;
        source.sync().map_err(e)?;
    }
    source.finalize((BASE + (TICKS - 1) * S, 0)).map_err(e)?;
    writer.join().map_err(e)?;
    let recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(path).unwrap()),
        None,
        BufferPool::new(64 << 20),
        None,
    )
    .unwrap();
    Ok(ArchiveReader::flatten(
        recordings.into_iter().map(|(_, r)| r).collect(),
        None,
    ))
}

/// The scrapes ingested into a live store.
fn live(old: bool) -> metriken_query::MemoryStore {
    let store = metriken_query::MemoryStore::builder()
        .sampling_interval_ms(1000)
        .build();
    for (ts, mut snapshot) in scrapes(old) {
        let Snapshot::V3(v3) = &mut snapshot else {
            panic!("a scrape is a V3 snapshot")
        };
        v3.systemtime = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ts);
        store.ingest_snapshot(snapshot);
    }
    store
}

fn range(
    source: &dyn MetricsSource,
    query: &str,
    per_series: bool,
) -> Result<QueryResult, metriken_query::QueryError> {
    let (lo, hi) = ((BASE / S) as f64, (BASE / S + TICKS - 1) as f64);
    let opts = QueryOptions::default().with_per_series_rates(per_series);
    source.query_range_opts(query, lo, hi, 1.0, &opts)
}

fn matrix(source: &dyn MetricsSource, query: &str, per_series: bool) -> Vec<MatrixSample> {
    match range(source, query, per_series) {
        Ok(QueryResult::Matrix { result, .. }) => result,
        other => panic!("{query}: {other:?}"),
    }
}

/// Each series of `query` by its `exported_unit`, with its values.
fn by_exported_unit(
    source: &dyn MetricsSource,
    query: &str,
    per_series: bool,
) -> BTreeMap<String, Vec<f64>> {
    matrix(source, query, per_series)
        .into_iter()
        .map(|m| {
            let key = m.metric.get("exported_unit").cloned().unwrap_or_default();
            (key, m.values.into_iter().map(|(_, v)| v).collect())
        })
        .collect()
}

/// `rate(x_total[3s])`, the reproduction in #394: two series, each with
/// its own rate, on both rate paths, recorded and live; and a matcher on
/// `exported_unit` selects one.
#[test]
fn series_differing_by_a_storage_key_label_rate_apart() {
    let dir = tempfile::tempdir().unwrap();
    let reader = recorded(&dir.path().join("a.dendro"), false);
    let store = live(false);
    for source in [&reader as &dyn MetricsSource, &store] {
        for per_series in [false, true] {
            let rates = by_exported_unit(source, "rate(x_total[3s])", per_series);
            assert_eq!(
                rates.keys().map(String::as_str).collect::<Vec<_>>(),
                ["a", "b", "c"],
                "per_series={per_series}: {rates:?}"
            );
            for (unit, want) in [("a", 1.0), ("b", 2.0), ("c", 3.0)] {
                let values = &rates[unit];
                assert!(!values.is_empty(), "{unit}: no points");
                assert!(
                    values.iter().all(|v| (v - want).abs() < 1e-9),
                    "per_series={per_series} {unit}: {values:?}"
                );
            }
            let b = by_exported_unit(source, "rate(x_total{exported_unit=\"b\"}[3s])", per_series);
            assert_eq!(b.keys().collect::<Vec<_>>(), ["b"], "{b:?}");
        }
        let gauges = by_exported_unit(source, "g", false);
        assert_eq!(gauges["a"].iter().copied().fold(0.0, f64::max), 1.0);
        assert_eq!(gauges["b"].iter().copied().fold(f64::MAX, f64::min), 2.0);
    }
}

/// The queries over the old recording's joined series, each of which is
/// refused, naming the metric and the label set.
const REFUSED: &[(&str, &str)] = &[
    ("rate(x_total[3s])", "x_total"),
    ("irate(x_total[3s])", "x_total"),
    ("sum(rate(x_total[3s]))", "x_total"),
    ("rate(x_total{job=\"j\" or job=\"\"}[3s])", "x_total"),
    ("g", "g"),
    ("avg_over_time(g[3s])", "g"),
    ("histogram_quantile(0.5, h)", "h"),
    ("histogram_quantiles([0.5], h, 3)", "h"),
    ("histogram_heatmap(h)", "h"),
    ("histogram_irate(h)", "h"),
    ("histogram_mean(h)", "h"),
    ("histogram_count(h)", "h"),
    ("histogram_sum(h)", "h"),
    ("histogram_quantile(0.5, h{job=\"j\" or job=\"\"})", "h"),
    ("rate(h_overflow[3s])", "h_overflow"),
];

/// An archive written the old way holds two columns of `x_total`, `g`, `h`
/// and `h_overflow` whose label sets are equal once `unit` and `scale` are
/// removed. A selector matching that label set is an error on both rate
/// paths, for counters, gauges and histograms, rather than a panic or one
/// series of interleaved samples; a selector that does not match it reads
/// as before.
#[test]
fn an_archive_written_before_the_rename_refuses_the_joined_series() {
    let dir = tempfile::tempdir().unwrap();
    let reader = recorded(&dir.path().join("a.dendro"), true);
    for per_series in [false, true] {
        for (query, name) in REFUSED {
            let err = range(&reader, query, per_series)
                .expect_err(query)
                .to_string();
            let series = format!("{name}{{source=\"e\"}}");
            assert!(
                err.contains(&series)
                    && err.contains("is two or more stored series with one label set"),
                "per_series={per_series} {query}: {err}"
            );
            assert!(
                err.contains("(metric_type, unit,") && !err.contains("metric, "),
                "the keys listed are those two series of one name can differ by: {err}"
            );
        }
        let job = matrix(&reader, "rate(x_total{job=\"j\"}[3s])", per_series);
        assert_eq!(job.len(), 1, "{job:?}");
        assert!(job[0].values.iter().all(|(_, v)| (v - 3.0).abs() < 1e-9));
    }
    // `columns`, which a caller can ask before a query, refuses it too.
    let err = reader.columns("rate(x_total[3s])").expect_err("columns");
    assert!(err.to_string().contains("x_total{source=\"e\"}"), "{err}");
    assert!(reader.columns("rate(x_total{job=\"j\"}[3s])").is_ok());
    // The listing shows the joined series once, as before.
    let mut listed = reader.counter_labels("x_total");
    listed.sort();
    let set = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    assert_eq!(
        listed,
        vec![
            set(&[("job", "j"), ("source", "e")]),
            set(&[("source", "e")])
        ]
    );
}

/// The live store refuses the same series when one snapshot holds two
/// values of one series, as a converter before the rename produced.
#[test]
fn a_live_store_refuses_a_series_given_twice_in_one_snapshot() {
    let store = live(true);
    for (query, name) in REFUSED {
        let err = range(&store, query, false).expect_err(query).to_string();
        assert!(
            err.contains(&format!("{name}{{}}"))
                && err.contains("is two or more stored series with one label set"),
            "{query}: {err}"
        );
    }
    let job = matrix(&store, "rate(x_total{job=\"j\"}[3s])", false);
    assert_eq!(job.len(), 1, "{job:?}");
}

/// Two exporter samples with one stored name and label set in one scrape:
/// `x_total{unit="a"}` beside the exporter's own
/// `x_total{exported_unit="a"}`, which the rename makes equal, and a line
/// served twice. The first of each is kept, and the archive seals.
#[test]
fn a_sample_whose_series_the_scrape_already_has_is_dropped() {
    let renamed = |t: u64| {
        format!(
            "# TYPE x_total counter\nx_total{{unit=\"a\"}} {t}\nx_total{{exported_unit=\"a\"}} {}\n",
            100 * t
        )
    };
    let twice = |t: u64| format!("# TYPE x_total counter\nx_total {t}\nx_total {}\n", 100 * t);
    for (case, text) in [
        ("renamed", &renamed as &dyn Fn(u64) -> String),
        ("twice", &twice),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let reader = record(&dir.path().join("a.dendro"), scrapes_of(text, false))
            .unwrap_or_else(|e| panic!("{case}: the archive seals: {e}"));
        for per_series in [false, true] {
            let m = matrix(&reader, "rate(x_total[3s])", per_series);
            assert_eq!(m.len(), 1, "{case}: {m:?}");
            assert!(
                m[0].values.iter().all(|(_, v)| (v - 1.0).abs() < 1e-9),
                "{case}: the first sample is kept: {m:?}"
            );
        }
    }
}

/// Two series that differ only by `unit` (and a histogram pair only by
/// `scale`), recorded the old way, at the ticks `a` and `b` hold, beside a
/// counter served every tick.
fn sparse(t: u64, a: &dyn Fn(u64) -> bool, b: &dyn Fn(u64) -> bool) -> String {
    let mut text = format!(
        "# TYPE other_total counter\nother_total {t}\n# TYPE x_total counter\n# TYPE g gauge\n# TYPE h histogram\n"
    );
    for (unit, held, scale) in [("a", a, 1), ("b", b, 10)] {
        if held(t) {
            text.push_str(&format!(
                "x_total{{unit=\"{unit}\"}} {}\ng{{unit=\"{unit}\"}} {}\nh_bucket{{scale=\"{unit}\",le=\"1\"}} {}\nh_bucket{{scale=\"{unit}\",le=\"+Inf\"}} {}\n",
                scale * t,
                scale * t,
                scale * t,
                scale * t,
            ));
        }
    }
    text
}

/// The queries over a joined series whose columns the footer check does
/// not refuse, each an error naming the series.
const OUT_OF_ORDER: &[(&str, &str)] = &[
    ("rate(x_total[3s])", "x_total"),
    ("irate(x_total[3s])", "x_total"),
    ("g", "g"),
    ("avg_over_time(g[3s])", "g"),
    ("deriv(g[3s])", "g"),
    ("histogram_quantile(0.5, h)", "h"),
    ("histogram_irate(h)", "h"),
];

/// Columns of one label set whose shared rows the footer's null counts do
/// not prove (`unit="a"` at ticks 0 and 1, `unit="b"` at 1 and 2: four
/// values in a five-row segment), and columns that share no row but
/// alternate (`a` at even ticks, `b` at odd): read as one series, their
/// samples step back in time, which every evaluator refuses on both rate
/// paths. A histogram's alternating columns are not caught: its rows are
/// read in time order across columns, so they never step back. A series
/// read alone still reads.
#[test]
fn a_joined_series_the_footer_check_misses_is_refused_where_it_steps_back() {
    let partial = |t: u64| sparse(t, &|t| t <= 1, &|t| (1..=2).contains(&t));
    let alternating = |t: u64| sparse(t, &|t| t % 2 == 0, &|t| t % 2 == 1);
    for (case, text) in [
        ("partial", &partial as &dyn Fn(u64) -> String),
        ("alternating", &alternating),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let reader = record(&dir.path().join("a.dendro"), scrapes_of(text, true)).unwrap();
        for per_series in [false, true] {
            for (query, name) in OUT_OF_ORDER {
                // A histogram stream's rows are in time order whatever
                // column they come from, so alternating columns of `h`
                // never step back: not caught.
                if case == "alternating" && *name == "h" {
                    continue;
                }
                let err = match range(&reader, query, per_series) {
                    Err(e) => e.to_string(),
                    Ok(r) => panic!("{case} per_series={per_series} {query}: {r:?}"),
                };
                assert!(
                    err.contains(&format!("{name}{{source=\"e\"}}"))
                        && err.contains("Two columns can hold one label set"),
                    "{case} per_series={per_series} {query}: {err}"
                );
            }
            let other = matrix(&reader, "rate(other_total[3s])", per_series);
            assert_eq!(other.len(), 1, "{case}: {other:?}");
        }
    }
}

/// `text`'s scrapes ingested into a live store, each at the systemtime
/// `at(t)` gives, `times` times over.
fn live_at(at: &dyn Fn(u64) -> u64, times: usize) -> metriken_query::MemoryStore {
    let store = metriken_query::MemoryStore::builder()
        .sampling_interval_ms(1000)
        .build();
    for (t, (_, snapshot)) in scrapes(false).into_iter().enumerate() {
        let mut snapshot = snapshot;
        let Snapshot::V3(v3) = &mut snapshot else {
            panic!("a scrape is a V3 snapshot")
        };
        v3.systemtime = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(at(t as u64));
        for _ in 0..times {
            store.ingest_snapshot(snapshot.clone());
        }
    }
    store
}

/// A matrix result's series, labels sorted, in label order.
#[allow(clippy::type_complexity)]
fn series_of(result: QueryResult) -> Vec<(Vec<(String, String)>, Vec<(f64, f64)>)> {
    let QueryResult::Matrix { result, .. } = result else {
        panic!("not a matrix: {result:?}")
    };
    let mut out: Vec<_> = result
        .into_iter()
        .map(|m| {
            let mut labels: Vec<(String, String)> = m.metric.into_iter().collect();
            labels.sort();
            (labels, m.values)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The live store keeps each series in time order at ingest: a snapshot
/// ingested twice at one systemtime replaces the first's values, and one
/// from a wall clock stepped back 3 s is dropped until the clock passes
/// the last sample. Every query reads, as before the order check.
#[test]
fn a_live_store_keeps_each_series_in_time_order() {
    let once = live_at(&|t| BASE + t * S, 1);
    let twice = live_at(&|t| BASE + t * S, 2);
    let stepped = live_at(
        &|t| {
            if t < 6 {
                BASE + t * S
            } else {
                BASE + (t - 3) * S
            }
        },
        1,
    );
    let queries = [
        "rate(x_total[3s])",
        "irate(x_total[3s])",
        "g",
        "avg_over_time(g[3s])",
        "deriv(g[3s])",
        "histogram_quantile(0.5, h)",
        "histogram_irate(h)",
        "rate(h_overflow[3s])",
    ];
    for query in queries {
        for per_series in [false, true] {
            let want = range(&once, query, per_series).unwrap_or_else(|e| panic!("{query}: {e}"));
            let got =
                range(&twice, query, per_series).unwrap_or_else(|e| panic!("twice {query}: {e}"));
            assert_eq!(series_of(got), series_of(want), "twice {query}");
            if let Err(e) = range(&stepped, query, per_series) {
                panic!("stepped back {query}: {e}");
            }
        }
    }
}

/// Two histogram columns of one label set sharing a time in each of two
/// segments (`a` at ticks 0, 1, 5, 6; `b` at 1, 2, 6, 7; sealed every 5
/// rows), read across both: the splice keeps each column apart, so the
/// shared times are refused as they are within one segment.
#[test]
fn joined_histogram_columns_across_segments_are_refused() {
    let text = |t: u64| {
        sparse(t, &|t| [0, 1, 5, 6].contains(&t), &|t| {
            [1, 2, 6, 7].contains(&t)
        })
    };
    let dir = tempfile::tempdir().unwrap();
    let reader = record(&dir.path().join("a.dendro"), scrapes_of(&text, true)).unwrap();
    for query in ["histogram_quantile(0.5, h)", "histogram_irate(h)"] {
        let err = match range(&reader, query, false) {
            Err(e) => e.to_string(),
            Ok(r) => panic!("{query}: {r:?}"),
        };
        assert!(
            err.contains("h{source=\"e\"}") && err.contains("at or before the one read before it"),
            "{query}: {err}"
        );
    }
}
