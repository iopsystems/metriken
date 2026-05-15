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
/// Strips `--` line comments and splits on top-level `;` boundaries
/// (semicolons inside `'..'` or `"..."` literals are part of the
/// statement body, not terminators).
pub fn register_all(conn: &Connection) -> duckdb::Result<()> {
    for stmt in split_statements(SHARED_MACROS) {
        conn.execute(&stmt, [])?;
    }
    Ok(())
}

/// Split a SQL script into individual statements.
///
/// Two-pass: first strip `--` line comments (a `--` outside string
/// literals starts a comment that runs to the end of the line); then
/// split on `;` characters that are outside string literals.
///
/// String-literal awareness is required because a macro body like
/// `SELECT ';' AS x` legitimately contains a semicolon — naive
/// substring splitting would cut the statement in half. DuckDB
/// recognises `'..'` (with `''` as an embedded single-quote escape)
/// and `"..."` for quoted identifiers; we treat both as opaque.
///
/// Returned statements are trimmed; empties are dropped.
fn split_statements(sql: &str) -> Vec<String> {
    let stripped = strip_line_comments(sql);
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = stripped.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                // `''` inside a single-quoted literal is an embedded
                // quote, not a terminator. Same for `""` in an
                // identifier. Both are SQL standard.
                if chars.peek() == Some(&q) {
                    current.push(c);
                    current.push(chars.next().unwrap());
                } else {
                    current.push(c);
                    quote = None;
                }
            }
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                current.push(c);
            }
            None if c == ';' => {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_owned());
                }
                current.clear();
            }
            None => current.push(c),
        }
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_owned());
    }
    out
}

/// Strip `--` line comments from a SQL script. A `--` inside a string
/// literal is part of the literal, not a comment marker, so the scan
/// is quote-aware (mirroring `split_statements` below).
fn strip_line_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut quote: Option<char> = None;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                if chars.peek() == Some(&q) {
                    out.push(c);
                    out.push(chars.next().unwrap());
                } else {
                    out.push(c);
                    quote = None;
                }
            }
            Some(_) => out.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                out.push(c);
            }
            None if c == '-' && chars.peek() == Some(&'-') => {
                // Drop through end-of-line.
                for nc in chars.by_ref() {
                    if nc == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            None => out.push(c),
        }
    }
    out
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
    fn splitter_preserves_semicolons_inside_string_literals() {
        // Naïve `split(';')` would cut these in half. The 19 macros we
        // ship today don't trigger this, but pinning the contract
        // prevents a future addition of e.g. `SELECT 'a; b'` from
        // silently breaking macro registration.
        let cases = [
            "SELECT ';' AS one",
            "SELECT 'a; b; c' AS two",
            r#"SELECT "id;col" AS three"#,
            "SELECT 'embedded ''quote; and'' more' AS four",
        ];
        for body in cases {
            let stmts = super::split_statements(&format!("{body};\n"));
            assert_eq!(stmts.len(), 1, "{body} produced {stmts:?}");
            assert_eq!(stmts[0], body);
        }
    }

    #[test]
    fn splitter_treats_dashdash_inside_literals_as_data() {
        // The comment stripper must not chew into a string literal that
        // happens to contain `--`. (Comes up in error messages and unit
        // strings; `--` is otherwise a SQL line comment.)
        let stmts = super::split_statements("SELECT '-- not a comment' AS x;");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "SELECT '-- not a comment' AS x");
    }

    #[test]
    fn splitter_strips_real_dashdash_comments() {
        let stmts = super::split_statements(
            "SELECT 1 AS a; -- trailing\nSELECT 2 AS b;\n-- whole line\nSELECT 3 AS c;\n",
        );
        assert_eq!(
            stmts,
            vec!["SELECT 1 AS a", "SELECT 2 AS b", "SELECT 3 AS c"]
        );
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

    fn one_list_u64(conn: &Connection, sql: &str) -> Option<Vec<u64>> {
        use duckdb::types::Value;
        conn.query_row(sql, [], |row| {
            let v: Value = row.get(0)?;
            Ok(match v {
                Value::Null => None,
                Value::List(items) | Value::Array(items) => Some(
                    items
                        .into_iter()
                        .map(|x| match x {
                            Value::UBigInt(n) => n,
                            _ => 0,
                        })
                        .collect(),
                ),
                _ => None,
            })
        })
        .expect("query")
    }

    #[test]
    fn h2_combine_lol_matches_variadic_udf_for_two_lists() {
        // Cross-backend parity: the native side ships both a variadic
        // `h2_combine(c1, ..., cN)` UDF (fast path) and the
        // `h2_combine_lol(lol)` shared macro (used by the dashboard's
        // `h2_combine_lol([*COLUMNS(...)])` shape). They must agree
        // for any same input. Test on two ragged lists so the
        // widest-input-wins/zero-fill rule is exercised.
        let conn = fresh();
        let variadic = one_list_u64(
            &conn,
            "SELECT h2_combine([1,2,3]::UBIGINT[], [10,20,30,40]::UBIGINT[])",
        )
        .expect("variadic non-null");
        let lol = one_list_u64(
            &conn,
            "SELECT h2_combine_lol([[1,2,3]::UBIGINT[], [10,20,30,40]::UBIGINT[]])",
        )
        .expect("lol non-null");
        assert_eq!(variadic, vec![11, 22, 33, 40]);
        assert_eq!(lol, variadic);
    }

    #[test]
    fn h2_combine_lol_with_empty_outer_returns_empty_list() {
        // Edge case: zero matching columns. `list_max(list_transform([], ...))`
        // → NULL, and `generate_series(1, NULL)` → empty list, so the
        // outer list_transform produces an empty result rather than erroring.
        let conn = fresh();
        let got = one_list_u64(&conn, "SELECT h2_combine_lol([]::UBIGINT[][])");
        // Either Some(vec![]) or None is acceptable; both encode "no buckets".
        assert!(got.as_ref().map_or(true, Vec::is_empty), "got {got:?}");
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
