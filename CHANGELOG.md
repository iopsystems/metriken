# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### metriken-query 0.12.0

- Add `MetricsSource::query_range_display` and a `display` module: a
  range query evaluated at native resolution and then decimated to a
  bounded point budget for *display*, without losing spikes. Each point
  is a per-bucket boxplot `EnvPoint { t, min, lo, median, hi, max }` —
  a robust median line, a hard min/max envelope so a 1-in-N spike
  survives the downsample, and a configurable inner band (`lo`/`hi` at
  the `DisplayOptions.band` quantiles, default IQR; the min/max outer
  band is invariant). Returns a `DisplayResult` (a richer, non-PromQL
  shape); only `Matrix` results are decimated, heatmap/scalar/vector
  pass through. The default trait method post-processes `query_range`,
  so every backend gets it with no per-impl code. Analysis consumers
  that recompute on the data should keep using `query_range`.

### metriken-query 0.10.6

- Add `histogram_irate(m)` — per-step rate of a histogram's
  cumulative sample count, returned as an instant vector. Lets
  dashboards derive a fallback event-rate line for histograms
  that have no standalone counter (`scheduler_runqueue_latency`,
  `scheduler_offcpu`, `scheduler_running`, `tcp_packet_latency`).
  Replaces the `sum(irate(histogram_count(m)[5m]))` idiom
  suggested for `histogram_count` in 0.10.5, which never parsed —
  PromQL disallows range vectors on function-call results.
- Add an optional `by (..)` / `without (..)` aggregation modifier
  to `histogram_irate`, `histogram_count`, and `histogram_mean`,
  matching standard PromQL aggregation-operator syntax. With no
  modifier, every matching series collapses into one
  `{__name__: metric_name}` output (today's behaviour); with
  `by`/`without`, one series per distinct projected-label tuple.
  Lets `histogram_mean by (source) (m)` return one mean per
  source in a single query — previously required N filtered
  queries.

### metriken-query 0.10.2

- Restore the matcher-less single-right binary broadcast. Queries
  shaped like `sum(rate(x[..])) / y` (where the aggregate strips
  labels and `y` carries some) were silently empty in 0.10.0/0.10.1
  on single-host parquets — the rezolus viewer's CPU-utilization
  tiles relied on this fallback. Now `matrix_matrix_op` materialises
  the lone unmatched right series into a shared timestamp lookup
  and broadcasts it across every unmatched left series, mirroring
  the eager engine's per-left fallback.

### metriken-query 0.10.1

- Cache the parquet footer once per load and decode columns one at a
  time within each row group. Restores load performance on wide files
  that regressed in 0.9.6's per-column projection rewrite — 5–28×
  faster than 0.10.0 across the rezolus dashboard fixtures
  (vllm.parquet 21.0s → 0.74s; sglang-nixl-16c 130s → 6.0s).

### metriken-query 0.10.0

Breaking — collapses the PromQL evaluator to streaming-only and
narrows the supported surface to the subset rezolus actually uses.

- All eager evaluation removed. `evaluate_expr` now forwards every
  expression to the streaming dispatcher; any AST shape the
  dispatcher doesn't recognise becomes `QueryError::Unsupported`.
- `histogram_heatmap` now streams its input — peak transient heap
  drops ~54% per query versus the eager merge-then-walk path.
- `histogram_quantile`, `histogram_quantiles`, counter `deriv`
  (the 2nd-derivative case), gauge `deriv`, and the binary
  operators (`+`, `-`, `*`, `/`) all flow through the streaming
  pipeline.
- The instant `query()` entry point now routes through
  `query_range` with `start = end = time` and collapses the
  resulting matrix to a vector by taking each series's latest
  point. Inherits the full streaming PromQL surface.
- Removed PromQL features (none used by the only known consumer):
  `scalar(...)`, `vector(...)`, `group_left` / `group_right`
  one-to-many binary matching, the matcher-less single-right
  binary broadcast, and the eager `sum(scalar(x))` passthrough.
- Removed crate features: `http` (along with the `axum` dep and
  the `promql::routes` axum router that lived behind it).
- Removed Tsdb / Collection / Series API surface that had no
  remaining callers: `Tsdb::counters` / `gauges` / `histograms`
  (cloning variants — use `*_ref` instead),
  `CounterCollection::filter` / `rate` / `filtered_rate`,
  `GaugeCollection::filter` / `filtered_sum`,
  `HistogramCollection::filter` / `sum`,
  `CounterSeries::rate` / `windowed_rate` / `windowed_irate`,
  `GaugeSeries::untyped`,
  `HistogramSeries::heatmap` / `percentiles` (the eager
  multi-quantile walker; streaming pipeline replaces it),
  `UntypedCollection`.
- Cumulative cachecannon-bench peak transient heap across 43
  representative queries: 12.82 MiB → 7.53 MiB (−41%).

### metriken-query 0.9.5

- Store histograms in the TSDB as `CumulativeROHistogram`, which only retains
  non-zero buckets in columnar form. This substantially reduces memory usage
  for sparse distributions and lets quantile queries run as a binary search on
  the cumulative counts. Delta and sum between two `CumulativeROHistogram`s
  are computed via a shared `combine()` helper.

### metriken-query 0.9.4

- Support PromQL `on(...)` and `ignoring(...)` label-matching modifiers on
  binary operators, allowing expressions whose operands carry mismatched label
  sets (e.g. `tx_bytes / ignoring(direction) link_bandwidth`) to combine
  correctly.

### 0.5.1
Metriken versions older than 0.5.1 did not have changelogs.
