// SQL macros mirroring the rezolus dashboard's recurring PromQL idioms.
//
// **Source of truth.** The macro bodies live in `shared_macros.sql`
// (sibling file). This module loads that file via `include_str!`,
// splits on `;` boundaries, and runs each CREATE MACRO statement
// against a DuckDB connection.
//
// The wasm-side viewer pulls the same string in via the
// `SHARED_MACROS` re-export below (see `/work/rezolus/crates/viewer-sql/src/lib.rs`
// and `crates/viewer-sql/src/macros.sql` for the H2 replacement macros
// it concatenates after the shared block). The parity scaffold at
// `rezolus/crates/viewer-sql/tests/macros.rs` exercises the same SQL
// against an in-memory DuckDB on the native side.
//
// See REVIEWING.md (both repos) for the "two macro libraries" hazard
// (now reduced to "one shared file plus a wasm-only H2 supplement").

use duckdb::Connection;

/// The shared SQL macros, as one script. Re-exported so the wasm-side
/// viewer can `include_str!` the same text through this crate. Layout
/// and semantics live in the file itself.
pub const SHARED_MACROS: &str = include_str!("shared_macros.sql");

/// Register the shared macros against an in-memory DuckDB connection.
/// Strips `--` line comments (which may contain `;` inside parenthetical
/// asides), splits on `;` boundaries, and executes each statement.
pub fn register_all(conn: &Connection) -> duckdb::Result<()> {
    for stmt in split_statements(SHARED_MACROS) {
        conn.execute(&stmt, [])?;
    }
    Ok(())
}

/// Strip `--` line comments and split on `;` boundaries. Returned
/// statements are trimmed; empties are dropped. Public so the wasm
/// parity test scaffold can reuse the splitter (DuckDB's executeBatch
/// behaves the same way).
fn split_statements(sql: &str) -> Vec<String> {
    let stripped: String = sql
        .lines()
        .map(|line| match line.find("--") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    stripped
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
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
    fn irate_1s_is_per_second_rate() {
        // ts is in nanoseconds (matches `_src` post-snap). 1e9-ns spacing
        // → dt=1s → rate equals delta. Verifies division by dt and not
        // bare-delta semantics.
        let conn = fresh();
        let r = col_f64(
            &conn,
            "WITH t(ts, x) AS (VALUES (1000000000, 100.0), (2000000000, 250.0), (3000000000, 425.0)) \
             SELECT irate_1s(x, ts) FROM t ORDER BY ts",
        );
        assert_eq!(r, vec![None, Some(150.0), Some(175.0)]);
    }

    #[test]
    fn irate_1s_divides_by_actual_dt_when_samples_are_gappy() {
        // 1s, then 2s gap. PromQL `irate` divides by the actual delta-t,
        // so the second result should be `delta / 2`, not bare delta.
        let conn = fresh();
        let r = col_f64(
            &conn,
            "WITH t(ts, x) AS (VALUES (1000000000, 0.0), (2000000000, 100.0), (4000000000, 300.0)) \
             SELECT irate_1s(x, ts) FROM t ORDER BY ts",
        );
        assert_eq!(r, vec![None, Some(100.0), Some(100.0)]);
    }

    #[test]
    fn rate_5m_lags_300_seconds_and_divides() {
        let conn = fresh();
        // x_n = n*(n-1)/2 with ts in nanoseconds.
        // At ts=305s: x=46360. The 300s-back range window starts at ts=5s,
        // so first_value(x)=10 and first_value(ts)=5e9.
        // delta=46350; dt=300s; rate=154.5.
        let r = col_f64(
            &conn,
            "WITH s AS (SELECT ts*1000000000 AS ts_ns, ts*(ts-1)/2 AS x FROM range(1, 306) t(ts)) \
             SELECT rate_5m(x, ts_ns) FROM s ORDER BY ts_ns DESC LIMIT 1",
        );
        assert_eq!(r, vec![Some(154.5)]);
    }

    #[test]
    fn rate_5m_handles_short_parquets_via_range_window() {
        // Pre-fix: positional `LAG(c, 300)` returned NULL on every sample
        // of a 60-row table, so SQL emitted no series. The range-based
        // form computes a rate over the actual span seen.
        // x = ts*(ts-1)/2 over ts in [1, 60] (in ns: ts*1e9).
        // At ts=60s: x=1770; first_value at ts=1s: x=0.
        // delta=1770; dt=59s; rate≈30.0.
        let conn = fresh();
        let r = col_f64(
            &conn,
            "WITH s AS (SELECT ts*1000000000 AS ts_ns, ts*(ts-1)/2 AS x FROM range(1, 61) t(ts)) \
             SELECT rate_5m(x, ts_ns) FROM s ORDER BY ts_ns DESC LIMIT 1",
        );
        let v = r[0].expect("non-NULL on short window");
        assert!((v - 30.0).abs() < 1e-9, "expected ~30.0 got {v}");
    }

    #[test]
    fn cpu_busy_pct_decomposes_to_irate_over_cores_over_1e9() {
        let conn = fresh();
        // 4 cores; usage in ns. 1e9 ns delta over 1s on 4 cores → 0.25 busy.
        let r = col_f64(
            &conn,
            "WITH t(ts, u) AS (VALUES (1000000000, 0.0), (2000000000, 1.0e9), (3000000000, 3.0e9)) \
             SELECT cpu_busy_pct(u, 4, ts) FROM t ORDER BY ts",
        );
        assert_eq!(r, vec![None, Some(0.25), Some(0.5)]);
    }

    #[test]
    fn ipc_is_ratio_of_two_irate_1s() {
        let conn = fresh();
        let r = col_f64(
            &conn,
            "WITH t(ts, i, c) AS (VALUES (1000000000, 0.0, 0.0), (2000000000, 200.0, 100.0), (3000000000, 700.0, 200.0)) \
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
                (1000000000, 0.0, 0.0, 0.0, 0.0, 0.0), \
                (2000000000, 200.0, 100.0, 1000.0, 800.0, 1000.0)) \
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
            "WITH t(ts, b) AS (VALUES (1000000000, 0.0), (2000000000, 100.0)) \
             SELECT bps_from_bytes(b, ts) FROM t ORDER BY ts",
        );
        assert_eq!(r, vec![None, Some(800.0)]);
    }

    #[test]
    fn delta_1s_equals_irate_1s_at_1s_spacing() {
        // `delta_1s` is the bare pairwise diff; `irate_1s` is delta/dt.
        // At evenly-1s spacing they coincide. Verifies the (deliberately
        // small) overlap between the two macros.
        let conn = fresh();
        let pair = col_f64(
            &conn,
            "WITH t(ts, x) AS (VALUES (1000000000, 5.0), (2000000000, 11.0), (3000000000, 20.0)) \
             SELECT delta_1s(x, ts) - irate_1s(x, ts) FROM t ORDER BY ts",
        );
        // None for the first row (LAG NULL), 0.0 thereafter.
        assert_eq!(pair, vec![None, Some(0.0), Some(0.0)]);
    }
}
