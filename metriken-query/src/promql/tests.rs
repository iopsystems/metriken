use std::collections::HashSet;
use std::sync::Arc;

use crate::labels::Labels;
use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::memory::Memory;
use crate::types::{Counter, Gauge, Histogram, HistogramSnapshot};

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
        source.add_counter("cgroup_cpu_usage", Counter { labels, timestamps, values });
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
fn test_windowed_rate_counter_reset() {
    let source = Arc::new(create_counter_reset_source());
    let engine = QueryEngine::new(source);

    let result = engine
        .query_range("rate(reset_counter[5s])", 1004.0, 1004.0, 1.0)
        .unwrap();

    let all_values = get_matrix_values(&result);
    assert_eq!(all_values.len(), 1);
    assert_eq!(all_values[0].len(), 1);

    let rate = all_values[0][0].1;
    assert!(
        (rate - 87.5).abs() < 1e-6,
        "Expected rate 87.5, got {}",
        rate
    );
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
fn test_rate_vs_irate_differ_with_reset() {
    let source = Arc::new(create_counter_reset_source());
    let engine = QueryEngine::new(source);

    let rate_result = engine
        .query_range("rate(reset_counter[5s])", 1004.0, 1004.0, 1.0)
        .unwrap();
    let irate_result = engine
        .query_range("irate(reset_counter[5s])", 1004.0, 1004.0, 1.0)
        .unwrap();

    let rate_val = get_matrix_values(&rate_result)[0][0].1;
    let irate_val = get_matrix_values(&irate_result)[0][0].1;

    assert!(
        (rate_val - 87.5).abs() < 1e-6,
        "Expected rate 87.5, got {}",
        rate_val
    );
    assert!(
        (irate_val - 100.0).abs() < 1e-6,
        "Expected irate 100.0, got {}",
        irate_val
    );
    assert!(
        (rate_val - irate_val).abs() > 1.0,
        "rate and irate should differ with counter reset"
    );
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
        },
    );
    source.add_gauge(
        "mem_available",
        Gauge {
            labels: Labels::default(),
            timestamps: ts.clone(),
            values: (0i64..5).map(|s| 800 - s * 100).collect(),
        },
    );
    source.add_gauge(
        "mem_reserved",
        Gauge {
            labels: Labels::default(),
            timestamps: ts,
            values: vec![50; 5],
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
            },
        );
    }

    source.add_gauge(
        "cpu_cores",
        Gauge {
            labels: Labels::default(),
            timestamps: ts,
            values: vec![8, 8],
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
                    HistogramSnapshot { index: vec![], count: vec![] },
                    HistogramSnapshot { index: vec![10], count: vec![1u64] },
                    HistogramSnapshot { index: vec![10], count: vec![2u64] },
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
        HistogramSnapshot { index: vec![], count: vec![] }
    } else {
        HistogramSnapshot { index: vec![10], count: vec![n] }
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
        let snapshots: Vec<HistogramSnapshot> =
            cumulative_per_cpu.iter().map(|&c| hist_snap_cumulative(c)).collect();
        source.add_histogram("req_latency", Histogram { labels, config, timestamps, snapshots });
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
        Err(QueryError::MetricNotFound(_)) => {}
        other => panic!("expected MetricNotFound (single delta → null), got {other:?}"),
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
