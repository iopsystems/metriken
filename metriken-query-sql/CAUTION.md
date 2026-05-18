# Known PromQL ↔ SQL discrepancies

This file tracks semantic gaps where the SQL twin produces a different
result from the PromQL evaluator. The live divergence harness is
`metriken-query/examples/sql_vs_promql.rs` (invocation in
`review/review.md` "Verification"). When that harness flags a divergent
plot, check this list — if the diff matches one of these patterns it's
expected; if not, it's a new bug worth investigating.

Two layers can diverge:

1. **Dashboard SQL emitters** — `rezolus/crates/dashboard/src/sql.rs`
   helpers that the per-section dashboard generators call to produce
   each plot's `sql` argument. This is the production path; the
   `sql_vs_promql` harness compares them plot-by-plot against PromQL.
2. **Wide-form translator** — `metriken-query/src/harness/translate.rs`,
   the PromQL→SQL bridge behind the (non-default) `harness` feature.
   Migration scaffolding; its known gaps appear here for historical
   completeness but don't surface in the live dashboard path.

---

## Dashboard SQL: per-name cgroup fan-out drops rows after a column goes NULL

**Affected queries:** per-cgroup fan-out templates driven by
`sql::cgroup_irate_by_name` (and structurally `sql::cgroup_ratio_by_name`,
which uses the same UNPIVOT + per-name aggregation pattern) in
`crates/dashboard/src/sql.rs`. Surfaces on the per-cgroup individual plots
(e.g. `individual-syscall-poll` — the divergence is per-op, so it shows
up most prominently on subseries plots where the affected column is the
*only* column for that name).

**Affected parquets:** any capture that contains a short-lived cgroup
whose counter column transitions to NULL after the cgroup exits — most
commonly boot-time one-shot units like
`/system.slice/systemd-update-utmp-runlevel.service`. Confirmed on
`AB_level_pin.parquet`; the harness reports it as "PromQL has N
integer-second timestamps SQL doesn't (>1 boundary tolerance)" on the
exited cgroup's series.

**Symptom:** PromQL emits one point per evaluation step for the full
window, SQL emits points only up to (and including) the last sample
where the counter was non-NULL. After the NULL transition PromQL's
`[5m]` lookback still finds the prior non-NULL sample and emits a
carried-forward rate (typically 0 for an exited cgroup whose counter
stopped advancing); SQL's `UNPIVOT` drops the NULL rows and the
windowed `irate_lag` has no `(timestamp, name)` row to project on.

**Why:** DuckDB's `UNPIVOT` excludes NULL values by default — the
exited cgroup's per-op column has NULL after exit, so it never lands
in the `joined` CTE, so `(timestamp, name)` doesn't appear in the
output. PromQL evaluates the expression at every step in the
query-range grid; the `irate(...[5m])` window sees the last
pre-NULL sample for ~300 s after the cgroup exits and produces a
rate row. The root cause is PromQL's range-emission cadence
interacting with sparse SQL inputs — whenever the SQL row count
drops below the PromQL evaluation-step count, a divergence of this
shape can appear.

**Fix when needed:** rewrite the per-name fan-out to drive emission
off the full `_src` timestamp grid (LEFT JOIN against `_src.timestamp`)
and carry the last-known rate forward per `(name, col)` pair. Costly
(touches every cgroup fan-out template) and the practical impact is
limited to the post-exit tail of one-shot cgroups whose data is
already zero/stale, so deferred.

---

## Catalogue: `cpu_cores` multi-series mismatch on combined parquets

**Affected queries:** entries that JOIN against the `cpu_cores` view —
`counter_irate_total_per_cpu_core_pct`,
`counter_irate_with_labels_per_cpu_core_pct`,
`rezolus_cpu_aperf_chain_total`, `rezolus_cpu_ipns`.

**Symptom:** before the fix, PromQL returned an empty matrix while SQL
returned a series whose values were per-source sums. Visible as `PromQL
series=0 SQL series=1`.

**Why:** combined parquets like `cachecannon` carry `cpu_cores` as a
multi-source gauge (`{node:rezolus-client}` and `{node:rezolus-server}`).
PromQL's binary-op vector matching requires the LHS and RHS label sets to
match; an aggregated LHS with empty labels (e.g. `sum(irate(cpu_usage[5m]))`
→ `{}`) doesn't match a labeled `cpu_cores`, so the whole expression
evaluates to empty. SQL's blind `JOIN cpu_cores ON timestamp` produced
extra rows.

**Fix landed:** every `cpu_cores`-joining template now uses a `cores_one`
CTE — `SELECT timestamp, ANY_VALUE(value) FROM cpu_cores GROUP BY
timestamp HAVING COUNT(*) = 1` — which matches PromQL's "single matching
pair only" semantics. Single-source parquets pass through (one row per
timestamp); multi-source parquets produce zero rows, matching PromQL.

---

## Wide-form translator: `irate` generator

The wide-form translator (`metriken-query/src/harness/translate.rs`,
gated to the non-default `harness` feature) emits SQL of the form:

```sql
WITH dt AS (
    SELECT *,
           CAST(timestamp - LAG(timestamp) OVER w AS DOUBLE) / 1e9 AS dt_s
    FROM _src
    WINDOW w AS (ORDER BY timestamp)
)
SELECT timestamp,
       (col_a - LAG(col_a) OVER w) / dt_s AS col_a_irate,
       (col_b - LAG(col_b) OVER w) / dt_s AS col_b_irate,
       ...
FROM dt
WINDOW w AS (ORDER BY timestamp)
```

This is close to PromQL `irate(metric[range])` but **not** identical.
Two gaps are accepted for now; the translator's only consumers today
are `examples/wide_form_coverage.rs` and the
`tests/{engine_pipeline,translate_snapshots,orphan_detector}.rs`
suite, so the gaps don't surface in the dashboard path. Documented
here so the translator's land-or-delete decision (see
`review/review.md`) can be made with eyes open.

### Range argument `[5m]` is ignored

PromQL's `irate(metric[5m])` says: "find the last two samples within the
preceding 5 minutes; compute their per-second rate." If the data has a gap
longer than 5 minutes, PromQL produces no result at that evaluation point.

Our SQL looks at the immediately preceding row via `LAG`, regardless of
how far back it is. At 1 Hz sampling with no gaps that matches what
PromQL produces (each evaluation has exactly one prior sample inside the
range). It diverges when:

- The series has a gap longer than the range — PromQL drops the point;
  SQL still computes a rate against an arbitrarily old prior sample.
- The range is intentionally short to filter stale data.

**Fix when needed:** wrap the inner select with
`WHERE dt_s <= <range_seconds>` so rows whose previous-row gap exceeds
the range vanish. The range comes from the catalogue's PromQL template;
the generator already has it in scope.

### Step parameter is not honoured

PromQL `query_range(start, end, step)` evaluates the expression at every
multiple of `step` between `start` and `end`. Our SQL emits one row per
raw timestamp in `_src` and lets the catalogue's projection layer hand
back whatever rows it gets.

That's fine for dashboards whose `step` matches the source sampling
interval, which is the common case in Rezolus today. It produces too
many rows for downsampled views (e.g. `step = 60` over a 1 Hz file).

**Fix when needed:** either

1. Decimate post-query in the projection layer (drop rows whose
   timestamp doesn't fall on a step boundary), or
2. Push the step into SQL via
   `WHERE (timestamp - <start_ns>) % <step_ns> = 0`, optionally combined
   with `WHERE timestamp BETWEEN <start_ns> AND <end_ns>` so the engine
   can prune.

Option 2 is cheaper for large files; option 1 is simpler and avoids
arithmetic in the WHERE clause that may defeat predicate pushdown.

### When this matters in practice

Both gaps are silent — the SQL returns *a* result, just not the same
result PromQL would. Running `wide_form_coverage` against a parquet
with non-trivial gaps would surface them; the dashboard SQL path
doesn't use the translator and so won't.

---

## Diagnostic tooling

When `sql_vs_promql` flags a divergent plot, the artifacts that help
isolate the cause:

- `--out <dir>/<parquet_stem>/<plot_id>.json` — full per-plot record
  with the PromQL and SQL strings, both result arrays, and the
  `verdict` object (`divergent.reason` carries the first
  `(timestamp, label, promql, sql)` mismatch).
- `--out <dir>/<parquet_stem>.divergences.txt` — one-line summary per
  divergent plot, useful for skimming.
- `--out <dir>/summary.json` — top-level per-parquet and per-section
  counts.
- `--rel-tol` / `--abs-tol` knobs reclassify near-misses into the
  `within_tolerance` bucket without changing source.
- `--max-plots N` caps the run for a quick smoke; pair with
  `--dashboard-dir` pointing at a freshly-generated section JSON
  directory (`cargo run -p dashboard -- <dir>` in the rezolus tree).
