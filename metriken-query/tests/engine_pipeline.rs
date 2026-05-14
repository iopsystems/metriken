//! End-to-end Engine pipeline tests against baked-in fixture parquets.
//!
//! Exercises the 5-step `Engine::query_range` pipeline documented at
//! `metriken-query/src/harness/engine.rs:1-18`:
//!   Catalogue::lookup → describe_parquet → translate::try_generate →
//!   run_sql → project::run → QueryResult.
//!
//! Each test asserts the *shape* of the output (Matrix vs Heatmap,
//! series count, samples non-empty) rather than exact values — exact
//! values are pinned by the snapshot tests in
//! `tests/translate_snapshots.rs` and the macro/UDF unit tests
//! elsewhere. This file's job is "does the wiring connect end-to-end
//! for a representative entry of each output shape".
//!
//! Covers gaps 6 and 7 from /work/coverage_audit.md (project.rs and
//! engine.rs had zero direct tests; the pipeline now has them).

#![cfg(feature = "harness")]

use std::path::PathBuf;

use metriken_query::harness::{Engine, EngineError};
use metriken_query::QueryResult;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("metriken-query-fixtures")
        .join("fixtures")
        .join(format!("{name}.parquet"))
}

#[test]
fn counter_irate_returns_matrix_with_one_series() {
    // counter_basic.parquet has a single monotonic counter series
    // `requests` with 11 timestamps. `irate(requests[5m])` matches
    // entry `counter_irate_basic` (literal-form). Result should be a
    // Matrix with exactly one series and 10 samples (first sample
    // returns NULL since there's no LAG — filtered by the projector).
    let path = fixture("counter_basic");
    assert!(path.exists(), "fixture missing at {}", path.display());
    let engine = Engine::new(path.to_string_lossy().into_owned()).expect("engine new");

    let result = engine
        .query_range("irate(requests[5m])", 0.0, 10.0, 1.0)
        .expect("query_range ok");

    match result {
        QueryResult::Matrix { result } => {
            assert_eq!(result.len(), 1, "expected exactly one series");
            let series = &result[0];
            assert!(!series.values.is_empty(), "series has no samples");
            // First-row LAG is NULL → dropped. 11 input rows → 10 output rows.
            assert_eq!(series.values.len(), 10, "expected 10 rate samples");
            // All samples should be positive (counter is monotonic).
            for (t, v) in &series.values {
                assert!(*t > 0.0, "t={t} should be > 0");
                assert!(*v > 0.0, "v={v} should be > 0 for monotonic counter");
            }
        }
        other => panic!("expected Matrix, got {:?}", std::mem::discriminant(&other)),
    }
}

#[test]
fn histogram_quantile_returns_matrix_with_quantile_label() {
    // histogram_basic.parquet has one histogram series `request_latency`
    // with events at two values. `histogram_quantile(0.5,
    // request_latency)` matches entry `histogram_quantile_with_reset`
    // (literal). Result should be a Matrix tagged with quantile=0.5.
    let path = fixture("histogram_basic");
    assert!(path.exists(), "fixture missing at {}", path.display());
    let engine = Engine::new(path.to_string_lossy().into_owned()).expect("engine new");

    let result = engine
        .query_range("histogram_quantile(0.5, request_latency)", 0.0, 10.0, 1.0)
        .expect("query_range ok");

    match result {
        QueryResult::Matrix { result } => {
            // Result must carry the quantile=0.5 label per the entry's
            // output_metric block in queries.toml:
            //   output_metric = { __name__ = "request_latency", quantile = "0.5" }
            assert!(!result.is_empty(), "expected at least one series");
            let series = &result[0];
            assert_eq!(
                series.metric.get("__name__").map(String::as_str),
                Some("request_latency"),
                "metric name should be request_latency"
            );
            assert_eq!(
                series.metric.get("quantile").map(String::as_str),
                Some("0.5"),
                "quantile label should be `0.5`"
            );
        }
        other => panic!(
            "expected Matrix (heatmap shape is for histogram_heatmap_generic, not _quantile), \
             got {:?}",
            std::mem::discriminant(&other)
        ),
    }
}

#[test]
fn unrecognized_query_returns_no_catalogue_match_error() {
    // The catalogue is a closed set — queries that don't match any
    // template are a hard error, not a fallback. Pipeline step 1 fails.
    let path = fixture("counter_basic");
    let engine = Engine::new(path.to_string_lossy().into_owned()).expect("engine new");

    let err = engine
        .query_range("absolutely_no_template_matches_this()", 0.0, 10.0, 1.0)
        .expect_err("expected NoCatalogueMatch");

    match err {
        EngineError::NoCatalogueMatch(_) => {}
        other => panic!("expected NoCatalogueMatch, got {other:?}"),
    }
}

#[test]
fn engine_new_with_missing_file_errors_cleanly() {
    // Catches regression in the engine's file-open path (read_introspection
    // → Parquet metadata load). Should fail with an EngineError, not panic.
    // `Engine` doesn't implement Debug, so unpack the Result manually.
    let result = Engine::new("/nonexistent/path/to/missing.parquet".to_string());
    let err = match result {
        Ok(_) => panic!("expected open to fail on missing file"),
        Err(e) => e,
    };
    // Any EngineError::Parquet is acceptable; the test asserts the
    // error path is reached at all rather than panicking.
    match err {
        EngineError::Parquet(_) => {}
        other => panic!("expected Parquet error, got {other:?}"),
    }
}

#[test]
fn matrix_with_no_label_columns_uses_single_series_fast_path() {
    // counter_basic has zero label columns on its `requests` series.
    // The matrix() projector's "label_names.is_empty()" fast path
    // (project.rs:111) bypasses the BTreeMap series grouping. This
    // test asserts the path is reached by verifying the single-series
    // output shape — multi-series fixtures would surface here as
    // result.len() > 1.
    let path = fixture("counter_basic");
    let engine = Engine::new(path.to_string_lossy().into_owned()).expect("engine new");
    let result = engine
        .query_range("rate(requests[5m])", 0.0, 10.0, 1.0)
        .expect("rate ok");
    match result {
        QueryResult::Matrix { result } => {
            assert_eq!(
                result.len(),
                1,
                "fast path should produce exactly one series, got {}",
                result.len()
            );
        }
        other => panic!("expected Matrix, got {:?}", std::mem::discriminant(&other)),
    }
}
