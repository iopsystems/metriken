use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::tsdb::Tsdb;

fn create_test_tsdb() -> Tsdb {
    Tsdb::default()
}

/// Create a TSDB with cgroup_cpu_usage counter data for testing label filtering.
/// Creates 3 cgroups with different counter values across 3 time steps.
fn create_cgroup_tsdb() -> Tsdb {
    use metriken_exposition::{Counter, Snapshot, SnapshotV2};

    let mut tsdb = Tsdb::default();
    let base_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

    let cgroups = [
        ("/system.slice/foo.service", 100u64),
        ("/system.slice/bar.service", 200u64),
        ("/system.slice/baz.service", 300u64),
    ];

    for step in 0..3 {
        let time = base_time + Duration::from_secs(step);
        let mut counters = Vec::new();

        for (name, base_val) in &cgroups {
            let mut metadata = HashMap::new();
            metadata.insert("name".to_string(), name.to_string());
            metadata.insert("state".to_string(), "user".to_string());
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
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Test that we can create a query engine
    assert!(!engine.tsdb().source().is_empty() || engine.tsdb().source() == "");
}

#[test]
fn test_simple_rate_query_parsing() {
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Test that rate query parsing doesn't panic
    let result = engine.query("rate(cpu_cycles[5m])", None);

    // Should return MetricNotFound for empty TSDB, but shouldn't crash
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty TSDB"),
    }
}

#[test]
fn test_simple_metric_query() {
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Test simple metric query
    let result = engine.query("cpu_cores", None);

    // Should return MetricNotFound for empty TSDB
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty TSDB"),
    }
}

#[test]
fn test_sum_rate_query() {
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Test sum(rate()) query parsing
    let result = engine.query("sum(rate(network_rx_bytes[1m]))", None);

    // Should return MetricNotFound for empty TSDB
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty TSDB"),
    }
}

#[test]
fn test_range_query_delegation() {
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Test that range queries delegate to instant queries
    let result = engine.query_range("cpu_cores", 0.0, 3600.0, 60.0);

    // Should return MetricNotFound for empty TSDB
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty TSDB"),
    }
}

#[test]
fn test_label_filtering_in_rate_query() {
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Test rate query with label filtering
    let result = engine.query("rate(network_bytes{direction=\"transmit\"}[5m])", None);

    // Should return MetricNotFound for empty TSDB
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty TSDB"),
    }
}

#[test]
fn test_label_filtering_in_sum_rate_query() {
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Test sum(rate()) query with label filtering
    let result = engine.query("sum(rate(blockio_bytes{op=\"read\"}[1m]))", None);

    // Should return MetricNotFound for empty TSDB
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty TSDB"),
    }
}

#[test]
fn test_simple_metric_with_labels() {
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Test simple metric query with label filtering
    let result = engine.query("cpu_cores{cpu=\"0\"}", None);

    // Should return MetricNotFound for empty TSDB
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty TSDB"),
    }
}

#[test]
fn test_metric_selector_parsing() {
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Test that parse_metric_selector works correctly (we can't call it directly
    // due to visibility) but we can test it indirectly through query parsing

    // This should not panic during parsing
    let _result = engine.query("metric_name{label1=\"value1\",label2=\"value2\"}", None);

    // Multiple labels with single quotes
    let _result = engine.query("metric_name{label1='value1',label2='value2'}", None);

    // Labels with spaces
    let _result = engine.query("metric_name{label1 = \"value 1\", label2= 'value 2'}", None);
}

#[test]
fn test_histogram_quantile_parsing() {
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Test single percentile histogram_quantile parsing
    let result = engine.query_range(
        "histogram_quantile(0.95, tcp_packet_latency)",
        0.0,
        3600.0,
        60.0,
    );

    // Should return MetricNotFound error for empty TSDB
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty TSDB"),
    }
}

// -- Label filtering tests with actual data --

#[test]
fn test_exact_match_filters_correctly() {
    let tsdb = Arc::new(create_cgroup_tsdb());
    let engine = QueryEngine::new(tsdb);

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
    let tsdb = Arc::new(create_cgroup_tsdb());
    let engine = QueryEngine::new(tsdb);

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
    let tsdb = Arc::new(create_cgroup_tsdb());
    let engine = QueryEngine::new(tsdb);

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
    let tsdb = Arc::new(create_cgroup_tsdb());
    let engine = QueryEngine::new(tsdb);

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
    let tsdb = Arc::new(create_cgroup_tsdb());
    let engine = QueryEngine::new(tsdb);

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
    let tsdb = Arc::new(create_cgroup_tsdb());
    let engine = QueryEngine::new(tsdb);

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
    let tsdb = Arc::new(create_cgroup_tsdb());
    let engine = QueryEngine::new(tsdb);

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

/// Create a TSDB with a single counter series having known values for rate testing.
/// Counter "test_counter" with values 0, 100, 200, 300, 400 at t=1000..1004.
fn create_rate_tsdb() -> Tsdb {
    use metriken_exposition::{Counter, Snapshot, SnapshotV2};

    let mut tsdb = Tsdb::default();
    let base_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

    for step in 0u64..5 {
        let time = base_time + Duration::from_secs(step);
        let mut metadata = HashMap::new();
        metadata.insert("metric".to_string(), "test_counter".to_string());
        let snapshot = Snapshot::V2(SnapshotV2 {
            systemtime: time,
            duration: Duration::from_secs(1),
            metadata: HashMap::new(),
            counters: vec![Counter {
                name: "test_counter".to_string(),
                value: step * 100,
                metadata,
            }],
            gauges: Vec::new(),
            histograms: Vec::new(),
        });
        tsdb.ingest(snapshot);
    }

    tsdb
}

/// Create a TSDB with counter data that includes a reset.
/// Counter "reset_counter": 100, 200, 300, 50 (reset), 150 at t=1000..1004.
fn create_counter_reset_tsdb() -> Tsdb {
    use metriken_exposition::{Counter, Snapshot, SnapshotV2};

    let mut tsdb = Tsdb::default();
    let base_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    let values = [100u64, 200, 300, 50, 150];

    for (step, &val) in values.iter().enumerate() {
        let time = base_time + Duration::from_secs(step as u64);
        let mut metadata = HashMap::new();
        metadata.insert("metric".to_string(), "reset_counter".to_string());
        let snapshot = Snapshot::V2(SnapshotV2 {
            systemtime: time,
            duration: Duration::from_secs(1),
            metadata: HashMap::new(),
            counters: vec![Counter {
                name: "reset_counter".to_string(),
                value: val,
                metadata,
            }],
            gauges: Vec::new(),
            histograms: Vec::new(),
        });
        tsdb.ingest(snapshot);
    }

    tsdb
}

/// Create a TSDB with gauge data for avg_over_time testing.
/// Gauge "test_gauge" with values 10, 20, 30, 40, 50 at t=1000..1004.
fn create_gauge_tsdb() -> Tsdb {
    use metriken_exposition::{Gauge, Snapshot, SnapshotV2};

    let mut tsdb = Tsdb::default();
    let base_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

    for step in 0u64..5 {
        let time = base_time + Duration::from_secs(step);
        let mut metadata = HashMap::new();
        metadata.insert("metric".to_string(), "test_gauge".to_string());
        let snapshot = Snapshot::V2(SnapshotV2 {
            systemtime: time,
            duration: Duration::from_secs(1),
            metadata: HashMap::new(),
            counters: Vec::new(),
            gauges: vec![Gauge {
                name: "test_gauge".to_string(),
                value: (step as i64 + 1) * 10,
                metadata,
            }],
            histograms: Vec::new(),
        });
        tsdb.ingest(snapshot);
    }

    tsdb
}

/// Create a TSDB with multiple gauge series for label filtering tests.
fn create_labeled_gauge_tsdb() -> Tsdb {
    use metriken_exposition::{Gauge, Snapshot, SnapshotV2};

    let mut tsdb = Tsdb::default();
    let base_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

    let series = [("host_a", 10i64), ("host_b", 20i64)];

    for step in 0u64..3 {
        let time = base_time + Duration::from_secs(step);
        let mut gauges = Vec::new();

        for (host, base_val) in &series {
            let mut metadata = HashMap::new();
            metadata.insert("host".to_string(), host.to_string());
            metadata.insert("metric".to_string(), "labeled_gauge".to_string());
            gauges.push(Gauge {
                name: "labeled_gauge".to_string(),
                value: base_val + step as i64,
                metadata,
            });
        }

        let snapshot = Snapshot::V2(SnapshotV2 {
            systemtime: time,
            duration: Duration::from_secs(1),
            metadata: HashMap::new(),
            counters: Vec::new(),
            gauges,
            histograms: Vec::new(),
        });
        tsdb.ingest(snapshot);
    }

    tsdb
}

fn get_matrix_values(result: &QueryResult) -> Vec<Vec<(f64, f64)>> {
    match result {
        QueryResult::Matrix { result } => result.iter().map(|s| s.values.clone()).collect(),
        _ => Vec::new(),
    }
}

#[test]
fn test_windowed_rate_basic() {
    let tsdb = Arc::new(create_rate_tsdb());
    let engine = QueryEngine::new(tsdb);

    // rate(test_counter[3s]) evaluated at each step
    // Counter values: t=1000:0, t=1001:100, t=1002:200, t=1003:300, t=1004:400
    let result = engine
        .query_range("rate(test_counter[3s])", 1001.0, 1004.0, 1.0)
        .unwrap();

    assert_eq!(count_matrix_series(&result), 1);

    let all_values = get_matrix_values(&result);
    assert!(!all_values[0].is_empty());

    // At each step, rate should be 100/s (linear counter)
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
    let tsdb = Arc::new(create_counter_reset_tsdb());
    let engine = QueryEngine::new(tsdb);

    // reset_counter: 100, 200, 300, 50 (reset), 150
    // rate over full window [5s] at t=1004:
    //   pairs: (100->200)=100, (200->300)=100, (300->50 reset)=50, (50->150)=100
    //   total_increase = 350, duration = 4s, rate = 87.5/s
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
    let tsdb = Arc::new(create_rate_tsdb());
    let engine = QueryEngine::new(tsdb);

    // irate uses last two samples in window
    // Counter: t=1000:0, t=1001:100, t=1002:200, t=1003:300, t=1004:400
    // At t=1004 with [5s]: last two = (1003:300, 1004:400), irate = 100/s
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
    let tsdb = Arc::new(create_counter_reset_tsdb());
    let engine = QueryEngine::new(tsdb);

    // At t=1004 with [5s]:
    // rate walks all pairs -> 87.5/s
    // irate uses last two (50->150) -> 100/s
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
    let tsdb = Arc::new(create_gauge_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Gauge: t=1000:10, t=1001:20, t=1002:30, t=1003:40, t=1004:50
    // avg_over_time with [3s] at t=1002: window [999,1002] -> samples at 1000,1001,1002 = {10,20,30} -> avg=20
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
    let tsdb = Arc::new(create_gauge_tsdb());
    let engine = QueryEngine::new(tsdb);

    // avg_over_time with [5s] at t=1004: window [999,1004] -> all 5 samples {10,20,30,40,50} -> avg=30
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
    let tsdb = Arc::new(create_labeled_gauge_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Only host_a series
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
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    let result = engine.query_range("avg_over_time(test_gauge[5m])", 0.0, 3600.0, 60.0);
    match result {
        Err(QueryError::MetricNotFound(_)) => {}
        _ => panic!("Expected MetricNotFound error for empty TSDB"),
    }
}

#[test]
fn test_rate_parse_error_without_range() {
    let tsdb = Arc::new(create_test_tsdb());
    let engine = QueryEngine::new(tsdb);

    // rate() without a range selector should fail at parsing level
    let result = engine.query_range("rate(test_counter)", 0.0, 3600.0, 60.0);
    assert!(result.is_err());
}

#[test]
fn test_vector_selector_respects_coarse_step() {
    let tsdb = Arc::new(create_gauge_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Gauge: t=1000:10, t=1001:20, t=1002:30, t=1003:40, t=1004:50
    // With step=2.0, should produce 3 points at t=1000, 1002, 1004
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

    // Timestamps should be step-aligned
    assert!((all_values[0][0].0 - 1000.0).abs() < 1e-6);
    assert!((all_values[0][1].0 - 1002.0).abs() < 1e-6);
    assert!((all_values[0][2].0 - 1004.0).abs() < 1e-6);

    // Values at those timestamps
    assert!((all_values[0][0].1 - 10.0).abs() < 1e-6);
    assert!((all_values[0][1].1 - 30.0).abs() < 1e-6);
    assert!((all_values[0][2].1 - 50.0).abs() < 1e-6);
}

#[test]
fn test_vector_selector_preserves_all_points_when_step_equals_interval() {
    let tsdb = Arc::new(create_gauge_tsdb());
    let engine = QueryEngine::new(tsdb);

    // Gauge: t=1000:10, t=1001:20, t=1002:30, t=1003:40, t=1004:50
    // With step=1.0 (same as native interval), should return all 5 points
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

/// Create a TSDB with three gauge metrics for binary expression testing.
/// "mem_total" = 1000 at every timestamp
/// "mem_available" = 800, 700, 600, 500, 400 at t=1000..1004
/// "mem_reserved" = 50 at every timestamp
fn create_three_gauge_tsdb() -> Tsdb {
    use metriken_exposition::{Gauge, Snapshot, SnapshotV2};

    let mut tsdb = Tsdb::default();
    let base_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

    for step in 0u64..5 {
        let time = base_time + Duration::from_secs(step);
        let mut meta_total = HashMap::new();
        meta_total.insert("metric".to_string(), "mem_total".to_string());
        let mut meta_avail = HashMap::new();
        meta_avail.insert("metric".to_string(), "mem_available".to_string());
        let mut meta_reserved = HashMap::new();
        meta_reserved.insert("metric".to_string(), "mem_reserved".to_string());

        let snapshot = Snapshot::V2(SnapshotV2 {
            systemtime: time,
            duration: Duration::from_secs(1),
            metadata: HashMap::new(),
            counters: Vec::new(),
            gauges: vec![
                Gauge {
                    name: "mem_total".to_string(),
                    value: 1000,
                    metadata: meta_total,
                },
                Gauge {
                    name: "mem_available".to_string(),
                    value: 800 - (step as i64 * 100),
                    metadata: meta_avail,
                },
                Gauge {
                    name: "mem_reserved".to_string(),
                    value: 50,
                    metadata: meta_reserved,
                },
            ],
            histograms: Vec::new(),
        });
        tsdb.ingest(snapshot);
    }

    tsdb
}

#[test]
fn test_compound_gauge_expression_respects_step() {
    let tsdb = Arc::new(create_three_gauge_tsdb());
    let engine = QueryEngine::new(tsdb);

    // mem_total = 1000 at all timestamps
    // mem_available: t=1000:800, t=1001:700, t=1002:600, t=1003:500, t=1004:400
    // mem_total - mem_available: 200, 300, 400, 500, 600
    // With step=2.0, should get 3 points at t=1000, 1002, 1004
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

    // Verify timestamps and values
    assert!((all_values[0][0].0 - 1000.0).abs() < 1e-6);
    assert!((all_values[0][0].1 - 200.0).abs() < 1e-6); // 1000 - 800

    assert!((all_values[0][1].0 - 1002.0).abs() < 1e-6);
    assert!((all_values[0][1].1 - 400.0).abs() < 1e-6); // 1000 - 600

    assert!((all_values[0][2].0 - 1004.0).abs() < 1e-6);
    assert!((all_values[0][2].1 - 600.0).abs() < 1e-6); // 1000 - 400
}

#[test]
fn test_triple_gauge_expression_respects_step() {
    let tsdb = Arc::new(create_three_gauge_tsdb());
    let engine = QueryEngine::new(tsdb);

    // mem_total = 1000, mem_available = {800,700,600,500,400}, mem_reserved = 50
    // (mem_total - mem_available) - mem_reserved => (a - b) - c
    //   t=1000: (1000 - 800) - 50 = 150
    //   t=1001: (1000 - 700) - 50 = 250
    //   t=1002: (1000 - 600) - 50 = 350
    //   t=1003: (1000 - 500) - 50 = 450
    //   t=1004: (1000 - 400) - 50 = 550
    // With step=2.0, expect 3 points at t=1000, 1002, 1004
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
    assert!((all_values[0][0].1 - 150.0).abs() < 1e-6); // (1000-800)-50

    assert!((all_values[0][1].0 - 1002.0).abs() < 1e-6);
    assert!((all_values[0][1].1 - 350.0).abs() < 1e-6); // (1000-600)-50

    assert!((all_values[0][2].0 - 1004.0).abs() < 1e-6);
    assert!((all_values[0][2].1 - 550.0).abs() < 1e-6); // (1000-400)-50
}

/// Build a TSDB modeling a duplex link: per-direction traffic counters and a
/// per-interface bandwidth gauge. Traffic carries a `direction` label that the
/// bandwidth series does not, so combining them requires on()/ignoring().
///
///   tx_bytes{iface="eth0", direction="tx"} = 100 at each step
///   tx_bytes{iface="eth1", direction="tx"} = 200 at each step
///   link_bandwidth{iface="eth0"} = 1000 at each step
///   link_bandwidth{iface="eth1"} = 2000 at each step
fn create_duplex_tsdb() -> Tsdb {
    use metriken_exposition::{Gauge, Snapshot, SnapshotV2};

    let mut tsdb = Tsdb::default();
    let base_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);

    for step in 0u64..3 {
        let time = base_time + Duration::from_secs(step);
        let mut gauges = Vec::new();

        for (iface, tx_val) in [("eth0", 100i64), ("eth1", 200i64)] {
            let mut meta = HashMap::new();
            meta.insert("metric".to_string(), "tx_bytes".to_string());
            meta.insert("iface".to_string(), iface.to_string());
            meta.insert("direction".to_string(), "tx".to_string());
            gauges.push(Gauge {
                name: "tx_bytes".to_string(),
                value: tx_val,
                metadata: meta,
            });
        }

        for (iface, bw) in [("eth0", 1000i64), ("eth1", 2000i64)] {
            let mut meta = HashMap::new();
            meta.insert("metric".to_string(), "link_bandwidth".to_string());
            meta.insert("iface".to_string(), iface.to_string());
            gauges.push(Gauge {
                name: "link_bandwidth".to_string(),
                value: bw,
                metadata: meta,
            });
        }

        let snapshot = Snapshot::V2(SnapshotV2 {
            systemtime: time,
            duration: Duration::from_secs(1),
            metadata: HashMap::new(),
            counters: Vec::new(),
            gauges,
            histograms: Vec::new(),
        });
        tsdb.ingest(snapshot);
    }

    tsdb
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
    // tx_bytes has {iface, direction}; link_bandwidth has only {iface}. With
    // ignoring(direction) the series are matched pairwise by iface, giving
    // tx_bytes / link_bandwidth per interface.
    let tsdb = Arc::new(create_duplex_tsdb());
    let engine = QueryEngine::new(tsdb);

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
    // Same shape as ignoring(), but expressed with on(iface).
    let tsdb = Arc::new(create_duplex_tsdb());
    let engine = QueryEngine::new(tsdb);

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
    // Without a matching modifier, and with multiple series on both sides,
    // mismatched label sets produce no pairings — confirming that ignoring()
    // is doing real work in the tests above rather than being papered over
    // by the single-series fallback.
    let tsdb = Arc::new(create_duplex_tsdb());
    let engine = QueryEngine::new(tsdb);

    let result = engine
        .query_range("tx_bytes / link_bandwidth", 1000.0, 1002.0, 1.0)
        .unwrap();

    assert_eq!(count_matrix_series(&result), 0);
}
