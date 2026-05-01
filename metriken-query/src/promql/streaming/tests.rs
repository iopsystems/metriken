//! Parity tests for the streaming prototype.
//!
//! Each streaming pipeline is run alongside an equivalent eager
//! `query_range` invocation against the same TSDB; the results are
//! sorted and compared pointwise. Any divergence — different number
//! of series, missing labels, mismatched values — fails the test.
//!
//! These tests pin down the contract the rest of the engine must
//! preserve when the migration proceeds operator-by-operator.

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
    let streaming = sort_by_name(collect_to_matrix(stream, "cgroup_cpu_usage"));

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
    let streaming = sort_by_name(collect_to_matrix(summed, "cgroup_cpu_usage"));

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
