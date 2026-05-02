# Known PromQL ↔ SQL discrepancies

This file tracks semantic gaps where the SQL twin produces a different
result from the PromQL evaluator. Use it whenever a shadow-mode diff
surfaces in production — if the diff matches one of these patterns, it's
expected; if not, it's a new bug worth investigating.

Two layers can diverge:

1. **Catalogue templates** — `metriken-query/queries.toml` entries
   shadow-tested against real Rezolus parquets via
   `tests/divergence_inspector.rs` and `tests/frontend_coverage.rs`. The
   `frontend_coverage` gate runs every production query × every parquet
   under `rezolus/site/viewer/data/`; primary parquets must produce zero
   divergences.
2. **Dynamic wide-form generator** — the lower-level SQL-emission layer
   used outside the catalogue (notebooks / ad-hoc tooling).

---

## Catalogue: combined-recording timestamp gaps

**Affected queries:** every counter-rate template (`irate(...)`, `rate(...)`,
`sum(irate(...))`, etc.) — covers ~30 catalogue entries.

**Affected parquets:** combined-recording parquets that contain at least one
inter-snapshot gap (consecutive snapshots > 1 second apart). Confirmed on
`AB_base.parquet`, `AB_base_pin.parquet`, `AB_level.parquet` (one gap each).
The single-source agent recordings in `rezolus/site/viewer/data/` (demo,
cachecannon, vllm, sglang, vllm_gemma3, AB_level_pin) are gap-free and
produce identical canonical JSON.

**Symptom:** PromQL emits N+1 points per series, SQL emits N. Values agree
at every shared timestamp; the canonical JSON arrays differ only in length.
The `frontend_coverage` test logs these as "AB-gap warnings" but does not
fail (set `METRIKEN_FRONTEND_COVERAGE_STRICT_AB=1` to upgrade to failure).

**Why:** PromQL `query_range(start, end, step)` evaluates the expression at
every step from `start` to `end`. When there's no data sample at a given
step but earlier samples are still inside the `[step - W, step]` window,
PromQL emits a rate computed from those earlier samples — a
carried-forward value. The SQL twin emits one row per actual data row in
`_src`; gaps in the data stream produce gaps in the output. Per-pair time
normalisation (`(value - LAG(value)) / (timestamp - LAG(timestamp))/1e9`,
landed during Phase 7.2) ensures the values agree at shared timestamps —
only the emission cadence differs.

**Fix when needed:** rewrite the rate-computation CTE to drive emission
off `generate_series(start_ns, end_ns, step_ns)` LEFT JOIN-ed to `_src`,
carrying the last-known per-series rate forward across gaps. Invasive
(touches every rate template) and currently uncalled-for in production
(agent recordings have no gaps); deferred.

**See also:** `metriken-query/queries.toml` header comment (idiom 6),
`metriken-query/tests/frontend_coverage.rs` test header.

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

## Dynamic wide-form `irate` generator

The dynamic wide-form generator emits SQL of the form:

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

This is close to PromQL `irate(metric[range])` but **not** identical. Two
gaps are accepted for now; document them here so we don't forget when a
divergence shows up in shadow-mode telemetry.

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
result PromQL would. Shadow-mode dispatch (`Mode::Shadow` in
`metriken-query/src/dispatch.rs`) will surface them as canonical-JSON
diffs against the PromQL twin. If a shadow diff lands on a query where
the only difference is "PromQL has fewer points" or "PromQL skipped a
gap-spanning sample," it is one of these two gaps and not a semantic
bug in the SQL generator.

---

## Diagnostic tooling

When a new shadow-mode divergence appears, three tools help isolate it:

- `cargo run --release --example probe_rate_diff -p metriken-query -- <parquet> '<query>'`
  prints the first few diverging `(timestamp, value)` pairs side-by-side
  for a given (parquet, query) pair.
- `cargo run --release --example shadow_replay -p metriken-query --
  --parquet <parquet>` walks every production query against one parquet
  and emits a markdown report (coverage / divergences / per-entry latency).
- `cargo test --release -p metriken-query --test frontend_coverage --
  --ignored --nocapture` is the gate test — runs the replay across every
  parquet and asserts zero primary divergences.
