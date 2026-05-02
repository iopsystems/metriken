# Plan: investigate the 33 PromQL/SQL divergences uncovered by the auto-walker

## Context

`metriken-query/tests/divergence_inspector.rs::divergence_report` walks every
`mode = "shadow"` catalogue entry × every `examples[*]` × every real Rezolus
parquet in `/work/rezolus/site/viewer/data/`, runs both engines, and compares
the canonical JSON. The current run reports **110 pairs checked, 70 skipped
(metric not in parquet), 33 divergences**. All divergences are pre-existing
or surfaced by the new auto-walker on real data; **every synthetic-fixture
golden is byte-equal**, so the SQL idioms work in isolation — the divergences
come from real-parquet shapes the catalogue entries don't yet handle.

Goal: convert each divergence to either (a) a fix that makes PromQL = SQL on
that real parquet, or (b) a documented "expected divergence" with the entry
moved to `mode = "off"` until a follow-up lands. After this work the
`#[ignore]`d strict gate (`every_shadow_entry_matches_promql_on_real_parquets`)
should pass — flipping the ignore makes the auto-walker a CI block.

## Inventory: 33 divergences grouped by root cause

I bucket the divergences into 5 root causes. Some entries appear in multiple
buckets across different parquets.

### Bucket 1 — Missing label projection (multi-source SQL collapses series)

Real Rezolus parquets often carry the same metric under multiple sources
(`0::memory_total`, `rezolus-client::memory_total`, …). The auto-generated
view UNION-ALLs them under a single `memory_total` view. PromQL surfaces
each source as its own series; the catalogue SQL projects only `value` (no
labels), so the matrix projector in `metriken-query-sql/src/backend.rs::run_matrix`
collapses every row into a single labelless series.

**Smoking-gun signature:** `PromQL series=N`, `SQL series=1` (N > 1).

| Entry | Parquet | Symptom |
|---|---|---|
| `gauge_bare::memory_total` | cachecannon | 2 vs 1 |

(`gauge_bare` on demo / vllm / sglang / vllm_gemma3 reports 1 vs 1 — same
shape — but those are likely Bucket 4 value differences.)

### Bucket 2 — Multi-series histogram aggregation missing in concrete entries

`histogram_quantile_generic` already uses `h2_combine(list(buckets)) GROUP BY
timestamp` so multi-`op` syscall_latency series get aggregated before the
quantile. The hardcoded entry `rezolus_syscall_p99` uses the older
single-series form (`SELECT … FROM syscall_latency` with `LAG(buckets) OVER
(ORDER BY timestamp)`) — fine for the synthetic single-series fixture, wrong
for real syscall_latency which has dozens of `op` series.

**Smoking-gun signature:** appears on *every* real parquet that has a
multi-series histogram.

| Entry | Parquets |
|---|---|
| `rezolus_syscall_p99::histogram_quantile(0.99, syscall_latency)` | demo, cachecannon, vllm, sglang_gemma3, vllm_gemma3 |

### Bucket 3 — Empty/error canonicalisation drift

When the metric is absent or the histogram is empty, PromQL and SQL surface
that condition differently. `metriken-query/src/promql/streaming/histogram.rs`
returns `MetricNotFound`; `metriken-query-sql/src/backend.rs::run_heatmap`
returns either `Err(SqlError::Backend("no histogram data"))` or an empty
`HistogramHeatmapResult` whose `min_value`/`max_value` default to `0.0`.
After `canonicalise()` the JSON shapes differ even though the user-visible
intent ("nothing here") is the same.

**Smoking-gun signature:** `series=0 vs series=0` divergence — both empty,
JSON differs.

| Entry | Parquet |
|---|---|
| `histogram_heatmap_generic::histogram_heatmap(request_latency)` | vllm |

### Bucket 4 — Value-level same-count divergences

Same series count, but the points (or label sets) differ. Several plausible
causes; need per-entry triage:

- Per-series reset semantics on multi-series counters (the `irate` SQL uses
  `CASE WHEN value >= LAG(value)` per series via `PARTITION BY col`, but
  PromQL's reset-aware path computes `extrapolatedRate` differently around
  range-edge / zero-deltas).
- FP precision in cross-series `SUM`/`AVG` (DuckDB's evaluation order vs
  PromQL's accumulator).
- First-sample boundary: PromQL skips the first sample when `LAG` is NULL;
  SQL filters with `WHERE rate IS NOT NULL` — usually equivalent, but on
  parquets whose first row coincides with the query `start` they may
  emit/skip a different point.
- Sampling-interval snap interaction: `views.rs::ensure_views` snaps every
  parquet timestamp; PromQL uses unsnapped timestamps. For metrics whose
  raw timestamps already align this is a no-op, otherwise there can be a
  one-tick offset.

**Smoking-gun signature:** `PromQL series=N, SQL series=N` (N matches) and
JSON differs.

Demo (cpu_usage family — 8 cpus × 2 states):
- `counter_sum_by_id::sum by (id) (irate(cpu_usage[5m]))` (8 vs 8)
- `counter_sum_by_id_filtered::sum by (id) (irate(cpu_usage{state="user"}[5m]))` (8 vs 8)
- `counter_total_sum::sum(irate(cpu_usage[5m]))` (1 vs 1)
- `counter_ratio::sum(irate(cpu_usage{state="user"}[5m])) / sum(irate(cpu_usage[5m]))` (1 vs 1)
- `counter_total_sum_generic::sum(irate(cpu_usage[5m]))` (1 vs 1) — same
  query as `counter_total_sum`; symptom is mirrored.
- `rezolus_cpu_busy_pct::sum(irate(cpu_usage[5m])) / 2 / 1000000000` (1 vs 1)
- `rezolus_cpu_user_per_id::sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` (8 vs 8)

Demo (softirq family):
- `softirq_irate_total_by_kind::…{kind="net_rx"}` (1 vs 1)
- `softirq_irate_total_by_kind::…{kind="timer"}` (1 vs 1)
- `softirq_irate_by_id_by_kind::sum by (id) (…{kind="net_rx"}…)` (8 vs 8)
- `softirq_time_pct_by_kind::…{kind="net_rx"} / 2 / 1e9` (1 vs 1)
- `softirq_time_pct_by_kind::…{kind="timer"} / 2 / 1e9` (1 vs 1)
- `softirq_time_pct_by_id_by_kind::sum by (id)…{kind="net_rx"}/1e9` (8 vs 8)

Demo (gauges with arithmetic):
- `gauge_sum_scaled::sum(memory_total) / 1024` (1 vs 1)
- `gauge_avg_scaled::avg(memory_total) / 1` (1 vs 1)
- `gauge_bare::memory_total` (1 vs 1) — same metric, simpler shape.

cachecannon (excl. Bucket 1):
- `softirq_irate_total_by_kind::…{kind="net_rx"}` (1 vs 1)
- `softirq_irate_total_by_kind::…{kind="timer"}` (1 vs 1)

vllm:
- `gauge_bare::memory_total` (1 vs 1)
- `counter_irate_basic::irate(requests[5m])` (1 vs 1)
- `counter_rate_basic::rate(requests[5m])` (1 vs 1)
- `counter_irate_reset::irate(requests[5m])` (1 vs 1)
- `counter_rate_reset::rate(requests[5m])` (1 vs 1)

sglang_gemma3 / vllm_gemma3:
- `gauge_bare::memory_total` (1 vs 1)
- `counter_rate_sum_scaled::sum(rate(requests[5m])) / 1` (1 vs 1, sglang only)

### Bucket 5 — Universally-recurring `rezolus_syscall_p99` (subset of Bucket 2)

Five identical `rezolus_syscall_p99` divergences (one per parquet) — all
explained by Bucket 2. Listed separately because fixing Bucket 2 closes all
five at once: it's a one-line catalogue change.

## Investigation playbook

Per bucket, the investigation steps + the likely fix shape. Each numbered
item is a self-contained 30–60 minute sub-task.

### Investigation 1 — Multi-source label projection (Bucket 1)

**Approach.** For `gauge_bare::memory_total` on cachecannon, run both
engines manually and dump:

```bash
duckdb -c "DESCRIBE memory_total" </path/to/cachecannon.parquet>   # column shape of the view
```

Confirm the auto-generated `memory_total` view exposes a label column
(`source` or similar) carrying the per-source identity. If yes, the SQL
needs to project that label and the entry needs the matching `label_columns`.

**Likely fix.** Either:
- Promote the entry to a "passthrough" form that projects `* EXCLUDE
  (timestamp, value)` as labels, with `label_columns` populated dynamically
  by the backend. This needs a small `metriken-query-sql/src/backend.rs`
  change to read the label column list from the prepared statement schema
  rather than from the catalogue entry.
- Or: write multiple per-shape entries (`gauge_bare_source_labelled`, etc.)
  — viable but produces N entries per metric shape.

The first option is the direction the plan should drive. Critical files:
- `metriken-query-sql/src/backend.rs::run_matrix` (lines 145–208) —
  currently reads `entry.label_columns` from the catalogue.
- `metriken-query-sql/src/views.rs::build_view_sql` (lines 288–337) —
  emits the label-keys union per metric.
- `metriken-query/queries.toml::gauge_bare`.

**Verification.** After the fix, the divergence inspector report's count for
Bucket 1 should be zero.

### Investigation 2 — Templatise `rezolus_syscall_p99` over the generic shape (Bucket 2 / 5)

Mechanical: replace the entry with the same `WITH per_ts AS (… h2_combine
(list(buckets)) GROUP BY timestamp …)` CTE that `histogram_quantile_generic`
uses. Critical files: `metriken-query/queries.toml` (the
`rezolus_syscall_p99` block, ~lines 591–608).

Alternative: since `histogram_quantile_generic` already matches `0.99,
syscall_latency`, **delete** `rezolus_syscall_p99` entirely. Verify that
golden snapshot `rezolus_syscall_p99.snap` produces the same canonical JSON
when matched by the templated entry (it should — same SQL output). Promote
the templated entry's example list to include `syscall_latency`.

**Verification.** Re-run divergence_report; all five `rezolus_syscall_p99`
rows should disappear. Re-run goldens; the existing `rezolus_syscall_p99`
golden either stays attached to a renamed entry or is removed.

### Investigation 3 — Empty heatmap canonicalisation (Bucket 3)

Decide on a contract for "no histogram data," then make both engines
conform. Two reasonable choices:
1. Both return an empty matrix-like result (`{"result": [], "resultType":
   "matrix"}` or its heatmap analogue with empty arrays and `min/max = 0.0`).
2. Both return `MetricNotFound`.

Choice (1) is more user-friendly; (2) is closer to PromQL's letter. Pick (1)
and align the PromQL path: in
`metriken-query/src/promql/streaming/histogram.rs::heatmap` (around
line ~430 per the explore report), replace the `return Err(MetricNotFound)`
on empty with a return of an empty `HistogramHeatmapResult`. The SQL path
already does this — its current "no histogram data" `Err` would also need
to be replaced with the same empty struct in
`metriken-query-sql/src/backend.rs::run_heatmap` (lines 235–240).

**Verification.** Run only the affected pair manually:

```bash
cargo test -p metriken-query --test divergence_inspector dump_histogram_quantile_diff -- --nocapture
```

(Adapt the diagnostic test or write a similar one targeted at
`histogram_heatmap(request_latency)` against vllm.parquet.)

### Investigation 4 — Same-count value drift (Bucket 4)

This is the largest bucket and most variable in cause. Recommend a
**diagnostic-first** approach: extend the report so each divergence dumps
the first pointwise difference. Then fix root causes top-down, each fix
typically closing several rows at once.

Suggested diagnostic addition (replaces the `series=N vs series=N` summary
with per-pair detail when `RUST_LOG=divergence_inspector=trace` is set):

```rust
// In divergence_inspector.rs, on a divergence:
//   - find the first series where PromQL ≠ SQL
//   - dump up to 5 (t, v_promql, v_sql) tuples around the first difference
//   - print the difference at index, the relative error, the SQL string used
```

Files to modify: `metriken-query/tests/divergence_inspector.rs`.

Once the inspector emits structured first-diff context, fix in this order:

**4a — `cpu_usage` family on demo (one common root cause).** Pick
`counter_total_sum::sum(irate(cpu_usage[5m]))` as the canonical entry-point
case (simplest shape). Compare PromQL's per-series irate values against the
SQL's `CASE WHEN value >= LAG(value) …` per timestamp. Hypothesis: PromQL's
streaming `irate` uses the *most-recent two* samples within the lookback
window (and divides by their `(end - prev_end) / 1e9`), whereas the SQL
emits the raw delta in nanoseconds (because `irate_1s` is "per-1-second").
Validate by reading
`metriken-query/src/promql/streaming/{rate,irate}.rs` and comparing with
the macro in `metriken-query-sql/src/macros.rs::irate_1s`.

If the hypothesis holds, fix is a one-symbol change in the macro: divide
by the actual `(ts - LAG(ts))/1e9` instead of trusting the 1Hz-sampling
assumption. The macro already takes `ts`, so this is local.

**4b — `softirq` family on demo.** Same `irate` semantics; should ride on
4a's fix.

**4c — `gauge_*_scaled` on demo.** Gauge cross-series sum/avg + scalar
divisor. Hypothesis: order-of-evaluation. PromQL evaluates `sum()` then
divides; SQL does `SUM(value) / divisor` per row but with possibly
different aggregation order. Re-check by emitting the per-row computed
value side-by-side.

**4d — `requests` on vllm.** vllm has only 1 `requests` column (per the
explorer probe), so the literal-template SQL `FROM requests` against the
view should agree with PromQL. Most likely cause: the literal entries
(`counter_irate_basic`, `counter_rate_basic`, `counter_irate_reset`,
`counter_rate_reset`) were tuned for the synthetic fixture's *specific*
range and step. On real vllm data the parquet has different timestamps
than the entry's hardcoded `start`/`end`/`step`. The auto-walker probes
the parquet's actual range (`probe_time_range`) and uses `step = 1.0`,
which the hardcoded entries never tested. Hypothesis: PromQL vs SQL
disagree on the *first* sample of the range when the parquet's first row
sits exactly at `start`. Test by narrowing the start by one step and
re-checking.

**4e — `gauge_bare` on every parquet (1 vs 1).** Probably the single
common cause: timestamp-snap interaction. Cachecannon's parquet metadata
declares a `sampling_interval_ms` that rounds *up* an off-by-microseconds
timestamp. PromQL doesn't snap. So the `t` axis is offset by ≤ half a
sampling interval. Confirm with:

```rust
// pick gauge_bare on cachecannon, dump first 3 PromQL points + first 3 SQL points
```

If confirmed, the fix is to also snap PromQL-side, or to bypass the snap
in `views.rs::ensure_views` for sources that don't actually need it. The
plan should bias toward the snap being canonical (it makes alignment
between metrics tractable on the SQL side); align PromQL to it.

### Investigation 5 — Diagnostic harness improvements

Two upgrades to the auto-walker that pay for themselves across every
bucket above:

1. **Per-failure first-difference dump** (mentioned in Investigation 4).
   Without this, every fix attempt requires a custom one-off debug
   harness — slow and error-prone.
2. **Persistent denylist + allowlist**. Add an
   `expected_divergences.toml` (or a comment block in `queries.toml`)
   listing `(entry_id, parquet_basename)` pairs known to diverge. The
   `every_shadow_entry_matches_promql_on_real_parquets` strict gate then
   passes when only known divergences remain — flipping `#[ignore]`
   becomes safe even before every divergence is fixed. As fixes land, the
   denylist shrinks; introducing new divergences fails the gate
   regardless.

Files to modify: `metriken-query/tests/divergence_inspector.rs`.

## Critical files (consolidated)

- `metriken-query/queries.toml` — entry definitions; most fixes land here.
- `metriken-query/tests/divergence_inspector.rs` — auto-walker + diagnostic
  upgrades (Investigation 5).
- `metriken-query-sql/src/backend.rs::run_matrix` (lines 145–208) — label
  projection; the Bucket-1 fix lives here.
- `metriken-query-sql/src/backend.rs::run_heatmap` (lines 213–313) —
  empty-result canonicalisation (Bucket 3).
- `metriken-query-sql/src/views.rs::ensure_views` (lines 97–212) — the
  parquet-load and timestamp-snap pass; relevant to Bucket 4e.
- `metriken-query-sql/src/macros.rs::irate_1s` (lines 27–32) — the
  per-second delta macro; possible Bucket 4a culprit.
- `metriken-query/src/promql/streaming/{irate,rate,histogram}.rs` — PromQL
  reference behaviour; consult for any value-drift investigation.

## Verification (end-to-end)

After each investigation:

```bash
# 1. Synthetic-fixture goldens still pass.
cargo test -p metriken-query --test golden

# 2. Divergence count went down, not up.
cargo test -p metriken-query --test divergence_inspector divergence_report -- --nocapture
# → look at the "shadow auto-walker: checked=N skipped=M failures=K" line.

# 3. Strict gate when ready.
cargo test -p metriken-query --test divergence_inspector --ignored
```

Final acceptance: `failures=0` on the auto-walker against all five
real parquets, and the strict gate (`#[ignore]` removed) is green.

## Priority order

1. **Investigation 5.1 (per-failure first-diff dump)** — tooling investment;
   makes every other investigation faster.
2. **Investigation 2 (rezolus_syscall_p99)** — closes 5 divergences with a
   one-block catalogue change.
3. **Investigation 4e (gauge_bare timestamp-snap)** — closes 5 divergences
   if the hypothesis holds.
4. **Investigation 4a (cpu_usage irate semantics)** — closes ~7 divergences
   on demo if the hypothesis holds.
5. **Investigation 1 (multi-source label projection)** — small bucket today
   but the same fix unblocks Group A/B/C/G entries from the parent plan.
6. **Investigation 3 (empty heatmap canonicalisation)** — single divergence,
   fix is local.
7. **Investigation 4d, 4b, 4c** — narrower scope, address last.
8. **Investigation 5.2 (denylist gate)** — final wrap; lets us flip `#[ignore]`.

## Out of scope

- New catalogue entries (Groups A, B, C, D, E, G from the parent plan).
  These need separate fixture work and are tracked in `/work/metriken/PLAN.md`.
- Promotion of any entry from `shadow` to `strict` / `primary`. That waits
  on a clean shadow window after this work lands.
- Adjustments to PromQL semantics that would break existing PromQL clients.
  Where PromQL and SQL disagree, prefer changing SQL unless the PromQL
  behaviour is itself a documented bug.
