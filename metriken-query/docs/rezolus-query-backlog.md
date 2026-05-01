# Rezolus query backlog (un-shadowed)

This file inventories the PromQL templates the Rezolus viewer issues against
`metriken-query` that **don't yet have a SQL twin** in `queries.toml`. The
20 catalogue entries we have today were tuned to synthetic test fixtures;
production Rezolus dashboards and service templates issue ~137 distinct
templates, and a survey (2026-05) found that only **3 of those overlap
verbatim** with the catalogue:

- `memory_total` (matches `gauge_bare`)
- `rate(memory_numa_local[5m])` and `rate(memory_numa_foreign[5m])` —
  match the `counter_rate_*` shape but not the literal text

The 134 remaining templates need to be added (or matched via capture-group
templating, when that lands) before shadow-mode dispatch produces meaningful
end-to-end coverage on real Rezolus traffic.

## How to read this file

Templates are grouped by **shape**, not by literal text — production queries
mostly differ from each other only in the metric name, label values, or
arithmetic constants, so the SQL machinery to handle one shape handles the
rest. For each group:

- **Count**: number of distinct production templates in that shape.
- **Source**: representative file:line in the Rezolus repo.
- **Status**: whether the catalogue has a comparable shape (even if for a
  different metric).
- **Lift**: rough estimate of work to land the shape in the catalogue.

A "lift" of *small* means the SQL pattern is already proven elsewhere in the
catalogue and we just need new entries with new metric names. *Medium* means
a new SQL idiom (e.g. service-label filtering with `{source="vllm"}`) but no
new infrastructure. *Large* means new infrastructure (e.g. cgroup-filter
templating, multi-arg ratio chains, sliding-window stride support).

---

## Group A — bare service histograms (32 templates)

LLM service extensions (`vllm`, `sglang`, `sglang-prefill`, `sglang-decode`,
`llm-perf`) issue per-source latency histograms as bare metric selectors
filtered by `source`, mostly for `histogram_quantile` / `histogram_heatmap`
wrapping at the JS layer.

- **Source**: `rezolus/config/templates/{vllm,sglang,llm-perf}.json` (`query` fields)
- **Examples**:
  - `vllm_time_to_first_token_seconds{source="vllm"}`
  - `vllm_inter_token_latency_seconds{source="vllm"}`
  - `vllm_e2e_request_latency_seconds{source="vllm"}`
  - `sglang_e2e_request_latency_seconds{source="sglang"}`
  - `ttft{source="llm-perf"}` / `tpot{source="llm-perf"}` / `itl{source="llm-perf"}`
- **Status**: shape matches `histogram_quantile_p50` (the bare-histogram
  reading is the same SQL view access). Catalogue lacks the *label-filter*
  axis (`{source="vllm"}` etc.) because the test fixtures have no `source`
  label.
- **Lift**: small. View generator already handles labels; need entries that
  use them in WHERE clauses. Adding a fixture with a `source`-labeled
  histogram is the only prerequisite.

## Group B — service throughput / counts with source label (22 templates)

LLM service extensions also issue `sum(irate(...))` queries scoped by a
`source` label and sometimes a status, direction, or kind label.

- **Source**: `rezolus/config/templates/{vllm,sglang,llm-perf}.json`
- **Examples**:
  - `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))`
  - `sum(irate(sglang_num_aborted_requests_total{source="sglang"}[5s]))`
  - `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))`
  - `sum(irate(requests{status="error",source="llm-perf"}[5s])) / sum(irate(requests{status="sent",source="llm-perf"}[5s]))`
- **Status**: shape matches `counter_sum_by_id_filtered` and `counter_ratio`.
  Differences: `[5s]` window instead of `[5m]`, multi-label filters
  (`status` AND `source`).
- **Lift**: small once we have a fixture with multi-label counter columns.

## Group C — cache / db service throughput (19 templates)

`cachecannon` and `valkey` service templates use the same shape as Group B
but for cache/DB metrics: throughput, hits/misses, command rates, IO bytes.

- **Source**: `rezolus/config/templates/{cachecannon,valkey}.json`
- **Examples**:
  - `sum(irate(requests_sent{source="cachecannon"}[5s]))`
  - `sum(irate(cache_hits{source="cachecannon"}[5s])) / sum(irate(requests_sent{source="cachecannon"}[5s]))`
  - `sum(irate(redis/total_commands_processed{source="valkey"}[5s]))` (note: `/` in metric name!)
- **Status**: same shape as Group B. The `redis/total_commands_processed`
  with a slash in the metric name is a wrinkle worth verifying — neither
  the matriken-query loader nor the catalogue matcher has been tested with
  slash-bearing metric names.
- **Lift**: small, plus a one-off check on slash-name handling.

## Group D — multi-operand CPU performance ratios (10 templates)

Cache hit rate, IPC, MPKI, frequency calculations chained out of multiple
`irate` aggregates with division/multiplication.

- **Source**: `rezolus/crates/dashboard/src/dashboard/cpu.rs:61-180`
- **Examples**:
  - `1 - sum(irate(cpu_l3_miss[5m])) / sum(irate(cpu_l3_access[5m]))`
  - `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` (IPC)
  - `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` (MPKI)
  - `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / 1000000000 / cpu_cores`
    (the IPNS query — six-way binary op chain)
- **Status**: shape matches `counter_ratio` for the simple ones. The IPNS
  query stresses the binary-op chaining that the existing PromQL
  evaluator has had bugs with historically (per Rezolus PR #86 — `on()` /
  `ignoring()` modifier handling).
- **Lift**: medium. The simpler ones are SQL `WITH ... GROUP BY ts` patterns;
  the IPNS query needs careful handling and may need an exhaustigen-style
  property test to lock semantics. Also needs a fixture with `cpu_cores` as
  a scalar gauge (or whatever the engine resolves it as).

## Group E — per-id heatmap aggregations (13 templates)

Per-CPU views: `sum by (id) (irate(...))` with a divisor, used to render
heatmaps in the dashboard.

- **Source**: `rezolus/crates/dashboard/src/dashboard/cpu.rs:22-204`
- **Examples**:
  - `sum by (id) (irate(cpu_usage[5m])) / 1000000000` (already shadowed)
  - `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))`
  - `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m])) * sum by (id) (irate(cpu_tsc[5m])) * sum by (id) (irate(cpu_aperf[5m])) / sum by (id) (irate(cpu_mperf[5m])) / 1000000000`
    (per-CPU IPNS)
- **Status**: shape matches `counter_sum_by_id` and `rezolus_cpu_user_per_id`.
- **Lift**: small for the simple ones, medium for per-CPU IPNS (chained
  aggregation).

## Group F — softirq per-kind family (~20 templates)

Format-string-generated queries for each softirq kind: `hi`, `irq_poll`,
`net_tx`, `net_rx`, `rcu`, `sched`, `tasklet`, `timer`, `hrtimer`, `block`.
Each kind gets four query variants (rate / rate-by-id / time-pct /
time-pct-by-id).

- **Source**: `rezolus/crates/dashboard/src/dashboard/softirq.rs:13-39`
  (template generator)
- **Examples**:
  - `sum(irate(softirq{kind="hi"}[5m]))`
  - `sum by (id) (irate(softirq{kind="net_rx"}[5m]))`
  - `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000`
- **Status**: shape matches Group B / Group E with a different label name.
- **Lift**: small. Once a label-filtered counter shape is in the catalogue,
  these ride on the same SQL pattern.

## Group G — cgroup-filtered queries (~30 templates)

Queries that conditionally include or exclude specific cgroups via the
`__SELECTED_CGROUPS__` JS-side placeholder and `name=~"..."` filters.

- **Source**: `rezolus/crates/dashboard/src/dashboard/cgroups.rs:36-134`
- **Examples**:
  - `sum by (name) (irate(cgroup_cpu_usage{__SELECTED_CGROUPS__}[5m]))`
  - `sum(irate(cgroup_cpu_usage{__SELECTED_CGROUPS__}[5m]))`
  - `sum(irate(cgroup_cpu_instructions{name="/system.slice/rezolus.service"}[5m])) / sum(irate(cgroup_cpu_cycles{name="/system.slice/rezolus.service"}[5m]))`
- **Status**: the `__SELECTED_CGROUPS__` placeholder is **JS-side**
  substitution that happens before the PromQL string reaches metriken-query.
  By the time we see the query it's a real label-matcher. Catalogue
  matching needs capture-group templating to handle the variable
  substitutions.
- **Lift**: medium. Capture-group templating is the prerequisite —
  trying to enumerate every concrete cgroup-name combination in the
  catalogue is a non-starter. See the in-tree
  `metriken-query/src/dispatch.rs` matcher for the current exact-match
  implementation.

## Group H — memory / NUMA gauges (7 templates)

Bare gauges and simple arithmetic on memory metrics.

- **Source**: `rezolus/crates/dashboard/src/dashboard/memory.rs:17-65`
- **Examples**:
  - `memory_available` (bare gauge)
  - `memory_buffers`, `memory_cached`, `memory_free`
  - `(memory_total - memory_available) / memory_total` (utilization %)
- **Status**: matches `gauge_bare` for the simple ones. The arithmetic
  combination is a new shape (gauge - gauge) / gauge.
- **Lift**: small. SQL pattern is `SELECT t, (a - b) / a FROM gauge_view`.

## Group I — GPU metrics (5 templates)

Mostly gauge reads with scaling factors.

- **Source**: `rezolus/crates/dashboard/src/dashboard/gpu.rs`
- **Examples**:
  - `sum(gpu_power_usage) / 1000`
  - `avg(gpu_memory_utilization) / 100`
  - `sum(rate(gpu_energy_consumption[5m])) / 1000`
- **Status**: shape matches `gauge_bare` and `counter_rate_basic` with
  divisors.
- **Lift**: small.

## Group J — Rezolus self-monitoring (3 templates)

BPF instrumentation overhead (the agent measuring itself).

- **Source**: `rezolus/crates/dashboard/src/dashboard/rezolus.rs:39-55`
- **Examples**:
  - `sum(irate(rezolus_bpf_run_time[5m])) / 1000000000`
  - `sum by (sampler) (irate(rezolus_bpf_run_time[5m])) / 1000000000`
  - `(sum by (sampler) (irate(rezolus_bpf_run_time[5m])) / sum by (sampler) (irate(rezolus_bpf_run_count[5m]))) / 1000000000`
- **Status**: matches `rezolus_cpu_busy_pct` and `rezolus_cpu_user_per_id`
  shapes.
- **Lift**: small.

## Group K — singletons (network drops, scheduler context switches, blockio I/O bytes/ops)

Five queries that fit existing shapes — listed for completeness.

- `sum(irate(network_drop[5m]))` (`network.rs:51`)
- `sum(irate(scheduler_context_switch[5m]))` (`scheduler.rs:41`)
- `sum(irate(blockio_bytes[5m]))` (`blockio.rs:21`)
- `sum(irate(blockio_operations[5m]))` (`blockio.rs:25`)
- **Lift**: trivial.

---

## Total un-shadowed: ~134 templates across 11 shape groups

**Recommended order to land**:

1. **Group K + I + H + J** (small lift, ~20 templates total) — simple
   irate sums, gauge reads, divisors. Mostly new fixtures with new metric
   names.
2. **Group A + B + C + F** (~93 templates) — once a `source`-labeled
   fixture exists, these mostly reduce to filling in catalogue entries.
3. **Group D + E** (23 templates) — multi-operand ratio chains. New SQL
   idioms; worth an exhaustigen test for the IPNS family.
4. **Group G** (~30 templates) — capture-group templating in the catalogue
   matcher. New infrastructure.

## Open questions

- **`__SELECTED_CGROUPS__` substitution.** Happens JS-side via
  `buildEffectiveQuery` in `rezolus/src/viewer/assets/lib/data.js`. The
  resulting concrete query reaches the dispatcher with real label matchers.
  Capture-group templating (Phase 2.2's "future work") is the answer.
- **Metric names with slashes.** `redis/total_commands_processed` has a
  `/` in it. Verify metriken-query loader and the dispatcher matcher both
  handle it cleanly — could be a parser corner case.
- **`cpu_cores` resolution.** Several queries reference `cpu_cores` as if
  it were a scalar. Trace how metriken-query resolves it and whether the
  view generator needs a special case (it currently produces views per
  metric name, so `cpu_cores` would need a single-row view).
