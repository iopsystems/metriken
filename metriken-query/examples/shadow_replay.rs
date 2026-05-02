//! In-process shadow replay + perf characterisation.
//!
//! For each production query (default: `tests/data/production_queries.txt`),
//! runs both engines against a single parquet, captures `promql_ms` and
//! `sql_ms`, and emits a markdown report:
//!
//! - **Coverage** — total queries, catalogue hits, misses (always zero after
//!   Phase 7.2; the count tracks it across catalogue evolution).
//! - **Correctness** — divergences observed, with first-diff snippet.
//! - **Performance** — per-catalogue-entry-id rollup of (n, promql p50/p95,
//!   sql p50/p95, sql/promql ratio) and an aggregate wall-clock comparison.
//!
//! Why in-process and not HTTP: the HTTP shell adds JSON serialisation and
//! Tokio overhead that the engines themselves don't pay; the in-process path
//! is what `divergence_inspector.rs` already uses and gives the cleanest
//! engine-vs-engine timing comparison. Real-data correctness coverage is
//! already provided by `divergence_inspector` — this binary's value is the
//! per-entry latency table and the mass-replay coverage check.
//!
//! Usage:
//!   cargo run --release --example shadow_replay -- \
//!     --parquet /work/rezolus/site/viewer/data/demo.parquet
//!   cargo run --release --example shadow_replay -- \
//!     --parquet /work/rezolus/site/viewer/data/cachecannon.parquet \
//!     --queries metriken-query/tests/data/production_queries.txt \
//!     --out /tmp/shadow_replay_report.md

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use metriken_query::{canonicalise, Catalogue, Mode, QueryEngine, SqlBackend, Tsdb};
use metriken_query_sql::DuckDbBackend;

const DEFAULT_QUERIES: &str = "metriken-query/tests/data/production_queries.txt";

#[derive(Default)]
struct Args {
    parquet: Option<PathBuf>,
    queries: Option<PathBuf>,
    out: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--parquet" => {
                args.parquet = Some(PathBuf::from(&raw[i + 1]));
                i += 2;
            }
            "--queries" => {
                args.queries = Some(PathBuf::from(&raw[i + 1]));
                i += 2;
            }
            "--out" => {
                args.out = Some(PathBuf::from(&raw[i + 1]));
                i += 2;
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: shadow_replay --parquet PATH [--queries FILE] [--out FILE]"
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

struct PerEntryStats {
    id: String,
    n: usize,
    promql_ms: Vec<f64>,
    sql_ms: Vec<f64>,
    divergences: usize,
    first_diff_summary: Option<String>,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn probe_time_range(parquet: &PathBuf) -> Option<(f64, f64)> {
    use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};
    let bytes = std::fs::read(parquet).ok()?;
    let bytes = metriken_query::Bytes::from(bytes);
    let meta = ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::default()).ok()?;
    // Quick path: load the parquet via Tsdb::load and use its time bounds.
    // Cheap because we already have the bytes — Tsdb::load re-reads parquet
    // but caches no state we'd reuse.
    drop(meta);
    let tsdb = Tsdb::load(parquet).ok()?;
    let (min_ns, max_ns) = tsdb.time_range()?;
    Some((min_ns as f64 / 1e9, max_ns as f64 / 1e9))
}

fn run_one(
    engine: &QueryEngine<Arc<Tsdb>>,
    backend: &DuckDbBackend,
    parquet: &str,
    cat: &Catalogue,
    query: &str,
    start: f64,
    end: f64,
    step: f64,
) -> Option<(String, f64, f64, bool, Option<String>)> {
    let (entry, captures) = cat.lookup(query)?;
    if entry.mode == Mode::Off {
        return None;
    }

    let t0 = Instant::now();
    let promql = engine.query_range_promql(query, start, end, step).ok()?;
    let promql_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t0 = Instant::now();
    let sql = backend
        .run(entry, &captures, parquet, start, end, step)
        .ok()?;
    let sql_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let p_json = canonicalise(&promql);
    let s_json = canonicalise(&sql);
    let diverged = p_json != s_json;
    let summary = if diverged {
        Some(short_diff(&p_json, &s_json, query))
    } else {
        None
    };
    Some((entry.id.clone(), promql_ms, sql_ms, diverged, summary))
}

fn short_diff(p: &serde_json::Value, s: &serde_json::Value, query: &str) -> String {
    let p_n = p
        .get("result")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let s_n = s
        .get("result")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    format!(
        "{} — PromQL series={} SQL series={}",
        truncate(query, 80),
        p_n,
        s_n
    )
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

fn main() -> std::io::Result<()> {
    let args = parse_args();
    let parquet = args
        .parquet
        .expect("--parquet PATH is required");
    let queries_file = args
        .queries
        .unwrap_or_else(|| PathBuf::from(DEFAULT_QUERIES));
    let out_path = args.out;

    let queries: Vec<String> = std::fs::read_to_string(&queries_file)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();

    eprintln!(
        "loaded {} queries from {}",
        queries.len(),
        queries_file.display()
    );
    eprintln!("loading parquet {} ...", parquet.display());

    let (start, end) = probe_time_range(&parquet)
        .expect("could not read parquet time range");
    let step = 1.0;

    let tsdb = Arc::new(Tsdb::load(&parquet).expect("Tsdb::load"));
    let engine = QueryEngine::new(tsdb);
    let backend = DuckDbBackend::new();
    let cat = Catalogue::embedded();

    let mut by_entry: std::collections::BTreeMap<String, PerEntryStats> = Default::default();
    let mut hits = 0usize;
    let mut misses = 0usize;
    let mut errors = 0usize;
    let mut divergences = 0usize;

    for q in &queries {
        match run_one(
            &engine, &backend, parquet.to_str().unwrap(), &cat, q, start, end, step,
        ) {
            Some((id, p_ms, s_ms, diverged, summary)) => {
                hits += 1;
                if diverged {
                    divergences += 1;
                }
                let entry_stats = by_entry
                    .entry(id.clone())
                    .or_insert_with(|| PerEntryStats {
                        id,
                        n: 0,
                        promql_ms: Vec::new(),
                        sql_ms: Vec::new(),
                        divergences: 0,
                        first_diff_summary: None,
                    });
                entry_stats.n += 1;
                entry_stats.promql_ms.push(p_ms);
                entry_stats.sql_ms.push(s_ms);
                if diverged {
                    entry_stats.divergences += 1;
                    if entry_stats.first_diff_summary.is_none() {
                        entry_stats.first_diff_summary = summary;
                    }
                }
            }
            None => {
                // Distinguish miss-vs-error: if catalogue doesn't match, miss;
                // otherwise either engine errored. Repeat lookup cheaply.
                if cat.lookup(q).is_some() {
                    errors += 1;
                } else {
                    misses += 1;
                }
            }
        }
    }

    let mut report = String::new();
    use std::fmt::Write as _;

    writeln!(report, "# Shadow replay report\n").unwrap();
    writeln!(report, "Parquet: `{}`", parquet.display()).unwrap();
    writeln!(report, "Time range: {} → {}\n", start, end).unwrap();

    writeln!(report, "## Coverage\n").unwrap();
    writeln!(report, "| metric | count |").unwrap();
    writeln!(report, "|---|---|").unwrap();
    writeln!(report, "| total queries | {} |", queries.len()).unwrap();
    writeln!(report, "| catalogue hits | {} |", hits).unwrap();
    writeln!(report, "| catalogue misses | {} |", misses).unwrap();
    writeln!(report, "| engine errors (missing metric / view) | {} |", errors).unwrap();
    writeln!(report, "| divergences | {} |\n", divergences).unwrap();

    writeln!(report, "## Correctness\n").unwrap();
    if divergences == 0 {
        writeln!(report, "Zero divergences. ✅\n").unwrap();
    } else {
        writeln!(report, "{} divergent (entry, query) pairs:\n", divergences).unwrap();
        for s in by_entry.values() {
            if s.divergences > 0 {
                if let Some(summary) = &s.first_diff_summary {
                    writeln!(report, "- **{}** (n={} divergent): {}", s.id, s.divergences, summary)
                        .unwrap();
                }
            }
        }
        writeln!(report).unwrap();
    }

    writeln!(report, "## Performance — per-entry rollup\n").unwrap();
    writeln!(
        report,
        "| id | n | promql p50/p95 (ms) | sql p50/p95 (ms) | sql/promql p50 |"
    )
    .unwrap();
    writeln!(report, "|---|---:|---:|---:|---:|").unwrap();
    let mut entries: Vec<&PerEntryStats> = by_entry.values().collect();
    entries.sort_by(|a, b| b.n.cmp(&a.n).then_with(|| a.id.cmp(&b.id)));
    for s in entries {
        let mut p_sorted = s.promql_ms.clone();
        p_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut q_sorted = s.sql_ms.clone();
        q_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = percentile(&p_sorted, 0.5);
        let p95 = percentile(&p_sorted, 0.95);
        let s50 = percentile(&q_sorted, 0.5);
        let s95 = percentile(&q_sorted, 0.95);
        let ratio = if p50 > 0.0 { s50 / p50 } else { 0.0 };
        writeln!(
            report,
            "| {} | {} | {:.2} / {:.2} | {:.2} / {:.2} | {:.2} |",
            s.id, s.n, p50, p95, s50, s95, ratio
        )
        .unwrap();
    }
    writeln!(report).unwrap();

    let total_promql_ms: f64 = by_entry.values().flat_map(|s| s.promql_ms.iter()).sum();
    let total_sql_ms: f64 = by_entry.values().flat_map(|s| s.sql_ms.iter()).sum();
    writeln!(report, "## Aggregate\n").unwrap();
    writeln!(
        report,
        "| total | wall-clock (s) | series sum |"
    )
    .unwrap();
    writeln!(report, "|---|---:|---:|").unwrap();
    writeln!(
        report,
        "| PromQL | {:.3} | {} |",
        total_promql_ms / 1000.0,
        hits
    )
    .unwrap();
    writeln!(
        report,
        "| SQL    | {:.3} | {} |",
        total_sql_ms / 1000.0,
        hits
    )
    .unwrap();
    let agg_ratio = if total_promql_ms > 0.0 {
        total_sql_ms / total_promql_ms
    } else {
        0.0
    };
    writeln!(report, "\nSQL is {:.2}× PromQL on aggregate.", agg_ratio).unwrap();

    if let Some(out) = &out_path {
        std::fs::write(out, &report)?;
        eprintln!("wrote report to {}", out.display());
    } else {
        print!("{}", report);
    }

    Ok(())
}
