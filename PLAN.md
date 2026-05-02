# Plan: shadow every Rezolus frontend query through SQL twins

## Context

The original task — surveyed at the start of this session — was to itemize every PromQL query the Rezolus frontend issues and add a SQL twin for each one in `metriken-query/queries.toml`. The previous round of work shipped the *infrastructure* that makes this practical:

- **Capture-group templating** (`metriken-query/src/template.rs`, `metriken-query-sql/src/interp.rs`) — one entry like `histogram_quantile(${q:number}, ${m:ident})` covers every (quantile, metric) pair.
- **Two real semantic fixes** the shadow harness surfaced against `demo.parquet`: timestamp-snap in `_src` (`metriken-query-sql/src/views.rs`) and `h2_combine(list(buckets))` GROUP BY for multi-series histogram aggregation (`queries.toml: histogram_quantile_generic`).
- **A divergence inspector** (`metriken-query/tests/divergence_inspector.rs`) that runs both engines against `/work/rezolus/site/viewer/data/demo.parquet` and asserts byte-equal canonical JSON.

What's left is the actual enumeration: walking the 11 shape groups in `metriken-query/docs/rezolus-query-backlog.md` (~196 production templates) and producing templated entries that cover them. Done right, those 196 queries collapse to ~20–30 templated catalogue entries. Done wrong, this is 196 near-duplicate entries — that's why the templating infrastructure had to land first.

End state of this plan: every PromQL template the Rezolus dashboards issue (Rust dashboards in `rezolus/crates/dashboard/src/dashboard/*.rs` and JSON service templates in `rezolus/config/templates/*.json`) matches a `mode = "shadow"` catalogue entry, and curling any of them against `rezolus view <real.parquet>` produces zero `WARN ... divergence` lines in the server log.

## Status going in

Catalogue today (20 entries — all `mode = "shadow"`):

```
gauge_bare, gauge_negative, gauge_avg_over_time
counter_irate_basic, counter_rate_basic, counter_irate_reset, counter_rate_reset
counter_sum_by_id, counter_sum_by_id_filtered, counter_total_sum, counter_ratio
histogram_quantile_generic*, histogram_quantiles_rezolus_ext, histogram_heatmap
histogram_quantile_with_reset, histogram_quantile_empty_period
rezolus_cpu_busy_pct, rezolus_cpu_user_per_id, rezolus_syscall_p99
softirq_irate_total_by_kind*
```
(* = templated, capture-group)

Inventory of what's *not* yet covered, from `metriken-query/docs/rezolus-query-backlog.md`:

| Group | Shape                                              | # templates | Lift     |
|-------|----------------------------------------------------|------------:|----------|
| K     | network/scheduler/blockio singletons               | 5           | trivial  |
| I     | GPU gauges with scaling                            | 5           | small    |
| H     | memory/NUMA gauges + arithmetic                    | 7           | small    |
| J     | Rezolus self-monitoring (BPF overhead)             | 3           | small    |
| F     | softirq per-kind family (4 shapes × 10 kinds)      | ~20 (4 ent.)| small    |
| A     | bare service histograms (`{source="..."}` filter)  | 32          | small    |
| B     | service throughput w/ source label                 | 22          | small    |
| C     | cache/db service throughput                        | 19          | small    |
| D     | multi-operand CPU ratios (incl. IPNS chain)        | 10          | medium   |
| E     | per-id heatmap aggregations                        | 13          | small/med|
| G     | cgroup-filtered (`{__SELECTED_CGROUPS__}`)         | ~30         | medium   |

`histogram_heatmap` exists but is hard-coded to `request_latency`; production needs the same templated form as `histogram_quantile_generic`.

## Strategy

1. **One SQL pattern per shape, not per metric.** Most groups collapse to 1–4 templated entries. Group D's IPNS chain may stay concrete because its operand list is fixed.

2. **Real-parquet shadow validation is the promotion gate.** Synthetic fixtures cover edge cases; `demo.parquet` and friends cover what dashboards actually emit. An entry isn't "done" until the divergence inspector confirms PromQL=SQL on at least one real parquet.

3. **Auto-walk the catalogue in the divergence inspector.** Replace the hand-coded `divergence_inspector.rs` test list with a loop over `(entry, example, representative_parquet)` so adding a new entry's `examples` automatically gets shadow coverage. This is the smallest possible feedback loop for new entries.

4. **Order by lift, but small first.** Per the backlog's recommendation:
   1. **K + I + H + J + heatmap-templating** (~25 templates → ~6–8 entries) — one weekend's worth of work; proves the auto-walker.
   2. **A + B + C + F** (~93 templates → ~10 entries) — needs the `${range:duration}` capture kind so `[5m]` and `[5s]` collapse into one entry per shape.
   3. **D + E** (23 templates → ~10 entries, mostly concrete for D) — IPNS family worth an exhaustigen test.
   4. **G** (~30 templates → 3–5 entries with `${labels:labels}` captures).

5. **Document SQL idioms in the catalogue header** as we go. Future maintainers should be able to add a Group-K-shaped entry by reading queries.toml's preamble.

## Infrastructure changes (small, land first)

### I.1 Auto-walking divergence inspector

`metriken-query/tests/divergence_inspector.rs` today has 4 hand-coded `#[test] fn` entries for histogram_quantile variants. Replace with one parameterised harness:

```rust
fn parquets() -> Vec<PathBuf> {
    let dir = Path::new("/work/rezolus/site/viewer/data");
    ["demo", "cachecannon", "vllm", "sglang_gemma3"]
        .iter()
        .map(|n| dir.join(format!("{n}.parquet")))
        .filter(|p| p.exists())
        .collect()
}

#[test]
fn every_shadow_entry_matches_promql_on_real_parquets() {
    let cat = Catalogue::embedded();
    let mut failures = Vec::new();
    for parquet in parquets() {
        for entry in cat.entries() {
            if entry.mode != Mode::Shadow { continue; }
            for q in concrete_queries(entry) {
                if let Some((promql, sql)) = run_pair(&parquet, entry, &q) {
                    if canonicalise(&promql) != canonicalise(&sql) {
                        failures.push(format!("{}::{} on {}", entry.id, q,
                                              parquet.file_name().unwrap().to_string_lossy()));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "shadow divergences:\n{}", failures.join("\n"));
}
```

`concrete_queries(entry)` returns `[entry.promql.clone()]` for literal entries and `entry.examples.iter().map(|e| e.query.clone()).collect()` for templated ones. `run_pair` returns `None` when the metric isn't in this parquet (so vllm-only entries don't fail against demo.parquet).

This test fails loud the moment any new entry diverges on real data. **It IS the promotion gate.**

### I.2 `${range:duration}` capture kind

Add to `template.rs`. Grammar: `<digits><s|m|h>`. Capture value: `Duration(u64)` in nanoseconds (or `seconds: u64` — pick whichever interpolates cleanest). SQL transform `{r:as_seconds}` emits the integer second count.

Rezolus's Rust dashboards always use `[5m]`; the JSON service templates use `[5s]`. With `${range:duration}` plus `{r:as_seconds}`, one entry covers both. Without it, every shape needs duplicate entries.

Wire-time impact: small. ~30 LOC in `template.rs` + 2 tests + a transform branch in `interp.rs`.

### I.3 Generalise `histogram_heatmap`

The existing entry is hardcoded to `request_latency`. Templatise the same way as `histogram_quantile_generic`:

```toml
[[query]]
id = "histogram_heatmap_generic"
mode = "shadow"
output_shape = "heatmap"
promql = "histogram_heatmap(${m:ident})"
sql = """
WITH paired AS (
    SELECT a.timestamp,
           h2_delta(h2_combine(list(a.buckets)),
                    h2_combine(list(b.buckets))) AS d,
           ANY_VALUE(a.p) AS p
    FROM {m} a
    JOIN {m} b ON CAST(b.timestamp AS HUGEINT) = CAST(a.timestamp AS HUGEINT) - 1000000000
    GROUP BY a.timestamp
)
SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t,
       (u.ordinal - 1)::INTEGER AS bucket_idx,
       u.value::DOUBLE AS count,
       p::INTEGER AS p
FROM paired
CROSS JOIN UNNEST(d) WITH ORDINALITY AS u(value, ordinal)
WHERE u.value > 0
ORDER BY t, bucket_idx
"""
examples = [
    { id_suffix = "request_latency", query = "histogram_heatmap(request_latency)" },
    { id_suffix = "scheduler_runqueue_latency", query = "histogram_heatmap(scheduler_runqueue_latency)" },
]
```

Same pattern as `histogram_quantile_generic` — `h2_combine(list(buckets))` GROUP BY timestamp aggregates across multi-series metrics. Self-join workaround sidesteps the `LAG(buckets)` chunk-boundary issue.

## Per-group execution plan

For each group below: the SQL pattern, the captures, and the metric list it absorbs from the backlog. Implementation is mechanical — write the entry, drop a `[[query.examples]]` for one synthetic + one real-data instantiation, run `cargo test --test divergence_inspector`, fix any divergence found.

### Group K — singletons (5 templates → 1 entry)

`sum(irate(${m:ident}[5m]))` against bare counters with no labels. Reuses `counter_total_sum`'s SQL. Metrics absorbed: `network_drop`, `tcp_retransmit`, `scheduler_context_switch`, `blockio_bytes`, `blockio_operations`.

### Group I — GPU metrics (5 templates → 2 entries)

- `sum(${m:ident}) / ${d:number}` (gauge × scalar): `gpu_power_usage / 1000`, `avg(gpu_memory_utilization) / 100`, `sum(gpu_pcie_throughput{direction="..."})`. Two flavours: `sum` and `avg` (different aggregations). 2 entries with `${d:number}` capture.
- `sum(rate(${m:ident}[5m])) / ${d:number}` (gpu_energy_consumption — `rate`, not `irate`).

### Group H — memory / NUMA gauges (7 templates → 3 entries)

- Bare gauge `${m:ident}` (already handled by `gauge_bare` if we generalise the metric capture — currently `gauge_bare` is hardcoded to `memory_total`. Promote it to `${m:ident}`).
- Gauge subtraction `${a:ident} - ${b:ident}` for `memory_total - memory_available`.
- Gauge ratio `(${a:ident} - ${b:ident}) / ${a:ident}` for utilisation %.

### Group J — Rezolus self-monitoring (3 templates → 3 entries)

Concrete (no templating gain) because each one has a unique formula (`rezolus_bpf_run_time / 1e9`, etc.). Pattern matches `rezolus_cpu_busy_pct`.

### Group F — softirq per-kind (4 shapes × 10 kinds → 4 entries)

We already have one (`softirq_irate_total_by_kind`). Add three more:
- `sum by (id) (irate(softirq{kind=${k:string}}[5m]))`
- `sum(irate(softirq_time{kind=${k:string}}[5m])) / ${cores:number} / 1000000000`
- `sum by (id) (irate(softirq_time{kind=${k:string}}[5m])) / 1000000000`

### Group A — bare service histograms (32 templates → 1 entry)

`${m:ident}{source=${s:string}}` — any histogram metric filtered by `source` label. The viewer wraps with `histogram_percentiles(...)` (handled separately by JSON template-side machinery) or `histogram_heatmap(...)` — but the *raw* PromQL query reaching us is the bare selector with the source filter. SQL: same as the `request_latency` view access, but with `WHERE source = {s}` filter on the auto-generated view.

Caveat: many service metrics include `/` in the metric name (`redis/total_commands_processed`). The view generator quotes identifiers; verify the `${m:ident}` capture handles this — it does NOT today (regex is `\w+`). Need to extend `${m:ident}` to allow `/` OR add a new `${m:metric_name}` kind that allows the wider character set Rezolus uses.

### Group B — service throughput w/ source label (22 templates → 1 entry, with caveat)

`sum(irate(${m:ident}{${labels:labels}}[${r:duration}]))` — covers `[5s]` (JSON) and `[5m]` (Rust). Multi-label filter via `${labels:labels}`. SQL pattern: `counter_total_sum` + WHERE `{labels:as_predicate}`.

Caveat: depends on `${range:duration}` (I.2) and the wider `${m:ident}` (Group A caveat). Once both land this collapses to one entry covering most of Groups A, B, C, F.

### Group C — cache/db throughput (19 templates → 0 new entries)

Subsumed by Group B's templated entry. Verify by adding examples for cachecannon/valkey queries to Group B's entry.

### Group D — multi-operand CPU ratios (10 templates → ~6 entries)

Mostly concrete (each formula has a fixed operand list). But we can collapse:
- IPC: `sum(irate(${a:ident}[5m])) / sum(irate(${b:ident}[5m]))` covers IPC, branch-miss-rate, etc. — same shape, different operand pair.
- IPNS chain (6 metrics): one concrete entry, no templating gain. Add an exhaustigen test to lock semantics (mentioned in master plan §2.6).
- L3 hit %: `1 - sum(irate(${a:ident}[5m])) / sum(irate(${b:ident}[5m]))` — separate entry due to leading constant.
- DTLB MPKI: `sum(irate(${a:ident}[5m])) / sum(irate(${b:ident}[5m])) * 1000` — separate entry due to trailing scalar.

### Group E — per-id heatmap aggregations (13 templates → 3 entries)

- Simple: `sum by (id) (irate(${m:ident}[5m])) / ${d:number}` — covers per-CPU usage variants.
- Per-CPU IPC: `sum by (id) (irate(${a:ident}[5m])) / sum by (id) (irate(${b:ident}[5m]))`.
- Per-CPU IPNS chain: concrete.

### Group G — cgroup-filtered (~30 templates → 3 entries)

The matching prerequisite (capture-group templating with `${labels:labels}`) is already done. The PromQL form after JS substitution is e.g. `sum by (name) (irate(cgroup_cpu_usage{name=~"/system.slice/.+"}[5m]))`. The same shape entries as Group B with `name=~` regex matchers — `{labels:as_predicate}` already emits `regexp_matches(...)` for `=~`. No new infrastructure needed.

## Files modified

**Modify:**
- `metriken-query/queries.toml` — add ~20 new templated entries; update header docs with the new SQL idioms; promote `gauge_bare` to a templated `${m:ident}` form.
- `metriken-query/src/template.rs` — add `${name:duration}` capture kind (~30 LOC + 2 tests).
- `metriken-query-sql/src/interp.rs` — add `as_seconds` transform for duration captures.
- `metriken-query/tests/divergence_inspector.rs` — replace hand-coded tests with the auto-walker (the promotion gate).
- `metriken-query-fixtures/src/bin/generate.rs` and `metriken-query-fixtures/src/lib.rs` — add 1–2 fixtures: a multi-source histogram (for Group A goldens) and a NUMA gauge fixture (for Group H).

**Reuse (do not duplicate):**
- `metriken-query-sql/src/macros.rs` — `irate_1s`, `rate_5m`, `cpu_busy_pct`, `ipc`, `ipns`, `l3_hit_pct`, `branch_miss_pct`, `dtlb_mpki`, `gpu_mem_used_pct`, `bps_from_bytes`. Most Group-D and Group-E SQL twins are one-liners that compose these macros.
- `metriken-query-sql/src/udf.rs` — `h2_combine` (for multi-series histogram aggregation), `h2_delta`, `h2_quantile`, `h2_quantiles`. The snap fix in `views.rs` is universal — every templated entry inherits it.
- `metriken-query/src/template.rs` and `metriken-query-sql/src/interp.rs` — the templating + interpolation surface is complete; this plan adds only the duration kind.

## Verification

1. `cargo test -p metriken-query --lib template::` and `-p metriken-query-sql --lib interp::` — template and interp tests pass after duration-capture additions.
2. `cargo test -p metriken-query --test golden` — every existing snapshot still passes (the entry-set change is additive plus the gauge_bare templatisation, which the existing goldens absorb).
3. `cargo test -p metriken-query --test divergence_inspector` — auto-walker iterates every shadow-mode entry × every example × every available real parquet (`demo`, `cachecannon`, `vllm`, `sglang_gemma3`). Zero failures = ready to ship.
4. **Curl smoke test** against the live viewer for each shape group:
   ```
   /work/rezolus/target/release/rezolus view /work/rezolus/site/viewer/data/demo.parquet --listen 127.0.0.1:18090
   for q in <one query per group>; do
       curl -s "http://127.0.0.1:18090/api/v1/query_range?query=$q&start=...&end=...&step=1" >/dev/null
   done
   grep -c divergence /var/log/rezolus.log   # MUST be 0
   grep -c "no catalogue match" /var/log/rezolus.log   # ← optional new log line for unmatched queries
   ```
5. **Browser smoke test** — open the dashboard against `demo.parquet`, navigate every section (CPU, memory, network, blockio, syscall, scheduler, softirq, cgroups, gpu, overview, rezolus). Every chart should render and the server log should show zero divergence warnings.

## Out of scope

- Promoting any entry from `shadow` to `strict` or `primary`. That's the next phase, gated by a quiescence period of clean shadow telemetry.
- Removing the PromQL evaluator (Phase 4 of the master plan at `hello-please-give-me-agile-moonbeam.md`).
- WASM viewer support — the `rezolus/crates/viewer/src/lib.rs` WASM build still uses metriken-query directly without DuckDB.
- Service KPIs that wrap with `histogram_percentiles([...], q)` — that wrapper is applied by `rezolus/crates/dashboard/src/service_extension.rs` before the query reaches metriken-query, so by the time the dispatcher sees it the query is `histogram_percentiles(...)` not the raw selector. Either: (a) handle `histogram_percentiles` as its own templated entry shape (cheap), or (b) intercept at the wrapper boundary. Pick when we hit the first wrap-bearing JSON template.
- An exhaustigen property test for the IPNS chain (mentioned in master plan §2.6) — track separately if Group D's concrete entry passes shadow without it.
