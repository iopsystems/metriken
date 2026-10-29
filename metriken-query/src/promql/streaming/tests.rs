//! Tests for the streaming pipeline.
//!
//! * Per-operator unit tests build a small Memory by hand,
//!   exercise the streaming function directly (e.g. `Counters::irate` +
//!   `sum_by` + `collect_to_matrix`), and compare against the
//!   `QueryEngine::query_range` output for the equivalent PromQL —
//!   a coherence check that the dispatcher and the streaming
//!   functions agree.
//! * `cachecannon_smoke_test` (gated on `CACHECANNON_PARQUET`)
//!   runs every dashboard query against a real parquet fixture and
//!   asserts no errors, catching regressions where a streaming
//!   operator silently drops series or fails on a real workload.

use std::sync::Arc;

use crate::labels::Labels;
use crate::memory::Memory;
use crate::promql::streaming::{sum_by, LabeledSeries, Point};
use crate::promql::{MatrixSample, QueryEngine, QueryResult};
use crate::types::{Counter, Gauge};

/// Three counter series (foo/bar/baz) at t=1000..1002 with values
/// (100,200,300) / (200,300,400) / (300,400,500). irate over a [5s]
/// range yields 100/s at every emitted tick for every series.
fn cgroup_source() -> Arc<Memory> {
    let mut source = Memory::new(1000);
    for (name, base_val) in [
        ("/system.slice/foo.service", 100u64),
        ("/system.slice/bar.service", 200u64),
        ("/system.slice/baz.service", 300u64),
    ] {
        let mut labels = Labels::default();
        labels.inner.insert("name".to_string(), name.to_string());
        let timestamps: Vec<u64> = (0u64..3).map(|s| (1000 + s) * 1_000_000_000).collect();
        let values: Vec<u64> = (0u64..3).map(|s| base_val + s * 100).collect();
        source.add_counter(
            "cgroup_cpu_usage",
            Counter {
                labels,
                timestamps,
                values,
                windows: None,
            },
        );
    }
    Arc::new(source)
}

/// Sort matrix samples by their `name` label so unordered HashMap
/// iteration on either side doesn't trip the comparison.
fn sort_by_name(mut v: Vec<MatrixSample>) -> Vec<MatrixSample> {
    v.sort_by(|a, b| {
        a.metric
            .get("name")
            .cloned()
            .unwrap_or_default()
            .cmp(&b.metric.get("name").cloned().unwrap_or_default())
    });
    v
}

fn into_sorted(result: QueryResult) -> Vec<MatrixSample> {
    match result {
        QueryResult::Matrix { result, .. } => sort_by_name(result),
        other => panic!("expected matrix result, got {other:?}"),
    }
}

/// Assert two matrices are point-for-point equal.
fn assert_matrices_eq(a: &[MatrixSample], b: &[MatrixSample]) {
    assert_eq!(a.len(), b.len(), "series count");
    assert!(!a.is_empty(), "fixture yields at least one series");
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.metric.get("name"), y.metric.get("name"));
        assert_eq!(x.values.len(), y.values.len(), "point count");
        for ((xt, xv), (yt, yv)) in x.values.iter().zip(y.values.iter()) {
            assert!((xt - yt).abs() < 1e-9, "ts mismatch: {xt} vs {yt}");
            assert!((xv - yv).abs() < 1e-9, "value mismatch: {xv} vs {yv}");
        }
    }
}

#[test]
fn irate_is_rate_over_one_step() {
    // `irate` ignores its `[range]` and computes the rate over one step, as
    // `rate` over a range of one step does.
    let engine = QueryEngine::new(cgroup_source());
    let rate = into_sorted(
        engine
            .query_range("rate(cgroup_cpu_usage[1s])", 1000.0, 1003.0, 1.0)
            .unwrap(),
    );
    let irate = into_sorted(
        engine
            .query_range("irate(cgroup_cpu_usage[5s])", 1000.0, 1003.0, 1.0)
            .unwrap(),
    );
    assert_matrices_eq(&rate, &irate);
}

#[test]
fn sum_by_rate_equals_sum_by_irate() {
    // `sum by (name)` over the per-step rate: `irate`, or `rate` over one
    // step, yields the same series. Exercises sum_by over the grid producer.
    let engine = QueryEngine::new(cgroup_source());
    let via_rate = into_sorted(
        engine
            .query_range(
                "sum by (name) (rate(cgroup_cpu_usage[1s]))",
                1000.0,
                1003.0,
                1.0,
            )
            .unwrap(),
    );
    let via_irate = into_sorted(
        engine
            .query_range(
                "sum by (name) (irate(cgroup_cpu_usage[5s]))",
                1000.0,
                1003.0,
                1.0,
            )
            .unwrap(),
    );
    assert_matrices_eq(&via_rate, &via_irate);
}

#[test]
fn sum_by_groups_disjoint_label_into_one_series() {
    // Build two streams with different `name` labels; sum_by(["name"])
    // keeps them apart; sum_by([]) folds them into a single group.
    let mut a_labels = Labels::default();
    a_labels.inner.insert("name".to_string(), "a".to_string());
    let mut b_labels = Labels::default();
    b_labels.inner.insert("name".to_string(), "b".to_string());

    let a_pts: Vec<Point> = vec![Point::at(1, 1.0), Point::at(2, 2.0), Point::at(3, 3.0)];
    let b_pts: Vec<Point> = vec![Point::at(1, 10.0), Point::at(2, 20.0), Point::at(3, 30.0)];

    let stream = vec![
        LabeledSeries::new(a_labels.clone(), a_pts.clone().into_iter()),
        LabeledSeries::new(b_labels.clone(), b_pts.clone().into_iter()),
    ];
    let by_name = sum_by(stream, &["name".to_string()]);
    assert_eq!(by_name.len(), 2, "name-group keeps a and b separate");

    let stream = vec![
        LabeledSeries::new(a_labels, a_pts.into_iter()),
        LabeledSeries::new(b_labels, b_pts.into_iter()),
    ];
    let folded = sum_by(stream, &[]);
    assert_eq!(folded.len(), 1, "empty by-list collapses into one group");
    let mut iter = folded.into_iter().next().unwrap().iter;
    assert_eq!(iter.next(), Some(Point::at(1, 11.0)));
    assert_eq!(iter.next(), Some(Point::at(2, 22.0)));
    assert_eq!(iter.next(), Some(Point::at(3, 33.0)));
    assert_eq!(iter.next(), None);
}

/// Models the rezolus viewer's CPU-utilization pattern:
/// `sum(irate(cpu_usage[5s])) / cpu_cores`. The aggregator strips
/// labels from the LHS, so default-matching against `cpu_cores`
/// (which carries `node`, `source`) finds nothing — without the
/// single-right broadcast the query returns empty and the viewer
/// shows no data.
#[test]
fn single_right_broadcasts_against_label_stripping_aggregate() {
    let mut source = Memory::new(1000);

    // Two cpu_usage counter series at different cpus, each ramping 100/s.
    // sum(irate(...)) over both = 200/s.
    for cpu in 0..2u64 {
        let mut labels = Labels::default();
        labels.inner.insert("cpu".to_string(), cpu.to_string());
        source.add_counter(
            "cpu_usage",
            Counter {
                labels,
                timestamps: vec![1_000_000_000_000, 1_001_000_000_000, 1_002_000_000_000],
                values: vec![cpu * 1000, cpu * 1000 + 100, cpu * 1000 + 200],
                windows: None,
            },
        );
    }

    // cpu_cores gauge: single series with node/source labels, value=4.
    let mut labels = Labels::default();
    labels
        .inner
        .insert("node".to_string(), "agent-0".to_string());
    labels
        .inner
        .insert("source".to_string(), "rezolus".to_string());
    source.add_gauge(
        "cpu_cores",
        Gauge {
            labels,
            timestamps: vec![1_000_000_000_000, 1_001_000_000_000, 1_002_000_000_000],
            values: vec![4, 4, 4],
            windows: None,
        },
    );

    let engine = QueryEngine::new(Arc::new(source));
    let q = "sum(irate(cpu_usage[5s])) / cpu_cores";
    let r = engine.query(q, None).expect("query must succeed");
    let samples = match r {
        QueryResult::Vector { result, .. } => result,
        other => panic!("expected vector result, got {other:?}"),
    };
    assert_eq!(samples.len(), 1, "expected one broadcast series");
    // sum(irate) = 200 c/s across both cpus; / cpu_cores (=4) = 50.
    assert!((samples[0].value.1 - 50.0).abs() < 1e-9, "got {samples:?}");
}

// ---------------------------------------------------------------------------
// Cachecannon-fixture smoke test.
//
// Exercises the dispatcher against the real cachecannon dashboard
// queries on a real parquet capture, asserting each one runs without
// error. Gated on `CACHECANNON_PARQUET` (or the default rezolus
// checkout path) being present so CI doesn't depend on the fixture;
// run locally with:
//
//     CACHECANNON_PARQUET=/path/to/cachecannon.parquet \
//       cargo test -p metriken-query streaming::tests::cachecannon -- --nocapture
//
// Per-query wall-clock is printed with `--nocapture` for a quick
// perf sanity-check on local dev.
// ---------------------------------------------------------------------------

/// Every query-string the cachecannon dashboard generates, after the
/// dashboard wrapping logic (sum-irate for counters, raw selector
/// for gauges, histogram_quantiles/heatmap for histograms), plus
/// representative shapes the broader rezolus dashboards generate
/// against the same parquet (rate, avg/min/max/count, sum without,
/// avg_over_time, idelta).
const CACHECANNON_QUERIES: &[&str] = &[
    // --- cachecannon dashboard (loadgen, cardinality 1) ---
    // Gauge selector — streaming gauge step-grid.
    "target_rate{source=\"cachecannon\"}",
    // sum(irate(..)) — streaming counter+sum.
    "sum(irate(requests_sent{source=\"cachecannon\"}[5s]))",
    "sum(irate(responses_received{source=\"cachecannon\"}[5s]))",
    "sum(irate(bytes_rx{source=\"cachecannon\"}[5s]))",
    "sum(irate(bytes_tx{source=\"cachecannon\"}[5s]))",
    "sum(irate(request_errors{source=\"cachecannon\"}[5s]))",
    "sum(irate(connections_failed{source=\"cachecannon\"}[5s]))",
    "sum(irate(cache_hits{source=\"cachecannon\"}[5s]))",
    "sum(irate(cache_misses{source=\"cachecannon\"}[5s]))",
    "sum(irate(get_count{source=\"cachecannon\"}[5s]))",
    "sum(irate(set_count{source=\"cachecannon\"}[5s]))",
    // --- additional operators the broader rezolus dashboards use ---
    // rate (instead of irate)
    "sum(rate(cpu_cycles[5s]))",
    "sum by (cpu) (rate(cpu_usage[5s]))",
    // avg / min / max / count aggregations
    "avg(irate(cpu_usage[5s]))",
    "max(irate(cpu_usage[5s]))",
    "min(irate(cpu_usage[5s]))",
    "count(irate(cpu_usage[5s]))",
    // sum without (..) modifier
    "sum without (cpu) (irate(cpu_cycles[5s]))",
    "sum without (id) (irate(softirq_time[5s]))",
    // deriv on a gauge (target_rate is the cachecannon loadgen target)
    "deriv(target_rate{source=\"cachecannon\"}[5s])",
    // binary ops: matrix x scalar (byte->bit, percent, complement)
    "sum(irate(bytes_rx{source=\"cachecannon\"}[5s])) * 8",
    "sum(irate(cache_hits{source=\"cachecannon\"}[5s])) / 1000",
    // binary ops: matrix x matrix (cache hit rate, IPC analogue)
    "sum(irate(cache_hits{source=\"cachecannon\"}[5s])) / sum(irate(cache_misses{source=\"cachecannon\"}[5s]))",
    "sum(irate(cpu_instructions[5s])) / sum(irate(cpu_cycles[5s]))",
    // by-grouped binary op
    "sum by (cpu) (irate(cpu_instructions[5s])) / sum by (cpu) (irate(cpu_cycles[5s]))",
    // Histograms — eager path.
    "histogram_quantiles([0.5, 0.9, 0.99, 0.999], response_latency{source=\"cachecannon\"})",
    "histogram_quantiles([0.5, 0.9, 0.99, 0.999], get_latency{source=\"cachecannon\"})",
    "histogram_quantiles([0.5, 0.9, 0.99, 0.999], set_latency{source=\"cachecannon\"})",
    "histogram_heatmap(response_latency{source=\"cachecannon\"})",
];

/// Look up the cachecannon parquet path, falling back to the
/// rezolus checkout location used in dev. Returns `None` (and emits
/// a diagnostic) when neither path is readable; the calling test
/// then exits cleanly without failing.
fn cachecannon_parquet_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("CACHECANNON_PARQUET") {
        return Some(std::path::PathBuf::from(p));
    }
    let rezolus =
        std::path::PathBuf::from("/home/user/rezolus/site/viewer/data/cachecannon.parquet");
    if rezolus.exists() {
        return Some(rezolus);
    }
    None
}

fn count_points(result: &QueryResult) -> usize {
    match result {
        QueryResult::Matrix { result, .. } => result.iter().map(|s| s.values.len()).sum(),
        QueryResult::Vector { result, .. } => result.len(),
        QueryResult::Scalar { .. } => 1,
        QueryResult::HistogramHeatmap { result, .. } => result.data.len(),
    }
}

/// Smoke test: run every query the cachecannon dashboard generates
/// (plus representative shapes from the broader rezolus dashboards
/// that hit the same parquet) against a real fixture, and assert
/// each one returns `Ok` with at least one point.
#[test]
fn cachecannon_smoke_test() {
    let Some(path) = cachecannon_parquet_path() else {
        eprintln!(
            "skipping cachecannon smoke test: set CACHECANNON_PARQUET=/path/to/cachecannon.parquet \
             or check out rezolus alongside metriken"
        );
        return;
    };

    let engine = match crate::parquet::ParquetReader::open(&path) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping cachecannon smoke test: failed to open {path:?}: {e}");
            return;
        }
    };

    let Some((start, end)) = engine.time_range() else {
        eprintln!("skipping cachecannon smoke test: parquet file appears empty");
        return;
    };
    let step = 1.0;

    let mut total = std::time::Duration::ZERO;

    println!(
        "\ncachecannon smoke: {n} queries, range = [{start:.0}, {end:.0}], step = {step}",
        n = CACHECANNON_QUERIES.len(),
    );
    println!("{:<70} {:>9} {:>10}", "query", "points", "µs");

    for q in CACHECANNON_QUERIES {
        let t0 = std::time::Instant::now();
        let result = engine.query_range(q, start, end, step);
        let dt = t0.elapsed();
        total += dt;

        let points = match &result {
            Ok(r) => count_points(r),
            Err(e) => panic!("query failed: {q}: {e}"),
        };

        let q_short: String = if q.len() > 68 {
            format!("{}…", &q[..67])
        } else {
            (*q).to_string()
        };
        println!("{:<70} {:>9} {:>10}", q_short, points, dt.as_micros());
    }

    println!("\ntotal: {} µs", total.as_micros());
}

/// A gauge series at `start + k * every` (seconds) for `n` rows, value `v`.
fn gauge_rows(
    source: &mut Memory,
    name: &str,
    labels: &[(&str, &str)],
    start: f64,
    every: f64,
    n: u64,
    v: i64,
) {
    let timestamps: Vec<u64> = (0..n)
        .map(|k| ((start + k as f64 * every) * 1e9).round() as u64)
        .collect();
    source.add_gauge(
        name,
        Gauge {
            labels: Labels::from(labels.to_vec().as_slice()),
            values: vec![v; timestamps.len()],
            timestamps,
            windows: None,
        },
    );
}

fn raw_points(engine: &QueryEngine, query: &str, start: f64, end: f64) -> Vec<Vec<(f64, f64)>> {
    let opts = crate::QueryOptions::with_rate_mode(crate::RateMode::Raw);
    let QueryResult::Matrix { result, .. } = engine
        .query_range_opts(query, start, end, 1.0, &opts)
        .unwrap()
    else {
        panic!("a matrix");
    };
    result.into_iter().map(|m| m.values).collect()
}

/// In raw mode each series' points are at its own sample times. `sum` over
/// two series read half a second apart reads both at every point once both
/// have started, each held for its own spacing, rather than alternating
/// between them (10, 100, 10, ...) by summing only the series with a sample
/// at that exact instant.
#[test]
fn raw_sum_reads_every_series_at_its_latest_sample() {
    let mut source = Memory::new(1000);
    gauge_rows(&mut source, "g", &[("source", "a")], 1000.0, 1.0, 5, 10);
    gauge_rows(&mut source, "g", &[("source", "b")], 1000.5, 1.0, 5, 100);
    let engine = QueryEngine::new(Arc::new(source));
    let series = raw_points(&engine, "sum(g)", 1000.0, 1004.5);
    assert_eq!(series.len(), 1);
    let points = &series[0];
    assert_eq!(points[0], (1000.0, 10.0), "only a has started: {points:?}");
    assert_eq!(points.len(), 10, "{points:?}");
    assert!(points[1..].iter().all(|(_, v)| *v == 110.0), "{points:?}");
}

/// A series' last point before the range is held at its start: with both
/// series read from well before the range, the first point of the range
/// has both, for gauges and for rates. Nothing before the start is
/// returned.
#[test]
fn raw_sum_holds_points_from_before_the_range() {
    let mut source = Memory::new(1000);
    gauge_rows(&mut source, "g", &[("source", "a")], 990.0, 1.0, 16, 10);
    gauge_rows(&mut source, "g", &[("source", "b")], 990.5, 1.0, 16, 100);
    for (label, offset) in [("a", 0.0), ("b", 0.5)] {
        let timestamps: Vec<u64> = (0..16u64)
            .map(|k| ((990.0 + offset + k as f64) * 1e9) as u64)
            .collect();
        source.add_counter(
            "c",
            Counter {
                labels: Labels::from([("source", label)]),
                values: (0..16u64)
                    .map(|k| k * if label == "a" { 10 } else { 100 })
                    .collect(),
                timestamps,
                windows: None,
            },
        );
    }
    let engine = QueryEngine::new(Arc::new(source));
    for q in ["sum(g)", "sum(irate(c[5s]))", "sum(irate(c[1s]))"] {
        let series = raw_points(&engine, q, 1000.0, 1004.5);
        let points = &series[0];
        assert_eq!(points[0], (1000.0, 110.0), "{q}: {points:?}");
        assert!(
            points.iter().all(|(t, v)| *t >= 1000.0 && *v == 110.0),
            "{q}: {points:?}"
        );
    }
}

/// The same for a binary operator: `x / y` with `x` and `y` read at
/// different instants has a point wherever either was read, once both have
/// been. On exact timestamps it was empty.
#[test]
fn raw_binary_pairs_each_side_at_its_latest_sample() {
    let mut source = Memory::new(1000);
    gauge_rows(&mut source, "x", &[], 1000.0, 1.0, 3, 10);
    gauge_rows(&mut source, "y", &[], 1000.5, 1.0, 3, 2);
    let engine = QueryEngine::new(Arc::new(source));
    let series = raw_points(&engine, "x / y", 1000.0, 1003.0);
    assert_eq!(series.len(), 1);
    let times: Vec<f64> = series[0].iter().map(|(t, _)| *t).collect();
    assert_eq!(
        times,
        vec![1000.5, 1001.0, 1001.5, 1002.0, 1002.5],
        "{series:?}"
    );
    assert!(series[0].iter().all(|(_, v)| *v == 5.0), "{series:?}");
}

/// A series is held only for the gap that preceded its latest sample: once
/// it stops, it stops contributing.
#[test]
fn raw_sum_stops_holding_a_series_that_ended() {
    let mut source = Memory::new(1000);
    gauge_rows(&mut source, "g", &[("source", "a")], 1000.0, 1.0, 2, 10);
    gauge_rows(&mut source, "g", &[("source", "b")], 1000.5, 1.0, 6, 100);
    let engine = QueryEngine::new(Arc::new(source));
    let series = raw_points(&engine, "sum(g)", 1000.0, 1006.0);
    let points = &series[0];
    // a's last sample is at 1001.0, held to 1002.0.
    assert!(points.contains(&(1001.5, 110.0)), "{points:?}");
    assert!(points.contains(&(1002.5, 100.0)), "{points:?}");
    assert!(points.contains(&(1005.5, 100.0)), "{points:?}");
}

/// A raw rate of a table slower than the source's finest interval keeps
/// its first point in range, when the source reports the table's interval
/// (as an archive does for each table): the read and the hold bound cover
/// two of that interval, so the sample before the range is there to pair
/// with.
#[test]
fn raw_rate_of_a_slow_table_keeps_its_first_point() {
    use metriken_storage::labeled::LabeledSource;
    use metriken_storage::recordings::RecordingsSource;
    let table = |name: &str, every: u64| -> Arc<dyn crate::DataSource> {
        let mut source = Memory::new(every * 1000);
        let timestamps: Vec<u64> = (0..=60 / every)
            .map(|k| (1000 + k * every) * 1_000_000_000)
            .collect();
        source.add_counter(
            name,
            Counter {
                labels: Labels::default(),
                values: (0..timestamps.len() as u64)
                    .map(|k| k * 10 * every)
                    .collect(),
                timestamps,
                windows: None,
            },
        );
        Arc::new(
            LabeledSource::new(Arc::new(source), Labels::default()).with_interval(every as f64),
        )
    };
    let engine = QueryEngine::new(Arc::new(RecordingsSource::new(vec![
        table("slow", 10),
        table("fast", 1),
    ])));
    let series = raw_points(&engine, "irate(slow[5m])", 1005.0, 1060.0);
    assert_eq!(series[0][0], (1010.0, 10.0), "{series:?}");
}
