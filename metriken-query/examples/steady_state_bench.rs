//! Per-query steady-state benchmark — PromQL vs DuckDB.
//!
//! For each parquet fixture (auto-discovered under
//! `/work/rezolus/site/viewer/data/` by default, including one level of
//! subdirectories like `disagg/`), runs the production query workload
//! through both engines K reps times after a warm-up pass. Cold-start
//! cost is measured separately so the per-query medians reflect steady
//! state.
//!
//! Output:
//! - **Cold-start** — per-fixture: Tsdb::load, DuckDB open / register UDFs
//!   / build views, plus a "first matching query" wall-clock for each
//!   engine to size what the user actually waits for on a fresh page load.
//! - **Per-fixture aggregate** — total wall-clock per engine across all
//!   queries × reps.
//! - **Per-entry-shape rollup** — median of per-query medians, grouped by
//!   catalogue `entry.id`. The headline table for spotting which
//!   catalogue shapes need optimization.
//! - **Top-10 worst & best per-query SQL/PromQL ratios** — with the
//!   literal query string for direct inspection.
//! - Optional CSV (long format: fixture, query, entry_id, engine, rep, ms).
//!
//! Methodology — see `/home/yurivish/.claude/plans/hello-we-are-working-glittery-nebula.md`.
//!
//! Usage:
//!   cargo run --release --example steady_state_bench -- --out /tmp/bench.md
//!
//!   cargo run --release --example steady_state_bench -- \
//!     --parquet /work/rezolus/site/viewer/data/demo.parquet \
//!     --reps 3 --out /tmp/smoke.md

use std::collections::BTreeMap;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use duckdb::Connection;
use metriken_query::{CatalogueEntry, Captures, Catalogue, Mode, QueryEngine, SqlBackend, Tsdb};
use metriken_query_sql::DuckDbBackend;

const DEFAULT_DATA_DIR: &str = "/work/rezolus/site/viewer/data";
const DEFAULT_QUERIES: &str = "metriken-query/tests/data/production_queries.txt";
const DEFAULT_REPS: usize = 10;
const DEFAULT_WARMUP_PASSES: usize = 1;

#[derive(Default)]
struct Args {
    data_dir: Option<PathBuf>,
    parquets: Vec<PathBuf>,
    queries: Option<PathBuf>,
    reps: Option<usize>,
    warmup_passes: Option<usize>,
    filter: Option<String>,
    out: Option<PathBuf>,
    csv: Option<PathBuf>,
}

fn next_value(raw: &[String], i: usize) -> String {
    raw.get(i + 1).cloned().unwrap_or_else(|| {
        eprintln!("missing value after {}", raw[i]);
        std::process::exit(2);
    })
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--data-dir" => {
                args.data_dir = Some(PathBuf::from(next_value(&raw, i)));
                i += 2;
            }
            "--parquet" => {
                args.parquets.push(PathBuf::from(next_value(&raw, i)));
                i += 2;
            }
            "--queries" => {
                args.queries = Some(PathBuf::from(next_value(&raw, i)));
                i += 2;
            }
            "--reps" => {
                args.reps = Some(
                    next_value(&raw, i)
                        .parse()
                        .expect("--reps must be a positive integer"),
                );
                i += 2;
            }
            "--warmup-passes" => {
                args.warmup_passes = Some(
                    next_value(&raw, i)
                        .parse()
                        .expect("--warmup-passes must be a non-negative integer"),
                );
                i += 2;
            }
            "--filter" => {
                args.filter = Some(next_value(&raw, i));
                i += 2;
            }
            "--out" => {
                args.out = Some(PathBuf::from(next_value(&raw, i)));
                i += 2;
            }
            "--csv" => {
                args.csv = Some(PathBuf::from(next_value(&raw, i)));
                i += 2;
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: steady_state_bench [--data-dir DIR | --parquet PATH ...]\n  \
                     [--queries FILE] [--reps N={DEFAULT_REPS}] [--warmup-passes N={DEFAULT_WARMUP_PASSES}]\n  \
                     [--filter SUBSTRING] [--out FILE] [--csv FILE]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }
    args
}

/// Find every `*.parquet` under `dir`, recursing one level (covers the
/// `disagg/` subdirectory in the Rezolus viewer data dir).
fn discover_parquets(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("parquet") {
            out.push(p);
        } else if p.is_dir() {
            if let Ok(inner) = fs::read_dir(&p) {
                for ie in inner.flatten() {
                    let ip = ie.path();
                    if ip.is_file() && ip.extension().and_then(|s| s.to_str()) == Some("parquet") {
                        out.push(ip);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Cold-start timing breakdown for one fixture.
struct ColdStart {
    parquet_size_mb: f64,
    tsdb_load_ms: f64,
    duck_open_ms: f64,
    duck_register_ms: f64,
    duck_views_ms: f64,
    /// Wall-clock of the first matching query, run on a fresh
    /// `QueryEngine` (PromQL) and a fresh `DuckDbBackend` (SQL). The
    /// SQL number is dominated by the cold-start path inside
    /// `DuckDbBackend::get_or_init`; reported because it's the latency
    /// the user actually sees on a cold open.
    first_promql_ms: Option<f64>,
    first_sql_ms: Option<f64>,
    probe_query: Option<String>,
}

/// Per-(fixture, query) measurement. The first measurement rep is
/// dropped before stats are computed, so `promql_ms`/`sql_ms` here have
/// length `reps - 1` for queries that ran cleanly.
struct QueryStat {
    query: String,
    entry_id: String,
    promql_ms: Vec<f64>,
    sql_ms: Vec<f64>,
}

/// Outcome buckets per fixture. `measured` queries have stats; the
/// remaining buckets carry just counts so the report can explain *why*
/// the bench didn't time them. The "skipped" bucket is dominated by
/// queries that target metrics absent from the fixture (e.g. GPU
/// queries against a non-GPU recording) — expected, not alarming.
struct FixtureRun {
    parquet: PathBuf,
    cold: ColdStart,
    queries: Vec<QueryStat>,
    misses: usize,
    skipped_both_err: usize,
    promql_only_err: usize,
    sql_only_err: usize,
}

/// Recorded when a fixture can't be measured at all because cold-start
/// fails (e.g. `ensure_views` panics on a malformed schema). Reported
/// in its own report section so a single broken fixture doesn't kill
/// the whole benchmark.
struct FixtureFailure {
    parquet: PathBuf,
    phase: &'static str,
    message: String,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn median(values: &[f64]) -> f64 {
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    percentile(&v, 0.5)
}

/// Match the viewer's step calculation: `max(1, floor(window / 500))`,
/// in seconds. See `/work/rezolus/site/viewer/lib/data.js:329`.
fn compute_step(start: f64, end: f64) -> f64 {
    ((end - start).max(1.0) / 500.0).floor().max(1.0)
}

fn measure_cold_start(parquet: &Path) -> Result<ColdStart, (&'static str, String)> {
    let parquet_size_mb = fs::metadata(parquet)
        .map(|m| m.len() as f64 / 1024.0 / 1024.0)
        .unwrap_or(0.0);

    let t0 = Instant::now();
    Tsdb::load(parquet).map_err(|e| ("tsdb_load", e.to_string()))?;
    let tsdb_load_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // Replicate the cold-start path from
    // metriken-query-sql/src/backend.rs:69-89 in-process so we can
    // measure each phase without parsing METRIKEN_SQL_TIMING stderr.
    let t0 = Instant::now();
    let conn = Connection::open_in_memory().map_err(|e| ("duck_open", e.to_string()))?;
    let duck_open_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t1 = Instant::now();
    metriken_query_sql::register_all(&conn).map_err(|e| ("duck_register", e.to_string()))?;
    let duck_register_ms = t1.elapsed().as_secs_f64() * 1000.0;

    let t2 = Instant::now();
    metriken_query_sql::views::ensure_views(&conn, parquet.to_str().expect("utf8 path"))
        .map_err(|e| ("duck_views", e.to_string()))?;
    let duck_views_ms = t2.elapsed().as_secs_f64() * 1000.0;

    drop(conn);

    Ok(ColdStart {
        parquet_size_mb,
        tsdb_load_ms,
        duck_open_ms,
        duck_register_ms,
        duck_views_ms,
        first_promql_ms: None,
        first_sql_ms: None,
        probe_query: None,
    })
}

/// On a *fresh* engine + backend, time the first runnable query through
/// both engines. Returns `(probe_query, first_promql_ms, first_sql_ms)`.
fn measure_first_query(
    parquet: &Path,
    queries: &[String],
    cat: &Catalogue,
) -> (Option<String>, Option<f64>, Option<f64>) {
    let tsdb = match Tsdb::load(parquet) {
        Ok(t) => Arc::new(t),
        Err(_) => return (None, None, None),
    };
    let (start, end) = match tsdb.time_range() {
        Some((min_ns, max_ns)) => (min_ns as f64 / 1e9, max_ns as f64 / 1e9),
        None => return (None, None, None),
    };
    let step = compute_step(start, end);
    let engine = QueryEngine::new(tsdb);
    let backend = DuckDbBackend::new();
    let parquet_str = parquet.to_str().expect("utf8 path");

    for q in queries {
        let (entry, captures) = match cat.lookup(q) {
            Some(x) => x,
            None => continue,
        };
        if entry.mode == Mode::Off {
            continue;
        }

        let t0 = Instant::now();
        let p_ok = engine.query_range_promql(q, start, end, step).is_ok();
        let p_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t0 = Instant::now();
        let s_ok = backend
            .run(entry, &captures, parquet_str, start, end, step)
            .is_ok();
        let s_ms = t0.elapsed().as_secs_f64() * 1000.0;

        if p_ok && s_ok {
            return (Some(q.clone()), Some(p_ms), Some(s_ms));
        }
    }
    (None, None, None)
}

struct Plan<'a> {
    query: String,
    entry: &'a CatalogueEntry,
    captures: Captures,
}

fn run_fixture(
    parquet: &Path,
    queries: &[String],
    cat: &Catalogue,
    reps: usize,
    warmup_passes: usize,
) -> Result<FixtureRun, FixtureFailure> {
    let mut cold = measure_cold_start(parquet).map_err(|(phase, message)| FixtureFailure {
        parquet: parquet.to_path_buf(),
        phase,
        message,
    })?;
    let (probe, fp, fs_ms) = measure_first_query(parquet, queries, cat);
    cold.probe_query = probe;
    cold.first_promql_ms = fp;
    cold.first_sql_ms = fs_ms;

    let tsdb = Arc::new(Tsdb::load(parquet).map_err(|e| FixtureFailure {
        parquet: parquet.to_path_buf(),
        phase: "tsdb_load",
        message: e.to_string(),
    })?);
    let (start, end) = match tsdb.time_range() {
        Some((min_ns, max_ns)) => (min_ns as f64 / 1e9, max_ns as f64 / 1e9),
        None => {
            return Err(FixtureFailure {
                parquet: parquet.to_path_buf(),
                phase: "tsdb_time_range",
                message: "fixture has no rows".to_string(),
            });
        }
    };
    let step = compute_step(start, end);
    let engine = QueryEngine::new(tsdb);
    let backend = DuckDbBackend::new();
    let parquet_str = parquet.to_str().expect("utf8 path");

    // Pre-flight: for every catalogue-matched query, run both engines
    // once. Bucket the outcome — only queries that succeeded on both
    // engines proceed into the measurement loop. The pre-flight result
    // also serves as the implicit first warm execution.
    let mut plans: Vec<Plan> = Vec::new();
    let mut misses = 0;
    let mut skipped_both_err = 0;
    let mut promql_only_err = 0;
    let mut sql_only_err = 0;
    for q in queries {
        let (entry, captures) = match cat.lookup(q) {
            Some((entry, captures)) if entry.mode != Mode::Off => (entry, captures),
            Some(_) => continue, // mode=off — skip silently
            None => {
                misses += 1;
                continue;
            }
        };

        let p_ok = engine.query_range_promql(q, start, end, step).is_ok();
        let s_ok = backend
            .run(entry, &captures, parquet_str, start, end, step)
            .is_ok();
        match (p_ok, s_ok) {
            (true, true) => plans.push(Plan {
                query: q.clone(),
                entry,
                captures,
            }),
            (false, false) => skipped_both_err += 1,
            (false, true) => promql_only_err += 1,
            (true, false) => sql_only_err += 1,
        }
    }

    // Additional warm-up passes (the pre-flight already counts as one).
    // Warms OS page cache and any internal DuckDB allocator state.
    for _ in 0..warmup_passes {
        for p in &plans {
            let _ = engine.query_range_promql(&p.query, start, end, step);
            let _ = backend.run(p.entry, &p.captures, parquet_str, start, end, step);
        }
    }

    let mut stats: Vec<QueryStat> = plans
        .iter()
        .map(|p| QueryStat {
            query: p.query.clone(),
            entry_id: p.entry.id.clone(),
            promql_ms: Vec::with_capacity(reps),
            sql_ms: Vec::with_capacity(reps),
        })
        .collect();

    // Per-rep loop: for each query, run promql then sql so a transient
    // slowdown affects both engines on the same iteration.
    for _rep in 0..reps {
        for (p, stat) in plans.iter().zip(stats.iter_mut()) {
            let t0 = Instant::now();
            let p_res = engine.query_range_promql(&p.query, start, end, step);
            let p_ms = t0.elapsed().as_secs_f64() * 1000.0;
            if p_res.is_ok() {
                stat.promql_ms.push(p_ms);
            }

            let t0 = Instant::now();
            let s_res = backend.run(p.entry, &p.captures, parquet_str, start, end, step);
            let s_ms = t0.elapsed().as_secs_f64() * 1000.0;
            if s_res.is_ok() {
                stat.sql_ms.push(s_ms);
            }
        }
    }

    // Drop the first measurement rep from each query (steady state only),
    // and drop any query where either engine produced no measurements
    // (intermittent failure post-pre-flight, very rare).
    for s in &mut stats {
        if s.promql_ms.len() > 1 {
            s.promql_ms.remove(0);
        }
        if s.sql_ms.len() > 1 {
            s.sql_ms.remove(0);
        }
    }
    stats.retain(|s| !s.promql_ms.is_empty() && !s.sql_ms.is_empty());

    Ok(FixtureRun {
        parquet: parquet.to_path_buf(),
        cold,
        queries: stats,
        misses,
        skipped_both_err,
        promql_only_err,
        sql_only_err,
    })
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n).collect();
        format!("{head}…")
    }
}

fn render_report(
    runs: &[FixtureRun],
    failures: &[FixtureFailure],
    reps: usize,
    warmup_passes: usize,
    query_count: usize,
) -> String {
    let mut out = String::new();

    writeln!(out, "# Steady-state bench — PromQL vs DuckDB\n").unwrap();
    writeln!(out, "**Reps per query**: {reps} (first rep dropped from stats)").unwrap();
    writeln!(out, "**Warm-up passes**: {warmup_passes}").unwrap();
    writeln!(out, "**Queries in workload**: {query_count}").unwrap();
    writeln!(
        out,
        "**Fixtures**: {} measured, {} failed cold-start\n",
        runs.len(),
        failures.len()
    )
    .unwrap();

    if !failures.is_empty() {
        writeln!(out, "## Fixtures that failed cold-start\n").unwrap();
        writeln!(out, "| fixture | phase | message |").unwrap();
        writeln!(out, "|---|---|---|").unwrap();
        for f in failures {
            writeln!(
                out,
                "| `{}` | `{}` | {} |",
                f.parquet.display(),
                f.phase,
                truncate(&f.message, 200),
            )
            .unwrap();
        }
        writeln!(out).unwrap();
    }

    // ---------- Cold-start ----------
    writeln!(out, "## Cold-start (per fixture)\n").unwrap();
    writeln!(
        out,
        "| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|").unwrap();
    for r in runs {
        let name = r
            .parquet
            .strip_prefix(DEFAULT_DATA_DIR)
            .unwrap_or(&r.parquet)
            .display();
        writeln!(
            out,
            "| `{}` | {:.1} | {:.1} ms | {:.1} ms | {:.1} ms | {:.1} ms | {} | {} |",
            name,
            r.cold.parquet_size_mb,
            r.cold.tsdb_load_ms,
            r.cold.duck_open_ms,
            r.cold.duck_register_ms,
            r.cold.duck_views_ms,
            r.cold
                .first_promql_ms
                .map(|x| format!("{x:.1} ms"))
                .unwrap_or_else(|| "—".into()),
            r.cold
                .first_sql_ms
                .map(|x| format!("{x:.1} ms"))
                .unwrap_or_else(|| "—".into()),
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    if let Some(probe) = runs.iter().find_map(|r| r.cold.probe_query.as_ref()) {
        writeln!(
            out,
            "_first-query probe used `{}` where it ran cleanly._\n",
            truncate(probe, 100),
        )
        .unwrap();
    }

    // ---------- Per-fixture aggregate ----------
    writeln!(out, "## Per-fixture aggregate (steady state)\n").unwrap();
    writeln!(
        out,
        "Total wall-clock summed across every measured query × rep, per engine."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|---:|").unwrap();
    for r in runs {
        let p_total: f64 = r.queries.iter().flat_map(|s| s.promql_ms.iter()).sum();
        let s_total: f64 = r.queries.iter().flat_map(|s| s.sql_ms.iter()).sum();
        let ratio = if p_total > 0.0 { s_total / p_total } else { f64::NAN };
        let name = r
            .parquet
            .strip_prefix(DEFAULT_DATA_DIR)
            .unwrap_or(&r.parquet)
            .display();
        writeln!(
            out,
            "| `{}` | {} | {} | {} | {} | {} | {:.3} | {:.3} | {:.2}× |",
            name,
            r.queries.len(),
            r.skipped_both_err,
            r.promql_only_err,
            r.sql_only_err,
            r.misses,
            p_total / 1000.0,
            s_total / 1000.0,
            ratio,
        )
        .unwrap();
    }
    writeln!(
        out,
        "\n_\"skipped (both err)\" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. \"promql-only err\" or \"sql-only err\" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._\n"
    )
    .unwrap();

    // ---------- Per-entry-shape rollup, per fixture ----------
    writeln!(out, "## Per-entry-shape rollup\n").unwrap();
    writeln!(
        out,
        "For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.\n"
    )
    .unwrap();
    for r in runs {
        let name = r
            .parquet
            .strip_prefix(DEFAULT_DATA_DIR)
            .unwrap_or(&r.parquet)
            .display();
        writeln!(out, "### `{name}`\n").unwrap();

        let mut by_entry: BTreeMap<&str, Vec<&QueryStat>> = BTreeMap::new();
        for s in &r.queries {
            by_entry.entry(s.entry_id.as_str()).or_default().push(s);
        }
        if by_entry.is_empty() {
            writeln!(out, "_no measured queries_\n").unwrap();
            continue;
        }
        writeln!(
            out,
            "| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |"
        )
        .unwrap();
        writeln!(out, "|---|---:|---:|---:|---:|---:|").unwrap();
        let mut rows: Vec<(String, usize, usize, f64, f64, f64)> = Vec::new();
        for (id, items) in &by_entry {
            let n_queries = items.len();
            let n_runs: usize = items.iter().map(|s| s.promql_ms.len()).sum();
            let p_medians: Vec<f64> = items.iter().map(|s| median(&s.promql_ms)).collect();
            let s_medians: Vec<f64> = items.iter().map(|s| median(&s.sql_ms)).collect();
            let p = median(&p_medians);
            let s = median(&s_medians);
            let ratio = if p > 0.0 { s / p } else { f64::NAN };
            rows.push((id.to_string(), n_queries, n_runs, p, s, ratio));
        }
        rows.sort_by(|a, b| {
            b.5.partial_cmp(&a.5)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.0.cmp(&b.0))
        });
        for (id, nq, nr, p, s, ratio) in rows {
            writeln!(
                out,
                "| `{id}` | {nq} | {nr} | {p:.2} | {s:.2} | {ratio:.2}× |",
            )
            .unwrap();
        }
        writeln!(out).unwrap();
    }

    // ---------- Top-N worst & best per-query ratios, per fixture ----------
    writeln!(out, "## Top-10 ratios per fixture\n").unwrap();
    for r in runs {
        let name = r
            .parquet
            .strip_prefix(DEFAULT_DATA_DIR)
            .unwrap_or(&r.parquet)
            .display();
        let mut rows: Vec<(f64, f64, f64, &str, &str)> = r
            .queries
            .iter()
            .map(|s| {
                let p = median(&s.promql_ms);
                let q = median(&s.sql_ms);
                let ratio = if p > 0.0 { q / p } else { f64::NAN };
                (ratio, p, q, s.entry_id.as_str(), s.query.as_str())
            })
            .collect();
        rows.retain(|(r, _, _, _, _)| r.is_finite());
        if rows.is_empty() {
            continue;
        }
        rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        writeln!(out, "### `{name}` — worst SQL/PromQL\n").unwrap();
        writeln!(out, "| ratio | PromQL ms | SQL ms | entry | query |").unwrap();
        writeln!(out, "|---:|---:|---:|---|---|").unwrap();
        for (ratio, p, q, id, query) in rows.iter().take(10) {
            writeln!(
                out,
                "| {:.2}× | {:.2} | {:.2} | `{}` | `{}` |",
                ratio,
                p,
                q,
                id,
                truncate(query, 100),
            )
            .unwrap();
        }
        writeln!(out).unwrap();

        writeln!(out, "### `{name}` — best SQL/PromQL (SQL wins)\n").unwrap();
        writeln!(out, "| ratio | PromQL ms | SQL ms | entry | query |").unwrap();
        writeln!(out, "|---:|---:|---:|---|---|").unwrap();
        for (ratio, p, q, id, query) in rows.iter().rev().take(10) {
            writeln!(
                out,
                "| {:.2}× | {:.2} | {:.2} | `{}` | `{}` |",
                ratio,
                p,
                q,
                id,
                truncate(query, 100),
            )
            .unwrap();
        }
        writeln!(out).unwrap();
    }

    out
}

fn write_csv(runs: &[FixtureRun], path: &Path) -> std::io::Result<()> {
    let mut f = fs::File::create(path)?;
    writeln!(f, "fixture,query,entry_id,engine,rep,ms")?;
    for r in runs {
        let fixture = r.parquet.display().to_string();
        for s in &r.queries {
            // CSV-escape the query string: wrap in quotes and double any
            // embedded quotes (queries here have no newlines).
            let q_csv = format!("\"{}\"", s.query.replace('"', "\"\""));
            for (rep, ms) in s.promql_ms.iter().enumerate() {
                writeln!(f, "{fixture},{q_csv},{},promql,{rep},{ms}", s.entry_id)?;
            }
            for (rep, ms) in s.sql_ms.iter().enumerate() {
                writeln!(f, "{fixture},{q_csv},{},sql,{rep},{ms}", s.entry_id)?;
            }
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let args = parse_args();

    let data_dir = args
        .data_dir
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR));
    let parquets = if !args.parquets.is_empty() {
        args.parquets
    } else {
        let v = discover_parquets(&data_dir);
        if v.is_empty() {
            eprintln!("no *.parquet files found under {}", data_dir.display());
            std::process::exit(2);
        }
        v
    };

    let queries_file = args
        .queries
        .unwrap_or_else(|| PathBuf::from(DEFAULT_QUERIES));
    let raw: Vec<String> = fs::read_to_string(&queries_file)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    let queries: Vec<String> = if let Some(filter) = args.filter.as_ref() {
        raw.into_iter().filter(|q| q.contains(filter)).collect()
    } else {
        raw
    };
    if queries.is_empty() {
        eprintln!("query workload is empty (filter excluded everything?)");
        std::process::exit(2);
    }

    let reps = args.reps.unwrap_or(DEFAULT_REPS);
    let warmup_passes = args.warmup_passes.unwrap_or(DEFAULT_WARMUP_PASSES);

    eprintln!(
        "loaded {} queries from {}",
        queries.len(),
        queries_file.display()
    );
    eprintln!(
        "benchmarking {} parquet fixtures, reps={reps}, warmup_passes={warmup_passes}",
        parquets.len()
    );

    let cat = Catalogue::embedded();
    let mut runs = Vec::new();
    let mut failures: Vec<FixtureFailure> = Vec::new();
    let t_total = Instant::now();
    for p in &parquets {
        eprintln!("--- {} ---", p.display());
        let t0 = Instant::now();
        match run_fixture(p, &queries, &cat, reps, warmup_passes) {
            Ok(run) => {
                let elapsed = t0.elapsed().as_secs_f64();
                eprintln!(
                    "    cold: tsdb_load={:.0}ms duck(open={:.0} reg={:.0} views={:.0}) first_promql={} first_sql={}",
                    run.cold.tsdb_load_ms,
                    run.cold.duck_open_ms,
                    run.cold.duck_register_ms,
                    run.cold.duck_views_ms,
                    run.cold.first_promql_ms.map(|x| format!("{x:.0}ms")).unwrap_or_else(|| "—".into()),
                    run.cold.first_sql_ms.map(|x| format!("{x:.0}ms")).unwrap_or_else(|| "—".into()),
                );
                eprintln!(
                    "    measured={} skipped(both)={} promql_only_err={} sql_only_err={} miss={} elapsed={:.1}s",
                    run.queries.len(),
                    run.skipped_both_err,
                    run.promql_only_err,
                    run.sql_only_err,
                    run.misses,
                    elapsed,
                );
                runs.push(run);
            }
            Err(failure) => {
                eprintln!(
                    "    FAILED in {} after {:.1}s: {}",
                    failure.phase,
                    t0.elapsed().as_secs_f64(),
                    failure.message,
                );
                failures.push(failure);
            }
        }
    }
    eprintln!("--- total elapsed {:.1}s ---", t_total.elapsed().as_secs_f64());

    let report = render_report(&runs, &failures, reps, warmup_passes, queries.len());
    if let Some(out) = args.out.as_ref() {
        fs::write(out, &report)?;
        eprintln!("wrote report to {}", out.display());
    } else {
        print!("{report}");
    }
    if let Some(csv) = args.csv.as_ref() {
        write_csv(&runs, csv)?;
        eprintln!("wrote csv to {}", csv.display());
    }

    Ok(())
}
