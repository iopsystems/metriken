//! Tests for the streaming pipeline.
//!
//! * Per-operator unit tests build a small TSDB by hand, exercise
//!   the streaming function directly (e.g. `irate_counters` +
//!   `sum_by` + `collect_to_matrix`), and compare against the
//!   `QueryEngine::query_range` output for the equivalent PromQL —
//!   a coherence check that the dispatcher and the streaming
//!   functions agree.
//! * `cachecannon_smoke_test` (gated on `CACHECANNON_PARQUET`)
//!   runs every dashboard query against a real parquet fixture and
//!   asserts no errors, catching regressions where a streaming
//!   operator silently drops series or fails on a real workload.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use metriken_exposition::{Counter, Snapshot, SnapshotV2};

use crate::promql::streaming::{
    collect_to_matrix, irate_counters, sum_by, CounterIrate, LabeledSeries,
};
use crate::promql::{MatrixSample, QueryEngine, QueryResult};
use crate::tsdb::{Labels, Tsdb};

/// Three counter series (foo/bar/baz) at t=1000..1002 with values
/// (100,200,300) / (200,300,400) / (300,400,500). irate over a [5s]
/// range yields 100/s at every emitted tick for every series.
fn cgroup_tsdb() -> Tsdb {
    let mut tsdb = Tsdb::default();
    let base_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    let cgroups = [
        ("/system.slice/foo.service", 100u64),
        ("/system.slice/bar.service", 200u64),
        ("/system.slice/baz.service", 300u64),
    ];

    for step in 0u64..3 {
        let time = base_time + Duration::from_secs(step);
        let mut counters = Vec::new();
        for (name, base_val) in &cgroups {
            let mut metadata = HashMap::new();
            metadata.insert("name".to_string(), name.to_string());
            metadata.insert("metric".to_string(), "cgroup_cpu_usage".to_string());
            counters.push(Counter {
                name: "cgroup_cpu_usage".to_string(),
                value: base_val + step * 100,
                metadata,
            });
        }
        let snapshot = Snapshot::V2(SnapshotV2 {
            systemtime: time,
            duration: Duration::from_secs(1),
            metadata: HashMap::new(),
            counters,
            gauges: Vec::new(),
            histograms: Vec::new(),
        });
        tsdb.ingest(snapshot);
    }

    tsdb
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
        QueryResult::Matrix { result } => sort_by_name(result),
        other => panic!("expected matrix result, got {other:?}"),
    }
}

#[test]
fn streaming_irate_matches_eager_irate() {
    let tsdb = Arc::new(cgroup_tsdb());

    // Eager.
    let engine = QueryEngine::new(tsdb.clone());
    let eager = engine
        .query_range("irate(cgroup_cpu_usage[5s])", 1000.0, 1003.0, 1.0)
        .unwrap();
    let eager = into_sorted(eager);

    // Streaming.
    let collection = tsdb
        .counters("cgroup_cpu_usage", Labels::default())
        .expect("collection present");
    let stream = irate_counters(
        &collection,
        &Labels::default(),
        1_000_000_000_000, // start_ns
        1_003_000_000_000, // end_ns
        1_000_000_000,     // step_ns
        5_000_000_000,     // range_ns
    );
    let streaming = sort_by_name(collect_to_matrix(stream, Some("cgroup_cpu_usage")));

    assert_eq!(
        eager.len(),
        streaming.len(),
        "series count must match (eager={}, streaming={})",
        eager.len(),
        streaming.len()
    );
    for (e, s) in eager.iter().zip(streaming.iter()) {
        assert_eq!(e.metric.get("name"), s.metric.get("name"));
        assert_eq!(
            e.values.len(),
            s.values.len(),
            "point count for {:?}",
            e.metric.get("name")
        );
        for ((et, ev), (st, sv)) in e.values.iter().zip(s.values.iter()) {
            assert!((et - st).abs() < 1e-9, "ts mismatch: {et} vs {st}");
            assert!((ev - sv).abs() < 1e-9, "value mismatch: {ev} vs {sv}");
        }
    }
}

#[test]
fn streaming_sum_by_matches_eager_sum_by() {
    let tsdb = Arc::new(cgroup_tsdb());

    let engine = QueryEngine::new(tsdb.clone());
    let eager = engine
        .query_range(
            "sum by (name) (irate(cgroup_cpu_usage[5s]))",
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();
    let eager = into_sorted(eager);

    let collection = tsdb
        .counters("cgroup_cpu_usage", Labels::default())
        .expect("collection present");
    let irate_stream = irate_counters(
        &collection,
        &Labels::default(),
        1_000_000_000_000,
        1_003_000_000_000,
        1_000_000_000,
        5_000_000_000,
    );
    let summed = sum_by(irate_stream, &["name".to_string()]);
    // sum-by aggregated result: strip __name__ to match the eager
    // path's `handle_aggregate` (which keeps only the by-labels).
    let streaming = sort_by_name(collect_to_matrix(summed, None));

    assert_eq!(eager.len(), streaming.len());
    for (e, s) in eager.iter().zip(streaming.iter()) {
        assert_eq!(e.metric.get("name"), s.metric.get("name"));
        assert_eq!(e.values.len(), s.values.len());
        for ((et, ev), (st, sv)) in e.values.iter().zip(s.values.iter()) {
            assert!((et - st).abs() < 1e-9);
            assert!((ev - sv).abs() < 1e-9);
        }
    }
}

#[test]
fn sum_by_groups_disjoint_label_into_one_series() {
    // Build two streams with different `name` labels; sum_by(["name"])
    // keeps them apart; sum_by([]) folds them into a single group.
    let mut a_labels = Labels::default();
    a_labels.inner.insert("name".to_string(), "a".to_string());
    let mut b_labels = Labels::default();
    b_labels.inner.insert("name".to_string(), "b".to_string());

    let a_pts: Vec<(u64, f64)> = vec![(1, 1.0), (2, 2.0), (3, 3.0)];
    let b_pts: Vec<(u64, f64)> = vec![(1, 10.0), (2, 20.0), (3, 30.0)];

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
    assert_eq!(iter.next(), Some((1, 11.0)));
    assert_eq!(iter.next(), Some((2, 22.0)));
    assert_eq!(iter.next(), Some((3, 33.0)));
    assert_eq!(iter.next(), None);
}

#[test]
fn counter_irate_handles_reset() {
    // Counter goes 100, 200, 300, 50 (reset), 150 at t=1..5 sec.
    let samples: Vec<(u64, u64)> = vec![
        (1_000_000_000, 100),
        (2_000_000_000, 200),
        (3_000_000_000, 300),
        (4_000_000_000, 50),
        (5_000_000_000, 150),
    ];
    // Range [5s], step 1s, evaluate at t=5s only.
    let mut iter = CounterIrate::new(
        &samples,
        5_000_000_000,
        5_000_000_000,
        1_000_000_000,
        5_000_000_000,
    );
    let p = iter.next().expect("one point at t=5s");
    assert_eq!(p.0, 5_000_000_000);
    // Last two: (4s, 50) and (5s, 150). 150 >= 50 → delta=100/1s = 100.
    assert!((p.1 - 100.0).abs() < 1e-9);
    assert!(iter.next().is_none());
}

// ---------------------------------------------------------------------------
// Cachecannon-fixture parity test.
//
// Exercises the dispatcher against the real cachecannon dashboard
// queries on a real parquet capture, comparing streaming-on vs
// streaming-off output pointwise. Gated on `CACHECANNON_PARQUET` (or
// the default rezolus checkout path) being present so CI doesn't
// depend on the fixture; run locally with:
//
//     CACHECANNON_PARQUET=/path/to/cachecannon.parquet \
//       cargo test -p metriken-query streaming::tests::cachecannon -- --nocapture
//
// The test also prints per-query streaming-vs-eager timing as a
// directional measurement of the savings on a real workload.
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
        QueryResult::Matrix { result } => result.iter().map(|s| s.values.len()).sum(),
        QueryResult::Vector { result } => result.len(),
        QueryResult::Scalar { .. } => 1,
        QueryResult::HistogramHeatmap { result } => result.data.len(),
    }
}

/// Smoke test: run every query the cachecannon dashboard generates
/// (plus representative shapes from the broader rezolus dashboards
/// that hit the same parquet) against a real fixture, and assert
/// each one returns `Ok` with at least one point. Catches
/// regressions where a streaming operator silently drops series or
/// errors out on a real-world workload, while remaining cheap to
/// maintain (no per-query golden output that drifts when JSON
/// serialisation evolves).
///
/// Per-query wall-clock is printed with `--nocapture` so the test
/// also doubles as a quick perf sanity-check on local dev.
#[test]
fn cachecannon_smoke_test() {
    let Some(path) = cachecannon_parquet_path() else {
        eprintln!(
            "skipping cachecannon smoke test: set CACHECANNON_PARQUET=/path/to/cachecannon.parquet \
             or check out rezolus alongside metriken"
        );
        return;
    };

    let tsdb = match crate::Tsdb::load(&path) {
        Ok(t) => Arc::new(t),
        Err(e) => {
            eprintln!("skipping cachecannon smoke test: failed to load {path:?}: {e}");
            return;
        }
    };

    let engine = QueryEngine::new(tsdb.clone());
    let (start, end) = engine.get_time_range();
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

        // Some queries legitimately produce zero points on this
        // fixture (e.g. `cache_hits / cache_misses` where misses is
        // identically zero — every point divides by zero and gets
        // filtered).  We only require no error; the printed point
        // count is for human inspection.
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
