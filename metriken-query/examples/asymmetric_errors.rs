//! Triage tool for `sql_only_err` / `promql_only_err` from the steady-state
//! bench. For each catalogue-matched query, runs both engines once and
//! prints the asymmetric failure cases, grouped by category, with the
//! engine error message attached so we can decide whether each one is
//! a real bug or expected behavior.
//!
//! Usage:
//!   cargo run --release --example asymmetric_errors -- \
//!     --parquet /work/rezolus/site/viewer/data/demo.parquet

use std::path::PathBuf;
use std::sync::Arc;

use metriken_query::{Catalogue, Mode, QueryEngine, SqlBackend, Tsdb};
use metriken_query_sql::DuckDbBackend;

const DEFAULT_QUERIES: &str = "metriken-query/tests/data/production_queries.txt";

fn main() -> std::io::Result<()> {
    let mut parquet: Option<PathBuf> = None;
    let mut queries_file: Option<PathBuf> = None;
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--parquet" => {
                parquet = Some(PathBuf::from(&raw[i + 1]));
                i += 2;
            }
            "--queries" => {
                queries_file = Some(PathBuf::from(&raw[i + 1]));
                i += 2;
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }
    let parquet = parquet.expect("--parquet required");
    let queries_file = queries_file.unwrap_or_else(|| PathBuf::from(DEFAULT_QUERIES));

    let queries: Vec<String> = std::fs::read_to_string(&queries_file)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();

    let tsdb = Arc::new(Tsdb::load(&parquet).expect("Tsdb::load"));
    let (start, end) = tsdb
        .time_range()
        .map(|(min_ns, max_ns)| (min_ns as f64 / 1e9, max_ns as f64 / 1e9))
        .expect("no rows");
    let step = ((end - start).max(1.0) / 500.0).floor().max(1.0);
    let engine = QueryEngine::new(tsdb);
    let backend = DuckDbBackend::new();
    let cat = Catalogue::embedded();
    let parquet_str = parquet.to_str().unwrap();

    let mut sql_only_err: Vec<(String, String, String)> = Vec::new();
    let mut promql_only_err: Vec<(String, String, String)> = Vec::new();

    for q in &queries {
        let (entry, captures) = match cat.lookup(q) {
            Some(x) => x,
            None => continue,
        };
        if entry.mode == Mode::Off || entry.sql.is_none() {
            continue;
        }
        let p = engine.query_range_promql(q, start, end, step);
        let s = backend.run(entry, &captures, parquet_str, start, end, step);
        match (&p, &s) {
            (Ok(_), Err(e)) => {
                sql_only_err.push((entry.id.clone(), q.clone(), e.to_string()));
            }
            (Err(e), Ok(_)) => {
                promql_only_err.push((entry.id.clone(), q.clone(), e.to_string()));
            }
            _ => {}
        }
    }

    println!("# {}\n", parquet.display());
    println!("## sql_only_err — PromQL OK, SQL Err (n={})\n", sql_only_err.len());
    for (id, q, e) in &sql_only_err {
        println!("- entry=`{id}` query=`{q}`");
        println!("  err: {}", e.lines().next().unwrap_or(e));
    }
    println!("\n## promql_only_err — SQL OK, PromQL Err (n={})\n", promql_only_err.len());
    for (id, q, e) in &promql_only_err {
        println!("- entry=`{id}` query=`{q}`");
        println!("  err: {}", e.lines().next().unwrap_or(e));
    }

    Ok(())
}
