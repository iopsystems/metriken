//! Gauge histograms through `MemoryStore`'s public query entry points: read
//! as the state at each step, never differenced; the functions of a row
//! difference refuse with an error naming the metric as a gauge histogram.
//! The rules are `docs/journal/2026-10-21-gauge-histogram.md`'s; the
//! archive path is `metriken-storage/tests/gauge_histogram.rs`.

use metriken_query::{MemoryStore, MetricsSource, QueryError, QueryResult};
use metriken_storage::{HistogramSnapshot, Labels};

const S: u64 = 1_000_000_000;
const T0: u64 = 1_000 * S;

/// `buckets` (index, count) in the cumulative sparse form a series holds.
fn snap(buckets: &[(u32, u64)]) -> HistogramSnapshot {
    let mut running = 0;
    HistogramSnapshot {
        index: buckets.iter().map(|b| b.0).collect(),
        count: buckets
            .iter()
            .map(|b| {
                running += b.1;
                running
            })
            .collect(),
    }
}

fn labels(dev: &str) -> Labels {
    let mut l = Labels::default();
    l.inner.insert("dev".to_string(), dev.to_string());
    l
}

/// Two devices, rows every second from 1000 s to 1010 s. sda's state is the
/// same at every row (100 extents of size 1, 10 of size 4 to 7); sdb's
/// shrinks in bucket 1 and grows in bucket 3, by one a second each.
fn store() -> MemoryStore {
    let store = MemoryStore::builder().sampling_interval_ms(1000).build();
    let config = histogram::Config::new(0, 16).unwrap();
    let ts: Vec<u64> = (0..=10).map(|i| T0 + i * S).collect();
    store
        .insert_gauge_histogram_series(
            "free",
            labels("sda"),
            config,
            ts.clone(),
            ts.iter().map(|_| snap(&[(1, 100), (3, 10)])).collect(),
        )
        .unwrap();
    store
        .insert_gauge_histogram_series(
            "free",
            labels("sdb"),
            config,
            ts.clone(),
            (0..=10u64)
                .map(|i| snap(&[(1, 50 - i), (3, 20 + i)]))
                .collect(),
        )
        .unwrap();
    store
}

fn matrix(store: &MemoryStore, q: &str, start: f64, end: f64, step: f64) -> Vec<Vec<(f64, f64)>> {
    match store.query_range(q, start, end, step).unwrap() {
        QueryResult::Matrix { result } => result.into_iter().map(|m| m.values).collect(),
        other => panic!("{q}: {other:?}"),
    }
}

fn refused(store: &MemoryStore, q: &str) -> String {
    match store.query_range(q, 1000.0, 1010.0, 1.0) {
        Err(QueryError::Unsupported(msg)) => msg,
        other => panic!("{q}: expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_gauge_histogram_is_listed_apart() {
    let store = store();
    assert_eq!(store.gauge_histogram_names(), ["free"]);
    assert!(store.histogram_names().is_empty());
    assert_eq!(store.gauge_histogram_labels("free").len(), 2);
    let devs = MetricsSource::label_values(&store, "free", "dev");
    assert_eq!(devs.len(), 2);
}

/// The count is the state's item count at every step, including steps where
/// the state did not change (sda: an empty delta, which the counting form
/// would skip) and where items moved between buckets (sdb: a delta of +1).
#[test]
fn histogram_count_is_the_state_at_each_step() {
    let store = store();
    let got = matrix(&store, "histogram_count(free)", 1000.0, 1010.0, 1.0);
    assert_eq!(got.len(), 1);
    let want: Vec<(f64, f64)> = (0..=10).map(|i| (1000.0 + i as f64, 180.0)).collect();
    assert_eq!(got[0], want);

    // Per device, as `sum by` and as the `by` clause.
    for q in [
        "sum by (dev) (histogram_count(free))",
        "histogram_count by (dev) (free)",
    ] {
        let got = matrix(&store, q, 1000.0, 1002.0, 1.0);
        assert_eq!(got.len(), 2, "{q}");
        for series in got {
            assert!(series.iter().all(|(_, v)| *v == 110.0 || *v == 70.0), "{q}");
            assert_eq!(series.len(), 3, "{q}");
        }
    }

    // A label matcher selects one device.
    let got = matrix(
        &store,
        "histogram_count(free{dev=\"sdb\"})",
        1003.0,
        1003.0,
        1.0,
    );
    assert_eq!(got, [[(1003.0, 70.0)]]);
}

/// Points fall on the step grid, not at the rows, and a series is held a
/// step (or its interval when longer) past its latest row.
#[test]
fn points_are_on_the_grid_and_go_stale() {
    let store = store();
    // A half-second step: every point holds the row at or before it.
    let got = matrix(&store, "histogram_count(free)", 1009.0, 1012.0, 0.5);
    let times: Vec<f64> = got[0].iter().map(|p| p.0).collect();
    // The last row is at 1010 s; it is held for the 1 s interval, to 1011 s.
    assert_eq!(times, [1009.0, 1009.5, 1010.0, 1010.5, 1011.0]);
    // A 5 s step holds for 5 s.
    let got = matrix(&store, "histogram_count(free)", 1010.0, 1020.0, 5.0);
    assert_eq!(got, [[(1010.0, 180.0), (1015.0, 180.0)]]);
}

#[test]
fn quantiles_sum_mean_and_heatmap_read_the_state() {
    let store = store();
    // At 1000 s: 150 items of size 1 and 30 of size 4 to 7. The median is
    // in bucket 1 (reported at its upper edge, 1), the maximum in bucket 3
    // (edge 7).
    let q = matrix(&store, "histogram_quantile(0.5, free)", 1000.0, 1000.0, 1.0);
    assert_eq!(q, [[(1000.0, 1.0)]]);
    let QueryResult::Matrix { result } = store
        .query_range("histogram_quantiles([0.5, 1.0], free)", 1000.0, 1000.0, 1.0)
        .unwrap()
    else {
        panic!("a matrix")
    };
    let by_q: Vec<(String, f64)> = result
        .iter()
        .map(|m| (m.metric["quantile"].clone(), m.values[0].1))
        .collect();
    assert_eq!(by_q, [("0.5".to_string(), 1.0), ("1".to_string(), 7.0)]);

    // Sum and mean from bucket midpoints: (150·1 + 30·5.5) = 315.
    let sum = matrix(&store, "histogram_sum(free)", 1000.0, 1000.0, 1.0);
    assert_eq!(sum, [[(1000.0, 315.0)]]);
    let mean = matrix(&store, "histogram_mean(free)", 1000.0, 1000.0, 1.0);
    assert_eq!(mean, [[(1000.0, 315.0 / 180.0)]]);

    let QueryResult::HistogramHeatmap { result } = store
        .query_range("histogram_heatmap(free)", 1000.0, 1001.0, 1.0)
        .unwrap()
    else {
        panic!("a heatmap")
    };
    assert_eq!(result.timestamps, [1000.0, 1001.0]);
    assert_eq!(result.bucket_bounds, [1, 3, 7]);
    assert_eq!(
        result.data,
        [(0, 0, 150.0), (0, 2, 30.0), (1, 0, 149.0), (1, 2, 31.0)]
    );
}

#[test]
fn an_instant_query_reads_the_state_at_the_instant() {
    let store = store();
    let QueryResult::Vector { result } = store
        .query("histogram_count(free{dev=\"sdb\"})", Some(1004.5))
        .unwrap()
    else {
        panic!("a vector")
    };
    assert_eq!(result.len(), 1);
    // sdb's row at 1004 s: 46 + 24 items.
    assert_eq!(result[0].value, (1004.5, 70.0));
}

/// Every function of a difference between rows refuses, and says what the
/// metric is and how to read it.
#[test]
fn delta_functions_refuse() {
    let store = store();
    for q in [
        "histogram_irate(free)",
        "histogram_count(free, 5)",
        "histogram_sum(free, 5)",
        "histogram_quantiles([0.5], free, 5)",
        "histogram_heatmap(free, 5)",
        "rate(free[1m])",
        "irate(free[5s])",
        "sum(rate(free[1m]))",
        "free",
    ] {
        let msg = refused(&store, q);
        assert!(msg.contains("free is a gauge histogram"), "{q}: {msg}");
        assert!(msg.contains("histogram_quantile(0.5, free)"), "{q}: {msg}");
        // Each says what it refused.
        let what = if q.starts_with("histogram_irate") {
            "histogram_irate takes the difference between rows"
        } else if q.starts_with("histogram_") {
            "a stride, which pools the observations between rows"
        } else if q == "free" {
            "this function reads a gauge"
        } else {
            "this function reads a counter"
        };
        assert!(msg.contains(what), "{q}: {msg}");
    }
}

/// A name held as both kinds is refused, not read as either.
#[test]
fn a_name_of_both_kinds_is_refused() {
    let store = store();
    store
        .insert_histogram_series(
            "free",
            labels("sdc"),
            histogram::Config::new(0, 16).unwrap(),
            vec![T0],
            vec![snap(&[(1, 1)])],
        )
        .unwrap();
    let msg = refused(&store, "histogram_count(free)");
    assert!(msg.contains("cannot be queried while it is both"), "{msg}");
}

/// `ingest_snapshot` stores a snapshot's `gauge_histograms` entries as gauge
/// histograms.
#[cfg(feature = "ingest")]
#[test]
fn an_ingested_gauge_histogram_entry_is_a_gauge_histogram() {
    use std::collections::HashMap;
    use std::time::{Duration, SystemTime};

    let store = MemoryStore::builder().sampling_interval_ms(1000).build();
    for t in 0..3u64 {
        let mut h = histogram::Histogram::new(0, 16).unwrap();
        h.as_mut_slice()[1] = 10 + t;
        let metadata: HashMap<String, String> = [
            ("metric", "free"),
            ("dev", "sda"),
            ("grouping_power", "0"),
            ("max_value_power", "16"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        store.ingest_snapshot(metriken_model::Snapshot::V2(metriken_model::SnapshotV2 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(T0 + t * S),
            duration: Duration::from_millis(1),
            metadata: HashMap::new(),
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: Vec::new(),
            gauge_histograms: vec![metriken_model::Histogram::new("7".to_string(), h, metadata)],
        }));
    }
    assert_eq!(store.gauge_histogram_names(), ["free"]);
    assert!(store.histogram_names().is_empty());
    assert_eq!(
        store.gauge_histogram_labels("free"),
        [[("dev".to_string(), "sda".to_string())].into()]
    );
    let got = matrix(&store, "histogram_count(free)", 1000.0, 1002.0, 1.0);
    assert_eq!(got, [[(1000.0, 10.0), (1001.0, 11.0), (1002.0, 12.0)]]);
}

/// A read of a gauge histogram that matches nothing is an empty answer, not
/// `MetricNotFound`: a label matching no series, and an instant past every
/// series' staleness.
#[test]
fn an_empty_read_is_empty_not_missing() {
    let store = store();
    for q in [
        "histogram_quantile(0.5, free{dev=\"none\"})",
        "histogram_quantiles([0.5], free{dev=\"none\"})",
        "histogram_count(free{dev=\"none\"})",
        "histogram_quantile(0.5, free)",
    ] {
        let start = if q.ends_with("(0.5, free)") {
            2000.0
        } else {
            1000.0
        };
        match store.query_range(q, start, start + 10.0, 1.0) {
            Ok(QueryResult::Matrix { result }) => assert!(result.is_empty(), "{q}: {result:?}"),
            other => panic!("{q}: {other:?}"),
        }
    }
    match store.query("histogram_quantile(0.5, free)", Some(2000.0)) {
        Ok(QueryResult::Vector { result }) => assert!(result.is_empty(), "{result:?}"),
        other => panic!("{other:?}"),
    }
}

/// What the default listings report for `src`: every name, the series count,
/// every label key, and each key's values for `free`.
type Listings = (
    Vec<String>,
    usize,
    std::collections::BTreeSet<String>,
    std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
);

fn listings<S: MetricsSource + ?Sized>(src: &S) -> Listings {
    (
        src.all_names(),
        src.total_series_count(),
        src.all_label_keys(),
        src.label_values_by_key("free")
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect(),
    )
}

fn set(items: &[&str]) -> std::collections::BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn devs() -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
    [("dev".to_string(), set(&["sda", "sdb"]))].into()
}

/// A store holding only a gauge histogram lists it in every default
/// listing, alone and as the only child of a union.
#[test]
fn a_gauge_histogram_alone_is_in_every_listing() {
    use metriken_query::UnionMetricsSource;
    use metriken_storage::union::UnionChild;

    let store = store();
    let want = (vec!["free".to_string()], 2, set(&["dev"]), devs());
    assert_eq!(listings(&store), want);
    let union = UnionMetricsSource::new(vec![UnionChild::from(&store)]);
    assert_eq!(listings(&union), want);
}

/// Beside a counter and a gauge, a gauge histogram is counted once and its
/// labels join theirs; a union of the gauge histogram's store and the
/// other kinds' store lists the same.
#[test]
fn a_gauge_histogram_beside_other_kinds_is_in_every_listing() {
    use metriken_query::UnionMetricsSource;
    use metriken_storage::union::UnionChild;

    let ts: Vec<u64> = (0..=10).map(|i| T0 + i * S).collect();
    let mut cpu = Labels::default();
    cpu.inner.insert("cpu".to_string(), "0".to_string());
    let add_others = |store: &MemoryStore| {
        let n = ts.len();
        store
            .insert_counter_series("cycles", cpu.clone(), ts.clone(), vec![1; n], None)
            .unwrap();
        store
            .insert_gauge_series("temp", cpu.clone(), ts.clone(), vec![1; n], None)
            .unwrap();
    };
    let mixed = store();
    add_others(&mixed);
    let other = MemoryStore::builder().sampling_interval_ms(1000).build();
    add_others(&other);

    let want = (
        vec!["cycles".to_string(), "free".to_string(), "temp".to_string()],
        4,
        set(&["cpu", "dev"]),
        devs(),
    );
    assert_eq!(listings(&mixed), want);
    let gauge_histograms = store();
    let union = UnionMetricsSource::new(vec![
        UnionChild::from(&gauge_histograms),
        UnionChild::from(&other),
    ]);
    assert_eq!(listings(&union), want);
}
