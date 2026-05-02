# Known semantic gaps — dynamic wide-form `irate` SQL

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

## 1. Range argument `[5m]` is ignored

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

## 2. Step parameter is not honoured

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

## When this matters in practice

Both gaps are silent — the SQL returns *a* result, just not the same
result PromQL would. Shadow-mode dispatch (`Mode::Shadow` in
`metriken-query/src/dispatch.rs`) will surface them as canonical-JSON
diffs against the PromQL twin. If a shadow diff lands on a query where
the only difference is "PromQL has fewer points" or "PromQL skipped a
gap-spanning sample," it is one of these two gaps and not a semantic
bug in the SQL generator.
