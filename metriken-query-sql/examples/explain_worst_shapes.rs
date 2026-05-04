//! Dump EXPLAIN + EXPLAIN ANALYZE for the worst-performing shapes.
//!
//! Usage:
//!   cargo run --release --example explain_worst_shapes -- \
//!       [PARQUET_PATH] [--shape SHAPE_ID]...
//!
//! Defaults to the AB_base fixture (where per-shape ratios are measured) and
//! all six worst shapes. Writes a markdown report to stdout (also dumps
//! per-shape generated SQL).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use duckdb::Connection;
use metriken_query::{Catalogue, Tsdb};
use metriken_query_sql::{register_all, views, wide_form};

const DEFAULT_PARQUET: &str = "/work/rezolus/site/viewer/data/AB_base.parquet";
const DEFAULT_QUERIES: &str = "/work/metriken/metriken-query/tests/data/production_queries.txt";

/// Catalogue entry IDs we want to profile (worst-performing shapes from the
/// steady-state bench). Concrete queries are discovered automatically by
/// scanning the production query list.
const WORST_SHAPES: &[&str] = &[
    "counter_irate_ratio_with_labels",
    "counter_rate_bare_generic",
    "counter_ratio_scaled",
    "counter_irate_total_mul",
    "softirq_irate_total_by_kind",
    "counter_irate_sum_with_labels",
];

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut parquet: Option<PathBuf> = None;
    let mut shapes: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--shape" {
            i += 1;
            shapes.push(args[i].clone());
        } else if !a.starts_with('-') {
            parquet = Some(PathBuf::from(a));
        }
        i += 1;
    }
    let parquet = parquet.unwrap_or_else(|| PathBuf::from(DEFAULT_PARQUET));

    let cat = Catalogue::embedded();
    let conn = Connection::open_in_memory().expect("open duckdb");
    register_all(&conn).expect("register UDFs");
    let catalog = views::ensure_views(&conn, parquet.to_str().unwrap()).expect("ensure_views");

    // Tsdb load is only needed to compute the time range for parity, not
    // strictly for EXPLAIN — but we want the SQL to bind to a concrete time
    // window matching what the bench would run.
    let tsdb = Arc::new(Tsdb::load(&parquet).expect("tsdb load"));
    let (start_ns, end_ns) = tsdb.time_range().expect("time range");
    let start = start_ns as f64 / 1e9;
    let end = end_ns as f64 / 1e9;
    let step = ((end - start) / 500.0).floor().max(1.0);

    println!("# EXPLAIN report — {}", parquet.display());
    println!();
    println!("Time range: [{start:.2}, {end:.2}], step = {step}\n");

    // Read the production query corpus; each line is one PromQL.
    let raw = fs::read_to_string(DEFAULT_QUERIES).expect("read production_queries.txt");
    let production: Vec<String> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();

    let target_shapes: Vec<&str> = if shapes.is_empty() {
        WORST_SHAPES.to_vec()
    } else {
        shapes.iter().map(|s| s.as_str()).collect::<Vec<_>>().into_iter().collect()
    };

    // For each target shape, scan production queries and pick the first
    // concrete one that (a) catalogues to this shape and (b) yields
    // non-empty wide-form SQL on this fixture.
    let mut targets: Vec<(String, String, String)> = Vec::new(); // (shape_id, query, sql)
    for shape_id in &target_shapes {
        let mut found = false;
        for q in &production {
            let Some((entry, captures)) = cat.lookup(q) else {
                continue;
            };
            if entry.id != *shape_id {
                continue;
            }
            let Some(sql) = wide_form::try_generate(entry, &captures, &catalog) else {
                continue;
            };
            // Skip the empty-SQL fallback emitted when no metric column
            // matches.
            if sql.contains("WHERE FALSE") {
                continue;
            }
            targets.push((shape_id.to_string(), q.clone(), sql));
            found = true;
            break;
        }
        if !found {
            println!("(no production query for `{shape_id}` matches non-empty SQL on this fixture)\n");
        }
    }

    for (shape_id, query, sql) in &targets {
        println!("---\n## `{shape_id}`\n");
        println!("PromQL: `{query}`\n");
        println!("Generated SQL:\n```sql\n{sql}\n```\n");

        // Time the SQL warm. Run 50 times after a warm-up so we get a
        // stable median per shape — the per-call cost is ~ms range so
        // single-shot timing is noisy.
        for _ in 0..5 {
            let _ = exec_query(&conn, sql);
        }
        let mut samples: Vec<f64> = Vec::with_capacity(50);
        for _ in 0..50 {
            let t0 = Instant::now();
            let _ = exec_query(&conn, sql);
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = samples[samples.len() / 2];
        let p10 = samples[samples.len() / 10];
        let p90 = samples[samples.len() * 9 / 10];
        println!("Warm exec (50x): median {median:.3} ms, p10 {p10:.3} ms, p90 {p90:.3} ms\n");

        // EXPLAIN — operator tree with cardinality estimates.
        match dump_explain(&conn, sql, false) {
            Ok(s) => {
                println!("### EXPLAIN\n```\n{s}\n```\n");
            }
            Err(e) => println!("EXPLAIN failed: {e}\n"),
        }

        // EXPLAIN ANALYZE — actual operator timings.
        match dump_explain(&conn, sql, true) {
            Ok(s) => {
                println!("### EXPLAIN ANALYZE\n```\n{s}\n```\n");
            }
            Err(e) => println!("EXPLAIN ANALYZE failed: {e}\n"),
        }
    }

    // Silence unused warnings for fields we deliberately captured for parity.
    let _ = (start, end, step);
}

fn exec_query(conn: &Connection, sql: &str) -> duckdb::Result<usize> {
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query([])?;
    let mut n = 0usize;
    while rows.next()?.is_some() {
        n += 1;
    }
    Ok(n)
}

fn dump_explain(conn: &Connection, sql: &str, analyze: bool) -> duckdb::Result<String> {
    let prefix = if analyze { "EXPLAIN ANALYZE " } else { "EXPLAIN " };
    let wrapped = format!("{prefix}{sql}");
    let mut stmt = conn.prepare(&wrapped)?;
    let mut rows = stmt.query([])?;
    let mut out = String::new();
    while let Some(r) = rows.next()? {
        // EXPLAIN returns two columns: explain_key, explain_value.
        let key: String = r.get(0)?;
        let value: String = r.get(1)?;
        out.push_str(&format!("[{key}]\n{value}\n"));
    }
    Ok(out)
}
