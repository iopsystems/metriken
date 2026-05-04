//! Probe whether the `WHERE val_0 IS NOT NULL OR ... OR val_N IS NOT NULL`
//! filter on rates CTEs forces DuckDB to recompute `irate_lag` in the
//! FILTER predicate (vs. reading the projected column).
//!
//! Hypothesis: DuckDB inlines the rates CTE — the FILTER ends up calling
//! `irate_lag(...)` once per column per row, doubling the UDF work.
//!
//! We test 3 alternatives against the baseline:
//!  - `MATERIALIZED` CTE (forces materialization — no inlining).
//!  - Filter on the first-row sentinel `LAG(timestamp) OVER w IS NOT NULL`
//!    (drops just the first row, no UDF recomputation).
//!  - Drop the filter entirely; downstream `WHERE v IS NOT NULL`
//!    handles all-null rows (after computing the SUM = NULLIF over
//!    COALESCE+sum-of-nulls).
//!
//! Run:  cargo run --release --example probe_filter_cost

use std::time::Instant;

use duckdb::Connection;
use metriken_query_sql::{register_all, views};

const PARQUET: &str = "/work/rezolus/site/viewer/data/AB_base.parquet";

// Pulled verbatim from explain_worst_shapes output for
// `counter_irate_ratio_with_labels` against AB_base. The query has 30 cols
// per lane (cgroup_cpu_instructions / cgroup_cpu_cycles minus rezolus.service).
const COLS_A: &[&str] = &[
    "0::15x1", "0::15x11", "0::15x18", "0::15x2", "0::15x20", "0::15x21",
    "0::15x23", "0::15x26", "0::15x27", "0::15x28", "0::15x29", "0::15x30",
    "0::15x31", "0::15x33", "0::15x34", "0::15x35", "0::15x36", "0::15x37",
    "0::15x40", "0::15x42", "0::15x43", "0::15x44", "0::15x45", "0::15x46",
    "0::15x47", "0::15x48", "0::15x49", "0::15x50", "0::15x51", "0::15x6",
];
const COLS_B: &[&str] = &[
    "0::14x1", "0::14x11", "0::14x18", "0::14x2", "0::14x20", "0::14x21",
    "0::14x23", "0::14x26", "0::14x27", "0::14x28", "0::14x29", "0::14x30",
    "0::14x31", "0::14x33", "0::14x34", "0::14x35", "0::14x36", "0::14x37",
    "0::14x40", "0::14x42", "0::14x43", "0::14x44", "0::14x45", "0::14x46",
    "0::14x47", "0::14x48", "0::14x49", "0::14x50", "0::14x51", "0::14x6",
];

fn rates_cte(name: &str, cols: &[&str], materialized: bool) -> String {
    let mat = if materialized { " MATERIALIZED" } else { "" };
    let lags: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "    irate_lag(\"{c}\", LAG(\"{c}\") OVER w, timestamp - LAG(timestamp) OVER w) AS val_{i}"
            )
        })
        .collect();
    format!(
        "{name} AS{mat} (\n  SELECT timestamp,\n{}\n  FROM _src\n  WINDOW w AS (ORDER BY timestamp)\n)",
        lags.join(",\n")
    )
}

fn sum_filter_or(prefix: &str, n: usize, with_filter: bool) -> String {
    let coalesces: Vec<String> = (0..n).map(|i| format!("COALESCE(val_{i}, 0)")).collect();
    let nulls: Vec<String> = (0..n).map(|i| format!("val_{i} IS NOT NULL")).collect();
    let sum_expr = coalesces.join(" + ");
    let where_clause = if with_filter {
        format!(" WHERE {}", nulls.join(" OR "))
    } else {
        String::new()
    };
    format!(
        "{prefix}_summed AS (\n  SELECT timestamp, ({sum_expr}) AS v FROM {prefix}_rates{where_clause}\n)"
    )
}

fn sum_filter_lag_sentinel(prefix: &str, n: usize) -> String {
    // Drop the first row of the partition by checking that LAG() gave us
    // a previous timestamp. No UDF recomputation; just looks at val_0's
    // structure indirectly via a separately-projected column. For this we
    // need the rates CTE to also project the prev_ts column.
    let coalesces: Vec<String> = (0..n).map(|i| format!("COALESCE(val_{i}, 0)")).collect();
    let sum_expr = coalesces.join(" + ");
    format!(
        "{prefix}_summed AS (\n  SELECT timestamp, ({sum_expr}) AS v FROM {prefix}_rates WHERE prev_ts IS NOT NULL\n)"
    )
}

fn rates_cte_with_prev(name: &str, cols: &[&str], materialized: bool) -> String {
    // Adds `LAG(timestamp) OVER w AS prev_ts` so the downstream filter can
    // drop the first row without recomputing irate_lag.
    let mat = if materialized { " MATERIALIZED" } else { "" };
    let lags: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "    irate_lag(\"{c}\", LAG(\"{c}\") OVER w, timestamp - LAG(timestamp) OVER w) AS val_{i}"
            )
        })
        .collect();
    format!(
        "{name} AS{mat} (\n  SELECT timestamp, LAG(timestamp) OVER w AS prev_ts,\n{}\n  FROM _src\n  WINDOW w AS (ORDER BY timestamp)\n)",
        lags.join(",\n")
    )
}

fn final_select() -> &'static str {
    "SELECT CAST(a.timestamp AS DOUBLE) / 1e9 AS t, (a.v / NULLIF(b.v, 0)) AS v
FROM a_summed a JOIN b_summed b ON a.timestamp = b.timestamp"
}

fn variant_baseline() -> String {
    let a_rates = rates_cte("a_rates", COLS_A, false);
    let b_rates = rates_cte("b_rates", COLS_B, false);
    let a_sum = sum_filter_or("a", COLS_A.len(), true);
    let b_sum = sum_filter_or("b", COLS_B.len(), true);
    format!(
        "WITH\n{a_rates},\n{a_sum},\n{b_rates},\n{b_sum}\n{}",
        final_select()
    )
}

fn variant_materialized() -> String {
    let a_rates = rates_cte("a_rates", COLS_A, true);
    let b_rates = rates_cte("b_rates", COLS_B, true);
    let a_sum = sum_filter_or("a", COLS_A.len(), true);
    let b_sum = sum_filter_or("b", COLS_B.len(), true);
    format!(
        "WITH\n{a_rates},\n{a_sum},\n{b_rates},\n{b_sum}\n{}",
        final_select()
    )
}

fn variant_sentinel() -> String {
    let a_rates = rates_cte_with_prev("a_rates", COLS_A, false);
    let b_rates = rates_cte_with_prev("b_rates", COLS_B, false);
    let a_sum = sum_filter_lag_sentinel("a", COLS_A.len());
    let b_sum = sum_filter_lag_sentinel("b", COLS_B.len());
    format!(
        "WITH\n{a_rates},\n{a_sum},\n{b_rates},\n{b_sum}\n{}",
        final_select()
    )
}

fn variant_no_filter() -> String {
    // No row-level filter at all. Includes the first row (where v=0 for both
    // lanes) and lets NULLIF(b.v, 0) produce NULL there.
    let a_rates = rates_cte("a_rates", COLS_A, false);
    let b_rates = rates_cte("b_rates", COLS_B, false);
    let a_sum = sum_filter_or("a", COLS_A.len(), false);
    let b_sum = sum_filter_or("b", COLS_B.len(), false);
    format!(
        "WITH\n{a_rates},\n{a_sum},\n{b_rates},\n{b_sum}\n{}",
        final_select()
    )
}

fn variant_fused() -> String {
    // Both lanes' irate_lag projected in ONE rates CTE (single WINDOW pass).
    // No JOIN — final SELECT just reads (a_sum, b_sum) from one row.
    let a_lags: Vec<String> = COLS_A
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "    irate_lag(\"{c}\", LAG(\"{c}\") OVER w, timestamp - LAG(timestamp) OVER w) AS a_{i}"
            )
        })
        .collect();
    let b_lags: Vec<String> = COLS_B
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "    irate_lag(\"{c}\", LAG(\"{c}\") OVER w, timestamp - LAG(timestamp) OVER w) AS b_{i}"
            )
        })
        .collect();
    let a_sum: Vec<String> = (0..COLS_A.len())
        .map(|i| format!("COALESCE(a_{i}, 0)"))
        .collect();
    let b_sum: Vec<String> = (0..COLS_B.len())
        .map(|i| format!("COALESCE(b_{i}, 0)"))
        .collect();
    let a_sum_expr = a_sum.join(" + ");
    let b_sum_expr = b_sum.join(" + ");
    format!(
        "WITH rates AS (\n  SELECT timestamp,\n{},\n{}\n  FROM _src\n  WINDOW w AS (ORDER BY timestamp)\n)\nSELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, ({a_sum_expr}) / NULLIF(({b_sum_expr}), 0) AS v FROM rates",
        a_lags.join(",\n"),
        b_lags.join(",\n"),
    )
}

fn variant_fused_correct() -> String {
    // Matches the semantic invariant of the current code:
    //   a_summed has WHERE (any a_i IS NOT NULL) — so first row drops
    //   b_summed has WHERE (any b_i IS NOT NULL) — so first row drops
    //   inner JOIN drops rows where one lane has nothing.
    // Fused version: emit NULL for a_sum if all a_i are NULL, NULL for
    // b_sum if all b_i are NULL. NULLIF(b, 0) drops rows where b=0 OR b=NULL,
    // and a/NULL=NULL → run_matrix drops that row.
    let a_lags: Vec<String> = COLS_A
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "    irate_lag(\"{c}\", LAG(\"{c}\") OVER w, timestamp - LAG(timestamp) OVER w) AS a_{i}"
            )
        })
        .collect();
    let b_lags: Vec<String> = COLS_B
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "    irate_lag(\"{c}\", LAG(\"{c}\") OVER w, timestamp - LAG(timestamp) OVER w) AS b_{i}"
            )
        })
        .collect();
    let a_sum: Vec<String> = (0..COLS_A.len())
        .map(|i| format!("COALESCE(a_{i}, 0)"))
        .collect();
    let b_sum: Vec<String> = (0..COLS_B.len())
        .map(|i| format!("COALESCE(b_{i}, 0)"))
        .collect();
    let a_any: Vec<String> = (0..COLS_A.len())
        .map(|i| format!("a_{i} IS NOT NULL"))
        .collect();
    let b_any: Vec<String> = (0..COLS_B.len())
        .map(|i| format!("b_{i} IS NOT NULL"))
        .collect();
    let a_sum_expr = format!(
        "CASE WHEN {} THEN ({}) END",
        a_any.join(" OR "),
        a_sum.join(" + ")
    );
    let b_sum_expr = format!(
        "CASE WHEN {} THEN ({}) END",
        b_any.join(" OR "),
        b_sum.join(" + ")
    );
    format!(
        "WITH rates AS (\n  SELECT timestamp,\n{},\n{}\n  FROM _src\n  WINDOW w AS (ORDER BY timestamp)\n)\nSELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, ({a_sum_expr}) / NULLIF(({b_sum_expr}), 0) AS v FROM rates",
        a_lags.join(",\n"),
        b_lags.join(",\n"),
    )
}

fn variant_fused_materialized() -> String {
    let a_lags: Vec<String> = COLS_A
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "    irate_lag(\"{c}\", LAG(\"{c}\") OVER w, timestamp - LAG(timestamp) OVER w) AS a_{i}"
            )
        })
        .collect();
    let b_lags: Vec<String> = COLS_B
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "    irate_lag(\"{c}\", LAG(\"{c}\") OVER w, timestamp - LAG(timestamp) OVER w) AS b_{i}"
            )
        })
        .collect();
    let a_sum: Vec<String> = (0..COLS_A.len())
        .map(|i| format!("COALESCE(a_{i}, 0)"))
        .collect();
    let b_sum: Vec<String> = (0..COLS_B.len())
        .map(|i| format!("COALESCE(b_{i}, 0)"))
        .collect();
    let a_sum_expr = a_sum.join(" + ");
    let b_sum_expr = b_sum.join(" + ");
    format!(
        "WITH rates AS MATERIALIZED (\n  SELECT timestamp,\n{},\n{}\n  FROM _src\n  WINDOW w AS (ORDER BY timestamp)\n)\nSELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, ({a_sum_expr}) / NULLIF(({b_sum_expr}), 0) AS v FROM rates",
        a_lags.join(",\n"),
        b_lags.join(",\n"),
    )
}

fn time_query(conn: &Connection, sql: &str, reps: usize) -> (f64, f64, f64, usize) {
    // Warm prepare/cache.
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
        ("baseline (current)", variant_baseline()),
        ("MATERIALIZED CTE", variant_materialized()),
        ("LAG sentinel filter", variant_sentinel()),
        ("no filter (NULLIF on 0/0)", variant_no_filter()),
        ("FUSED single-CTE (no JOIN)", variant_fused()),
        ("FUSED correct (NULL when all-null)", variant_fused_correct()),
        ("FUSED + MATERIALIZED", variant_fused_materialized()),
    ];

    println!("# Filter-cost probe — {PARQUET}\n");
    println!("Each variant runs `counter_irate_ratio_with_labels`'s SQL pattern");
    println!("(30 cols per lane). Reps = 200, after 3 warm-up runs.\n");
    println!("| variant | p10 (ms) | median (ms) | p90 (ms) | rows |");
    println!("|---|---:|---:|---:|---:|");
    for (name, sql) in &variants {
        let (p10, median, p90, rows) = time_query(&conn, sql, 200);
        println!("| {name} | {p10:.3} | {median:.3} | {p90:.3} | {rows} |");
    }

    // Also probe the 1-column (most common) shape — counter_irate_sum_with_labels
    // emits one rates_cte with 1 col and one summed CTE. Test if MATERIALIZED
    // helps or hurts there.
    println!("\n## Single-column shape (1 col, no JOIN — `counter_irate_sum_with_labels` pattern)\n");
    let single_baseline = format!(
        "WITH rates AS (\n  SELECT timestamp, irate_lag(\"0::154\", LAG(\"0::154\") OVER w, timestamp - LAG(timestamp) OVER w) AS val_0\n  FROM _src WINDOW w AS (ORDER BY timestamp)\n)\nSELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, COALESCE(val_0, 0) AS v FROM rates WHERE val_0 IS NOT NULL"
    );
    let single_materialized = format!(
        "WITH rates AS MATERIALIZED (\n  SELECT timestamp, irate_lag(\"0::154\", LAG(\"0::154\") OVER w, timestamp - LAG(timestamp) OVER w) AS val_0\n  FROM _src WINDOW w AS (ORDER BY timestamp)\n)\nSELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, COALESCE(val_0, 0) AS v FROM rates WHERE val_0 IS NOT NULL"
    );
    let single_no_filter = format!(
        "WITH rates AS (\n  SELECT timestamp, irate_lag(\"0::154\", LAG(\"0::154\") OVER w, timestamp - LAG(timestamp) OVER w) AS val_0\n  FROM _src WINDOW w AS (ORDER BY timestamp)\n)\nSELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, COALESCE(val_0, 0) AS v FROM rates"
    );
    // No COALESCE either — let val_0's NULL propagate as v NULL. Backend
    // skips NULL v rows, same observable result as the explicit filter.
    let single_null_propagate = format!(
        "WITH rates AS (\n  SELECT timestamp, irate_lag(\"0::154\", LAG(\"0::154\") OVER w, timestamp - LAG(timestamp) OVER w) AS val_0\n  FROM _src WINDOW w AS (ORDER BY timestamp)\n)\nSELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, val_0 AS v FROM rates"
    );
    let single_variants: Vec<(&str, String)> = vec![
        ("baseline (1 col)", single_baseline),
        ("MATERIALIZED (1 col)", single_materialized),
        ("no filter (1 col)", single_no_filter),
        ("null-propagate (1 col)", single_null_propagate),
    ];
    println!("| variant | p10 (ms) | median (ms) | p90 (ms) | rows |");
    println!("|---|---:|---:|---:|---:|");
    for (name, sql) in &single_variants {
        let (p10, median, p90, rows) = time_query(&conn, sql, 200);
        println!("| {name} | {p10:.3} | {median:.3} | {p90:.3} | {rows} |");
    }

    // Also dump the EXPLAIN for the materialized variant so we can confirm
    // the operator change.
    println!("\n## EXPLAIN — MATERIALIZED variant\n");
    let sql = variant_materialized();
    let mut stmt = conn.prepare(&format!("EXPLAIN {sql}")).unwrap();
    let mut rows = stmt.query([]).unwrap();
    while let Some(r) = rows.next().unwrap() {
        let key: String = r.get(0).unwrap();
        let value: String = r.get(1).unwrap();
        println!("[{key}]\n{value}");
    }
}
