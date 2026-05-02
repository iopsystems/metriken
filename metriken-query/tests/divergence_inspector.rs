//! Diagnostic test: run a templated catalogue query against demo.parquet via
//! BOTH PromQL and SQL, dump both canonical JSON outputs, and print the diff.
//! Not a regression test — used to investigate shadow-mode divergences caught
//! during end-to-end curl testing.
//!
//! Run with: `cargo test -p metriken-query --test divergence_inspector -- --nocapture`

use std::path::Path;
use std::sync::Arc;

use metriken_query::{
    canonicalise, CompiledTemplate, QueryEngine, SqlBackend, Tsdb,
};
use metriken_query_sql::DuckDbBackend;

#[test]
fn dump_histogram_quantile_diff() {
    run_diff_check("histogram_quantile(0.5, scheduler_runqueue_latency)");
}

#[test]
fn p99_matches() {
    run_diff_check("histogram_quantile(0.99, scheduler_runqueue_latency)");
}

#[test]
fn whitespace_variant_matches() {
    run_diff_check("histogram_quantile(0.5,scheduler_runqueue_latency)");
}

#[test]
fn syscall_latency_multi_op_series() {
    run_diff_check("histogram_quantile(0.5, syscall_latency)");
}

fn run_diff_check(query: &str) {
    let parquet = Path::new("/work/rezolus/site/viewer/data/demo.parquet");
    if !parquet.exists() {
        eprintln!("demo.parquet not present at {}; skipping", parquet.display());
        return;
    }
    eprintln!("\n>>> {query}");
    // Time range matching demo.parquet (~1768956638..1768956939 in seconds).
    let start = 1768956638.0;
    let end = 1768956939.0;
    let step = 1.0;

    // --- PromQL pass ---
    let tsdb = Arc::new(Tsdb::load(parquet).expect("load"));
    let engine = QueryEngine::new(tsdb);
    let promql = engine
        .query_range_promql(query, start, end, step)
        .expect("promql ok");
    let promql_json = canonicalise(&promql);

    // --- SQL pass via the catalogue's templated entry ---
    let cat = metriken_query::Catalogue::embedded();
    let (entry, captures) = cat.lookup(query).expect("entry must match");
    eprintln!("matched entry id = {}", entry.id);
    eprintln!("captures = {:?}", captures);

    let backend = DuckDbBackend::new();
    let sql = backend
        .run(entry, &captures, parquet.to_str().unwrap(), start, end, step)
        .expect("sql ok");
    let sql_json = canonicalise(&sql);

    // Brief side-by-side: series-count, first label-set, first 3 points.
    let summarize = |label: &str, v: &serde_json::Value| {
        let r = v.get("result").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        eprintln!("=== {label} ===");
        eprintln!("  series_count={}", r.len());
        for (i, s) in r.iter().enumerate().take(3) {
            let m = s.get("metric").map(|m| m.to_string()).unwrap_or_default();
            let vs = s.get("values").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            eprintln!("  series[{i}] metric={m} n_points={}", vs.len());
            for p in vs.iter().take(3) {
                eprintln!("    point={p}");
            }
        }
    };
    summarize("PromQL", &promql_json);
    summarize("SQL", &sql_json);

    // Full byte-equivalence check. After the snap fix, these MUST match.
    if promql_json != sql_json {
        let p = serde_json::to_string_pretty(&promql_json).unwrap();
        let s = serde_json::to_string_pretty(&sql_json).unwrap();
        eprintln!("=== PromQL JSON ===\n{p}");
        eprintln!("=== SQL JSON ===\n{s}");
        panic!("shadow divergence: PromQL ≠ SQL canonical JSON");
    } else {
        eprintln!("=== PROMQL == SQL (byte-equivalent canonical JSON) ===");
    }

    // Spot-check: compare the rendered SQL string to confirm interpolation.
    let template = CompiledTemplate::parse(&entry.promql).unwrap();
    let caps2 = template.match_query(query).unwrap();
    let rendered_sql = metriken_query_sql::interp::interpolate(
        entry.sql.as_ref().unwrap(),
        &caps2,
        parquet.to_str().unwrap(),
    )
    .unwrap();
    eprintln!("=== rendered SQL ===\n{rendered_sql}");
}
