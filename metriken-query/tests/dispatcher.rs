//! End-to-end tests for the catalogue-driven runtime dispatcher.
//!
//! These tests exercise `QueryEngine::query_range` with a `DispatchConfig`
//! attached, verifying:
//!   1. Queries not in the catalogue fall through to PromQL.
//!   2. Shadow-mode entries run both engines; identical outputs (the
//!      vertical-slice contract) produce no diff.
//!   3. Shadow-mode entries with backend errors are logged but do not fail
//!      the request.
//!
//! The harness uses the same `metriken-query-fixtures` parquet files as
//! the golden suite, so any divergence between `golden.rs`'s findings and
//! these tests would point at a real dispatcher bug.

use std::path::Path;
use std::sync::{Arc, Mutex};

use metriken_query::{
    Catalogue, CatalogueEntry, Diff, DispatchConfig, DispatchObserver, NoopObserver, QueryEngine,
    QueryResult, Tsdb,
};
use metriken_query_sql::DuckDbBackend;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("metriken-query-fixtures")
        .join("fixtures")
        .join(format!("{name}.parquet"))
}

fn engine_with_dispatch(
    fixture_name: &str,
    observer: Arc<dyn DispatchObserver>,
) -> QueryEngine {
    let parquet = fixture(fixture_name);
    let tsdb = Arc::new(Tsdb::load(&parquet).expect("load fixture"));
    let cfg = DispatchConfig {
        catalogue: Catalogue::embedded(),
        backend: Arc::new(DuckDbBackend::new()),
        observer,
        data_source: parquet.to_str().unwrap().to_string(),
    };
    QueryEngine::new(tsdb).with_dispatch(cfg)
}

/// Observer that records every diff for later assertion.
#[derive(Default)]
struct Capture(Mutex<Vec<String>>);

impl DispatchObserver for Capture {
    fn on_diff(&self, entry: &CatalogueEntry, _diff: &Diff) {
        self.0.lock().unwrap().push(entry.id.clone());
    }
}

#[test]
fn unmatched_query_falls_through_to_promql() {
    // `queue_depth` (a bare gauge in gauge_basic) isn't in the catalogue —
    // the dispatcher returns the PromQL output unchanged.
    let engine = engine_with_dispatch("gauge_basic", Arc::new(NoopObserver));
    let result = engine
        .query_range("queue_depth", 0.0, 10.0, 1.0)
        .expect("query");
    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix");
    };
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].values.len(), 11);
}

#[test]
fn shadow_mode_runs_both_engines_with_no_diff_on_vertical_slice() {
    // `memory_total` matches the `gauge_bare` catalogue entry, which is on
    // `mode = "shadow"` and has a SQL twin that the golden suite verifies
    // produces byte-identical output. The dispatcher should run both and
    // report no diff.
    let observer = Arc::new(Capture::default());
    let engine = engine_with_dispatch("gauge_basic", observer.clone());

    let result = engine
        .query_range("memory_total", 0.0, 10.0, 1.0)
        .expect("query");
    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix");
    };
    // PromQL output (returned by the dispatcher in shadow mode).
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].values.len(), 11);

    let diffs = observer.0.lock().unwrap();
    assert!(diffs.is_empty(), "expected no diffs, got: {diffs:?}");
}

#[test]
fn shadow_mode_logs_diff_without_failing_when_sql_diverges() {
    // Hand-build a catalogue with one entry whose SQL deliberately produces
    // wrong output, so the shadow comparison detects a diff.
    let toml = r#"
        [[query]]
        id = "diverging"
        mode = "shadow"
        promql = "memory_total"
        sql = """
            SELECT t::DOUBLE, v::DOUBLE
            FROM (VALUES
                (0.0, 1.0), (1.0, 1.0), (2.0, 1.0), (3.0, 1.0), (4.0, 1.0),
                (5.0, 1.0), (6.0, 1.0), (7.0, 1.0), (8.0, 1.0), (9.0, 1.0),
                (10.0, 1.0)
            ) AS s(t, v)
            ORDER BY t
        """
        value_columns = ["v"]
        output_metric = { __name__ = "memory_total" }
    "#;

    let parquet = fixture("gauge_basic");
    let tsdb = Arc::new(Tsdb::load(&parquet).unwrap());
    let observer = Arc::new(Capture::default());
    let cfg = DispatchConfig {
        catalogue: Catalogue::from_toml(toml).expect("parse"),
        backend: Arc::new(DuckDbBackend::new()),
        observer: observer.clone(),
        data_source: parquet.to_str().unwrap().to_string(),
    };
    let engine = QueryEngine::new(tsdb).with_dispatch(cfg);

    // Should succeed and return PromQL output (which has values 1024, not 1).
    let result = engine
        .query_range("memory_total", 0.0, 10.0, 1.0)
        .expect("shadow mode never fails on divergence");
    let QueryResult::Matrix { result } = result else {
        panic!("expected Matrix");
    };
    assert_eq!(result[0].values[0].1, 1024.0, "PromQL output preserved");

    // The observer should have seen exactly one diff for the `diverging` entry.
    let diffs = observer.0.lock().unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0], "diverging");
}
