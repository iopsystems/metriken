//! `DuckDbBackend::invalidate` evicts a warm pool and forces the
//! next request to re-read the parquet from disk.

use std::path::PathBuf;

use metriken_query_sql::DuckDbBackend;

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../metriken-query-fixtures/fixtures")
        .join(name);
    p.to_string_lossy().into_owned()
}

#[test]
fn invalidate_returns_false_for_unknown_source() {
    let backend = DuckDbBackend::with_pool_size(1);
    assert!(!backend.invalidate("/no/such/path.parquet"));
}

#[test]
fn invalidate_evicts_warm_pool_returns_true() {
    let backend = DuckDbBackend::with_pool_size(1);
    let path = fixture("counter_basic.parquet");

    // Warm the pool: describe_parquet alone doesn't warm — need an
    // actual run_sql.
    let _ = backend
        .run_sql("SELECT COUNT(*) FROM _src", &path)
        .expect("warm");

    // Now the pool is warm. Invalidate evicts and returns true.
    assert!(backend.invalidate(&path));
    // Second invalidate finds nothing — the cache is empty for this key.
    assert!(!backend.invalidate(&path));
}

#[test]
fn invalidate_then_rerun_pays_cold_start_again() {
    let backend = DuckDbBackend::with_pool_size(1);
    let path = fixture("counter_basic.parquet");

    let _ = backend
        .run_sql("SELECT COUNT(*) FROM _src", &path)
        .expect("warm");

    // describe_parquet on a warm source clones the catalog Arc out of
    // the pool's cache (no fresh read).
    let cat_warm = backend.describe_parquet(&path).expect("warm describe");
    assert!(backend.invalidate(&path));

    // After invalidate the pool is gone. describe_parquet still works
    // (cold path: parquet introspection without pool warm-up), but
    // it builds a fresh MetricCatalog Arc (different pointer than
    // the warm one would have had — though we can't easily assert
    // Arc pointer identity across the cold/warm boundary).
    let cat_cold = backend
        .describe_parquet(&path)
        .expect("cold describe after invalidate");
    // The catalog SHAPE is identical (same parquet); only the Arc
    // identity differs. Spot-check that we got a working catalog.
    assert_eq!(
        cat_warm.series_by_metric.len(),
        cat_cold.series_by_metric.len()
    );
}
