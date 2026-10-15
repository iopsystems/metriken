use std::collections::HashSet;
use std::sync::Arc;

use crate::labels::Labels;
use crate::memory::Memory;
use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::types::{Counter, Gauge, Histogram, HistogramSnapshot};
use crate::{DataSource, QueryOptions, RateMode};

/// Counter climbing 100/s whose samples land at `.374s` past each second —
/// a phase deliberately offset from any round-second evaluation grid.
fn create_phase_offset_source() -> Memory {
    let mut source = Memory::new(1000);
    let phase = 374_000_000u64; // 0.374s
    source.add_counter(
        "c",
        Counter {
            labels: Labels::default(),
            timestamps: (0u64..6).map(|s| s * 1_000_000_000 + phase).collect(),
            values: (0u64..6).map(|s| s * 100).collect(),
            windows: None,
        },
    );
    source
}

fn make_labels(pairs: &[(&str, &str)]) -> Labels {
    let mut l = Labels::default();
    for (k, v) in pairs {
        l.inner.insert(k.to_string(), v.to_string());
    }
    l
}

fn create_empty_source() -> Memory {
    Memory::new(1000)
}

/// Create a Memory with cgroup_cpu_usage counter data.
/// Creates 3 cgroups with different counter values across 3 time steps.
fn create_cgroup_source() -> Memory {
    let mut source = Memory::new(1000);
    let cgroups = [
        ("/system.slice/foo.service", 100u64),
        ("/system.slice/bar.service", 200u64),
        ("/system.slice/baz.service", 300u64),
    ];
    for (name, base_val) in &cgroups {
        let labels = make_labels(&[("name", name), ("state", "user")]);
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
    source
}

fn count_matrix_series(result: &QueryResult) -> usize {
    match result {
        QueryResult::Matrix { result } => result.len(),
        _ => 0,
    }
}

fn get_matrix_series_names(result: &QueryResult) -> Vec<String> {
    match result {
        QueryResult::Matrix { result } => result
            .iter()
            .filter_map(|s| s.metric.get("name").cloned())
            .collect(),
        _ => Vec::new(),
    }
}

#[test]
fn test_query_engine_creation() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);
    // Engine creation itself is the test; an empty source returns MetricNotFound.
    let result = engine.query("nonexistent_metric", None);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        other => panic!("Expected MetricNotFound, got {other:?}"),
    }
}

/// A function that reads one kind, given a name the source holds only as
/// another, says so rather than answering empty or reporting it missing.
#[test]
fn a_name_held_as_another_kind_is_unsupported() {
    let unsupported = |engine: &QueryEngine, q: &str, kind: &str| match engine
        .query_range(q, 1000.0, 1004.0, 1.0)
    {
        Err(QueryError::Unsupported(msg)) => assert!(msg.contains(kind), "{q}: {msg}"),
        other => panic!("{q}: expected Unsupported, got {other:?}"),
    };
    let gauges = QueryEngine::new(Arc::new(create_gauge_source()));
    unsupported(&gauges, "rate(test_gauge[2s])", "gauge");
    unsupported(&gauges, "histogram_mean(test_gauge)", "gauge");
    let counters = QueryEngine::new(Arc::new(create_rate_source()));
    unsupported(&counters, "avg_over_time(test_counter[2s])", "counter");
    unsupported(&counters, "histogram_heatmap(test_counter)", "counter");
    let histograms = QueryEngine::new(Arc::new(create_hist_source(&[0, 5, 10])));
    unsupported(&histograms, "req_latency", "histogram");
    unsupported(&histograms, "rate(req_latency[2s])", "histogram");
}

/// A missing name is reported whichever operand it is, even beside an
/// operand that matched nothing.
#[test]
fn a_missing_name_is_reported_from_either_operand() {
    let engine = QueryEngine::new(Arc::new(create_rate_source()));
    for q in [
        "rate(test_counter{x=\"y\"}[2s]) / rate(typo_counter[2s])",
        "rate(typo_counter[2s]) + rate(test_counter{x=\"y\"}[2s])",
    ] {
        match engine.query_range(q, 1000.0, 1004.0, 1.0) {
            Err(QueryError::MetricNotFound(name)) => assert_eq!(name, "typo_counter", "{q}"),
            other => panic!("{q}: expected MetricNotFound, got {other:?}"),
        }
    }
}

/// A metric the source holds gives an empty matrix where nothing matches;
/// only a name the source does not hold is `MetricNotFound`.
#[test]
fn a_held_metric_with_nothing_in_range_is_empty_not_missing() {
    let engine = QueryEngine::new(Arc::new(create_rate_source()));
    let empty = |q: &str, start: f64, end: f64| match engine.query_range(q, start, end, 1.0) {
        Ok(QueryResult::Matrix { result }) => assert!(result.is_empty(), "{q}: {result:?}"),
        other => panic!("{q}: expected an empty matrix, got {other:?}"),
    };
    empty("rate(test_counter[2s])", 2000.0, 2004.0);
    empty("sum(rate(test_counter[2s]))", 2000.0, 2004.0);
    empty(
        "rate(test_counter{no_such_label=\"x\"}[2s])",
        1000.0,
        1004.0,
    );
    match engine.query_range("rate(absent_metric[2s])", 1000.0, 1004.0, 1.0) {
        Err(QueryError::MetricNotFound(name)) => assert_eq!(name, "absent_metric"),
        other => panic!("expected MetricNotFound, got {other:?}"),
    }
}

/// A bare selector naming a counter says to use rate(), rather than
/// reporting a metric the source holds as missing.
#[test]
fn a_bare_counter_selector_names_the_fix() {
    let engine = QueryEngine::new(Arc::new(create_rate_source()));
    match engine.query_range("test_counter", 1000.0, 1004.0, 1.0) {
        Err(QueryError::Unsupported(msg)) => {
            assert!(
                msg.contains("is a counter") && msg.contains("rate("),
                "{msg}"
            )
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
    assert!(engine
        .query_range("rate(test_counter[2s])", 1000.0, 1004.0, 1.0)
        .is_ok());
    match engine.query_range("absent_metric", 1000.0, 1004.0, 1.0) {
        Err(QueryError::MetricNotFound(_)) => {}
        other => panic!("expected MetricNotFound, got {other:?}"),
    }
}

#[test]
fn test_simple_rate_query_parsing() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let result = engine.query("rate(cpu_cycles[5m])", None);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty source"),
    }
}

#[test]
fn test_simple_metric_query() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let result = engine.query("cpu_cores", None);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty source"),
    }
}

#[test]
fn test_sum_rate_query() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let result = engine.query("sum(rate(network_rx_bytes[1m]))", None);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty source"),
    }
}

#[test]
fn test_range_query_delegation() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let result = engine.query_range("cpu_cores", 0.0, 3600.0, 60.0);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty source"),
    }
}

#[test]
fn test_label_filtering_in_rate_query() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let result = engine.query("rate(network_bytes{direction=\"transmit\"}[5m])", None);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty source"),
    }
}

#[test]
fn test_label_filtering_in_sum_rate_query() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let result = engine.query("sum(rate(blockio_bytes{op=\"read\"}[1m]))", None);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty source"),
    }
}

#[test]
fn test_simple_metric_with_labels() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let result = engine.query("cpu_cores{cpu=\"0\"}", None);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty source"),
    }
}

#[test]
fn test_metric_selector_parsing() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let _result = engine.query("metric_name{label1=\"value1\",label2=\"value2\"}", None);
    let _result = engine.query("metric_name{label1='value1',label2='value2'}", None);
    let _result = engine.query("metric_name{label1 = \"value 1\", label2= 'value 2'}", None);
}

#[test]
fn test_histogram_quantile_parsing() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let result = engine.query_range(
        "histogram_quantile(0.95, tcp_packet_latency)",
        0.0,
        3600.0,
        60.0,
    );
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty source"),
    }
}

// -- Label filtering tests with actual data --

#[test]
fn test_exact_match_filters_correctly() {
    let source = Arc::new(create_cgroup_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            r#"irate(cgroup_cpu_usage{name="/system.slice/foo.service"}[5s])"#,
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();

    let names = get_matrix_series_names(&result);
    assert_eq!(names.len(), 1, "exact match should return 1 series");
    assert_eq!(names[0], "/system.slice/foo.service");
}

#[test]
fn test_regex_match_filters_correctly() {
    let source = Arc::new(create_cgroup_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            r#"irate(cgroup_cpu_usage{name=~"/system.slice/foo.service"}[5s])"#,
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();

    let names = get_matrix_series_names(&result);
    assert_eq!(names.len(), 1, "=~ match should return 1 series");
    assert_eq!(names[0], "/system.slice/foo.service");
}

#[test]
fn test_regex_alternation_filters_correctly() {
    let source = Arc::new(create_cgroup_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            r#"irate(cgroup_cpu_usage{name=~"(/system.slice/foo.service|/system.slice/bar.service)"}[5s])"#,
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();

    let names = get_matrix_series_names(&result);
    assert_eq!(names.len(), 2, "=~ alternation should return 2 series");
    assert!(names.contains(&"/system.slice/foo.service".to_string()));
    assert!(names.contains(&"/system.slice/bar.service".to_string()));
}

#[test]
fn test_negative_exact_match_excludes() {
    let source = Arc::new(create_cgroup_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            r#"irate(cgroup_cpu_usage{name!="/system.slice/foo.service"}[5s])"#,
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();

    let names = get_matrix_series_names(&result);
    assert_eq!(names.len(), 2, "!= should exclude 1 of 3 series");
    assert!(!names.contains(&"/system.slice/foo.service".to_string()));
    assert!(names.contains(&"/system.slice/bar.service".to_string()));
    assert!(names.contains(&"/system.slice/baz.service".to_string()));
}

#[test]
fn test_negative_regex_excludes() {
    let source = Arc::new(create_cgroup_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            r#"irate(cgroup_cpu_usage{name!~"(/system.slice/foo.service|/system.slice/bar.service)"}[5s])"#,
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();

    let names = get_matrix_series_names(&result);
    assert_eq!(names.len(), 1, "!~ should exclude 2 of 3 series");
    assert_eq!(names[0], "/system.slice/baz.service");
}

#[test]
fn test_sum_by_name_with_regex_match() {
    let source = Arc::new(create_cgroup_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            r#"sum by (name) (irate(cgroup_cpu_usage{name=~"(/system.slice/foo.service|/system.slice/bar.service)"}[5s]))"#,
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();

    let names = get_matrix_series_names(&result);
    assert_eq!(
        names.len(),
        2,
        "sum by (name) with =~ should return 2 series"
    );
    assert!(names.contains(&"/system.slice/foo.service".to_string()));
    assert!(names.contains(&"/system.slice/bar.service".to_string()));
}

#[test]
fn test_sum_with_negative_match_excludes() {
    let source = Arc::new(create_cgroup_source());
    let engine = QueryEngine::new(source.clone());

    let total = engine
        .query_range(r#"sum(irate(cgroup_cpu_usage[5s]))"#, 1000.0, 1003.0, 1.0)
        .unwrap();

    let excluded = engine
        .query_range(
            r#"sum(irate(cgroup_cpu_usage{name!="/system.slice/foo.service"}[5s]))"#,
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();

    assert_eq!(count_matrix_series(&total), 1);
    assert_eq!(count_matrix_series(&excluded), 1);
}

// -- Windowed rate / irate / avg_over_time tests --

/// Single counter series: values 0, 100, 200, 300, 400 at t=1000..1004.
fn create_rate_source() -> Memory {
    let mut source = Memory::new(1000);
    source.add_counter(
        "test_counter",
        Counter {
            labels: Labels::default(),
            timestamps: (0u64..5).map(|s| (1000 + s) * 1_000_000_000).collect(),
            values: (0u64..5).map(|s| s * 100).collect(),
            windows: None,
        },
    );
    source
}

/// Counter with a reset: 100, 200, 300, 50, 150 at t=1000..1004.
fn create_counter_reset_source() -> Memory {
    let mut source = Memory::new(1000);
    source.add_counter(
        "reset_counter",
        Counter {
            labels: Labels::default(),
            timestamps: (0u64..5).map(|s| (1000 + s) * 1_000_000_000).collect(),
            values: vec![100, 200, 300, 50, 150],
            windows: None,
        },
    );
    source
}

/// Gauge: values 10, 20, 30, 40, 50 at t=1000..1004.
fn create_gauge_source() -> Memory {
    let mut source = Memory::new(1000);
    source.add_gauge(
        "test_gauge",
        Gauge {
            labels: Labels::default(),
            timestamps: (0u64..5).map(|s| (1000 + s) * 1_000_000_000).collect(),
            values: (1i64..=5).map(|v| v * 10).collect(),
            windows: None,
        },
    );
    source
}

/// Two labeled gauge series: host_a (10+step) and host_b (20+step).
fn create_labeled_gauge_source() -> Memory {
    let mut source = Memory::new(1000);
    for (host, base_val) in [("host_a", 10i64), ("host_b", 20i64)] {
        let labels = make_labels(&[("host", host)]);
        source.add_gauge(
            "labeled_gauge",
            Gauge {
                labels,
                timestamps: (0u64..3).map(|s| (1000 + s) * 1_000_000_000).collect(),
                values: (0i64..3).map(|s| base_val + s).collect(),
                windows: None,
            },
        );
    }
    source
}

fn get_matrix_values(result: &QueryResult) -> Vec<Vec<(f64, f64)>> {
    match result {
        QueryResult::Matrix { result } => result.iter().map(|s| s.values.clone()).collect(),
        _ => Vec::new(),
    }
}

#[test]
fn test_windowed_rate_basic() {
    let source = Arc::new(create_rate_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("rate(test_counter[3s])", 1001.0, 1004.0, 1.0)
        .unwrap();

    assert_eq!(count_matrix_series(&result), 1);

    let all_values = get_matrix_values(&result);
    assert!(!all_values[0].is_empty());

    for (_ts, rate) in &all_values[0] {
        assert!(
            (*rate - 100.0).abs() < 1e-6,
            "Expected rate ~100.0, got {}",
            rate
        );
    }
}

#[test]
fn test_grid_rate_counter_reset() {
    // reset_counter [100,200,300,50,150] at 1000..1004 s: reset-adjusted
    // increments are 100,100,50,100 over 1 s each. A 5 s window at 1004
    // starts at the first sample (1000): 350 over 4 s. A 1 s window is the
    // 50→150 delta.
    let source = Arc::new(create_counter_reset_source());
    let engine = QueryEngine::new(source);

    for (query, expected) in [
        ("rate(reset_counter[5s])", 87.5),
        ("rate(reset_counter[2s])", 75.0),
        ("rate(reset_counter[1s])", 100.0),
    ] {
        let result = engine.query_range(query, 1004.0, 1004.0, 1.0).unwrap();
        let all_values = get_matrix_values(&result);
        assert_eq!(all_values.len(), 1);
        assert_eq!(all_values[0].len(), 1);

        let (ts, rate) = all_values[0][0];
        assert!((ts - 1004.0).abs() < 1e-9, "grid timestamp {ts}");
        assert!(
            (rate - expected).abs() < 1e-6,
            "{query}: expected {expected}, got {rate}"
        );
    }
}

#[test]
fn test_windowed_irate_basic() {
    let source = Arc::new(create_rate_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("irate(test_counter[5s])", 1004.0, 1004.0, 1.0)
        .unwrap();

    let all_values = get_matrix_values(&result);
    assert_eq!(all_values.len(), 1);
    assert_eq!(all_values[0].len(), 1);

    let rate = all_values[0][0].1;
    assert!(
        (rate - 100.0).abs() < 1e-6,
        "Expected irate 100.0, got {}",
        rate
    );
}

#[test]
fn test_irate_is_rate_over_one_step_with_reset() {
    // `irate` ignores its `[range]` and is the rate over one step, across a
    // counter reset too: `rate` over a range of one step.
    let source = Arc::new(create_counter_reset_source());
    let engine = QueryEngine::new(source);

    let rate = get_matrix_values(
        &engine
            .query_range("rate(reset_counter[1s])", 1001.0, 1004.0, 1.0)
            .unwrap(),
    );
    let irate = get_matrix_values(
        &engine
            .query_range("irate(reset_counter[5s])", 1001.0, 1004.0, 1.0)
            .unwrap(),
    );
    let wide = get_matrix_values(
        &engine
            .query_range("rate(reset_counter[5s])", 1001.0, 1004.0, 1.0)
            .unwrap(),
    );

    assert!(!rate.is_empty() && !rate[0].is_empty(), "expected points");
    assert_eq!(rate, irate, "irate must be rate over one step");
    assert_ne!(wide, irate, "rate over 5 s must average over more");
}

#[test]
fn test_default_mode_snaps_grid_to_round_timestamps() {
    // Samples land at .374s; the viewer passes start = first-sample time. The
    // default mode (Grid) must emit points on round-second grid boundaries, not
    // at the sample phase — this is the phase-offset bug fix.
    let engine = QueryEngine::new(Arc::new(create_phase_offset_source()));
    let result = engine
        .query_range("rate(c[1s])", 0.374, 5.374, 1.0)
        .unwrap();
    let vals = get_matrix_values(&result);
    assert_eq!(vals.len(), 1);
    assert!(!vals[0].is_empty(), "expected grid points");
    for (ts, v) in &vals[0] {
        assert!(
            (ts - ts.round()).abs() < 1e-6,
            "timestamp {ts} is not on a round-second grid"
        );
        assert!((v - 100.0).abs() < 1e-6, "constant-rate value {v}");
    }
    // Explicit Grid opts match the default exactly.
    let explicit = get_matrix_values(
        &engine
            .query_range_opts(
                "rate(c[1s])",
                0.374,
                5.374,
                1.0,
                &QueryOptions::with_rate_mode(RateMode::Grid),
            )
            .unwrap(),
    );
    assert_eq!(vals, explicit, "default must equal RateMode::Grid");
}

/// A fast counter divided by a slow, IRREGULARLY sampled gauge, evaluated at
/// the slow side's own reading times.
///
/// This is the cross-cadence case the option exists for. On the uniform grid,
/// most points land where the slow gauge has no reading, so its value is held
/// forward and the quotient is part measurement, part staleness. Worse, the
/// slow readings are not evenly spaced — 2 s then 4 s apart here, mirroring the
/// 30 s / 60 s spacing measured on a real recording — so no step and no phase
/// can make a uniform grid sit on them — the readings here are deliberately
/// off-grid (x.3 s) as well as unevenly spaced, so a grid CANNOT reproduce
/// this result by coincidence.
///
/// Evaluating at the supplied timestamps puts every point where BOTH sides
/// genuinely have data.
#[test]
fn eval_timestamps_place_points_on_a_slow_irregular_sources_readings() {
    const S: u64 = 1_000_000_000;
    let mut source = Memory::new(1000);
    // Fast counter: every second, +100/s.
    source.add_counter(
        "c",
        Counter {
            labels: Labels::default(),
            timestamps: (0u64..=10).map(|s| s * S).collect(),
            values: (0u64..=10).map(|s| s * 100).collect(),
            windows: None,
        },
    );
    // Slow gauge, unevenly sampled AND off-grid: 0.3 s, 2.3 s, 6.3 s only.
    source.add_gauge(
        "g",
        Gauge {
            labels: Labels::default(),
            timestamps: vec![S / 10 * 3, 2 * S + S / 10 * 3, 6 * S + S / 10 * 3],
            values: vec![2, 4, 5],
            windows: None,
        },
    );

    let engine = QueryEngine::new(Arc::new(source));
    let points: Arc<[u64]> = vec![S / 10 * 3, 2 * S + S / 10 * 3, 6 * S + S / 10 * 3].into();
    let result = engine
        .query_range_opts(
            "rate(c[1s]) / g",
            0.0,
            10.0,
            1.0,
            &QueryOptions::with_rate_mode(RateMode::Grid)
                .with_eval_timestamps(Some(points.clone())),
        )
        .unwrap();

    let vals = get_matrix_values(&result);
    assert_eq!(vals.len(), 1, "one series expected");
    let got: Vec<(f64, f64)> = vals[0].clone();

    // Points sit on the gauge's real readings, never between them. A `rate`
    // is over its `[range]`, so the first supplied timestamp has a value too:
    // at 0.3 s the window starts at the first sample, 0 s.
    let times: Vec<f64> = got.iter().map(|(t, _)| *t).collect();
    assert_eq!(times, vec![0.3, 2.3, 6.3], "got {got:?}");

    // 100/s over a gauge reading taken at that same instant: 100/2, 100/4,
    // then 100/5. A held-forward gauge would have divided by the stale
    // reading instead.
    assert!(
        (got[0].1 - 50.0).abs() < 1e-6,
        "at 0.3s expected 50, got {got:?}"
    );
    assert!(
        (got[1].1 - 25.0).abs() < 1e-6,
        "at 2.3s expected 25, got {got:?}"
    );
    assert!(
        (got[2].1 - 20.0).abs() < 1e-6,
        "at 6.3s expected 20, got {got:?}"
    );
}

#[test]
fn test_raw_mode_emits_real_sample_timestamps() {
    // Raw places points at the actual (phase-offset) sample timestamps, one per
    // consecutive sample pair — the honest sample cadence, un-snapped.
    let engine = QueryEngine::new(Arc::new(create_phase_offset_source()));
    let result = engine
        .query_range_opts(
            "rate(c[1s])",
            0.374,
            5.374,
            1.0,
            &QueryOptions::with_rate_mode(RateMode::Raw),
        )
        .unwrap();
    let vals = get_matrix_values(&result);
    assert_eq!(vals.len(), 1);
    let times: Vec<f64> = vals[0].iter().map(|(t, _)| *t).collect();
    // Pairwise over samples at .374 → points at 1.374 … 5.374.
    let expected = [1.374, 2.374, 3.374, 4.374, 5.374];
    assert_eq!(times.len(), expected.len(), "got {times:?}");
    for (got, exp) in times.iter().zip(expected.iter()) {
        assert!((got - exp).abs() < 1e-6, "raw timestamp {got} vs {exp}");
    }
    for (_, v) in &vals[0] {
        assert!((v - 100.0).abs() < 1e-6, "raw value {v}");
    }
}

#[test]
fn test_avg_over_time_basic() {
    let source = Arc::new(create_gauge_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("avg_over_time(test_gauge[3s])", 1002.0, 1002.0, 1.0)
        .unwrap();

    let all_values = get_matrix_values(&result);
    assert_eq!(all_values.len(), 1);
    assert_eq!(all_values[0].len(), 1);

    let avg = all_values[0][0].1;
    assert!((avg - 20.0).abs() < 1e-6, "Expected avg 20.0, got {}", avg);
}

#[test]
fn test_avg_over_time_full_window() {
    let source = Arc::new(create_gauge_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("avg_over_time(test_gauge[5s])", 1004.0, 1004.0, 1.0)
        .unwrap();

    let all_values = get_matrix_values(&result);
    assert_eq!(all_values.len(), 1);
    let avg = all_values[0][0].1;
    assert!((avg - 30.0).abs() < 1e-6, "Expected avg 30.0, got {}", avg);
}

#[test]
fn test_avg_over_time_with_label_filter() {
    let source = Arc::new(create_labeled_gauge_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            r#"avg_over_time(labeled_gauge{host="host_a"}[3s])"#,
            1002.0,
            1002.0,
            1.0,
        )
        .unwrap();

    assert_eq!(count_matrix_series(&result), 1);
    let names: Vec<String> = match &result {
        QueryResult::Matrix { result } => result
            .iter()
            .filter_map(|s| s.metric.get("host").cloned())
            .collect(),
        _ => Vec::new(),
    };
    assert_eq!(names, vec!["host_a"]);
}

#[test]
fn test_avg_over_time_empty_tsdb() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let result = engine.query_range("avg_over_time(test_gauge[5m])", 0.0, 3600.0, 60.0);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty source"),
    }
}

#[test]
fn test_rate_parse_error_without_range() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let result = engine.query_range("rate(test_counter)", 0.0, 3600.0, 60.0);
    assert!(result.is_err());
}

#[test]
fn test_vector_selector_respects_coarse_step() {
    let source = Arc::new(create_gauge_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("test_gauge", 1000.0, 1004.0, 2.0)
        .unwrap();

    let all_values = get_matrix_values(&result);
    assert_eq!(all_values.len(), 1);
    assert_eq!(
        all_values[0].len(),
        3,
        "Expected 3 step-aligned points, got {}",
        all_values[0].len()
    );

    assert!((all_values[0][0].0 - 1000.0).abs() < 1e-6);
    assert!((all_values[0][1].0 - 1002.0).abs() < 1e-6);
    assert!((all_values[0][2].0 - 1004.0).abs() < 1e-6);

    assert!((all_values[0][0].1 - 10.0).abs() < 1e-6);
    assert!((all_values[0][1].1 - 30.0).abs() < 1e-6);
    assert!((all_values[0][2].1 - 50.0).abs() < 1e-6);
}

#[test]
fn test_vector_selector_preserves_all_points_when_step_equals_interval() {
    let source = Arc::new(create_gauge_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("test_gauge", 1000.0, 1004.0, 1.0)
        .unwrap();

    let all_values = get_matrix_values(&result);
    assert_eq!(all_values.len(), 1);
    assert_eq!(
        all_values[0].len(),
        5,
        "Expected all 5 raw points, got {}",
        all_values[0].len()
    );
}

/// Three gauge metrics for binary expression tests.
/// "mem_total" = 1000, "mem_available" = 800..400, "mem_reserved" = 50.
fn create_three_gauge_source() -> Memory {
    let mut source = Memory::new(1000);
    let ts: Vec<u64> = (0u64..5).map(|s| (1000 + s) * 1_000_000_000).collect();

    source.add_gauge(
        "mem_total",
        Gauge {
            labels: Labels::default(),
            timestamps: ts.clone(),
            values: vec![1000; 5],
            windows: None,
        },
    );
    source.add_gauge(
        "mem_available",
        Gauge {
            labels: Labels::default(),
            timestamps: ts.clone(),
            values: (0i64..5).map(|s| 800 - s * 100).collect(),
            windows: None,
        },
    );
    source.add_gauge(
        "mem_reserved",
        Gauge {
            labels: Labels::default(),
            timestamps: ts,
            values: vec![50; 5],
            windows: None,
        },
    );
    source
}

#[test]
fn test_compound_gauge_expression_respects_step() {
    let source = Arc::new(create_three_gauge_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("mem_total - mem_available", 1000.0, 1004.0, 2.0)
        .unwrap();

    let all_values = get_matrix_values(&result);
    assert_eq!(all_values.len(), 1, "should have 1 result series");
    assert_eq!(
        all_values[0].len(),
        3,
        "Expected 3 step-aligned points, got {}",
        all_values[0].len()
    );

    assert!((all_values[0][0].0 - 1000.0).abs() < 1e-6);
    assert!((all_values[0][0].1 - 200.0).abs() < 1e-6);

    assert!((all_values[0][1].0 - 1002.0).abs() < 1e-6);
    assert!((all_values[0][1].1 - 400.0).abs() < 1e-6);

    assert!((all_values[0][2].0 - 1004.0).abs() < 1e-6);
    assert!((all_values[0][2].1 - 600.0).abs() < 1e-6);
}

#[test]
fn test_triple_gauge_expression_respects_step() {
    let source = Arc::new(create_three_gauge_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            "mem_total - mem_available - mem_reserved",
            1000.0,
            1004.0,
            2.0,
        )
        .unwrap();

    let all_values = get_matrix_values(&result);
    assert_eq!(all_values.len(), 1, "should have 1 result series");
    assert_eq!(
        all_values[0].len(),
        3,
        "Expected 3 step-aligned points, got {}",
        all_values[0].len()
    );

    assert!((all_values[0][0].0 - 1000.0).abs() < 1e-6);
    assert!((all_values[0][0].1 - 150.0).abs() < 1e-6);

    assert!((all_values[0][1].0 - 1002.0).abs() < 1e-6);
    assert!((all_values[0][1].1 - 350.0).abs() < 1e-6);

    assert!((all_values[0][2].0 - 1004.0).abs() < 1e-6);
    assert!((all_values[0][2].1 - 550.0).abs() < 1e-6);
}

/// Duplex-link source: tx_bytes{iface, direction} and link_bandwidth{iface}.
fn create_duplex_source() -> Memory {
    let mut source = Memory::new(1000);
    let ts: Vec<u64> = (0u64..3).map(|s| (1000 + s) * 1_000_000_000).collect();

    for (iface, tx_val) in [("eth0", 100i64), ("eth1", 200i64)] {
        let labels = make_labels(&[("iface", iface), ("direction", "tx")]);
        source.add_gauge(
            "tx_bytes",
            Gauge {
                labels,
                timestamps: ts.clone(),
                values: vec![tx_val; 3],
                windows: None,
            },
        );
    }

    for (iface, bw) in [("eth0", 1000i64), ("eth1", 2000i64)] {
        let labels = make_labels(&[("iface", iface)]);
        source.add_gauge(
            "link_bandwidth",
            Gauge {
                labels,
                timestamps: ts.clone(),
                values: vec![bw; 3],
                windows: None,
            },
        );
    }
    source
}

fn series_for_iface(result: &QueryResult, iface: &str) -> Option<Vec<(f64, f64)>> {
    match result {
        QueryResult::Matrix { result } => result
            .iter()
            .find(|s| s.metric.get("iface").map(|v| v.as_str()) == Some(iface))
            .map(|s| s.values.clone()),
        _ => None,
    }
}

#[test]
fn test_ignoring_matches_mismatched_labels() {
    let source = Arc::new(create_duplex_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            "tx_bytes / ignoring(direction, metric) link_bandwidth",
            1000.0,
            1002.0,
            1.0,
        )
        .unwrap();

    assert_eq!(
        count_matrix_series(&result),
        2,
        "expected one series per iface"
    );

    let eth0 = series_for_iface(&result, "eth0").expect("eth0 series");
    let eth1 = series_for_iface(&result, "eth1").expect("eth1 series");

    assert_eq!(eth0.len(), 3);
    for (_, v) in &eth0 {
        assert!((v - 0.1).abs() < 1e-9, "eth0: 100/1000 = 0.1, got {v}");
    }

    assert_eq!(eth1.len(), 3);
    for (_, v) in &eth1 {
        assert!((v - 0.1).abs() < 1e-9, "eth1: 200/2000 = 0.1, got {v}");
    }
}

#[test]
fn test_on_matches_shared_labels() {
    let source = Arc::new(create_duplex_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("tx_bytes / on(iface) link_bandwidth", 1000.0, 1002.0, 1.0)
        .unwrap();

    assert_eq!(count_matrix_series(&result), 2);

    let eth0 = series_for_iface(&result, "eth0").expect("eth0 series");
    let eth1 = series_for_iface(&result, "eth1").expect("eth1 series");

    for (_, v) in &eth0 {
        assert!((v - 0.1).abs() < 1e-9);
    }
    for (_, v) in &eth1 {
        assert!((v - 0.1).abs() < 1e-9);
    }
}

#[test]
fn test_mismatched_labels_without_modifier_do_not_match() {
    let source = Arc::new(create_duplex_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("tx_bytes / link_bandwidth", 1000.0, 1002.0, 1.0)
        .unwrap();

    assert_eq!(count_matrix_series(&result), 0);
}

// -- columns() resolver tests --

fn create_columns_source() -> Memory {
    let mut source = Memory::new(1000);
    let ts: Vec<u64> = (0u64..2).map(|s| (1000 + s) * 1_000_000_000).collect();

    for cpu in ["0", "1"] {
        let labels = make_labels(&[("cpu", cpu)]);
        source.add_counter(
            "cpu_cycles",
            Counter {
                labels,
                timestamps: ts.clone(),
                values: vec![0, 100],
                windows: None,
            },
        );
    }

    for host in ["a", "b"] {
        let labels = make_labels(&[("host", host)]);
        source.add_gauge(
            "cpu_temp",
            Gauge {
                labels,
                timestamps: ts.clone(),
                values: vec![50, 50],
                windows: None,
            },
        );
    }

    source.add_gauge(
        "cpu_cores",
        Gauge {
            labels: Labels::default(),
            timestamps: ts,
            values: vec![8, 8],
            windows: None,
        },
    );
    source
}

fn create_columns_histogram_source() -> Memory {
    let mut source = Memory::new(1000);
    let config = ::histogram::Config::new(4, 16).unwrap();

    for cpu in ["0", "1"] {
        let labels = make_labels(&[("cpu", cpu)]);
        source.add_histogram(
            "tcp_packet_latency",
            Histogram {
                labels,
                config,
                timestamps: vec![1_000_000_000_000, 1_001_000_000_000, 1_002_000_000_000],
                snapshots: vec![
                    HistogramSnapshot {
                        index: vec![],
                        count: vec![],
                    },
                    HistogramSnapshot {
                        index: vec![10],
                        count: vec![1u64],
                    },
                    HistogramSnapshot {
                        index: vec![10],
                        count: vec![2u64],
                    },
                ],
            },
        );
    }
    source
}

#[test]
fn test_columns_bare_gauge_selector_matches_all_labels() {
    let source = Arc::new(create_columns_source());
    let engine = QueryEngine::new(source);

    let cols = engine.columns("cpu_temp").unwrap();
    assert_eq!(cols, ["cpu_temp".to_string()].into_iter().collect());
}

#[test]
fn test_columns_bare_selector_with_label_filter() {
    let source = Arc::new(create_columns_source());
    let engine = QueryEngine::new(source);

    let cols = engine.columns(r#"cpu_temp{host="a"}"#).unwrap();
    assert_eq!(cols, ["cpu_temp".to_string()].into_iter().collect());
}

#[test]
fn test_columns_bare_selector_no_match_is_empty() {
    let source = Arc::new(create_columns_source());
    let engine = QueryEngine::new(source);

    let cols = engine.columns("nonexistent_metric").unwrap();
    assert!(cols.is_empty());
}

#[test]
fn test_columns_irate_resolves_inner_metric() {
    let source = Arc::new(create_columns_source());
    let engine = QueryEngine::new(source);

    let cols = engine.columns("irate(cpu_cycles[5s])").unwrap();
    assert_eq!(cols, ["cpu_cycles".to_string()].into_iter().collect());
}

#[test]
fn test_columns_rate_resolves_inner_metric() {
    let source = Arc::new(create_columns_source());
    let engine = QueryEngine::new(source);

    let cols = engine.columns("rate(cpu_cycles[5s])").unwrap();
    assert_eq!(cols, ["cpu_cycles".to_string()].into_iter().collect());
}

#[test]
fn test_columns_binary_op_unions_both_sides() {
    let source = Arc::new(create_columns_source());
    let engine = QueryEngine::new(source);

    let cols = engine
        .columns("sum(irate(cpu_cycles[5s])) / cpu_cores")
        .unwrap();
    let expected: HashSet<String> = ["cpu_cycles".to_string(), "cpu_cores".to_string()]
        .into_iter()
        .collect();
    assert_eq!(cols, expected);
}

#[test]
fn test_columns_aggregation_preserves_all_inner_columns() {
    let source = Arc::new(create_columns_source());
    let engine = QueryEngine::new(source);

    let cols = engine
        .columns("sum without (cpu) (irate(cpu_cycles[5s]))")
        .unwrap();
    assert_eq!(cols, ["cpu_cycles".to_string()].into_iter().collect());
}

#[test]
fn test_columns_name_regex_resolves_multiple_metrics() {
    let source = Arc::new(create_columns_source());
    let engine = QueryEngine::new(source);

    let cols = engine.columns(r#"{__name__=~"cpu_.*"}"#).unwrap();
    let expected: HashSet<String> = [
        "cpu_temp".to_string(),
        "cpu_cores".to_string(),
        "cpu_cycles".to_string(),
    ]
    .into_iter()
    .collect();
    assert_eq!(cols, expected);
}

#[test]
fn test_columns_histogram_quantile_resolves_buckets_column() {
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);

    let cols = engine
        .columns("histogram_quantile(0.5, tcp_packet_latency)")
        .unwrap();
    assert_eq!(
        cols,
        ["tcp_packet_latency:buckets".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn test_columns_histogram_quantiles_array_form() {
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);

    let cols = engine
        .columns("histogram_quantiles([0.5, 0.99], tcp_packet_latency)")
        .unwrap();
    assert_eq!(
        cols,
        ["tcp_packet_latency:buckets".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn test_columns_histogram_percentiles_alias() {
    // `histogram_percentiles(...)` is accepted as a synonym for
    // `histogram_quantiles(...)` — same array-of-quantiles form.
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);

    let cols = engine
        .columns("histogram_percentiles([0.5, 0.99], tcp_packet_latency)")
        .unwrap();
    assert_eq!(
        cols,
        ["tcp_packet_latency:buckets".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn test_columns_histogram_heatmap() {
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);

    let cols = engine
        .columns("histogram_heatmap(tcp_packet_latency)")
        .unwrap();
    assert_eq!(
        cols,
        ["tcp_packet_latency:buckets".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn test_columns_histogram_heatmap_with_label_filter() {
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);

    let cols = engine
        .columns(r#"histogram_heatmap(tcp_packet_latency{cpu="0"})"#)
        .unwrap();
    assert_eq!(
        cols,
        ["tcp_packet_latency:buckets".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn test_columns_histogram_mean_resolves_buckets_column() {
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);

    let cols = engine
        .columns("histogram_mean(tcp_packet_latency)")
        .unwrap();
    assert_eq!(
        cols,
        ["tcp_packet_latency:buckets".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn test_columns_histogram_count_with_label_filter() {
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);

    let cols = engine
        .columns(r#"histogram_count(tcp_packet_latency{cpu="0"})"#)
        .unwrap();
    assert_eq!(
        cols,
        ["tcp_packet_latency:buckets".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn test_columns_histogram_sum_resolves_buckets_column() {
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);

    let cols = engine.columns("histogram_sum(tcp_packet_latency)").unwrap();
    assert_eq!(
        cols,
        ["tcp_packet_latency:buckets".to_string()]
            .into_iter()
            .collect()
    );
}

// ─── Histogram execution tests ────────────────────────────────────────────────
//
// Raw cumulative observations: two label series (`cpu` 0/1), each with
// cumulative observation counts stored directly (e.g. [0, 5, 12] at
// t1000, t1001, t1002). All observations land in the exact bucket for
// value 10 (values < 16 are exact under grouping_power=4, so the bucket
// index equals the value). The streaming layer computes per-interval
// deltas: 5 then 7 per series; collapsed → 10 then 14. Mean is 10.0.

/// Build a raw cumulative `HistogramSnapshot` with `n` total observations
/// at bucket index 10.
fn hist_snap_cumulative(n: u64) -> HistogramSnapshot {
    if n == 0 {
        HistogramSnapshot {
            index: vec![],
            count: vec![],
        }
    } else {
        HistogramSnapshot {
            index: vec![10],
            count: vec![n],
        }
    }
}

/// Build a Memory with `req_latency` histograms storing raw cumulative
/// snapshot data. All observations land at bucket 10 (value 10 in a
/// histogram(4,16) → exact bucket at index 10). The streaming layer
/// computes deltas between consecutive snapshots, mirroring counters.
fn create_hist_source(cumulative_per_cpu: &[u64]) -> Memory {
    let mut source = Memory::new(1000);
    let config = ::histogram::Config::new(4, 16).unwrap();

    let n = cumulative_per_cpu.len();
    if n == 0 {
        return source;
    }

    for cpu in ["0", "1"] {
        let labels = make_labels(&[("cpu", cpu)]);
        let timestamps: Vec<u64> = (0..n).map(|i| (1000 + i as u64) * 1_000_000_000).collect();
        let snapshots: Vec<HistogramSnapshot> = cumulative_per_cpu
            .iter()
            .map(|&c| hist_snap_cumulative(c))
            .collect();
        source.add_histogram(
            "req_latency",
            Histogram {
                labels,
                config,
                timestamps,
                snapshots,
            },
        );
    }
    source
}

#[test]
fn test_histogram_count_collapses_series_and_sums_counts() {
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("histogram_count(req_latency)", 1000.0, 1003.0, 1.0)
        .unwrap();

    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    assert_eq!(result.len(), 1, "series collapse to one __name__ series");
    assert_eq!(
        result[0].metric.get("__name__").map(String::as_str),
        Some("req_latency")
    );
    assert!(
        !result[0].metric.contains_key("quantile"),
        "count has no quantile label"
    );
    let counts: Vec<f64> = result[0].values.iter().map(|(_, v)| *v).collect();
    assert_eq!(counts, vec![10.0, 14.0]);
}

#[test]
fn test_histogram_mean_is_bucket_weighted_midpoint() {
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("histogram_mean(req_latency)", 1000.0, 1003.0, 1.0)
        .unwrap();

    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    assert_eq!(result.len(), 1);
    assert_eq!(
        result[0].metric.get("__name__").map(String::as_str),
        Some("req_latency")
    );
    for (_, v) in &result[0].values {
        assert!((v - 10.0).abs() < 1e-9, "mean should be 10.0, got {v}");
    }
    assert!(!result[0].values.is_empty());
}

#[test]
fn test_histogram_sum_is_count_times_mean() {
    // value=10 is an exact bucket so mean=10.0; sum = 10.0 × count.
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("histogram_sum(req_latency)", 1000.0, 1003.0, 1.0)
        .unwrap();

    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    assert_eq!(result.len(), 1);
    assert_eq!(
        result[0].metric.get("__name__").map(String::as_str),
        Some("req_latency")
    );
    let sums: Vec<f64> = result[0].values.iter().map(|(_, v)| *v).collect();
    assert_eq!(sums, vec![100.0, 140.0]);
}

#[test]
fn test_histogram_sum_label_filter_selects_single_series() {
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            r#"histogram_sum(req_latency{cpu="0"})"#,
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();

    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    let sums: Vec<f64> = result[0].values.iter().map(|(_, v)| *v).collect();
    assert_eq!(sums, vec![50.0, 70.0]);
}

#[test]
fn test_histogram_sum_by_emits_per_group_series() {
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("histogram_sum by (cpu) (req_latency)", 1000.0, 1003.0, 1.0)
        .unwrap();

    let QueryResult::Matrix { mut result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    result.sort_by(|a, b| a.metric.get("cpu").cmp(&b.metric.get("cpu")));
    assert_eq!(result.len(), 2);
    for series in &result {
        let sums: Vec<f64> = series.values.iter().map(|(_, v)| *v).collect();
        assert_eq!(sums, vec![50.0, 70.0]);
    }
}

#[test]
fn test_histogram_sum_metric_not_found() {
    let source = Arc::new(create_hist_source(&[]));
    let engine = QueryEngine::new(source);

    let result = engine.query_range("histogram_sum(does_not_exist)", 1000.0, 1003.0, 1.0);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        other => panic!("expected MetricNotFound, got {other:?}"),
    }
}

#[test]
fn test_sum_wrapping_histogram_sum_matches_bare() {
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let bare = engine
        .query_range("histogram_sum(req_latency)", 1000.0, 1003.0, 1.0)
        .unwrap();
    let wrapped = engine
        .query_range("sum(histogram_sum(req_latency))", 1000.0, 1003.0, 1.0)
        .unwrap();

    let values_of = |r: QueryResult| -> Vec<f64> {
        let QueryResult::Matrix { result } = r else {
            panic!("expected Matrix");
        };
        assert_eq!(result.len(), 1);
        result[0].values.iter().map(|(_, v)| *v).collect()
    };
    assert_eq!(values_of(bare), values_of(wrapped));
}

#[test]
fn test_histogram_count_label_filter_selects_single_series() {
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            r#"histogram_count(req_latency{cpu="0"})"#,
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();

    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    let counts: Vec<f64> = result[0].values.iter().map(|(_, v)| *v).collect();
    assert_eq!(counts, vec![5.0, 7.0]);
}

#[test]
fn test_histogram_mean_metric_not_found() {
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let result = engine.query_range("histogram_mean(does_not_exist)", 1000.0, 1003.0, 1.0);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        other => panic!("expected MetricNotFound, got {other:?}"),
    }
}

#[test]
fn test_histogram_irate_steady_rate_is_constant() {
    let source = Arc::new(create_hist_source(&[0, 10, 20, 30, 40]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("histogram_irate(req_latency)", 1000.0, 1005.0, 1.0)
        .unwrap();

    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    assert_eq!(result.len(), 1, "irate collapses to one __name__ series");
    assert_eq!(
        result[0].metric.get("__name__").map(String::as_str),
        Some("req_latency")
    );

    let values: Vec<f64> = result[0].values.iter().map(|(_, v)| *v).collect();
    assert_eq!(values.len(), 3, "first observed delta yields null");
    for v in values {
        assert!(
            (v - 20.0).abs() < 1e-9,
            "steady rate should be 20.0, got {v}"
        );
    }
}

#[test]
fn test_histogram_irate_burst_then_idle_spikes_then_zero() {
    let source = Arc::new(create_hist_source(&[0, 0, 50, 50, 50]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("histogram_irate(req_latency)", 1000.0, 1005.0, 1.0)
        .unwrap();

    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    let values: Vec<f64> = result[0].values.iter().map(|(_, v)| *v).collect();
    assert_eq!(values.len(), 3);
    assert!((values[0] - 100.0).abs() < 1e-9, "spike, got {}", values[0]);
    assert_eq!(values[1], 0.0, "idle window emits zero rate");
    assert_eq!(values[2], 0.0, "idle window emits zero rate");
}

#[test]
fn test_histogram_irate_counter_drop_clamps_to_zero() {
    let source = Arc::new(create_hist_source(&[0, 10, 5, 15]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("histogram_irate(req_latency)", 1000.0, 1004.0, 1.0)
        .unwrap();

    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    let values: Vec<f64> = result[0].values.iter().map(|(_, v)| *v).collect();
    assert_eq!(values.len(), 2);
    assert_eq!(
        values[0], 0.0,
        "drop in cumulative count clamps to zero, got {}",
        values[0]
    );
    assert!(
        (values[1] - 20.0).abs() < 1e-9,
        "recovery after drop, got {}",
        values[1]
    );
}

#[test]
fn test_histogram_irate_first_step_is_null() {
    let source = Arc::new(create_hist_source(&[0, 5]));
    let engine = QueryEngine::new(source);

    let result = engine.query_range("histogram_irate(req_latency)", 1000.0, 1002.0, 1.0);
    match result {
        Ok(QueryResult::Matrix { result }) if result.is_empty() => {}
        other => panic!("expected an empty matrix (single delta → null), got {other:?}"),
    }
}

#[test]
fn test_histogram_irate_label_filter_selects_single_series() {
    let source = Arc::new(create_hist_source(&[0, 10, 20, 30]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            r#"histogram_irate(req_latency{cpu="0"})"#,
            1000.0,
            1004.0,
            1.0,
        )
        .unwrap();

    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    let values: Vec<f64> = result[0].values.iter().map(|(_, v)| *v).collect();
    assert_eq!(values.len(), 2);
    for v in values {
        assert!((v - 10.0).abs() < 1e-9, "expected 10.0, got {v}");
    }
}

#[test]
fn test_histogram_irate_by_groups_per_cpu() {
    let source = Arc::new(create_hist_source(&[0, 10, 20, 30, 40]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            "histogram_irate by (cpu) (req_latency)",
            1000.0,
            1005.0,
            1.0,
        )
        .unwrap();
    let QueryResult::Matrix { mut result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    result.sort_by(|a, b| a.metric.get("cpu").cmp(&b.metric.get("cpu")));
    assert_eq!(result.len(), 2, "one series per cpu");
    for series in &result {
        assert_eq!(
            series.metric.get("__name__").map(String::as_str),
            Some("req_latency")
        );
        assert!(series.metric.contains_key("cpu"));
        let values: Vec<f64> = series.values.iter().map(|(_, v)| *v).collect();
        assert_eq!(values.len(), 3, "first delta is null");
        for v in values {
            assert!((v - 10.0).abs() < 1e-9, "expected 10.0 per cpu, got {v}");
        }
    }
    assert_eq!(result[0].metric.get("cpu").map(String::as_str), Some("0"));
    assert_eq!(result[1].metric.get("cpu").map(String::as_str), Some("1"));
}

#[test]
fn test_histogram_irate_without_drops_named_label() {
    let source = Arc::new(create_hist_source(&[0, 10, 20, 30, 40]));
    let engine = QueryEngine::new(source);

    let bare = engine
        .query_range("histogram_irate(req_latency)", 1000.0, 1005.0, 1.0)
        .unwrap();
    let without = engine
        .query_range(
            "histogram_irate without (cpu) (req_latency)",
            1000.0,
            1005.0,
            1.0,
        )
        .unwrap();

    let values_of = |r: &QueryResult| -> Vec<f64> {
        let QueryResult::Matrix { result } = r else {
            panic!("expected Matrix");
        };
        assert_eq!(result.len(), 1);
        result[0].values.iter().map(|(_, v)| *v).collect()
    };
    assert_eq!(values_of(&bare), values_of(&without));
}

#[test]
fn test_histogram_count_by_emits_per_group_series() {
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range(
            "histogram_count by (cpu) (req_latency)",
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();
    let QueryResult::Matrix { mut result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    result.sort_by(|a, b| a.metric.get("cpu").cmp(&b.metric.get("cpu")));
    assert_eq!(result.len(), 2);
    let counts_0: Vec<f64> = result[0].values.iter().map(|(_, v)| *v).collect();
    let counts_1: Vec<f64> = result[1].values.iter().map(|(_, v)| *v).collect();
    assert_eq!(counts_0, vec![5.0, 7.0]);
    assert_eq!(counts_1, vec![5.0, 7.0]);
}

#[test]
fn test_histogram_mean_by_preserves_mean_per_group() {
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("histogram_mean by (cpu) (req_latency)", 1000.0, 1003.0, 1.0)
        .unwrap();
    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix, got {result:?}");
    };
    assert_eq!(result.len(), 2);
    for series in &result {
        for (_, v) in &series.values {
            assert!((v - 10.0).abs() < 1e-9);
        }
    }
}

#[test]
fn test_histogram_irate_rejects_invalid_label_in_grouping() {
    let source = Arc::new(create_hist_source(&[0, 10, 20]));
    let engine = QueryEngine::new(source);

    let result = engine.query_range(
        r#"histogram_irate by ("cpu") (req_latency)"#,
        1000.0,
        1003.0,
        1.0,
    );
    match result {
        Err(QueryError::ParseError(msg)) => {
            assert!(msg.contains("invalid label name"), "got: {msg}");
        }
        other => panic!("expected ParseError, got {other:?}"),
    }
}

#[test]
fn test_sum_wrapping_histogram_irate_matches_bare() {
    let source = Arc::new(create_hist_source(&[0, 10, 20, 30, 40]));
    let engine = QueryEngine::new(source);

    let bare = engine
        .query_range("histogram_irate(req_latency)", 1000.0, 1005.0, 1.0)
        .unwrap();
    let wrapped = engine
        .query_range("sum(histogram_irate(req_latency))", 1000.0, 1005.0, 1.0)
        .unwrap();

    let values_of = |r: &QueryResult| -> Vec<f64> {
        let QueryResult::Matrix { result } = r else {
            panic!("expected Matrix");
        };
        assert_eq!(result.len(), 1);
        result[0].values.iter().map(|(_, v)| *v).collect()
    };
    assert_eq!(values_of(&bare), values_of(&wrapped));
}

#[test]
fn test_sum_by_wrapping_histogram_irate_matches_native_grouping() {
    let source = Arc::new(create_hist_source(&[0, 10, 20, 30, 40]));
    let engine = QueryEngine::new(source);

    let native = engine
        .query_range(
            "histogram_irate by (cpu) (req_latency)",
            1000.0,
            1005.0,
            1.0,
        )
        .unwrap();
    let wrapped = engine
        .query_range(
            "sum by (cpu) (histogram_irate(req_latency))",
            1000.0,
            1005.0,
            1.0,
        )
        .unwrap();

    let sorted_pairs = |r: QueryResult| -> Vec<(String, Vec<f64>)> {
        let QueryResult::Matrix { mut result } = r else {
            panic!("expected Matrix");
        };
        result.sort_by(|a, b| a.metric.get("cpu").cmp(&b.metric.get("cpu")));
        result
            .into_iter()
            .map(|s| {
                (
                    s.metric.get("cpu").cloned().unwrap_or_default(),
                    s.values.iter().map(|(_, v)| *v).collect(),
                )
            })
            .collect()
    };
    assert_eq!(sorted_pairs(native), sorted_pairs(wrapped));
}

#[test]
fn test_sum_without_wrapping_histogram_irate_matches_native_grouping() {
    let source = Arc::new(create_hist_source(&[0, 10, 20, 30, 40]));
    let engine = QueryEngine::new(source);

    let native = engine
        .query_range(
            "histogram_irate without (cpu) (req_latency)",
            1000.0,
            1005.0,
            1.0,
        )
        .unwrap();
    let wrapped = engine
        .query_range(
            "sum without (cpu) (histogram_irate(req_latency))",
            1000.0,
            1005.0,
            1.0,
        )
        .unwrap();

    let values_of = |r: QueryResult| -> Vec<f64> {
        let QueryResult::Matrix { result } = r else {
            panic!("expected Matrix");
        };
        assert_eq!(result.len(), 1);
        result[0].values.iter().map(|(_, v)| *v).collect()
    };
    assert_eq!(values_of(native), values_of(wrapped));
}

#[test]
fn test_sum_wrapping_histogram_count_matches_bare() {
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let bare = engine
        .query_range("histogram_count(req_latency)", 1000.0, 1003.0, 1.0)
        .unwrap();
    let wrapped = engine
        .query_range("sum(histogram_count(req_latency))", 1000.0, 1003.0, 1.0)
        .unwrap();

    let values_of = |r: QueryResult| -> Vec<f64> {
        let QueryResult::Matrix { result } = r else {
            panic!("expected Matrix");
        };
        assert_eq!(result.len(), 1);
        result[0].values.iter().map(|(_, v)| *v).collect()
    };
    assert_eq!(values_of(bare), values_of(wrapped));
}

#[test]
fn test_sum_by_wrapping_histogram_mean_matches_native_grouping() {
    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let engine = QueryEngine::new(source);

    let native = engine
        .query_range("histogram_mean by (cpu) (req_latency)", 1000.0, 1003.0, 1.0)
        .unwrap();
    let wrapped = engine
        .query_range(
            "sum by (cpu) (histogram_mean(req_latency))",
            1000.0,
            1003.0,
            1.0,
        )
        .unwrap();

    let sorted_pairs = |r: QueryResult| -> Vec<(String, Vec<f64>)> {
        let QueryResult::Matrix { mut result } = r else {
            panic!("expected Matrix");
        };
        result.sort_by(|a, b| a.metric.get("cpu").cmp(&b.metric.get("cpu")));
        result
            .into_iter()
            .map(|s| {
                (
                    s.metric.get("cpu").cloned().unwrap_or_default(),
                    s.values.iter().map(|(_, v)| *v).collect(),
                )
            })
            .collect()
    };
    assert_eq!(sorted_pairs(native), sorted_pairs(wrapped));
}

#[test]
fn test_sum_wrapping_histogram_irate_with_label_matcher() {
    let source = Arc::new(create_hist_source(&[0, 10, 20, 30, 40]));
    let engine = QueryEngine::new(source);

    let bare = engine
        .query_range(
            r#"histogram_irate(req_latency{cpu="0"})"#,
            1000.0,
            1005.0,
            1.0,
        )
        .unwrap();
    let wrapped = engine
        .query_range(
            r#"sum(histogram_irate(req_latency{cpu="0"}))"#,
            1000.0,
            1005.0,
            1.0,
        )
        .unwrap();

    let values_of = |r: &QueryResult| -> Vec<f64> {
        let QueryResult::Matrix { result } = r else {
            panic!("expected Matrix");
        };
        assert_eq!(result.len(), 1);
        result[0].values.iter().map(|(_, v)| *v).collect()
    };
    assert_eq!(values_of(&bare), values_of(&wrapped));
}

#[test]
fn test_histogram_irate_rejects_stride_argument() {
    let source = Arc::new(create_hist_source(&[0, 10, 20]));
    let engine = QueryEngine::new(source);

    let result = engine.query_range("histogram_irate(req_latency, 5)", 1000.0, 1003.0, 1.0);
    match result {
        Err(QueryError::ParseError(msg)) => {
            assert!(msg.contains("stride") || msg.contains("window"));
        }
        other => panic!("expected ParseError, got {other:?}"),
    }
}

#[test]
fn test_columns_histogram_irate_resolves_buckets_column() {
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);

    let cols = engine
        .columns("histogram_irate(tcp_packet_latency)")
        .unwrap();
    assert_eq!(
        cols,
        ["tcp_packet_latency:buckets".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn test_columns_resolves_histogram_with_by_grouping() {
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);

    let cols = engine
        .columns("histogram_irate by (cpu) (tcp_packet_latency)")
        .unwrap();
    assert_eq!(
        cols,
        ["tcp_packet_latency:buckets".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn test_columns_returns_parse_error_for_invalid_syntax() {
    let source = Arc::new(create_columns_source());
    let engine = QueryEngine::new(source);

    let result = engine.columns("foo[[");
    match result {
        Err(QueryError::ParseError(_)) => {}
        other => panic!("expected ParseError, got {other:?}"),
    }
}

#[test]
fn test_columns_empty_tsdb_resolves_empty_set() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);

    let cols = engine.columns("cpu_temp").unwrap();
    assert!(cols.is_empty());
}

// ─── HistogramStream::merge and ParquetBuilder guard tests ─────────────────────

#[test]
fn test_histogram_stream_merge_two_streams() {
    use crate::histogram_stream::HistogramStream;

    // Source A covers t1000, t1001 (cumulative [0, 5])
    let source_a = Arc::new(create_hist_source(&[0, 5]));
    let stream_a = source_a
        .histogram_stream("req_latency", &Labels::default(), 0, u64::MAX)
        .expect("stream_a should have data");

    // Source B covers t1002 only (cumulative [12])
    let mut mem_b = Memory::new(1000);
    let config = ::histogram::Config::new(4, 16).unwrap();
    for cpu in ["0", "1"] {
        mem_b.add_histogram(
            "req_latency",
            Histogram {
                labels: make_labels(&[("cpu", cpu)]),
                config,
                timestamps: vec![1002 * 1_000_000_000u64],
                snapshots: vec![hist_snap_cumulative(12)],
            },
        );
    }
    let source_b = Arc::new(mem_b);
    let stream_b = source_b
        .histogram_stream("req_latency", &Labels::default(), 0, u64::MAX)
        .expect("stream_b should have data");

    // Merge and inspect the result
    let merged = HistogramStream::merge(vec![stream_a, stream_b]).expect("merge should succeed");

    // Should have 2 global series (cpu=0, cpu=1)
    assert_eq!(merged.meta.series.len(), 2);

    // Collect all rows; A: 2 rows/series × 2 series = 4; B: 1 row/series × 2 = 2 → 6 total
    let rows: Vec<_> = merged.rows.collect();
    assert_eq!(rows.len(), 6);

    // All rows must be in (timestamp, series_idx) order
    for w in rows.windows(2) {
        assert!(
            (w[0].timestamp, w[0].series_idx) <= (w[1].timestamp, w[1].series_idx),
            "rows not in order: {:?} vs {:?}",
            (w[0].timestamp, w[0].series_idx),
            (w[1].timestamp, w[1].series_idx)
        );
    }
}

#[test]
fn test_histogram_stream_merge_empty_returns_none() {
    use crate::histogram_stream::HistogramStream;
    assert!(HistogramStream::merge(vec![]).is_none());
}

#[test]
fn test_parquet_builder_empty_errors() {
    use crate::parquet::ParquetReader;
    let result = ParquetReader::builder().build();
    assert!(result.is_err());
}

// ─── Label injection tests ────────────────────────────────────────────────────

#[test]
fn test_file_labeled_injects_labels_into_series() {
    use crate::histogram_stream::HistogramStream;

    // Two "files": source_a gets run="a" injected, source_b gets run="b"
    let source_a = Arc::new(create_hist_source(&[0, 5]));
    let mut stream_a = source_a
        .histogram_stream("req_latency", &Labels::default(), 0, u64::MAX)
        .unwrap();
    for labels in &mut stream_a.meta.series {
        labels.inner.insert("run".to_string(), "a".to_string());
    }

    let source_b = Arc::new(create_hist_source(&[0, 3]));
    let mut stream_b = source_b
        .histogram_stream("req_latency", &Labels::default(), 0, u64::MAX)
        .unwrap();
    for labels in &mut stream_b.meta.series {
        labels.inner.insert("run".to_string(), "b".to_string());
    }

    let merged = HistogramStream::merge(vec![stream_a, stream_b]).unwrap();

    // 2 cpus × 2 runs = 4 global series
    assert_eq!(merged.meta.series.len(), 4);
    // Every series must have a "run" label
    for s in &merged.meta.series {
        assert!(
            s.inner.contains_key("run"),
            "series missing injected 'run' label: {:?}",
            s
        );
    }
    // Both run values must be present
    let run_values: std::collections::HashSet<String> = merged
        .meta
        .series
        .iter()
        .filter_map(|s| s.inner.get("run").cloned())
        .collect();
    assert!(run_values.contains("a"), "run=a missing");
    assert!(run_values.contains("b"), "run=b missing");
}

#[test]
fn test_histogram_stream_merge_single_passthrough() {
    use crate::histogram_stream::HistogramStream;

    let source = Arc::new(create_hist_source(&[0, 5, 12]));
    let stream = source
        .histogram_stream("req_latency", &Labels::default(), 0, u64::MAX)
        .unwrap();
    let n_series = stream.meta.series.len();
    let merged = HistogramStream::merge(vec![stream]).unwrap();
    assert_eq!(merged.meta.series.len(), n_series);
}

// ─── Schema introspection tests ───────────────────────────────────────────────

#[test]
fn test_counter_names_returns_metric_names() {
    let source = Arc::new(create_cgroup_source());
    let engine = QueryEngine::new(source);
    let names = engine.counter_names();
    assert!(names.contains(&"cgroup_cpu_usage".to_string()));
}

#[test]
fn test_counter_labels_returns_label_sets() {
    let source = Arc::new(create_cgroup_source());
    let engine = QueryEngine::new(source);
    let labels = engine.counter_labels("cgroup_cpu_usage");
    assert_eq!(labels.len(), 3); // 3 cgroups
    assert!(labels
        .iter()
        .any(|l| l.get("name") == Some(&"/system.slice/foo.service".to_string())));
}

#[test]
fn test_histogram_names_returns_metric_names() {
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);
    let names = engine.histogram_names();
    assert!(names.contains(&"tcp_packet_latency".to_string()));
}

#[test]
fn test_histogram_labels_returns_label_sets() {
    let source = Arc::new(create_columns_histogram_source());
    let engine = QueryEngine::new(source);
    let labels = engine.histogram_labels("tcp_packet_latency");
    assert!(!labels.is_empty());
    assert!(labels.iter().any(|l| l.contains_key("cpu")));
}

#[test]
fn test_unknown_metric_returns_empty_labels() {
    let source = Arc::new(create_empty_source());
    let engine = QueryEngine::new(source);
    assert!(engine.counter_labels("does_not_exist").is_empty());
    assert!(engine.gauge_labels("does_not_exist").is_empty());
    assert!(engine.histogram_labels("does_not_exist").is_empty());
}

#[test]
fn test_injected_label_filter_excludes_incompatible_file() {
    use crate::histogram_stream::HistogramStream;

    let source_a = Arc::new(create_hist_source(&[0, 5]));
    let mut stream_a = source_a
        .histogram_stream("req_latency", &Labels::default(), 0, u64::MAX)
        .unwrap();
    for labels in &mut stream_a.meta.series {
        labels.inner.insert("run".to_string(), "a".to_string());
    }

    let source_b = Arc::new(create_hist_source(&[0, 3]));
    let mut stream_b = source_b
        .histogram_stream("req_latency", &Labels::default(), 0, u64::MAX)
        .unwrap();
    for labels in &mut stream_b.meta.series {
        labels.inner.insert("run".to_string(), "b".to_string());
    }

    let merged = HistogramStream::merge(vec![stream_a, stream_b]).unwrap();

    // Filter the merged stream to only run="a" series
    let run_a_filter = make_labels(&[("run", "a")]);
    let series_matching_a: Vec<_> = merged
        .meta
        .series
        .iter()
        .filter(|s| s.matches(&run_a_filter))
        .collect();

    assert_eq!(
        series_matching_a.len(),
        2,
        "should get 2 series (cpu=0,1) with run=a"
    );
    for s in &series_matching_a {
        assert_eq!(s.inner.get("run").map(String::as_str), Some("a"));
    }
}

#[test]
fn test_parquet_reader_open_bytes_compiles() {
    // Compile-time check that open_bytes accepts Vec<u8>, bytes::Bytes, and builder forms.
    // Both calls fail at runtime (invalid parquet), but the important thing is they compile.
    fn _check() {
        let _ = crate::ParquetReader::open_bytes(vec![0u8; 4]);
        let _ = crate::ParquetReader::builder().bytes(vec![0u8; 4]).build();
        let _ = crate::ParquetReader::builder()
            .bytes_labeled(
                bytes::Bytes::from_static(b""),
                crate::labels::Labels::default(),
            )
            .build();
    }
}

// ─── Item 1: filename tests ───────────────────────────────────────────────────

#[test]
fn test_parquet_reader_with_filename() {
    // Runtime: open_bytes fails on invalid parquet, but with_filename is chainable and compile-time correct.
    fn _check() {
        let _ = crate::ParquetReader::open_bytes(vec![0u8; 4])
            .map(|r| r.with_filename("myfile.parquet"));
    }
    // Verify via builder that filename() returns what was set (compile-time shape check).
    fn _check_builder() {
        let _ = crate::ParquetReader::builder()
            .filename("custom.parquet")
            .bytes(vec![0u8; 4])
            .build()
            .map(|r| {
                assert_eq!(r.filename(), Some("custom.parquet"));
            });
    }
}

#[test]
fn test_memory_store_builder_filename() {
    let store = crate::MemoryStore::builder().filename("my-store").build();
    assert_eq!(store.filename(), Some("my-store".to_string()));
}

#[test]
fn test_memory_store_set_filename() {
    let store = crate::MemoryStore::builder().build();
    assert_eq!(store.filename(), None);
    store.set_filename("set-later");
    assert_eq!(store.filename(), Some("set-later".to_string()));
}

#[test]
fn test_memory_store_filename_via_trait() {
    use crate::MetricsSource;
    let store = crate::MemoryStore::builder().filename("trait-name").build();
    let src: &dyn MetricsSource = &store;
    assert_eq!(src.filename(), Some("trait-name".to_string()));
}

// ─── Item 3: metadata_get tests ──────────────────────────────────────────────

#[test]
fn test_memory_store_metadata_get_matches_source() {
    use crate::MetricsSource;
    let store = crate::MemoryStore::builder()
        .source("rezolus")
        .version("3.0")
        .build();
    assert_eq!(store.metadata_get("source"), Some("rezolus".to_string()));
    assert_eq!(store.metadata_get("version"), Some("3.0".to_string()));
    assert_eq!(store.metadata_get("nonexistent"), None);
    // Trait path
    let src: &dyn MetricsSource = &store;
    assert_eq!(src.metadata_get("source"), Some("rezolus".to_string()));
    assert_eq!(src.source(), "rezolus");
}

// ─── Item 5: label_values tests ──────────────────────────────────────────────

/// Test label_values via QueryEngine + Memory (the internal DataSource layer).
/// We can't easily put counters into a public MemoryStore without the ingest
/// feature, but we can drive `label_values` through the `MetricsSource` default
/// method on an internal MemoryStore-like object using the Memory DataSource.
#[test]
fn test_label_values_via_query_engine() {
    use crate::types::{Counter, Gauge};

    // Build a MemoryStore and use its label_values default method.
    // We populate via the inner Memory (accessible inside the crate).
    let mut mem = Memory::new(1000);
    let ts: Vec<u64> = vec![1_000_000_000_000, 2_000_000_000_000];
    for cpu in ["0", "1"] {
        let labels = make_labels(&[("cpu", cpu)]);
        mem.add_counter(
            "cpu_cycles",
            Counter {
                labels,
                timestamps: ts.clone(),
                values: vec![0, 100],
                windows: None,
            },
        );
    }
    mem.add_gauge(
        "cpu_temp",
        Gauge {
            labels: make_labels(&[("cpu", "0")]),
            timestamps: ts.clone(),
            values: vec![50, 51],
            windows: None,
        },
    );

    // Wrap in a DataSource-backed QueryEngine and drive label_values
    // through a MemoryStoreInner, which has access to the DataSource methods.
    // Since MemoryStoreInner is pub(crate), construct it directly.
    let inner = Arc::new(metriken_storage::memory_store::MemoryStoreInner {
        memory: std::sync::RwLock::new(mem),
        metadata: std::sync::RwLock::new(std::collections::HashMap::new()),
        filename: std::sync::RwLock::new(None),
    });

    // Call counter_labels/gauge_labels directly (DataSource trait methods).
    // Then verify the label_values default impl through a QueryEngine call.
    let cpu_counter_labels = inner.counter_labels("cpu_cycles");
    assert_eq!(cpu_counter_labels.len(), 2);

    // Collect expected label values manually (mirrors label_values logic).
    let mut expected: std::collections::HashSet<String> = std::collections::HashSet::new();
    for lbl in inner.counter_labels("cpu_cycles") {
        if let Some(v) = lbl.get("cpu") {
            expected.insert(v.clone());
        }
    }
    for lbl in inner.gauge_labels("cpu_cycles") {
        if let Some(v) = lbl.get("cpu") {
            expected.insert(v.clone());
        }
    }
    assert!(expected.contains("0"));
    assert!(expected.contains("1"));
    assert_eq!(expected.len(), 2);
}

#[cfg(feature = "ingest")]
#[test]
fn test_memory_store_label_values_ingest() {
    use crate::MetricsSource;
    use std::collections::HashMap;
    use std::time::{Duration, SystemTime};

    fn make_snap(ts: SystemTime, name: &str, value: u64, cpu: &str) -> metriken_model::Snapshot {
        let mut metadata: HashMap<String, String> = HashMap::new();
        metadata.insert("metric".to_string(), name.to_string());
        metadata.insert("cpu".to_string(), cpu.to_string());
        metriken_model::Snapshot::V2(metriken_model::SnapshotV2 {
            systemtime: ts,
            duration: Duration::from_secs(0),
            metadata: HashMap::new(),
            counters: vec![metriken_model::Counter::new(
                name.to_string(),
                value,
                metadata,
            )],
            gauges: vec![],
            histograms: vec![],
        })
    }

    let store = crate::MemoryStore::builder()
        .sampling_interval_ms(1000)
        .build();
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1001);
    store.ingest_snapshot(make_snap(t0, "cpu_cycles", 100, "0"));
    store.ingest_snapshot(make_snap(t1, "cpu_cycles", 200, "1"));

    let vals = store.label_values("cpu_cycles", "cpu");
    assert_eq!(vals.len(), 2);
    assert!(vals.contains("0"));
    assert!(vals.contains("1"));

    let empty = store.label_values("cpu_cycles", "nonexistent");
    assert!(empty.is_empty());

    let empty2 = store.label_values("unknown_metric", "cpu");
    assert!(empty2.is_empty());
}

// ─── Items 1, 2, 3, 7, 8, 9: MetricsSource convenience defaults ──────────────

/// Build a mixed-type Memory (counters + gauges + histograms) for testing the
/// new MetricsSource default methods without the `ingest` feature.
fn make_mixed_memory() -> Arc<metriken_storage::memory_store::MemoryStoreInner> {
    use crate::types::{Counter, Gauge, Histogram, HistogramSnapshot};
    let mut mem = Memory::new(1000);
    let ts: Vec<u64> = vec![1_000_000_000_000, 2_000_000_000_000];

    for cpu in ["0", "1"] {
        let labels = make_labels(&[("cpu", cpu), ("kind", "counter")]);
        mem.add_counter(
            "cpu_cycles",
            Counter {
                labels,
                timestamps: ts.clone(),
                values: vec![0, 100],
                windows: None,
            },
        );
    }
    mem.add_gauge(
        "cpu_temp",
        Gauge {
            labels: make_labels(&[("cpu", "0")]),
            timestamps: ts.clone(),
            values: vec![50, 51],
            windows: None,
        },
    );
    mem.add_gauge(
        "mem_used",
        Gauge {
            labels: make_labels(&[("node", "n1")]),
            timestamps: ts.clone(),
            values: vec![1024, 2048],
            windows: None,
        },
    );

    let config = ::histogram::Config::new(4, 16).unwrap();
    mem.add_histogram(
        "req_latency",
        Histogram {
            labels: make_labels(&[("cpu", "0")]),
            config,
            timestamps: ts.clone(),
            snapshots: vec![
                HistogramSnapshot {
                    index: vec![],
                    count: vec![],
                },
                HistogramSnapshot {
                    index: vec![10],
                    count: vec![5],
                },
            ],
        },
    );

    Arc::new(metriken_storage::memory_store::MemoryStoreInner {
        memory: std::sync::RwLock::new(mem),
        metadata: std::sync::RwLock::new(std::collections::HashMap::new()),
        filename: std::sync::RwLock::new(None),
    })
}

#[test]
fn test_all_names_is_sorted_union_of_all_types() {
    use crate::MetricsSource as _;
    let inner = make_mixed_memory();
    let store = crate::MemoryStore::from_inner(inner);

    let names = store.all_names();
    // Sorted: cpu_cycles, cpu_temp, mem_used, req_latency
    assert_eq!(
        names,
        vec!["cpu_cycles", "cpu_temp", "mem_used", "req_latency"]
    );

    // Each name appears exactly once even if the same metric appears in multiple types
    let dedup: std::collections::HashSet<_> = names.iter().collect();
    assert_eq!(dedup.len(), names.len(), "all_names should be deduplicated");
}

#[test]
fn test_total_series_count_sums_all_types() {
    use crate::MetricsSource;
    let inner = make_mixed_memory();
    let store = crate::MemoryStore::from_inner(inner);

    // cpu_cycles: 2 series, cpu_temp: 1, mem_used: 1, req_latency: 1
    let count = store.total_series_count();
    assert_eq!(count, 5);
}

#[test]
fn test_has_counter_gauge_histogram() {
    use crate::MetricsSource;
    let inner = make_mixed_memory();
    let store = crate::MemoryStore::from_inner(inner);

    assert!(store.has_counter("cpu_cycles"));
    assert!(!store.has_counter("cpu_temp")); // gauge, not counter
    assert!(!store.has_counter("absent"));

    assert!(store.has_gauge("cpu_temp"));
    assert!(store.has_gauge("mem_used"));
    assert!(!store.has_gauge("cpu_cycles")); // counter, not gauge
    assert!(!store.has_gauge("absent"));

    assert!(store.has_histogram("req_latency"));
    assert!(!store.has_histogram("cpu_cycles")); // counter, not histogram
    assert!(!store.has_histogram("absent"));
}

#[test]
fn test_all_label_keys_unions_across_all_metrics() {
    use crate::MetricsSource;
    let inner = make_mixed_memory();
    let store = crate::MemoryStore::from_inner(inner);

    let keys = store.all_label_keys();
    // cpu (from cpu_cycles counter and cpu_temp gauge and req_latency histogram)
    // kind (from cpu_cycles counter)
    // node (from mem_used gauge)
    assert!(keys.contains("cpu"), "expected 'cpu' in label keys");
    assert!(keys.contains("kind"), "expected 'kind' in label keys");
    assert!(keys.contains("node"), "expected 'node' in label keys");
    // Must be sorted (BTreeSet)
    let sorted: Vec<_> = keys.iter().collect();
    let mut expected = sorted.clone();
    expected.sort();
    assert_eq!(sorted, expected, "all_label_keys should be sorted");
}

#[test]
fn test_label_values_by_key_groups_correctly() {
    use crate::MetricsSource;
    let inner = make_mixed_memory();
    let store = crate::MemoryStore::from_inner(inner);

    // cpu_cycles has two series: cpu=0 and cpu=1 (both counters)
    let by_key = store.label_values_by_key("cpu_cycles");
    let cpu_vals = by_key.get("cpu").expect("'cpu' key should be present");
    assert!(cpu_vals.contains("0"), "cpu=0 should be present");
    assert!(cpu_vals.contains("1"), "cpu=1 should be present");
    assert_eq!(cpu_vals.len(), 2);

    let kind_vals = by_key.get("kind").expect("'kind' key should be present");
    assert_eq!(kind_vals, &["counter".to_string()].into_iter().collect());

    // Unknown metric → empty map
    let empty = store.label_values_by_key("nonexistent");
    assert!(empty.is_empty());
}

#[test]
fn test_filename_or_default() {
    use crate::MetricsSource;

    // No filename set → empty string
    let store_no_name = crate::MemoryStore::builder().build();
    assert_eq!(store_no_name.filename_or_default(), "");

    // Filename set → returns name
    let store_with_name = crate::MemoryStore::builder().filename("myfile").build();
    assert_eq!(store_with_name.filename_or_default(), "myfile");
}

// ─── Item 6: time_range_ns ────────────────────────────────────────────────────

#[test]
fn test_time_range_ns_is_exact_for_memory_store() {
    let inner = make_mixed_memory();
    let store = crate::MemoryStore::from_inner(inner);

    let (lo, hi) = store
        .time_range_ns()
        .expect("non-empty store should have time range");
    assert_eq!(lo, 1_000_000_000_000, "expected 1e12 ns (1000 s)");
    assert_eq!(hi, 2_000_000_000_000, "expected 2e12 ns (2000 s)");

    // time_range_ns and time_range should agree (modulo f64 precision)
    let (lo_s, hi_s) = store.time_range().unwrap();
    assert!((lo_s - lo as f64 / 1e9).abs() < 1e-3, "lo seconds mismatch");
    assert!((hi_s - hi as f64 / 1e9).abs() < 1e-3, "hi seconds mismatch");
}

#[test]
fn test_time_range_ns_empty_store_is_none() {
    let store = crate::MemoryStore::builder().build();
    assert!(store.time_range_ns().is_none());
}

// ─── Item 4: ParquetBuilder composition (compile-time API checks) ─────────────

#[test]
fn test_parquet_builder_file_owned_compiles() {
    // file_owned and file_owned_labeled must accept std::fs::File.
    fn _check(f1: std::fs::File, f2: std::fs::File) {
        let _ = crate::ParquetReader::builder().file_owned(f1);
        let _ = crate::ParquetReader::builder().file_owned_labeled(f2, [("run", "a")]);
    }
}

#[test]
fn test_parquet_builder_reader_and_reader_labeled_compile() {
    // reader() and reader_labeled() must accept Arc<ParquetReader>.
    fn _check(r1: Arc<crate::ParquetReader>, r2: Arc<crate::ParquetReader>) {
        let _ = crate::ParquetReader::builder().reader(r1.clone());
        let _ = crate::ParquetReader::builder().reader_labeled(r2, [("run", "b")]);
    }
}

// ── referenced_metrics: routing without a source ─────────────────────────
//
// A reader holding many tables uses these names to decide which tables a query
// could touch, before opening any of them. Over-matching costs an unnecessary
// open; under-matching silently drops a table the query needed, so the bias
// here is deliberately toward returning a name.

#[test]
fn referenced_metrics_collects_a_bare_selector() {
    let m = crate::referenced_metrics("cpu_usage").unwrap();
    assert_eq!(m, ["cpu_usage".to_string()].into_iter().collect());
}

#[test]
fn referenced_metrics_reaches_through_functions_and_aggregates() {
    let m = crate::referenced_metrics("sum(rate(cpu_usage[1m]))").unwrap();
    assert_eq!(m, ["cpu_usage".to_string()].into_iter().collect());
}

/// The case routing exists for: a binary op whose sides live in different
/// tables. Both names must come back or one table never gets opened.
#[test]
fn referenced_metrics_collects_both_sides_of_a_binary_op() {
    let m =
        crate::referenced_metrics("sum(rate(cpu_instructions[1m])) / sum(rate(cpu_cycles[1m]))")
            .unwrap();
    assert_eq!(
        m,
        ["cpu_instructions".to_string(), "cpu_cycles".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn referenced_metrics_reads_the_name_matcher_form() {
    let m = crate::referenced_metrics(r#"{__name__="cpu_usage"}"#).unwrap();
    assert_eq!(m, ["cpu_usage".to_string()].into_iter().collect());
}

/// Label matchers are ignored on purpose: a table holding the metric but no
/// matching series answers with an empty result, which is correct. Skipping it
/// on a label mismatch would be routing on data the catalog does not have.
#[test]
fn referenced_metrics_ignores_label_matchers() {
    let m = crate::referenced_metrics(r#"cpu_usage{id="3"}"#).unwrap();
    assert_eq!(m, ["cpu_usage".to_string()].into_iter().collect());
}

#[test]
fn referenced_metrics_handles_the_rezolus_quantiles_wrapper() {
    let m = crate::referenced_metrics("histogram_quantiles([0.5,0.9], blockio_latency)").unwrap();
    assert!(m.contains("blockio_latency"), "got {m:?}");
}

#[test]
fn referenced_metrics_is_empty_for_a_scalar_and_errors_on_syntax() {
    assert!(crate::referenced_metrics("42").unwrap().is_empty());
    assert!(crate::referenced_metrics("sum(((").is_err());
}

/// `referenced_metrics` must not narrow what `columns` would have matched:
/// every metric name behind a physical column `columns` returns has to appear
/// here, or routing drops a table the query engine would then need.
#[test]
fn referenced_metrics_is_a_superset_of_what_columns_resolves() {
    for q in [
        "cpu_usage",
        "sum(rate(cpu_usage[1m]))",
        "sum(rate(cpu_usage[1m])) / sum(rate(cpu_cycles[1m]))",
        r#"cpu_usage{id="0"}"#,
    ] {
        let names = crate::referenced_metrics(q).unwrap();
        assert!(
            !names.is_empty(),
            "{q}: routing would open nothing and the query would be refused"
        );
    }
}

/// The value of `query` at `t` from a range query starting at `start`.
fn rate_at(engine: &QueryEngine, query: &str, start: f64, t: f64) -> Option<f64> {
    let result = engine.query_range(query, start, t + 20.0, 1.0).unwrap();
    get_matrix_values(&result)
        .first()?
        .iter()
        .find(|(ts, _)| (ts - t).abs() < 1e-9)
        .map(|(_, v)| *v)
}

fn counter_engine(interval_ms: u64, timestamps: Vec<u64>, values: Vec<u64>) -> QueryEngine {
    let mut source = Memory::new(interval_ms);
    source.add_counter(
        "c",
        Counter {
            labels: Labels::default(),
            timestamps,
            values,
            windows: None,
        },
    );
    QueryEngine::new(Arc::new(source))
}

/// A `rate`'s window does not depend on where the query starts: across a
/// hole longer than the reach before its left edge, it starts at the first
/// sample after the edge whether or not the read reached the sample before
/// the hole.
#[test]
fn a_rate_across_a_hole_is_the_same_from_any_start() {
    const S: u64 = 1_000_000_000;
    // Rows every second from 1000 s, none in [1050, 1100); 10/s, and 5000
    // more accrued inside the hole.
    let ts: Vec<u64> = (1000..1200u64)
        .filter(|s| !(1050..1100).contains(s))
        .map(|s| s * S)
        .collect();
    let vals: Vec<u64> = ts
        .iter()
        .map(|t| {
            let s = t / S - 1000;
            s * 10 + if s >= 100 { 5000 } else { 0 }
        })
        .collect();
    let engine = counter_engine(1000, ts, vals);
    for t in [1105.0, 1108.0] {
        let early = rate_at(&engine, "rate(c[10s])", 1040.0, t);
        let late = rate_at(&engine, "rate(c[10s])", t, t);
        assert_eq!(early, late, "at {t}");
        assert!((late.unwrap() - 10.0).abs() < 1e-9, "at {t}: {late:?}");
    }
}

/// A counter read every 10 s from a source whose interval says 1 s: a
/// `rate` at one time has one value from any start.
#[test]
fn a_slow_counter_rate_is_the_same_from_any_start() {
    const S: u64 = 1_000_000_000;
    let ts: Vec<u64> = (0..60u64).map(|k| (1005 + 10 * k) * S).collect();
    // Flat until 1100 s, then 10/s.
    let vals: Vec<u64> = ts
        .iter()
        .map(|t| (t / S).saturating_sub(1100) * 10)
        .collect();
    let engine = counter_engine(1000, ts, vals);
    for t in [1121.0, 1124.0, 1128.0] {
        let early = rate_at(&engine, "rate(c[30s])", t - 200.0, t);
        let late = rate_at(&engine, "rate(c[30s])", t, t);
        assert!(early.is_some(), "at {t}");
        assert_eq!(early, late, "at {t}");
    }
}

/// At explicit evaluation timestamps a `rate` has a point at each one in
/// the range, the first included, and none before it; an `irate` has none
/// at the first, which has no predecessor.
#[test]
fn a_rate_at_explicit_timestamps_has_a_point_at_each() {
    const S: u64 = 1_000_000_000;
    let engine = counter_engine(
        1000,
        (0..20u64).map(|k| (1000 + k) * S).collect(),
        (0..20u64).map(|k| k * 10).collect(),
    );
    let times = |query: &str, points: Vec<u64>| -> Vec<f64> {
        let opts = QueryOptions::default().with_eval_timestamps(Some(points.into()));
        let result = engine
            .query_range_opts(query, 1005.0, 1009.0, 1.0, &opts)
            .unwrap();
        get_matrix_values(&result)
            .first()
            .map(|s| s.iter().map(|(t, _)| *t).collect())
            .unwrap_or_default()
    };
    let three = vec![1005 * S, 1007 * S, 1009 * S];
    assert_eq!(
        times("rate(c[3s])", three.clone()),
        vec![1005.0, 1007.0, 1009.0]
    );
    assert_eq!(times("rate(c[3s])", vec![1009 * S]), vec![1009.0]);
    // A timestamp before the range's start is not a point.
    assert_eq!(times("rate(c[3s])", vec![1003 * S, 1005 * S]), vec![1005.0]);
    assert_eq!(times("irate(c[3s])", three), vec![1007.0, 1009.0]);
}

/// The points of `result` at or after `from`.
fn points_from(result: &QueryResult, from: f64) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = match result {
        QueryResult::Matrix { result } => result.iter().flat_map(|s| s.values.clone()).collect(),
        QueryResult::Vector { result } => result.iter().map(|s| s.value).collect(),
        other => panic!("{other:?}"),
    };
    out.retain(|(t, _)| *t >= from);
    out
}

/// A point held past a series' last sample has one value from any start:
/// its window ends at that sample, up to the series' spacing before the
/// range's start, and the read reaches far enough back for the sample
/// before the window's left edge.
#[test]
fn a_held_rate_is_the_same_from_any_start() {
    const S: u64 = 1_000_000_000;
    // Every 10 s to 1140 s, then a row 2 s late and every 10 s to 1200 s;
    // flat until 1160 s, then 100/s.
    let mut ts: Vec<u64> = (0..15u64).map(|k| 1000 + 10 * k).collect();
    ts.extend([1152, 1172, 1182, 1192, 1200]);
    let vals: Vec<u64> = ts.iter().map(|s| s.saturating_sub(1160) * 100).collect();
    let engine = counter_engine(10_000, ts.iter().map(|s| s * S).collect(), vals);
    let q = "rate(c[30s])";
    let wide = engine.query_range(q, 1100.0, 1205.0, 1.0).unwrap();
    let narrow = engine.query_range(q, 1203.0, 1205.0, 1.0).unwrap();
    let instant = engine.query(q, Some(1205.0)).unwrap();
    let at = |r: &QueryResult| points_from(r, 1205.0).first().map(|p| p.1);
    assert!(at(&wide).is_some(), "{wide:?}");
    assert_eq!(at(&wide), at(&narrow));
    assert_eq!(at(&wide), at(&instant));
}

/// Across recordings, a series' rate is the same whichever other
/// recordings the query reaches: each series takes its reach from its own
/// table's interval.
#[test]
fn a_rate_across_recordings_takes_its_own_tables_reach() {
    use metriken_storage::labeled::LabeledSource;
    use metriken_storage::recordings::RecordingsSource;
    const S: u64 = 1_000_000_000;
    let source = |interval_ms: u64, ts: Vec<u64>, vals: Vec<u64>, name: &str| {
        let mut m = Memory::new(interval_ms);
        m.add_counter(
            "c",
            Counter {
                labels: Labels::default(),
                timestamps: ts.iter().map(|s| s * S).collect(),
                values: vals,
                windows: None,
            },
        );
        let labeled = LabeledSource::new(Arc::new(m), make_labels(&[("source", name)]))
            .with_interval(interval_ms as f64 / 1000.0);
        Arc::new(labeled) as Arc<dyn DataSource>
    };
    // a: every second, none in [1050, 1062), 5000 more inside the gap.
    let ts_a: Vec<u64> = (1000..1200u64)
        .filter(|s| !(1050..1062).contains(s))
        .collect();
    let v_a: Vec<u64> = ts_a
        .iter()
        .map(|s| (s - 1000) * 10 + if *s >= 1062 { 5000 } else { 0 })
        .collect();
    let a = source(1000, ts_a, v_a, "a");
    // b: every 10 s.
    let ts_b: Vec<u64> = (0..20u64).map(|k| 1000 + 10 * k).collect();
    let v_b: Vec<u64> = ts_b.iter().map(|s| s - 1000).collect();
    let b = source(10_000, ts_b, v_b, "b");
    let alone = QueryEngine::new(Arc::new(RecordingsSource::new(vec![a.clone()])));
    let both = QueryEngine::new(Arc::new(RecordingsSource::new(vec![a, b])));
    let q = r#"rate(c{source="a"}[10s])"#;
    let x = alone.query_range(q, 1060.0, 1070.0, 1.0).unwrap();
    let y = both.query_range(q, 1060.0, 1070.0, 1.0).unwrap();
    assert_eq!(points_from(&x, 0.0), points_from(&y, 0.0));
    assert!(
        points_from(&y, 1063.0)
            .iter()
            .all(|(_, v)| (v - 10.0).abs() < 1e-9),
        "{y:?}"
    );
}

/// A source over `(name, seconds, values)` counters at `interval_ms`.
fn counters_source(interval_ms: u64, counters: Vec<(&str, Vec<u64>, Vec<u64>)>) -> Memory {
    const S: u64 = 1_000_000_000;
    let mut m = Memory::new(interval_ms);
    for (name, ts, values) in counters {
        m.add_counter(
            name,
            Counter {
                labels: Labels::default(),
                timestamps: ts.iter().map(|s| s * S).collect(),
                values,
                windows: None,
            },
        );
    }
    m
}

/// In raw mode, a series' point before the range, held at the range's
/// start, has one value from any start: a point whose window the read
/// does not hold is not emitted.
#[test]
fn a_raw_rate_point_before_the_range_is_the_same_from_any_start() {
    // `a` every second with a gap over (1105, 1130) and a step of 1000 at
    // 1098; `b` every second.
    let ts_a: Vec<u64> = (1000..=1105u64).chain(1130..=1200).collect();
    let v_a: Vec<u64> = ts_a
        .iter()
        .map(|t| 10 * (t - 1000) + if *t >= 1098 { 1000 } else { 0 })
        .collect();
    let ts_b: Vec<u64> = (1000..=1200u64).collect();
    let v_b: Vec<u64> = ts_b.iter().map(|t| t - 1000).collect();
    let engine = QueryEngine::new(Arc::new(counters_source(
        1000,
        vec![("a", ts_a, v_a), ("b", ts_b, v_b)],
    )));
    assert_raw_point_before_the_range(&engine, "rate(a[10s]) + rate(b[10s])", 111.0);
}

/// Across recordings whose sources set no interval, a series takes its
/// own recording's: its rate is the same with or without a slower
/// recording beside it.
#[test]
fn a_rate_across_plain_recordings_takes_its_own_recordings_reach() {
    use metriken_storage::labeled::LabeledSource;
    use metriken_storage::recordings::RecordingsSource;
    let ts_a: Vec<u64> = (1000..1200u64)
        .filter(|s| !(1050..1062).contains(s))
        .collect();
    let v_a: Vec<u64> = ts_a
        .iter()
        .map(|s| (s - 1000) * 10 + if *s >= 1062 { 5000 } else { 0 })
        .collect();
    let ts_b: Vec<u64> = (0..20u64).map(|k| 1000 + 10 * k).collect();
    let v_b: Vec<u64> = ts_b.iter().map(|s| s - 1000).collect();
    let a: Arc<dyn DataSource> = Arc::new(LabeledSource::new(
        Arc::new(counters_source(1000, vec![("c", ts_a, v_a)])),
        make_labels(&[("source", "a")]),
    ));
    let b: Arc<dyn DataSource> = Arc::new(LabeledSource::new(
        Arc::new(counters_source(10_000, vec![("c", ts_b, v_b)])),
        make_labels(&[("source", "b")]),
    ));
    let alone = QueryEngine::new(Arc::new(RecordingsSource::new(vec![a.clone()])));
    let both = QueryEngine::new(Arc::new(RecordingsSource::new(vec![a, b])));
    let q = r#"rate(c{source="a"}[10s])"#;
    let x = alone.query_range(q, 1060.0, 1070.0, 1.0).unwrap();
    let y = both.query_range(q, 1060.0, 1070.0, 1.0).unwrap();
    assert_eq!(points_from(&x, 0.0), points_from(&y, 0.0));
}

/// An outer `with_interval` is the one a series carries, so its reach is
/// what the read covers: one value from any start.
#[test]
fn nested_intervals_give_one_value_from_any_start() {
    use metriken_storage::labeled::LabeledSource;
    let ts: Vec<u64> = (0..30u64)
        .map(|k| 1000 + 10 * k)
        .filter(|t| *t != 1100)
        .collect();
    let v: Vec<u64> = ts
        .iter()
        .map(|t| (t - 1000) * 10 + if *t > 1100 { 5000 } else { 0 })
        .collect();
    let inner = LabeledSource::new(
        Arc::new(counters_source(10_000, vec![("c", ts, v)])),
        Labels::default(),
    )
    .with_interval(10.0);
    let outer = LabeledSource::new(Arc::new(inner), Labels::default()).with_interval(1.0);
    let engine = QueryEngine::new(Arc::new(outer));
    let at = |start: f64| {
        let r = engine
            .query_range("rate(c[10s])", start, 1125.0, 1.0)
            .unwrap();
        points_from(&r, 1115.0).first().copied()
    };
    assert_eq!(at(1050.0), at(1115.0));
}

/// The same for the raw windowed gauge functions: `avg_over_time`,
/// `idelta` and `deriv`.
#[test]
fn a_raw_avg_over_time_point_before_the_range_is_the_same_from_any_start() {
    const S: u64 = 1_000_000_000;
    let mut m = Memory::new(1000);
    // `g` every second with a gap over (1105, 1130), stepping up at 1098;
    // `h` every second.
    let ts_g: Vec<u64> = (1000..=1105u64).chain(1130..=1200).collect();
    m.add_gauge(
        "g",
        Gauge {
            labels: Labels::default(),
            timestamps: ts_g.iter().map(|s| s * S).collect(),
            values: ts_g
                .iter()
                .map(|t| if *t >= 1098 { 100 } else { 0 })
                .collect(),
            windows: None,
        },
    );
    m.add_gauge(
        "h",
        Gauge {
            labels: Labels::default(),
            timestamps: (1000..=1200u64).map(|s| s * S).collect(),
            values: vec![1; 201],
            windows: None,
        },
    );
    let engine = QueryEngine::new(Arc::new(m));
    for (query, expected) in [
        ("avg_over_time(g[10s]) + h", 100.0 * 8.0 / 11.0 + 1.0),
        ("idelta(g[10s]) + h", 1.0),
        // The least-squares slope of 0, 0, 0 then eight 100s, a second apart.
        ("deriv(g[10s]) + h", 120.0 / 11.0 + 1.0),
    ] {
        assert_raw_point_before_the_range(&engine, query, expected);
    }
}

/// In raw mode, the points of `query` from `start` until 1130 s, for starts
/// from 1106 s to 1125 s, are one point at 1106 s of `expected` when the
/// start is 1106 s, and none otherwise: the series' last point before its
/// gap, at 1105 s, is over its whole window and held for the gap before it
/// (1 s), whatever the start.
fn assert_raw_point_before_the_range(engine: &QueryEngine, query: &str, expected: f64) {
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    for start in [1106.0, 1107.0, 1110.0, 1115.0, 1120.0, 1125.0] {
        let r = engine
            .query_range_opts(query, start, 1131.0, 1.0, &raw)
            .unwrap();
        let got: Vec<(f64, f64)> = points_from(&r, start)
            .into_iter()
            .filter(|(t, _)| *t < 1130.0)
            .collect();
        if start == 1106.0 {
            assert_eq!(got.len(), 1, "{query} from {start}: {got:?}");
            assert_eq!(got[0].0, 1106.0, "{query} from {start}: {got:?}");
            assert!(
                (got[0].1 - expected).abs() < 1e-9,
                "{query} from {start}: {got:?}"
            );
        } else {
            assert!(got.is_empty(), "{query} from {start}: {got:?}");
        }
    }
}

/// A raw `idelta` or `deriv` point before the range whose sample before it
/// has no value of its own (alone in its window) is held for the gap to
/// that sample, not to the series' previous point.
#[test]
fn a_raw_point_after_a_sample_without_a_value_is_held_for_its_own_gap() {
    const S: u64 = 1_000_000_000;
    let mut m = Memory::new(1000);
    // `g` (its value the seconds past 1000) every second to 1080, then at
    // 1100 and 1105, then every second from 1130; `h` every second.
    let ts_g: Vec<u64> = (1000..=1080u64)
        .chain([1100, 1105])
        .chain(1130..=1200)
        .collect();
    m.add_gauge(
        "g",
        Gauge {
            labels: Labels::default(),
            timestamps: ts_g.iter().map(|s| s * S).collect(),
            values: ts_g.iter().map(|t| (*t - 1000) as i64).collect(),
            windows: None,
        },
    );
    m.add_gauge(
        "h",
        Gauge {
            labels: Labels::default(),
            timestamps: (1000..=1200u64).map(|s| s * S).collect(),
            values: vec![1; 201],
            windows: None,
        },
    );
    let engine = QueryEngine::new(Arc::new(m));
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    // The point at 1105 s is held 5 s, to 1110 s. At a 5 s step the hold
    // bound is 10 s (two steps), longer than the gaps compared.
    for (query, expected) in [("idelta(g[10s]) + h", 6.0), ("deriv(g[10s]) + h", 2.0)] {
        for start in [1106.0, 1108.0, 1112.0, 1120.0] {
            let r = engine
                .query_range_opts(query, start, 1131.0, 5.0, &raw)
                .unwrap();
            let got: Vec<(f64, f64)> = points_from(&r, start)
                .into_iter()
                .filter(|(t, _)| *t < 1130.0)
                .collect();
            let want: Vec<(f64, f64)> = (start as u64..=1110)
                .map(|t| (t as f64, expected))
                .collect();
            assert_eq!(got.len(), want.len(), "{query} from {start}: {got:?}");
            for ((t, v), (wt, wv)) in got.iter().zip(&want) {
                assert_eq!(t, wt, "{query} from {start}: {got:?}");
                assert!((v - wv).abs() < 1e-9, "{query} from {start}: {got:?}");
            }
        }
    }
}

/// A sample whose function gives no value is a timing point, never a value:
/// nothing without a value reaches the range.
#[test]
fn a_timing_point_without_its_point_is_dropped() {
    const S: u64 = 1_000_000_000;
    // Samples to 1080, then at 1102 and 1120 for the counter and at 1110
    // and 1122 for the gauge, then every second from 1128. Each pair's
    // first is read and its second, the point before the range, has no
    // value: a 10 s rate at 1120 and a 10 s `deriv` at 1122 have no sample
    // close enough before their windows.
    let series = |a: u64, b: u64| -> Vec<u64> {
        (1000..=1080u64).chain([a, b]).chain(1128..=1200).collect()
    };
    let ts = series(1102, 1120);
    let ts_g = series(1110, 1122);
    let mut m = Memory::new(1000);
    m.add_counter(
        "a",
        Counter {
            labels: Labels::default(),
            timestamps: ts.iter().map(|s| s * S).collect(),
            values: ts.iter().map(|t| t - 1000).collect(),
            windows: None,
        },
    );
    m.add_gauge(
        "g",
        Gauge {
            labels: Labels::default(),
            timestamps: ts_g.iter().map(|s| s * S).collect(),
            values: ts_g.iter().map(|t| (*t - 1000) as i64).collect(),
            windows: None,
        },
    );
    m.add_gauge(
        "h",
        Gauge {
            labels: Labels::default(),
            timestamps: (1000..=1200u64).map(|s| s * S).collect(),
            values: vec![1; 201],
            windows: None,
        },
    );
    let engine = QueryEngine::new(Arc::new(m));
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    for query in ["rate(a[10s]) + h", "deriv(g[10s]) + h"] {
        let r = engine
            .query_range_opts(query, 1125.0, 1140.0, 1.0, &raw)
            .unwrap();
        let got = points_from(&r, 0.0);
        assert!(
            got.iter().all(|(t, v)| *t >= 1128.0 && v.is_finite()),
            "{query}: {got:?}"
        );
    }
}

/// Raw compositions of out-of-phase recordings, nested and not, never show
/// a timing point: every value in a result, matrix or display, is finite.
#[test]
fn raw_compositions_never_show_a_timing_point() {
    use metriken_storage::labeled::LabeledSource;
    use metriken_storage::recordings::RecordingsSource;
    const MS: u64 = 1_000_000;
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }
    // Row times in ms, spaced from one to two intervals.
    let times = |rng: &mut Rng, iv: u64, phase: u64| -> Vec<u64> {
        let mut t = 1_000_000 + phase;
        let mut out = vec![];
        while t < 1_200_000 {
            out.push(t);
            t += iv + rng.below(iv + 1);
        }
        out
    };
    let queries = [
        r#"(sum(rate(a{source="x"}[5s])) + sum(rate(a{source="y"}[5s]))) + sum(rate(a{source="z"}[5s]))"#,
        r#"(sum(rate(a{source="z"}[25s])) + sum(rate(a{source="y"}[4s]))) * sum(rate(a{source="x"}[2s]))"#,
        r#"(sum(avg_over_time(g{source="z"}[30s])) + sum(idelta(g{source="y"}[8s]))) + sum(deriv(g{source="x"}[3s]))"#,
        r#"(sum(irate(a{source="z"}[5s])) + sum(irate(a{source="y"}[5s]))) + sum(g)"#,
        "count(rate(a[20s])) + sum(rate(a{source=\"z\"}[20s]))",
        r#"(2 * sum(rate(a{source="x"}[5s])) + sum(rate(a{source="y"}[5s]))) + sum(g)"#,
    ];
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    for seed in 1..12u64 {
        let mut rng = Rng(seed);
        let mut children: Vec<Arc<dyn DataSource>> = vec![];
        for (name, iv) in [("x", 1000u64), ("y", 3000), ("z", 10_000)] {
            let mut m = Memory::new(iv);
            let phase = rng.below(iv);
            let ts = times(&mut rng, iv, phase);
            let mut acc = 0u64;
            let values: Vec<u64> = ts
                .iter()
                .map(|_| {
                    acc += rng.below(50);
                    acc
                })
                .collect();
            m.add_counter(
                "a",
                Counter {
                    labels: Labels::default(),
                    timestamps: ts.iter().map(|s| s * MS).collect(),
                    values,
                    windows: None,
                },
            );
            let ts = times(&mut rng, iv, phase);
            let values: Vec<i64> = ts.iter().map(|_| rng.below(100) as i64).collect();
            m.add_gauge(
                "g",
                Gauge {
                    labels: Labels::default(),
                    timestamps: ts.iter().map(|s| s * MS).collect(),
                    values,
                    windows: None,
                },
            );
            let labeled = LabeledSource::new(Arc::new(m), make_labels(&[("source", name)]))
                .with_interval(iv as f64 / 1000.0);
            children.push(Arc::new(labeled));
        }
        let engine = QueryEngine::new(Arc::new(RecordingsSource::new(children)));
        for q in queries {
            for half in (2100..2240u64).step_by(3) {
                let start = half as f64 / 2.0;
                let r = engine
                    .query_range_opts(q, start, 1150.0, 1.0, &raw)
                    .unwrap();
                let bad: Vec<(f64, f64)> = points_from(&r, 0.0)
                    .into_iter()
                    .filter(|(_, v)| !v.is_finite())
                    .collect();
                assert!(bad.is_empty(), "seed {seed} {q} from {start}: {bad:?}");
                let d = engine
                    .query_range_display_opts(
                        q,
                        start,
                        1150.0,
                        1.0,
                        &crate::DisplayOptions::default(),
                        &raw,
                    )
                    .unwrap();
                assert!(
                    !format!("{d:?}").contains("NaN"),
                    "seed {seed} {q} from {start}: display {d:?}"
                );
            }
        }
    }
}

/// A raw `irate`'s point before the range is held for the gap before it,
/// as a `rate`'s is.
#[test]
fn a_raw_irate_point_before_the_range_is_held_for_the_gap_before_it() {
    const S: u64 = 1_000_000_000;
    let ts: Vec<u64> = (1000..=1099u64).chain(1130..=1200).collect();
    let mut m = Memory::new(1000);
    m.add_counter(
        "a",
        Counter {
            labels: Labels::default(),
            timestamps: ts.iter().map(|s| s * S).collect(),
            values: ts.iter().map(|t| (t - 1000) * 3).collect(),
            windows: None,
        },
    );
    m.add_gauge(
        "h",
        Gauge {
            labels: Labels::default(),
            timestamps: (1000..1200u64).map(|s| s * S).collect(),
            values: vec![1; 200],
            windows: None,
        },
    );
    let engine = QueryEngine::new(Arc::new(m));
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    for q in ["irate(a[5s]) + h", "rate(a[5s]) + h"] {
        // The point at 1099 s is held 1 s: only 1100 s in [1100, 1130).
        for (start, want) in [(1095.0, 1), (1100.0, 1), (1110.0, 0)] {
            let r = engine
                .query_range_opts(q, start, 1135.0, 1.0, &raw)
                .unwrap();
            let got = points_from(&r, 0.0)
                .into_iter()
                .filter(|(t, _)| (1100.0..1130.0).contains(t))
                .count();
            assert_eq!(got, want, "{q} from {start}");
        }
    }
}

/// A point that gives no value, before the range or in it, is the timing
/// point for the next, so the next is held for the gap before it, from any
/// start.
#[test]
fn a_first_point_after_a_point_without_a_value_is_held_for_its_own_gap() {
    const S: u64 = 1_000_000_000;
    // Every second to 1080; the counter at 1102 and 1120, the gauge at 1110
    // and 1122; both at 1128 and then every second from 1150. At 1120 a
    // 10 s rate, and at 1122 a 10 s `deriv`, has no value.
    let series = |a: u64, b: u64| -> Vec<u64> {
        (1000..=1080u64)
            .chain([a, b, 1128])
            .chain(1150..=1200)
            .collect()
    };
    let (ts, ts_g) = (series(1102, 1120), series(1110, 1122));
    let mut m = Memory::new(1000);
    m.add_counter(
        "a",
        Counter {
            labels: Labels::default(),
            timestamps: ts.iter().map(|s| s * S).collect(),
            values: ts.iter().map(|t| t - 1000).collect(),
            windows: None,
        },
    );
    m.add_gauge(
        "g",
        Gauge {
            labels: Labels::default(),
            timestamps: ts_g.iter().map(|s| s * S).collect(),
            values: ts_g.iter().map(|t| (*t - 1000) as i64).collect(),
            windows: None,
        },
    );
    m.add_gauge(
        "h",
        Gauge {
            labels: Labels::default(),
            timestamps: (1000..=1200u64).map(|s| s * S).collect(),
            values: vec![1; 201],
            windows: None,
        },
    );
    let engine = QueryEngine::new(Arc::new(m));
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    // At a 5 s step the hold bound is 10 s (two steps), longer than the
    // gaps compared. The point at 1128 is held for 8 s after the counter's
    // 1120 and 6 s
    // after the gauge's 1122 (where neither `deriv` nor `idelta` has a
    // value), from any start.
    for (query, last) in [
        ("rate(a[10s]) + h", 1136.0),
        ("deriv(g[10s]) + h", 1134.0),
        ("idelta(g[10s]) + h", 1134.0),
    ] {
        for start in [1050.0, 1101.0, 1105.0, 1110.0, 1115.0, 1121.0, 1125.0] {
            let r = engine
                .query_range_opts(query, start, 1149.0, 5.0, &raw)
                .unwrap();
            let times: Vec<f64> = points_from(&r, 1126.0)
                .into_iter()
                .map(|(t, _)| t)
                .collect();
            let want: Vec<f64> = (1128..=last as u64).map(|t| t as f64).collect();
            assert_eq!(times, want, "{query} from {start}");
        }
    }
}

/// A timing point inside the range, the sample of a point there that gave
/// no value, is not in the result, matrix or display.
#[test]
fn a_timing_point_in_the_range_is_not_in_the_result() {
    const S: u64 = 1_000_000_000;
    let mut m = Memory::new(1000);
    // At 1013 and 1030, a 10 s window holds no sample but the point's own.
    let ts = [900u64, 1000, 1013, 1030, 1031, 1032, 1033];
    m.add_counter(
        "a",
        Counter {
            labels: Labels::default(),
            timestamps: ts.iter().map(|s| s * S).collect(),
            values: ts.iter().map(|t| t - 900).collect(),
            windows: None,
        },
    );
    m.add_gauge(
        "g",
        Gauge {
            labels: Labels::default(),
            timestamps: ts.iter().map(|s| s * S).collect(),
            values: ts.iter().map(|t| (*t - 900) as i64).collect(),
            windows: None,
        },
    );
    m.add_gauge(
        "h",
        Gauge {
            labels: Labels::default(),
            timestamps: (900..=1040u64).map(|s| s * S).collect(),
            values: vec![1; 141],
            windows: None,
        },
    );
    let engine = QueryEngine::new(Arc::new(m));
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    for q in [
        "rate(a[10s])",
        "rate(a[10s]) + h",
        "sum(rate(a[10s]))",
        "deriv(g[10s])",
        "deriv(g[10s]) + h",
        "idelta(g[10s])",
    ] {
        for start in [1001.0, 1012.0, 1013.5, 1014.0, 1020.0, 1029.0] {
            let r = engine
                .query_range_opts(q, start, 1036.0, 1.0, &raw)
                .unwrap();
            let bad: Vec<(f64, f64)> = points_from(&r, 0.0)
                .into_iter()
                .filter(|(_, v)| !v.is_finite())
                .collect();
            assert!(bad.is_empty(), "{q} from {start}: {bad:?}");
            let d = engine
                .query_range_display_opts(
                    q,
                    start,
                    1036.0,
                    1.0,
                    &crate::DisplayOptions::default(),
                    &raw,
                )
                .unwrap();
            assert!(!format!("{d:?}").contains("NaN"), "{q} from {start}: {d:?}");
        }
    }
}

/// The one-to-one check counts a series' points in the range: a raw series'
/// point before the range counts only when it is held to the range's
/// start.
#[test]
fn a_point_before_the_range_does_not_count_for_one_to_one() {
    const S: u64 = 1_000_000_000;
    let mut m = Memory::new(1000);
    let ts: Vec<u64> = (1000..=1200).collect();
    m.add_counter(
        "a",
        Counter {
            labels: Labels::default(),
            timestamps: ts.iter().map(|s| s * S).collect(),
            values: ts.iter().map(|t| t - 1000).collect(),
            windows: None,
        },
    );
    // `h{i="1"}` ends at 1099, `h{i="2"}` starts at 1101.
    for (i, range) in [("1", 1000..=1099u64), ("2", 1101..=1200u64)] {
        let t: Vec<u64> = range.collect();
        m.add_gauge(
            "h",
            Gauge {
                labels: make_labels(&[("i", i)]),
                timestamps: t.iter().map(|s| s * S).collect(),
                values: vec![1; t.len()],
                windows: None,
            },
        );
    }
    let engine = QueryEngine::new(Arc::new(m));
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    for start in [1100.5, 1105.0, 1110.0] {
        let r = engine.query_range_opts(
            "rate(a[5s]) + on() avg_over_time(h[5s])",
            start,
            1150.0,
            1.0,
            &raw,
        );
        assert!(r.is_ok(), "from {start}: {r:?}");
    }
    // From 1100, `h{i="1"}`'s point at 1099 is held to the start (for the
    // second before it), so both series have points in the range.
    let r = engine.query_range_opts(
        "rate(a[5s]) + on() avg_over_time(h[5s])",
        1100.0,
        1150.0,
        1.0,
        &raw,
    );
    assert!(
        matches!(&r, Err(e) if e.to_string().contains("many-to-many")),
        "{r:?}"
    );
}

/// In raw mode a query's result at a time does not depend on where the
/// range starts: over sparse series with holes longer than their windows,
/// for single and nested joins, aggregates, scalar ops and bare selectors,
/// every point at a time is the same from every start at or before it, and
/// present from all of them.
#[test]
fn raw_results_do_not_depend_on_the_start() {
    use std::collections::BTreeMap;
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }
    // In ms: mostly a second or two apart with up to 0.3 s of jitter,
    // often a hole of 4 to 25 s.
    let times = |rng: &mut Rng| -> Vec<u64> {
        let mut t = 1_000_000u64 + rng.below(3000);
        let mut out = vec![];
        while t < 1_200_000 {
            out.push(t);
            let r = rng.below(100);
            t += if r < 25 {
                4000 + rng.below(22_000)
            } else if r < 50 {
                2000
            } else {
                1000
            } + rng.below(300);
        }
        out
    };
    let queries = [
        "rate(a[2s])",
        "rate(a[3s]) + h",
        "sum(rate(a[2s]))",
        "count(rate(a[2s]))",
        "count(rate(a[3s])) + count(deriv(g[3s]))",
        "min(rate(a[2s]))",
        "avg(rate(a[3s]))",
        "rate(a[2s]) + on(i) deriv(g[3s])",
        "rate(a[2s]) + on(i) idelta(g[2s])",
        "sum(rate(a[2s]) + on(i) deriv(g[3s]))",
        "sum(rate(a[2s])) + sum(idelta(g[3s]))",
        r#"(rate(a{i="0"}[2s]) + h) * 2 + h"#,
        r#"(rate(a{i="0"}[2s]) + rate(a{i="1"}[3s])) + rate(a{i="2"}[2s])"#,
        "deriv(g[2s]) + h",
        "max(idelta(g[2s])) / sum(rate(a[2s]))",
        "irate(a[2s]) + h",
        "avg_over_time(g[2s]) + rate(a[2s])",
        "g + h",
        r#"sum(g) + on() g{i="0"}"#,
    ];
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    let mut failures = Vec::new();
    for seed in 1..10u64 {
        let mut rng = Rng(seed);
        let mut m = Memory::new(1000);
        for i in 0..3 {
            let labels = make_labels(&[("i", &i.to_string())]);
            let ts = times(&mut rng);
            let mut acc = 0u64;
            let values: Vec<u64> = ts
                .iter()
                .map(|_| {
                    acc += rng.below(50);
                    acc
                })
                .collect();
            m.add_counter(
                "a",
                Counter {
                    labels: labels.clone(),
                    timestamps: ts.iter().map(|ms| ms * 1_000_000).collect(),
                    values,
                    windows: None,
                },
            );
            let ts = times(&mut rng);
            let values: Vec<i64> = ts.iter().map(|_| rng.below(100) as i64).collect();
            m.add_gauge(
                "g",
                Gauge {
                    labels,
                    timestamps: ts.iter().map(|ms| ms * 1_000_000).collect(),
                    values,
                    windows: None,
                },
            );
        }
        let ts = times(&mut rng);
        m.add_gauge(
            "h",
            Gauge {
                labels: Labels::default(),
                timestamps: ts.iter().map(|ms| ms * 1_000_000).collect(),
                values: vec![1; ts.len()],
                windows: None,
            },
        );
        let engine = QueryEngine::new(Arc::new(m));
        for q in queries {
            // (labels, ms) -> (start, value) from each start.
            let mut by: BTreeMap<(String, u64), Vec<(u64, f64)>> = BTreeMap::new();
            let starts: Vec<u64> = (2060..2240).step_by(2).collect();
            for &half in &starts {
                let start = half as f64 / 2.0;
                let r = engine
                    .query_range_opts(q, start, 1150.0, 1.0, &raw)
                    .unwrap();
                let QueryResult::Matrix { result } = r else {
                    panic!("{q}: not a matrix");
                };
                for s in result {
                    let mut k: Vec<_> = s.metric.iter().collect();
                    k.sort();
                    for (t, v) in s.values {
                        assert!(v.is_finite(), "seed {seed} {q} from {start}: {t} {v}");
                        by.entry((format!("{k:?}"), (t * 1000.0).round() as u64))
                            .or_default()
                            .push((half, v));
                    }
                }
            }
            for ((k, t), vs) in &by {
                let v0 = vs[0].1;
                let differ = vs
                    .iter()
                    .any(|(_, v)| (v - v0).abs() > 1e-9 * v0.abs().max(1.0));
                let from = starts.iter().filter(|s| **s * 500 <= *t).count();
                if differ || vs.len() != from {
                    failures.push(format!(
                        "seed {seed} {q} {k} at {}: {} of {from} starts, {:?}",
                        *t as f64 / 1000.0,
                        vs.len(),
                        &vs[..vs.len().min(3)]
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} differ, first: {:#?}",
        failures.len(),
        &failures[..failures.len().min(10)]
    );
}

/// The first sample a raw `irate` reads is a timing point, so the point
/// after it is held for the gap between them: from 1013 s the read starts
/// after 1000 s, 1010 s is the first sample read, and the point at 1011 s
/// is held 1 s, not to 1013 s.
#[test]
fn a_raw_irate_after_the_first_sample_read_is_held_for_its_gap() {
    const S: u64 = 1_000_000_000;
    let ts: Vec<u64> = [1000u64, 1010, 1011]
        .into_iter()
        .chain(1030..=1040)
        .collect();
    let mut m = Memory::new(1000);
    m.add_counter(
        "a",
        Counter {
            labels: Labels::default(),
            timestamps: ts.iter().map(|s| s * S).collect(),
            values: ts.iter().map(|t| t - 1000).collect(),
            windows: None,
        },
    );
    m.add_gauge(
        "h",
        Gauge {
            labels: Labels::default(),
            timestamps: (1000..=1040u64).map(|s| s * S).collect(),
            values: vec![1; 41],
            windows: None,
        },
    );
    let engine = QueryEngine::new(Arc::new(m));
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    for start in [1000.5, 1005.0, 1013.0] {
        let r = engine
            .query_range_opts("irate(a[2s]) + h", start, 1020.0, 1.0, &raw)
            .unwrap();
        let got = points_from(&r, 1013.0);
        assert!(got.is_empty(), "from {start}: {got:?}");
    }
}

/// A raw join holds a slow series across its own spacing: a gauge read
/// every 10 s beside one read every second gives a point every second,
/// since the hold bound covers two of the longest interval read.
#[test]
fn a_raw_join_holds_a_slow_series_across_its_spacing() {
    use metriken_storage::labeled::LabeledSource;
    use metriken_storage::recordings::RecordingsSource;
    const S: u64 = 1_000_000_000;
    let gauge = |interval_ms: u64, name: &str, ts: Vec<u64>| -> Arc<dyn DataSource> {
        let mut m = Memory::new(interval_ms);
        m.add_gauge(
            name,
            Gauge {
                labels: Labels::default(),
                timestamps: ts.iter().map(|s| s * S).collect(),
                values: vec![1; ts.len()],
                windows: None,
            },
        );
        Arc::new(LabeledSource::new(
            Arc::new(m),
            make_labels(&[("source", name)]),
        ))
    };
    let slow = gauge(10_000, "g", (1000..=1100).step_by(10).collect());
    let fast = gauge(1000, "h", (1000..=1100).collect());
    let engine = QueryEngine::new(Arc::new(RecordingsSource::new(vec![slow, fast])));
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    let r = engine
        .query_range_opts("sum(g) + sum(h)", 1050.0, 1080.0, 1.0, &raw)
        .unwrap();
    let times: Vec<f64> = points_from(&r, 0.0).into_iter().map(|(t, _)| t).collect();
    let want: Vec<f64> = (1050..=1080u64).map(|t| t as f64).collect();
    assert_eq!(times, want);
}

/// A raw join nested in another holds as the outer join needs from any
/// start: the read reaches back twice the hold bound for each nested join.
#[test]
fn a_nested_raw_join_is_the_same_from_any_start() {
    let gauge = |m: &mut Memory, name: &str, ms: &[u64], values: &[i64]| {
        m.add_gauge(
            name,
            Gauge {
                labels: Labels::default(),
                timestamps: ms.iter().map(|t| t * 1_000_000).collect(),
                values: values.to_vec(),
                windows: None,
            },
        );
    };
    let mut m = Memory::new(1000);
    gauge(&mut m, "a", &[994_600, 996_200, 999_000], &[1, 2, 3]);
    gauge(&mut m, "b", &[998_100, 999_000], &[10, 20]);
    gauge(&mut m, "c", &[1_000_000, 1_001_000], &[100, 100]);
    let engine = QueryEngine::new(Arc::new(m));
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    let q = "(a + b) + c";
    let late = engine
        .query_range_opts(q, 1000.0, 1001.0, 1.0, &raw)
        .unwrap();
    let early = engine
        .query_range_opts(q, 990.0, 1001.0, 1.0, &raw)
        .unwrap();
    assert_eq!(points_from(&late, 1000.0), points_from(&early, 1000.0));

    // Each nested join reads back twice the bound: here the inner join's
    // operand `a` needs its sample at 992.9 s, more than three bounds (of
    // 2 s) before a start at 1000 s.
    let mut m = Memory::new(1000);
    gauge(&mut m, "a", &[992_900, 994_500, 998_100], &[1, 2, 3]);
    gauge(&mut m, "b", &[996_000, 996_400, 998_100], &[10, 20, 30]);
    gauge(&mut m, "c", &[1_000_000, 1_001_000], &[100, 100]);
    let engine = QueryEngine::new(Arc::new(m));
    for q in ["(a + b) + c", "sum(a + b) + c", "sum(sum(a + b) + c)"] {
        let late = engine
            .query_range_opts(q, 1000.0, 1001.0, 1.0, &raw)
            .unwrap();
        let early = engine
            .query_range_opts(q, 980.0, 1001.0, 1.0, &raw)
            .unwrap();
        assert_eq!(
            points_from(&late, 1000.0),
            points_from(&early, 1000.0),
            "{q}"
        );
    }

    // Three nested joins: `a`'s sample at 988.9 s, more than five bounds
    // before the start, decides the result at 1000 s, so each join below
    // the first adds two bounds to the read, not one.
    let mut m = Memory::new(1000);
    gauge(&mut m, "a", &[988_900, 990_950, 994_500, 998_100], &[1; 4]);
    gauge(&mut m, "b", &[992_900, 994_500, 998_100], &[1; 3]);
    gauge(&mut m, "c", &[996_000, 996_400, 998_100], &[1; 3]);
    gauge(&mut m, "d", &[1_000_000, 1_001_000], &[1; 2]);
    let engine = QueryEngine::new(Arc::new(m));
    let q = "((a + b) + c) + d";
    let late = engine
        .query_range_opts(q, 1000.0, 1001.0, 1.0, &raw)
        .unwrap();
    let early = engine
        .query_range_opts(q, 980.0, 1001.0, 1.0, &raw)
        .unwrap();
    assert_eq!(
        points_from(&late, 1000.0),
        points_from(&early, 1000.0),
        "{q}"
    );
    assert!(!points_from(&early, 1000.0).is_empty(), "{q}");
}

/// A raw `irate` spans a missed tick with jitter, at any step: its pair may
/// be up to four of the counter's interval apart.
#[test]
fn a_raw_irate_spans_a_missed_tick() {
    let mut ts: Vec<u64> = (1000..=1010).map(|s| s * 1_000_000_000).collect();
    ts.extend([1_012_100_000_000, 1_013_100_000_000]);
    let values: Vec<u64> = (0..ts.len() as u64).map(|i| i * 100).collect();
    let mut m = Memory::new(1000);
    m.add_counter(
        "c",
        Counter {
            labels: Labels::default(),
            timestamps: ts,
            values,
            windows: None,
        },
    );
    // A pair of 3.5 s just before a range from 1004.5: the read reaches the
    // pair's first sample, so its point at 1003.5 is held into the range.
    let mut m2 = Memory::new(1000);
    let ts2: Vec<u64> = (990..=1000u64)
        .map(|s| s * 1_000_000_000)
        .chain([1_003_500_000_000])
        .collect();
    m2.add_counter(
        "c",
        Counter {
            labels: Labels::default(),
            values: (0..ts2.len() as u64).map(|i| i * 100).collect(),
            timestamps: ts2,
            windows: None,
        },
    );
    m2.add_gauge(
        "h",
        Gauge {
            labels: Labels::default(),
            timestamps: (990..=1010u64).map(|s| s * 1_000_000_000).collect(),
            values: vec![1; 21],
            windows: None,
        },
    );
    let engine2 = QueryEngine::new(Arc::new(m2));
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    for start in [995.0, 1004.5] {
        let r = engine2
            .query_range_opts("irate(c[1s]) + h", start, 1006.0, 1.0, &raw)
            .unwrap();
        let at = points_from(&r, 1005.0).first().copied();
        assert_eq!(at, Some((1005.0, 100.0 / 3.5 + 1.0)), "from {start}");
    }
    let engine = QueryEngine::new(Arc::new(m));
    for step in [1.0, 5.0] {
        let r = engine
            .query_range_opts("irate(c[1s])", 1005.0, 1014.0, step, &raw)
            .unwrap();
        let at = points_from(&r, 1012.0).first().copied();
        assert!(
            at.is_some_and(|(t, v)| (t - 1012.1).abs() < 1e-9 && (v - 100.0 / 2.1).abs() < 1e-6),
            "step {step}: {at:?}"
        );
    }
}

/// In raw mode, a series whose function gives no value at a sample is left
/// out of an aggregate there, directly or through a scalar op: the sum is over
/// the series that have a value.
#[test]
fn a_raw_aggregate_skips_a_series_with_no_value() {
    const S: u64 = 1_000_000_000;
    let mut m = Memory::new(1000);
    // `c{id="gap"}` misses 1040 to 1059, more than four of its interval, so
    // its irate at 1060 has no value; `c{id="full"}` reads every second.
    for (id, ts) in [
        (
            "gap",
            (1000..=1100u64)
                .filter(|s| !(1040..1060).contains(s))
                .collect::<Vec<_>>(),
        ),
        ("full", (1000..=1100u64).collect()),
    ] {
        m.add_counter(
            "c",
            Counter {
                labels: make_labels(&[("id", id)]),
                values: ts.iter().map(|s| (s - 1000) * 10).collect(),
                timestamps: ts.iter().map(|s| s * S).collect(),
                windows: None,
            },
        );
    }
    let engine = QueryEngine::new(Arc::new(m));
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    for (q, want) in [("sum(irate(c[5s]))", 10.0), ("sum(2 * irate(c[5s]))", 20.0)] {
        let r = engine
            .query_range_opts(q, 1000.0, 1070.0, 1.0, &raw)
            .unwrap();
        let at = points_from(&r, 1060.0).first().copied();
        assert_eq!(at, Some((1060.0, want)), "{q}");
    }
}

/// A `__name__="x"` matcher on a selector with no name selects `x`, with
/// any other matchers, and is how a query names a metric whose name is not
/// a PromQL identifier.
#[test]
fn a_name_matcher_names_the_selector() {
    let mut source = create_cgroup_source();
    source.add_counter(
        "svc/ops",
        Counter {
            labels: make_labels(&[("state", "user")]),
            timestamps: (0u64..3).map(|s| (1000 + s) * 1_000_000_000).collect(),
            values: (0u64..3).map(|s| s * 50).collect(),
            windows: None,
        },
    );
    let engine = QueryEngine::new(Arc::new(source));
    let range = |q: &str| {
        engine
            .query_range(q, 1001.0, 1002.0, 1.0)
            .unwrap_or_else(|e| panic!("{q}: {e}"))
    };
    // Each series' labels and values, in a fixed order.
    let series = |q: &str| {
        let QueryResult::Matrix { result } = range(q) else {
            panic!("{q}: not a matrix");
        };
        let mut out: Vec<_> = result
            .into_iter()
            .map(|m| {
                let labels: std::collections::BTreeMap<_, _> = m.metric.into_iter().collect();
                (labels, m.values)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    };
    assert_eq!(
        series(r#"sum(irate(cgroup_cpu_usage{name="/system.slice/foo.service"}[5s]))"#),
        series(
            r#"sum(irate({__name__="cgroup_cpu_usage", name="/system.slice/foo.service"}[5s]))"#
        )
    );
    let all = series("irate(cgroup_cpu_usage[5s])");
    assert_eq!(all.len(), 3);
    assert_eq!(all, series(r#"irate({__name__="cgroup_cpu_usage"}[5s])"#));
    let QueryResult::Matrix { result } = range(r#"sum(irate({__name__="svc/ops"}[5s]))"#) else {
        panic!("not a matrix");
    };
    assert_eq!(result.len(), 1);
    assert!(
        result[0].values.iter().all(|(_, v)| *v == 50.0),
        "{result:?}"
    );

    // A regex, a negation or two `__name__` matchers do not name one metric.
    for q in [
        r#"irate({__name__=~"cgroup_cpu_usage"}[5s])"#,
        r#"irate({__name__!="cgroup_cpu_usage"}[5s])"#,
        r#"irate({__name__="cgroup_cpu_usage", __name__="svc/ops"}[5s])"#,
    ] {
        assert!(engine.query_range(q, 1001.0, 1002.0, 1.0).is_err(), "{q}");
    }
}

/// In the histogram functions, which parse their selector apart from
/// PromQL, a `__name__` matcher names the selector under the same rule as
/// `name_selectors`, and anything else carrying one is refused.
#[test]
fn a_histogram_function_reads_a_name_matcher_by_the_promql_rule() {
    let engine = QueryEngine::new(Arc::new(create_hist_source(&[0, 10, 20, 30, 40])));
    let range = |q: &str| engine.query_range(q, 1001.0, 1004.0, 1.0);
    // Each series' labels and values, in a fixed order.
    let series = |q: &str| {
        let QueryResult::Matrix { result } = range(q).unwrap_or_else(|e| panic!("{q}: {e}")) else {
            panic!("{q}: not a matrix");
        };
        let mut out: Vec<_> = result
            .into_iter()
            .map(|m| {
                let labels: std::collections::BTreeMap<_, _> = m.metric.into_iter().collect();
                (labels, m.values)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    };
    for (by_matcher, by_name) in [
        (
            r#"histogram_irate({__name__="req_latency"})"#,
            "histogram_irate(req_latency)",
        ),
        (
            r#"histogram_quantiles([0.5], { __name__ = "req_latency" })"#,
            "histogram_quantiles([0.5], req_latency)",
        ),
        // `__name__` in a label's value or as part of a key is no matcher.
        (
            r#"histogram_irate(req_latency{path!="/__name__"})"#,
            "histogram_irate(req_latency)",
        ),
        (
            r#"histogram_irate(req_latency{__name__x!="1"})"#,
            "histogram_irate(req_latency)",
        ),
    ] {
        let got = series(by_matcher);
        assert!(!got.is_empty(), "{by_matcher}");
        assert_eq!(got, series(by_name), "{by_matcher}");
    }
    for q in [
        r#"histogram_mean({__name__="", __name__="req_latency"})"#,
        r#"histogram_mean({__name__="req_latency", __name__="req_latency"})"#,
        r#"histogram_mean({__name__=~"req_latency"})"#,
    ] {
        let err = range(q).expect_err(q).to_string();
        assert!(
            err.contains("accepted only as a single `__name__"),
            "{q}: {err}"
        );
    }
    // The quoted name is the whole value, comma and all.
    let err = range(r#"histogram_mean({__name__="req_latency,x"})"#)
        .expect_err("req_latency,x")
        .to_string();
    assert!(err.contains("req_latency,x"), "{err}");
    for q in [
        r#"histogram_mean({__name__="req_latency"} offset 5s)"#,
        r#"histogram_mean({__name__="req_latency"} @ 1003)"#,
    ] {
        let err = range(q).expect_err(q).to_string();
        assert!(err.contains("takes no `offset` or `@`"), "{q}: {err}");
    }
}
