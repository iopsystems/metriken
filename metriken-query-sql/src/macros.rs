// SQL macros mirroring the rezolus dashboard's recurring PromQL idioms.
//
// DuckDB validates macro bodies at CREATE time — a `LAG(c) OVER w` body fails
// because the window `w` doesn't exist yet, and `OVER (ORDER BY ts)` fails
// because `ts` is unbound. So every windowed macro takes the ordering column
// as an explicit parameter (typically `timestamp`). Macro-to-macro calls are
// fine: the catalog lookup happens at CREATE time and we register primitives
// before the dashboard-concept macros that compose them.
//
// Rezolus parquet sampling is 1Hz, so PromQL's `irate(x[5m])` reduces to the
// most recent pair-wise delta and `rate(x[5m])` to a 300-row LAG over 300s.

use duckdb::Connection;

const MACROS: &[&str] = &[
    // -------- Layer A: rate / delta primitives --------
    //
    // Reset semantics match PromQL `irate` / `rate`: when `c < LAG(c)`,
    // treat the post-reset value `c` as the increment. The metriken-query
    // PromQL engine is the source of truth; see
    // `metriken-query/src/promql/streaming/{rate,irate}.rs:60-70`. The
    // exhaustigen tests (`metriken-query-sql/tests/exhaustigen.rs`) walk
    // every short counter sequence and assert byte-equality.

    // Per-second delta with PromQL reset semantics.
    // NULL on the first sample of the table (no LAG).
    "CREATE OR REPLACE MACRO irate_1s(c, ts) AS \
        CASE \
            WHEN LAG(c) OVER (ORDER BY ts) IS NULL THEN NULL \
            WHEN c >= LAG(c) OVER (ORDER BY ts) THEN CAST(c - LAG(c) OVER (ORDER BY ts) AS DOUBLE) \
            ELSE CAST(c AS DOUBLE) \
        END",

    // Same value, different name — for callers who think in deltas, not rates.
    "CREATE OR REPLACE MACRO delta_1s(c, ts) AS \
        CASE \
            WHEN LAG(c) OVER (ORDER BY ts) IS NULL THEN NULL \
            WHEN c >= LAG(c) OVER (ORDER BY ts) THEN CAST(c - LAG(c) OVER (ORDER BY ts) AS DOUBLE) \
            ELSE CAST(c AS DOUBLE) \
        END",

    // 5-minute average rate over a windowed `c - LAG(c, 300)` / 300s.
    // **Caveat:** this is correct ONLY for monotonic counters. PromQL's
    // `rate(c[5m])` handles intra-window resets via sum-of-increments, but
    // DuckDB rejects `SUM(LAG(...) OVER ...) OVER ...` ("window function
    // calls cannot be nested"), so the reset-aware form has to be written
    // out as a CTE in the catalogue's SQL twin. For the common case
    // (Rezolus dashboards over reset-free data) this macro suffices.
    "CREATE OR REPLACE MACRO rate_5m(c, ts) AS \
        (c - LAG(c, 300) OVER (ORDER BY ts)) / 300.0",

    // -------- Layer A: histogram primitives --------
    // Cumulative quantiles do not need a window — just delegate to h2_quantile.

    "CREATE OR REPLACE MACRO hist_p(buckets, q) AS h2_quantile(buckets, q)",
    "CREATE OR REPLACE MACRO hist_p50(buckets) AS h2_quantile(buckets, 0.50)",
    "CREATE OR REPLACE MACRO hist_p90(buckets) AS h2_quantile(buckets, 0.90)",
    "CREATE OR REPLACE MACRO hist_p99(buckets) AS h2_quantile(buckets, 0.99)",
    "CREATE OR REPLACE MACRO hist_p999(buckets) AS h2_quantile(buckets, 0.999)",

    // Windowed quantile: quantile of the per-sample bucket-count delta.
    // Equivalent to PromQL `histogram_quantile(q, irate(<hist>[1s]))`.
    "CREATE OR REPLACE MACRO hist_irate_quantile(buckets, q, ts) AS \
        h2_quantile(h2_delta(buckets, LAG(buckets) OVER (ORDER BY ts)), q)",

    // Same but over a 5-minute window — equivalent to PromQL
    // `histogram_quantile(q, rate(<h>[5m]))`.
    "CREATE OR REPLACE MACRO hist_rate5m_quantile(buckets, q, ts) AS \
        h2_quantile(h2_delta(buckets, LAG(buckets, 300) OVER (ORDER BY ts)), q)",

    // -------- Layer B: dashboard-concept helpers --------
    //
    // Each composes the Layer A primitives; expanding by hand recovers the
    // same SQL the original PromQL `irate(...)` formulas spell out.

    // CPU fraction (0..1) — works for total CPU busy and for per-state usage
    // (user/system/etc), which are the same shape against different inputs.
    "CREATE OR REPLACE MACRO cpu_busy_pct(usage, cores, ts) AS \
        irate_1s(usage, ts) / cores / 1e9",

    // Instructions per cycle.
    "CREATE OR REPLACE MACRO ipc(instructions, cycles, ts) AS \
        irate_1s(instructions, ts) / nullif(irate_1s(cycles, ts), 0)",

    // Effective CPU frequency in Hz.
    "CREATE OR REPLACE MACRO frequency_hz(tsc, aperf, mperf, cores, ts) AS \
        irate_1s(tsc, ts) * irate_1s(aperf, ts) / nullif(irate_1s(mperf, ts), 0) / cores",

    // Instructions per nanosecond (wall-clock-normalised throughput) =
    // ipc × frequency / cores / 1e9.
    "CREATE OR REPLACE MACRO ipns(instructions, cycles, tsc, aperf, mperf, cores, ts) AS \
        ipc(instructions, cycles, ts) \
        * irate_1s(tsc, ts) * irate_1s(aperf, ts) \
        / nullif(irate_1s(mperf, ts) * cores * 1e9, 0)",

    // L3 cache hit fraction.
    "CREATE OR REPLACE MACRO l3_hit_pct(miss, access, ts) AS \
        1 - irate_1s(miss, ts) / nullif(irate_1s(access, ts), 0)",

    // Branch misprediction fraction.
    "CREATE OR REPLACE MACRO branch_miss_pct(misses, branches, ts) AS \
        irate_1s(misses, ts) / nullif(irate_1s(branches, ts), 0)",

    // DTLB misses per thousand instructions.
    "CREATE OR REPLACE MACRO dtlb_mpki(misses, instructions, ts) AS \
        irate_1s(misses, ts) / nullif(irate_1s(instructions, ts), 0) * 1000",

    // GPU memory used as fraction of total (used + free). No window needed.
    "CREATE OR REPLACE MACRO gpu_mem_used_pct(used, free) AS \
        used / nullif(used + free, 0)",

    // Bandwidth in bits per second from a byte counter.
    "CREATE OR REPLACE MACRO bps_from_bytes(bytes, ts) AS \
        irate_1s(bytes, ts) * 8",
];

pub fn register_all(conn: &Connection) -> duckdb::Result<()> {
    for sql in MACROS {
        conn.execute(sql, [])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        crate::udf::register_all(&conn).expect("register UDFs");
        register_all(&conn).expect("register macros");
        conn
    }

    fn col_f64(conn: &Connection, sql: &str) -> Vec<Option<f64>> {
        let mut stmt = conn.prepare(sql).expect("prepare");
        stmt.query_map([], |row| row.get::<_, Option<f64>>(0))
            .expect("query_map")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect")
    }

    #[test]
    fn all_macros_register_without_error() {
        // The fresh() helper does the real work; reaching this line is the test.
        let _conn = fresh();
    }

    #[test]
    fn irate_1s_is_pairwise_diff() {
        let conn = fresh();
        let r = col_f64(
            &conn,
            "WITH t(ts, x) AS (VALUES (1, 100.0), (2, 250.0), (3, 425.0)) \
             SELECT irate_1s(x, ts) FROM t ORDER BY ts",
        );
        assert_eq!(r, vec![None, Some(150.0), Some(175.0)]);
    }

    #[test]
    fn rate_5m_lags_300_seconds_and_divides() {
        let conn = fresh();
        // x_n = n*(n-1)/2 over ts = 1..305. At ts=305: x=46360, lag=10, delta=46350, /300=154.5.
        let r = col_f64(
            &conn,
            "WITH s AS (SELECT ts, ts*(ts-1)/2 AS x FROM range(1, 306) t(ts)) \
             SELECT rate_5m(x, ts) FROM s ORDER BY ts DESC LIMIT 1",
        );
        assert_eq!(r, vec![Some(154.5)]);
    }

    #[test]
    fn cpu_busy_pct_decomposes_to_irate_over_cores_over_1e9() {
        let conn = fresh();
        // 4 cores; usage in ns. 1e9 ns delta over 1s on 4 cores → 0.25 busy.
        let r = col_f64(
            &conn,
            "WITH t(ts, u) AS (VALUES (1, 0.0), (2, 1.0e9), (3, 3.0e9)) \
             SELECT cpu_busy_pct(u, 4, ts) FROM t ORDER BY ts",
        );
        assert_eq!(r, vec![None, Some(0.25), Some(0.5)]);
    }

    #[test]
    fn ipc_is_ratio_of_two_irate_1s() {
        let conn = fresh();
        let r = col_f64(
            &conn,
            "WITH t(ts, i, c) AS (VALUES (1, 0.0, 0.0), (2, 200.0, 100.0), (3, 700.0, 200.0)) \
             SELECT ipc(i, c, ts) FROM t ORDER BY ts",
        );
        assert_eq!(r, vec![None, Some(2.0), Some(5.0)]);
    }

    #[test]
    fn ipns_composes_three_layers_deep() {
        let conn = fresh();
        // ipns calls ipc which calls irate_1s — verify the 3-deep composition.
        // ipc=2, freq=tsc*aperf/mperf/cores=1000*800/1000/1=800, ipns=ipc*freq/1e9=1.6e-6.
        let r = col_f64(
            &conn,
            "WITH t(ts, i, c, tsc, ap, mp) AS (VALUES \
                (1, 0.0, 0.0, 0.0, 0.0, 0.0), \
                (2, 200.0, 100.0, 1000.0, 800.0, 1000.0)) \
             SELECT ipns(i, c, tsc, ap, mp, 1, ts) FROM t ORDER BY ts",
        );
        assert!(r[0].is_none());
        let v = r[1].unwrap();
        assert!((v - 1.6e-6).abs() < 1e-12, "got {v}");
    }

    #[test]
    fn hist_p99_delegates_to_h2_quantile() {
        let conn = fresh();
        let direct: u64 = conn
            .query_row(
                "SELECT h2_quantile([10,20,30,40]::UBIGINT[], 0.99)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let via_macro: u64 = conn
            .query_row("SELECT hist_p99([10,20,30,40]::UBIGINT[])", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(direct, via_macro);
    }

    #[test]
    fn bps_from_bytes_is_irate_times_8() {
        let conn = fresh();
        let r = col_f64(
            &conn,
            "WITH t(ts, b) AS (VALUES (1, 0.0), (2, 100.0)) \
             SELECT bps_from_bytes(b, ts) FROM t ORDER BY ts",
        );
        assert_eq!(r, vec![None, Some(800.0)]);
    }

    #[test]
    fn delta_1s_matches_irate_1s() {
        // The two macros have identical bodies; verify they produce the same output.
        let conn = fresh();
        let pair = col_f64(
            &conn,
            "WITH t(ts, x) AS (VALUES (1, 5.0), (2, 11.0), (3, 20.0)) \
             SELECT delta_1s(x, ts) - irate_1s(x, ts) FROM t ORDER BY ts",
        );
        // None for the first row (LAG NULL), 0.0 thereafter.
        assert_eq!(pair, vec![None, Some(0.0), Some(0.0)]);
    }
}
