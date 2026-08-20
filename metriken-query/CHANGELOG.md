# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Table-level acquisition-window columns. A bare `:window_begin`/`:window_width`
  pair (no metric prefix) is read as one acquisition window shared by every
  metric in the table — the shape a `.rez` group table (one table == one
  window) emits. Precedence, resolved as an atomic pair (never a begin from
  one source mixed with a width from another): a metric's own
  `<m>:window_begin`/`<m>:window_width` sidecar wins where BOTH are present;
  otherwise the table-level pair applies where BOTH are present; otherwise no
  window (unchanged). Both bare names remain reserved and never surface as
  metrics, matching the existing per-metric `:window_begin`/`:window_width`
  suffix reservation. `SegmentedParquetReader` splices table-level windows
  across segments identically to per-metric sidecars.

## [0.15.0]

### Changed

- **Breaking:** `Reducer::reduce` / `reduce_boxplot` take an additional
  `intervals: Option<&[(f64, f64)]>` argument (the per-sample measurement-
  uncertainty band parallel to `points`). `EnvPoint` gains `unc_lo` / `unc_hi`
  (`Option<f64>`), so struct-literal construction must supply them.

### Added

- Measurement-uncertainty bands survive display-mode decimation. When a
  decimated bucket collapses N native samples, its aggregated band is the median
  of the per-sample interval lows/highs (`unc_lo` / `unc_hi`) — robust, mirroring
  the median line, and orthogonal to the min/max value spread. At native
  resolution each sample keeps its exact interval, so zoomed-in and zoomed-out
  bands are consistent. `query_range_display` threads `MatrixSample::intervals`
  through automatically; series without uncertainty carry `None`.

## [0.14.1]

### Added

- Fleet fallback acquisition window. When a metric has no per-observation
  `:window_*` sidecar but the file carries a `duration` column, synthesize a
  coarse per-snapshot window `[timestamp, timestamp + duration]` (the same
  `[begin, begin+elapsed]` shape the agent records). This gives `rate()`/`irate()`
  measurement-uncertainty bands on windowless recordings (older files, plain
  `.parquet`) that previously had none. Per-observation sidecars still take
  precedence where present (`.rez` / live), so tight windows are unchanged.

## [0.14.0]

### Added

- Measurement-uncertainty bands (#117). Reads per-metric `:window_begin` /
  `:window_width` acquisition-window sidecar columns and turns them into honest
  interval bounds: `rate()` / `irate()` derive `[Δv/(e_last−b_first),
  Δv/(b_last−e_first)]` (widened to contain the nominal), propagated through
  scalar ops, sum/avg aggregation, and series-op-series binary ops. Histogram
  queries carry a value band from bucket resolution (`histogram_quantile`,
  `histogram_sum`, `histogram_mean`; `histogram_count` is exact). `QueryResult`
  gains optional `intervals`. Window offsets are anchored on the raw (un-snapped)
  timestamp, consistent with `sample_timestamps()`.
- `Sample::new` / `MatrixSample::new` + `with_interval` / `with_intervals`
  constructors.

### Changed

- **BREAKING:** `Sample` and `MatrixSample` are now `#[non_exhaustive]`; build
  them with the constructors instead of struct literals.
- **BREAKING:** the streaming `Point` is a struct `{ t, v, bounds }` (was a
  `(u64, f64)` tuple).

## [0.13.0] - 2026-07-16

### Added

- `MetricsSource::sample_timestamps()`: raw, un-snapped per-sample collection
  timestamps in row order. `ParquetReader` returns the actual on-disk
  `timestamp` column (unlike the query path, which rounds to the nominal
  sampling grid); `MemoryStore` keeps the empty default. Lets a viewer plot
  sampling jitter.

## [0.12.0] - 2026-07-13

### Added

- Display-mode range query: `MetricsSource::query_range_display` decimates a
  matrix result to a per-bucket boxplot (min/max envelope + median + inner
  band) via `DisplayOptions`/`Reducer::Boxplot`, returning `DisplayResult`
  (`Series` / `HistogramHeatmap` / scalar / vector). Lets a viewer render long
  recordings fast without dropping spikes. (#115)

## [0.11.0] - 2026-06-03

### Changed

- Refactored from a materialized in-memory query engine (`Tsdb`) to a streaming Arrow-native parquet reader. See `README.md` for migration. (#113)

### Added

- PromQL: `histogram_sum(metric)` function. (#112)

[Unreleased]: https://github.com/iopsystems/metriken/compare/metriken-query-v0.13.0...HEAD
[0.13.0]: https://github.com/iopsystems/metriken/compare/metriken-query-v0.12.0...metriken-query-v0.13.0
[0.12.0]: https://github.com/iopsystems/metriken/compare/metriken-query-v0.11.0...metriken-query-v0.12.0
[0.11.0]: https://github.com/iopsystems/metriken/compare/metriken-query-v0.10.8...metriken-query-v0.11.0
