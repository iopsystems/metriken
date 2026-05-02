//! Check whether AB_*-style parquets have raw timestamps that collide
//! after snapping to the sampling interval. If so, the SQL `_src` table
//! ends up with multiple rows at the same snapped timestamp, which breaks
//! per-column LAG ordering.

use std::path::PathBuf;
use std::sync::Arc;

use duckdb::Connection;
use metriken_query_sql::{register_all, views};

fn main() {
    let parquet = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: check_ts_dups <parquet>"),
    );
    let conn = Connection::open_in_memory().unwrap();
    register_all(&conn).unwrap();
    views::ensure_views(&conn, parquet.to_str().unwrap()).unwrap();

    // Count rows with the same snapped timestamp
    let dup_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM (SELECT timestamp, COUNT(*) AS n FROM _src GROUP BY timestamp HAVING n > 1)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    println!("rows with dup snapped timestamps: {dup_count}");

    let total: i64 = conn.query_row("SELECT COUNT(*) FROM _src", [], |r| r.get(0)).unwrap();
    let distinct: i64 = conn
        .query_row("SELECT COUNT(DISTINCT timestamp) FROM _src", [], |r| r.get(0))
        .unwrap();
    println!("_src total rows: {total}, distinct snapped timestamps: {distinct}");

    // Count distinct timestamps in raw parquet
    let raw_distinct: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(DISTINCT timestamp) FROM read_parquet('{}')",
                parquet.display()
            ),
            [],
            |r| r.get(0),
        )
        .unwrap();
    let raw_total: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM read_parquet('{}')",
                parquet.display()
            ),
            [],
            |r| r.get(0),
        )
        .unwrap();
    println!("raw parquet rows: {raw_total}, distinct raw timestamps: {raw_distinct}");

    // Check first 5 raw timestamps
    // Find timestamp gaps (where consecutive rows differ by more than 1s).
    // Operates on _src (snapped timestamps), which is what the SQL queries see.
    let gap_count: i64 = conn.query_row(
        "WITH ordered AS (SELECT timestamp, LAG(timestamp) OVER (ORDER BY timestamp) AS prev FROM _src) SELECT COUNT(DISTINCT timestamp) FROM ordered WHERE prev IS NOT NULL AND CAST(timestamp AS BIGINT) - CAST(prev AS BIGINT) > 1000000000",
        [], |r| r.get(0)).unwrap();
    println!("timestamp gaps in _src (consecutive snapped timestamps >1s apart): {gap_count}");

    let mut stmt = conn
        .prepare(&format!(
            "SELECT timestamp FROM read_parquet('{}') ORDER BY timestamp DESC LIMIT 5",
            parquet.display()
        ))
        .unwrap();
    let rows: Vec<u64> = stmt
        .query_map([], |row| row.get::<_, u64>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    println!("first 10 raw timestamps:");
    for (i, t) in rows.iter().enumerate() {
        let snapped = ((*t as i64 + 500_000_000) / 1_000_000_000) * 1_000_000_000;
        println!(
            "  [{}] raw={} ({:.3}s) snapped={} ({}s)",
            i,
            t,
            *t as f64 / 1e9,
            snapped,
            snapped / 1_000_000_000
        );
    }
}
