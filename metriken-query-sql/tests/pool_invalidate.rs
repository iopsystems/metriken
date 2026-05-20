//! `DuckDbBackend::invalidate` evicts a warm pool and forces the
//! next request to re-read the parquet from disk.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

/// `invalidate` evicts the cache entry, but in-flight queries that
/// already cloned the `Arc<ConnState>` (inside `run_sql::get_or_init`)
/// keep their pool slots alive until the query finishes. This pins
/// that contract by hammering the backend with concurrent queries
/// while a peer thread repeatedly invalidates: every query must
/// either succeed or return a well-formed error. The harness has
/// caught zero failures across long runs locally; the assertion here
/// is for regression — a future refactor that prematurely drops the
/// `Arc` (e.g. swapping the HashMap for one that synchronously
/// terminates entries) would surface as panics or `Err` returns.
#[test]
fn invalidate_concurrent_with_queries_never_drops_in_flight_connections() {
    let backend = Arc::new(DuckDbBackend::with_pool_size(4));
    let path = fixture("counter_basic.parquet");

    // Warm-start once so the first iteration in the workers doesn't
    // race with itself on `get_or_init`.
    backend.run_sql("SELECT 1", &path).expect("warm");

    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    for _ in 0..4 {
        let backend = backend.clone();
        let path = path.clone();
        let stop = stop.clone();
        workers.push(std::thread::spawn(move || -> Result<usize, String> {
            let mut n = 0usize;
            while !stop.load(Ordering::Relaxed) {
                backend
                    .run_sql("SELECT COUNT(*) FROM _src", &path)
                    .map_err(|e| format!("{e:?}"))?;
                n += 1;
            }
            Ok(n)
        }));
    }

    // Invalidate aggressively while the workers query. Each call
    // either evicts (returns true) or finds nothing to evict because
    // a worker hasn't re-warmed yet (returns false). Both outcomes
    // are valid; we're testing the absence of cross-thread fallout.
    for _ in 0..200 {
        backend.invalidate(&path);
    }
    stop.store(true, Ordering::Relaxed);

    let totals: Vec<usize> = workers
        .into_iter()
        .map(|h| h.join().expect("worker panic").expect("worker error"))
        .collect();
    assert!(totals.iter().sum::<usize>() > 0, "workers ran no queries");
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
