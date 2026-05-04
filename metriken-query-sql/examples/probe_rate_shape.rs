//! Probe optimization variants for `counter_rate_bare_generic`
//! (`rate(memory_numa_local[5m])`-style queries — current ratio 6.56×).
//!
//! The current SQL has two CTEs (per_pair + rates) and a `WHERE val_0 IS
//! NOT NULL` filter that DuckDB compiles into a re-evaluation of
//! `irate_lag` over the per_pair CTE inlining.

use std::time::Instant;

use duckdb::Connection;
use metriken_query_sql::{register_all, views};

const PARQUET: &str = "/work/rezolus/site/viewer/data/AB_base.parquet";
const COL: &str = "0::114"; // memory_numa_local

fn baseline() -> String {
    format!(
        "WITH per_pair AS (
  SELECT timestamp,
    irate_lag(\"{COL}\", LAG(\"{COL}\") OVER w, timestamp - LAG(timestamp) OVER w) AS inc_0
  FROM _src
  WINDOW w AS (ORDER BY timestamp)
),
rates AS (
  SELECT timestamp,
    (SUM(inc_0) OVER w - COALESCE(FIRST_VALUE(inc_0) OVER w, 0)) / NULLIF(CAST(timestamp - FIRST_VALUE(timestamp) OVER w AS DOUBLE) / 1e9, 0) AS val_0
  FROM per_pair
  WINDOW w AS (ORDER BY timestamp ROWS BETWEEN 300 PRECEDING AND CURRENT ROW)
)
SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, val_0 AS v FROM rates WHERE val_0 IS NOT NULL"
    )
}

fn no_filter() -> String {
    // Drop WHERE val_0 IS NOT NULL — let NULL propagate to backend filter.
    format!(
        "WITH per_pair AS (
  SELECT timestamp,
    irate_lag(\"{COL}\", LAG(\"{COL}\") OVER w, timestamp - LAG(timestamp) OVER w) AS inc_0
  FROM _src
  WINDOW w AS (ORDER BY timestamp)
),
rates AS (
  SELECT timestamp,
    (SUM(inc_0) OVER w - COALESCE(FIRST_VALUE(inc_0) OVER w, 0)) / NULLIF(CAST(timestamp - FIRST_VALUE(timestamp) OVER w AS DOUBLE) / 1e9, 0) AS val_0
  FROM per_pair
  WINDOW w AS (ORDER BY timestamp ROWS BETWEEN 300 PRECEDING AND CURRENT ROW)
)
SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, val_0 AS v FROM rates"
    )
}

fn materialized_per_pair() -> String {
    // Force per_pair to materialize so the rates CTE's window doesn't
    // re-evaluate irate_lag.
    format!(
        "WITH per_pair AS MATERIALIZED (
  SELECT timestamp,
    irate_lag(\"{COL}\", LAG(\"{COL}\") OVER w, timestamp - LAG(timestamp) OVER w) AS inc_0
  FROM _src
  WINDOW w AS (ORDER BY timestamp)
),
rates AS (
  SELECT timestamp,
    (SUM(inc_0) OVER w - COALESCE(FIRST_VALUE(inc_0) OVER w, 0)) / NULLIF(CAST(timestamp - FIRST_VALUE(timestamp) OVER w AS DOUBLE) / 1e9, 0) AS val_0
  FROM per_pair
  WINDOW w AS (ORDER BY timestamp ROWS BETWEEN 300 PRECEDING AND CURRENT ROW)
)
SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, val_0 AS v FROM rates"
    )
}

fn materialized_no_filter() -> String {
    // Combined: MATERIALIZED + drop the WHERE filter.
    format!(
        "WITH per_pair AS MATERIALIZED (
  SELECT timestamp,
    irate_lag(\"{COL}\", LAG(\"{COL}\") OVER w, timestamp - LAG(timestamp) OVER w) AS inc_0
  FROM _src
  WINDOW w AS (ORDER BY timestamp)
),
rates AS (
  SELECT timestamp,
    (SUM(inc_0) OVER w - COALESCE(FIRST_VALUE(inc_0) OVER w, 0)) / NULLIF(CAST(timestamp - FIRST_VALUE(timestamp) OVER w AS DOUBLE) / 1e9, 0) AS val_0
  FROM per_pair
  WINDOW w AS (ORDER BY timestamp ROWS BETWEEN 300 PRECEDING AND CURRENT ROW)
)
SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, val_0 AS v FROM rates"
    )
}

fn fused_one_cte() -> String {
    // Single CTE: project inc_0 once, compute rate inline using two named windows.
    format!(
        "WITH rates AS (
  SELECT timestamp,
    irate_lag(\"{COL}\", LAG(\"{COL}\") OVER w_lag, timestamp - LAG(timestamp) OVER w_lag) AS inc_0
  FROM _src
  WINDOW w_lag AS (ORDER BY timestamp)
)
SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t,
       (SUM(inc_0) OVER w_sum - COALESCE(FIRST_VALUE(inc_0) OVER w_sum, 0))
       / NULLIF(CAST(timestamp - FIRST_VALUE(timestamp) OVER w_sum AS DOUBLE) / 1e9, 0) AS v
FROM rates
WINDOW w_sum AS (ORDER BY timestamp ROWS BETWEEN 300 PRECEDING AND CURRENT ROW)"
    )
}

fn time_query(conn: &Connection, sql: &str, reps: usize) -> (f64, f64, f64, usize) {
    let mut stmt = conn.prepare_cached(sql).expect("prepare");
    for _ in 0..3 {
        let mut rows = stmt.query([]).expect("query");
        let mut n = 0;
        while rows.next().unwrap().is_some() {
            n += 1;
        }
        let _ = n;
    }
    let mut samples = Vec::with_capacity(reps);
    let mut total_rows = 0;
    for _ in 0..reps {
        let t0 = Instant::now();
        let mut rows = stmt.query([]).expect("query");
        let mut n = 0;
        while rows.next().unwrap().is_some() {
            n += 1;
        }
        total_rows = n;
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    let p10 = samples[samples.len() / 10];
    let p90 = samples[samples.len() * 9 / 10];
    (p10, median, p90, total_rows)
}

fn main() {
    let conn = Connection::open_in_memory().expect("open duckdb");
    register_all(&conn).expect("register UDFs");
    let _catalog = views::ensure_views(&conn, PARQUET).expect("ensure views");

    let variants: Vec<(&str, String)> = vec![
        ("baseline (current)", baseline()),
        ("no filter (drop WHERE)", no_filter()),
        ("per_pair MATERIALIZED", materialized_per_pair()),
        ("MATERIALIZED + no filter", materialized_no_filter()),
        ("fused single-CTE (two windows)", fused_one_cte()),
    ];

    println!("# Rate-shape probe — {PARQUET}\n");
    println!("`rate(memory_numa_local[5m])` patterns. 200 reps × 3 warm-ups.\n");
    println!("| variant | p10 (ms) | median (ms) | p90 (ms) | rows |");
    println!("|---|---:|---:|---:|---:|");
    for (name, sql) in &variants {
        let (p10, median, p90, rows) = time_query(&conn, sql, 200);
        println!("| {name} | {p10:.3} | {median:.3} | {p90:.3} | {rows} |");
    }
}
