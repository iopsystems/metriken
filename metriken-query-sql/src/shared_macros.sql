-- Shared SQL macros for the rezolus dashboard.
--
-- **Single source of truth** for the 20 macros that both the native
-- (duckdb-rs) and the wasm (duckdb-wasm) viewers need. Native loads this
-- file via `include_str!` in metriken-query-sql/src/macros.rs and splits
-- on `;` boundaries. Wasm pulls in the same string via the
-- `metriken_query_sql::SHARED_MACROS` constant (re-exported from this
-- crate) and concatenates it with its own H2 replacement macros (the
-- macros that stand in for the Rust vscalar UDFs duckdb-wasm can't run).
--
-- Layout:
--   Layer A — rate / delta primitives (irate_1s, delta_1s, rate_5m).
--   Layer A.h — histogram primitives  (hist_p, hist_p50/p90/p99/p999,
--                                      hist_irate_quantile, hist_rate5m_quantile).
--   Layer B — dashboard-concept helpers (cpu_busy_pct, ipc, frequency_hz,
--                                        ipns, l3_hit_pct, branch_miss_pct,
--                                        dtlb_mpki, gpu_mem_used_pct,
--                                        bps_from_bytes).
--
-- DuckDB validates macro bodies at CREATE time, so every windowed macro
-- takes the ordering column as an explicit parameter (typically `ts`).
-- Macro-to-macro calls are fine: the catalog lookup happens at CREATE
-- time, so the registration order in this file matters — primitives
-- first, then composed helpers.
--
-- Rezolus parquet sampling is 1Hz, so PromQL's `irate(x[5m])` reduces to
-- the most recent pair-wise delta and `rate(x[5m])` to a 300-row LAG
-- over 300s.

-- ---- Layer A: rate / delta primitives ----
--
-- Reset semantics match PromQL `irate` / `rate`: when `c < LAG(c)`,
-- treat the post-reset value `c` as the increment. The metriken-query
-- PromQL engine is the source of truth (see
-- metriken-query/src/promql/streaming/{rate,irate}.rs:60-70). Unit tests
-- for this file live in macros.rs `#[cfg(test)] mod tests` (irate_1s,
-- rate_5m, delta_1s) on the native side and in
-- crates/viewer-sql/tests/macros.rs on the wasm side.

-- Per-second rate with PromQL reset semantics. NULL on the first sample
-- of the table (no LAG) and on duplicate timestamps (dt=0). Divides by
-- the actual `(ts - LAG(ts))` so a missing sample (gap > sampling
-- interval) produces a rate over the longer interval — matching PromQL's
-- `irate(c[w])` which divides by the delta-t between the last two
-- samples in the window.
CREATE OR REPLACE MACRO irate_1s(c, ts) AS
    CASE
        WHEN LAG(c) OVER (ORDER BY ts) IS NULL THEN NULL
        WHEN c >= LAG(c) OVER (ORDER BY ts) THEN
            CAST(c - LAG(c) OVER (ORDER BY ts) AS DOUBLE)
            / NULLIF((ts - LAG(ts) OVER (ORDER BY ts))::DOUBLE / 1e9, 0)
        ELSE
            CAST(c AS DOUBLE)
            / NULLIF((ts - LAG(ts) OVER (ORDER BY ts))::DOUBLE / 1e9, 0)
    END;

-- Same value, different name — for callers who think in deltas, not rates.
CREATE OR REPLACE MACRO delta_1s(c, ts) AS
    CASE
        WHEN LAG(c) OVER (ORDER BY ts) IS NULL THEN NULL
        WHEN c >= LAG(c) OVER (ORDER BY ts) THEN CAST(c - LAG(c) OVER (ORDER BY ts) AS DOUBLE)
        ELSE CAST(c AS DOUBLE)
    END;

-- 5-minute average rate over a *time-range* window. RANGE BETWEEN
-- 300000000000 PRECEDING AND CURRENT ROW (300s in ns — DuckDB 1.1.1
-- requires a literal) so parquets shorter than 5 minutes still produce
-- values and sample gaps don't shift the lookback by row count. Pre-fix
-- used positional `LAG(c, 300)` which returned NULL on every sample for
-- ≤300-row tables.
--
-- **Caveat:** monotonic-only. PromQL's `rate(c[5m])` handles intra-window
-- resets via sum-of-increments; the reset-aware form is the per-pair CTE
-- pattern in metriken-query/src/translate.rs (Rate variant of
-- PerColExpr), not expressible as a macro under DuckDB's "window
-- function calls cannot be nested" rule.
CREATE OR REPLACE MACRO rate_5m(c, ts) AS
    (c - first_value(c) OVER (ORDER BY ts RANGE BETWEEN 300000000000 PRECEDING AND CURRENT ROW))
    / NULLIF((ts - first_value(ts) OVER (ORDER BY ts RANGE BETWEEN 300000000000 PRECEDING AND CURRENT ROW))::DOUBLE / 1e9, 0);

-- ---- Layer A.h: histogram primitives ----
-- Cumulative quantiles do not need a window — just delegate to h2_quantile.

CREATE OR REPLACE MACRO hist_p(buckets, q)   AS h2_quantile(buckets, q);
CREATE OR REPLACE MACRO hist_p50(buckets)    AS h2_quantile(buckets, 0.50);
CREATE OR REPLACE MACRO hist_p90(buckets)    AS h2_quantile(buckets, 0.90);
CREATE OR REPLACE MACRO hist_p99(buckets)    AS h2_quantile(buckets, 0.99);
CREATE OR REPLACE MACRO hist_p999(buckets)   AS h2_quantile(buckets, 0.999);

-- Windowed quantile: quantile of the per-sample bucket-count delta.
-- Equivalent to PromQL `histogram_quantile(q, irate(<hist>[1s]))`. `p`
-- is the histogram's `grouping_power` (always exposed by the metric
-- views); it's required because bucket bounds depend on it.
CREATE OR REPLACE MACRO hist_irate_quantile(buckets, q, ts, p) AS
    h2_quantile(h2_delta(buckets, LAG(buckets) OVER (ORDER BY ts)), q, p);

-- Same but over a 5-minute window — equivalent to PromQL
-- `histogram_quantile(q, rate(<h>[5m]))`.
CREATE OR REPLACE MACRO hist_rate5m_quantile(buckets, q, ts, p) AS
    h2_quantile(h2_delta(buckets, LAG(buckets, 300) OVER (ORDER BY ts)), q, p);

-- ---- Layer B: dashboard-concept helpers ----
--
-- Each composes the Layer A primitives; expanding by hand recovers the
-- same SQL the original PromQL `irate(...)` formulas spell out.

-- CPU fraction (0..1) — works for total CPU busy and for per-state usage
-- (user/system/etc), which are the same shape against different inputs.
CREATE OR REPLACE MACRO cpu_busy_pct(usage, cores, ts) AS
    irate_1s(usage, ts) / cores / 1e9;

-- Instructions per cycle.
CREATE OR REPLACE MACRO ipc(instructions, cycles, ts) AS
    irate_1s(instructions, ts) / nullif(irate_1s(cycles, ts), 0);

-- Effective CPU frequency in Hz.
CREATE OR REPLACE MACRO frequency_hz(tsc, aperf, mperf, cores, ts) AS
    irate_1s(tsc, ts) * irate_1s(aperf, ts) / nullif(irate_1s(mperf, ts), 0) / cores;

-- Instructions per nanosecond (wall-clock-normalised throughput) =
-- ipc × frequency / cores / 1e9.
CREATE OR REPLACE MACRO ipns(instructions, cycles, tsc, aperf, mperf, cores, ts) AS
    ipc(instructions, cycles, ts)
    * irate_1s(tsc, ts) * irate_1s(aperf, ts)
    / nullif(irate_1s(mperf, ts) * cores * 1e9, 0);

-- L3 cache hit fraction.
CREATE OR REPLACE MACRO l3_hit_pct(miss, access, ts) AS
    1 - irate_1s(miss, ts) / nullif(irate_1s(access, ts), 0);

-- Branch misprediction fraction.
CREATE OR REPLACE MACRO branch_miss_pct(misses, branches, ts) AS
    irate_1s(misses, ts) / nullif(irate_1s(branches, ts), 0);

-- DTLB misses per thousand instructions.
CREATE OR REPLACE MACRO dtlb_mpki(misses, instructions, ts) AS
    irate_1s(misses, ts) / nullif(irate_1s(instructions, ts), 0) * 1000;

-- GPU memory used as fraction of total (used + free). No window needed.
CREATE OR REPLACE MACRO gpu_mem_used_pct(used, free) AS
    used / nullif(used + free, 0);

-- Bandwidth in bits per second from a byte counter.
CREATE OR REPLACE MACRO bps_from_bytes(bytes, ts) AS
    irate_1s(bytes, ts) * 8;

-- h2_combine_lol: combine a LIST<LIST<UBIGINT>> of bucket arrays into
-- one elementwise-summed array. Used by the dashboard's
-- `hist_percentile_series_combined` emitter to fold the columns
-- matched by a regex (e.g. `syscall_latency/[a-z]+:buckets`) into a
-- single histogram before quantile fan-out.
--
-- Why this exists alongside the native variadic UDF: the native side
-- registers `h2_combine(UBIGINT[], UBIGINT[], ...)` (1..32 variadic
-- LIST<UBIGINT> signatures); the duckdb-wasm side cannot match that
-- shape (macros don't support variadic args) and previously shipped
-- its own `MACRO h2_combine(lol)` that took a LIST<LIST<UBIGINT>>.
-- That divergence made `h2_combine([*COLUMNS('regex')])` bind on wasm
-- but error with `h2_combine(UBIGINT[][])` on native. The shared
-- list-of-lists macro under a distinct name eliminates the parity
-- hazard while keeping the fast native variadic UDF for direct
-- column-by-column callers.
CREATE OR REPLACE MACRO h2_combine_lol(lol) AS
    list_transform(
        generate_series(1, list_max(list_transform(lol, h -> length(h::UBIGINT[])))),
        j -> list_sum(list_transform(lol, h -> coalesce((h::UBIGINT[])[j], 0::UBIGINT)))::UBIGINT
    );
