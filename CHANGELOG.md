# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

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
