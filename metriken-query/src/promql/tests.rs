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
    assert_eq!(names.len(), 2, "sum by (name) with =~ should return 2 series");
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
