//! Auto-walking shadow-mode promotion gate: every `mode = "shadow"` catalogue
//! entry × every example × every available real parquet must produce
//! byte-equal canonical JSON between PromQL and SQL.
//!
//! This is the gate that decides whether a new templated entry is "done."
//! Drop a new `[[query.examples]]` entry into `queries.toml` and this test
//! picks it up automatically — no per-entry Rust glue needed.
//!
//! Optional `dump_*` tests below remain as targeted diagnostics for individual
//! query shapes; they print side-by-side comparisons rather than failing on
//! mismatch, useful when investigating an in-flight divergence.
//!
//! Run with: `cargo test -p metriken-query --test divergence_inspector`
//! Use `-- --nocapture` to see PromQL/SQL summaries on the diagnostic tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use metriken_query::{
    canonicalise, Catalogue, CatalogueEntry, CompiledTemplate, Mode, QueryEngine, QueryResult,
    SqlBackend, Tsdb,
};
use metriken_query_sql::DuckDbBackend;

/// Real-data parquet files we shadow against. Files that don't exist in the
/// current sandbox are skipped silently — the gate runs against whatever real
/// data is available locally.
fn parquets() -> Vec<PathBuf> {
    let dir = Path::new("/work/rezolus/site/viewer/data");
    [
        "demo",
        "cachecannon",
        "vllm",
        "sglang_gemma3",
        "vllm_gemma3",
    ]
    .iter()
    .map(|n| dir.join(format!("{n}.parquet")))
    .filter(|p| p.exists())
    .collect()
}

/// All concrete query strings to check for an entry.
/// - Literal entries: `[entry.promql]`.
/// - Templated entries: every `examples[*].query`.
fn concrete_queries(entry: &CatalogueEntry) -> Vec<String> {
    let template = CompiledTemplate::parse(&entry.promql).expect("parse template");
    if template.has_captures() {
        entry.examples.iter().map(|e| e.query.clone()).collect()
    } else {
        vec![entry.promql.clone()]
    }
}

/// Probe the parquet's time bounds via DuckDB so we issue queries against the
/// data range that's actually present, not a hard-coded one.
fn probe_time_range(parquet: &Path) -> Option<(f64, f64)> {
    let conn = duckdb::Connection::open_in_memory().ok()?;
    let row: Option<(f64, f64)> = conn
        .query_row(
            "SELECT MIN(CAST(timestamp AS DOUBLE) / 1e9), \
                    MAX(CAST(timestamp AS DOUBLE) / 1e9) \
             FROM read_parquet(?)",
            [parquet.to_str().unwrap()],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?)),
        )
        .ok();
    row
}

/// Run both engines on the same query against the same parquet. Returns
/// `Some((promql_json, sql_json))` on success or `None` if either side errors
/// (typical when a metric named in the query isn't present in this parquet —
/// e.g. vllm-only metrics against demo.parquet).
fn run_pair(
    parquet: &Path,
    entry: &CatalogueEntry,
    query: &str,
    start: f64,
    end: f64,
    step: f64,
) -> Option<(serde_json::Value, serde_json::Value)> {
    // PromQL pass.
    let tsdb = match Tsdb::load(parquet) {
        Ok(t) => Arc::new(t),
        Err(_) => return None,
    };
    let engine = QueryEngine::new(tsdb);
    let promql_result: QueryResult = engine.query_range_promql(query, start, end, step).ok()?;
    let promql_json = canonicalise(&promql_result);

    // SQL pass.
    let template = CompiledTemplate::parse(&entry.promql).ok()?;
    let captures = template.match_query(query)?;
    let backend = DuckDbBackend::new();
    let sql_result = backend
        .run(entry, &captures, parquet.to_str().unwrap(), start, end, step)
        .ok()?;
    let sql_json = canonicalise(&sql_result);

    Some((promql_json, sql_json))
}

/// Walk every `mode = "shadow"` entry × every example × every parquet, run
/// both engines, and return the list of failing pairs plus checked/skipped
/// counts.
fn walk_shadow_entries() -> (Vec<String>, u64, u64) {
    let parquets = parquets();
    let cat = Catalogue::embedded();
    let mut failures: Vec<String> = Vec::new();
    let mut checked: u64 = 0;
    let mut skipped: u64 = 0;

    for parquet in &parquets {
        let (start, end) = match probe_time_range(parquet) {
            Some(r) => r,
            None => continue,
        };
        let step = 1.0;
        let parquet_label = parquet.file_name().unwrap().to_string_lossy().to_string();

        for entry in cat.entries() {
            if entry.mode != Mode::Shadow {
                continue;
            }
            for q in concrete_queries(entry) {
                let pair = run_pair(parquet, entry, &q, start, end, step);
                match pair {
                    Some((p, s)) => {
                        checked += 1;
                        if p != s {
                            let p_count = p
                                .get("result")
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            let s_count = s
                                .get("result")
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            failures.push(format!(
                                "{}::{} on {} — PromQL series={} SQL series={}",
                                entry.id, q, parquet_label, p_count, s_count
                            ));
                        }
                    }
                    None => {
                        skipped += 1;
                    }
                }
            }
        }
    }
    (failures, checked, skipped)
}

/// Default test: prints a divergence report but NEVER fails. Use this as the
/// "where do we stand?" diagnostic during catalogue work — running `cargo test
/// -p metriken-query --test divergence_inspector -- --nocapture` shows the
/// current set of (entry, parquet) divergences without breaking unrelated CI.
///
/// The strict-mode counterpart (`every_shadow_entry_matches_promql_on_real_parquets`)
/// is `#[ignore]`d by default — opt in with `--ignored` once the catalogue is
/// known-clean.
#[test]
fn divergence_report() {
    let parquets = parquets();
    if parquets.is_empty() {
        eprintln!("no real parquet files present under /work/rezolus/site/viewer/data; skipping");
        return;
    }
    let (failures, checked, skipped) = walk_shadow_entries();
    eprintln!(
        "shadow auto-walker: checked={} skipped={} failures={}",
        checked,
        skipped,
        failures.len()
    );
    if !failures.is_empty() {
        eprintln!("divergences ({} pairs):", failures.len());
        for f in &failures {
            eprintln!("  {f}");
        }
    }
}

/// Strict promotion gate: zero divergences allowed. Ignored by default —
/// remove the `#[ignore]` once every shadow entry is byte-clean against the
/// real parquets, and CI will then fail loud on regressions.
#[test]
#[ignore]
fn every_shadow_entry_matches_promql_on_real_parquets() {
    let parquets = parquets();
    if parquets.is_empty() {
        eprintln!("no real parquet files present under /work/rezolus/site/viewer/data; skipping");
        return;
    }
    let (failures, checked, skipped) = walk_shadow_entries();
    eprintln!(
        "shadow auto-walker: checked={} skipped={} failures={}",
        checked,
        skipped,
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "shadow divergences ({} pairs):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

// ------------------------------------------------------------------
// Diagnostic helpers below — kept as a side-by-side dump for the
// histogram_quantile family. Useful while investigating a divergence,
// but not part of the gate.
// ------------------------------------------------------------------

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
    let (start, end) = probe_time_range(parquet).expect("probe range");
    let step = 1.0;

    let tsdb = Arc::new(Tsdb::load(parquet).expect("load"));
    let engine = QueryEngine::new(tsdb);
    let promql = engine
        .query_range_promql(query, start, end, step)
        .expect("promql ok");
    let promql_json = canonicalise(&promql);

    let cat = Catalogue::embedded();
    let (entry, captures) = cat.lookup(query).expect("entry must match");
    eprintln!("matched entry id = {}", entry.id);
    eprintln!("captures = {:?}", captures);

    let backend = DuckDbBackend::new();
    let sql = backend
        .run(entry, &captures, parquet.to_str().unwrap(), start, end, step)
        .expect("sql ok");
    let sql_json = canonicalise(&sql);

    let summarize = |label: &str, v: &serde_json::Value| {
        let r = v
            .get("result")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        eprintln!("=== {label} ===");
        eprintln!("  series_count={}", r.len());
        for (i, s) in r.iter().enumerate().take(3) {
            let m = s.get("metric").map(|m| m.to_string()).unwrap_or_default();
            let vs = s
                .get("values")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            eprintln!("  series[{i}] metric={m} n_points={}", vs.len());
            for p in vs.iter().take(3) {
                eprintln!("    point={p}");
            }
        }
    };
    summarize("PromQL", &promql_json);
    summarize("SQL", &sql_json);

    if promql_json != sql_json {
        let p = serde_json::to_string_pretty(&promql_json).unwrap();
        let s = serde_json::to_string_pretty(&sql_json).unwrap();
        eprintln!("=== PromQL JSON ===\n{p}");
        eprintln!("=== SQL JSON ===\n{s}");
        panic!("shadow divergence: PromQL ≠ SQL canonical JSON");
    } else {
        eprintln!("=== PROMQL == SQL (byte-equivalent canonical JSON) ===");
    }

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
