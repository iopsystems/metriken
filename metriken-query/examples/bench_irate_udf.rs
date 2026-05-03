//! Quick A/B microbench: irate_lag UDF vs inline CASE on the worst-shape
//! query. Both run against an identical _src + 16 softirq columns,
//! 5 reps each, prepare_cached, no result iteration.

use std::sync::Arc;
use std::time::Instant;

use metriken_query::Tsdb;

fn main() {
    let parquet = "/work/rezolus/site/viewer/data/vllm.parquet";
    let _tsdb = Arc::new(Tsdb::load(std::path::Path::new(parquet)).unwrap());

    let conn = duckdb::Connection::open_in_memory().unwrap();
    metriken_query_sql::register_all(&conn).unwrap();
    conn.set_prepared_statement_cache_capacity(1024);

    // Load _src
    conn.execute_batch(&format!(
        "CREATE OR REPLACE TEMP TABLE _src AS \
         SELECT ((CAST(timestamp AS BIGINT) + 500000000) // 1000000000) * 1000000000 AS timestamp, \
                * EXCLUDE (timestamp) \
         FROM read_parquet('{parquet}')"
    )).unwrap();

    // Find rcu cols
    let mut stmt = conn.prepare(
        "SELECT column_name FROM (DESCRIBE _src) WHERE column_name LIKE 'softirq/rcu/%' ORDER BY column_name"
    ).unwrap();
    let rcu_cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    eprintln!("found {} rcu cols", rcu_cols.len());

    // Build CASE form SQL
    let mut case_rates = String::new();
    for (i, c) in rcu_cols.iter().enumerate() {
        if i > 0 { case_rates.push(','); }
        let cq = c.replace('"', "\"\"");
        case_rates.push_str(&format!(
            "\nCASE\n  WHEN LAG(\"{cq}\") OVER w IS NULL THEN NULL\n  \
             WHEN \"{cq}\" >= LAG(\"{cq}\") OVER w THEN CAST(\"{cq}\" - LAG(\"{cq}\") OVER w AS DOUBLE) / NULLIF(CAST(timestamp - LAG(timestamp) OVER w AS DOUBLE) / 1e9, 0)\n  \
             ELSE CAST(\"{cq}\" AS DOUBLE) / NULLIF(CAST(timestamp - LAG(timestamp) OVER w AS DOUBLE) / 1e9, 0)\n\
             END AS val_{i}"
        ));
    }
    let case_sql = format!(
        "WITH rates AS (SELECT timestamp,{case_rates} FROM _src WINDOW w AS (ORDER BY timestamp)) \
         SELECT timestamp, COALESCE(val_0,0)+COALESCE(val_1,0)+COALESCE(val_2,0)+COALESCE(val_3,0)+COALESCE(val_4,0)+COALESCE(val_5,0)+COALESCE(val_6,0)+COALESCE(val_7,0)+COALESCE(val_8,0)+COALESCE(val_9,0)+COALESCE(val_10,0)+COALESCE(val_11,0)+COALESCE(val_12,0)+COALESCE(val_13,0)+COALESCE(val_14,0)+COALESCE(val_15,0) AS v FROM rates"
    );

    // Build UDF form SQL
    let mut udf_rates = String::new();
    for (i, c) in rcu_cols.iter().enumerate() {
        if i > 0 { udf_rates.push(','); }
        let cq = c.replace('"', "\"\"");
        udf_rates.push_str(&format!(
            "\nirate_lag(\"{cq}\", LAG(\"{cq}\") OVER w, timestamp - LAG(timestamp) OVER w) AS val_{i}"
        ));
    }
    let udf_sql = format!(
        "WITH rates AS (SELECT timestamp,{udf_rates} FROM _src WINDOW w AS (ORDER BY timestamp)) \
         SELECT timestamp, COALESCE(val_0,0)+COALESCE(val_1,0)+COALESCE(val_2,0)+COALESCE(val_3,0)+COALESCE(val_4,0)+COALESCE(val_5,0)+COALESCE(val_6,0)+COALESCE(val_7,0)+COALESCE(val_8,0)+COALESCE(val_9,0)+COALESCE(val_10,0)+COALESCE(val_11,0)+COALESCE(val_12,0)+COALESCE(val_13,0)+COALESCE(val_14,0)+COALESCE(val_15,0) AS v FROM rates"
    );

    // Bench loop. prepare_cached caches the plan; we iterate just to drain the iterator.
    let warmup = 3;
    let reps = 30;
    let drain = |sql: &str| {
        let mut stmt = conn.prepare_cached(sql).unwrap();
        let mut rows = stmt.query([]).unwrap();
        let mut n = 0;
        while let Some(_) = rows.next().unwrap() { n += 1; }
        n
    };

    eprintln!("warmup CASE...");
    for _ in 0..warmup { drain(&case_sql); }
    eprintln!("measuring CASE × {reps}...");
    let mut case_times = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        let n = drain(&case_sql);
        case_times.push((t.elapsed().as_secs_f64() * 1000.0, n));
    }

    eprintln!("warmup UDF...");
    for _ in 0..warmup { drain(&udf_sql); }
    eprintln!("measuring UDF × {reps}...");
    let mut udf_times = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        let n = drain(&udf_sql);
        udf_times.push((t.elapsed().as_secs_f64() * 1000.0, n));
    }

    let med = |mut v: Vec<(f64, usize)>| {
        v.sort_by(|a,b| a.0.partial_cmp(&b.0).unwrap());
        v[v.len()/2]
    };
    let (cm, cn) = med(case_times.clone());
    let (um, un) = med(udf_times.clone());
    println!("CASE  median: {cm:.3}ms (n_rows={cn})");
    println!("UDF   median: {um:.3}ms (n_rows={un})");
    println!("UDF/CASE: {:.2}x", um/cm);
}
