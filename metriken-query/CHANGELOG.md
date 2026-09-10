# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.22.0]

### Changed

- **Requires `metriken-exposition` 0.20** (and through it `metriken` 0.11).
  No API of this crate broke; the dependency crosses a breaking boundary, so
  consumers move in lockstep.
- **`rate`/`irate` no longer fabricate an acquisition window at a timestamp the
  producer never read.** Interpolating a window between two bracketing samples
  is right when a grid edge falls between adjacent reads, but across a hole — a
  series null for a stretch — it invented a read and the band claimed a
  precision nobody measured. Such a point now carries its value and no band,
  flagged by the new `Point::interpolated`. The value across a hole is
  unchanged: the total is known even though its distribution inside it is not.

### Added

- **`MatrixSample::bands`** — the per-value uncertainty band, present when ANY
  value has one, with `None` at the values that do not. `intervals` is
  all-or-nothing by construction and reports `None` for the whole series as
  soon as one point lacks a band; `bands` is the lossless view. Additive:
  `intervals` keeps its type and behaviour.
- **`MatrixSample::interpolated`** — which values span a stretch the producer
  did not read. A renderer needs this to draw an interpolated point differently
  from a measured one; an uncertainty band cannot express it, because the
  honest bound on an unobserved interval is not a number. Propagates through
  scalar ops, aggregation and series-op-series the way bands do, and takes the
  band with it: a result combining an interpolated operand carries no band.

### Fixed

- **A parquet source parses its schema once instead of per lookup.** Eight call
  sites re-ran `parse_schema`, which walks every field in the file, so any "for
  each metric name" loop was O(names x columns) with nothing memoised. Measured
  on a 950-column, 14,210-series recording: `total_series_count()` 542 ms ->
  12.9 ms (5.6 ms warm); a dashboard section render 598 ms -> 5.3 ms.

## [0.21.0]

### Added

- **`ParquetBuilder::source_labeled` — compose heterogeneous sources under
  injected labels.** `reader_labeled` takes an `Arc<ParquetReader>` and nothing
  else, and `MultiParquetSource` held `Vec<(Arc<ParquetSource>, Labels)>`, so
  only single-file parquet sources could ever be composed. A
  `SegmentedParquetReader` could not enter the builder at all.

  That blocked the consumer this exists for: a `.rez` archive tables its data
  per sampler, and a table is a *segmented* source whenever its writer sealed
  more than once. Merging N such tables under per-artifact labels — so one
  PromQL query can span several recordings and slice by label — had no
  expressible form.

  `MultiParquetSource` now holds `Vec<(Arc<dyn DataSource>, Labels)>`, and
  `source_labeled` accepts anything convertible into the new opaque
  [`CompositionSource`] (`From<&ParquetReader>`, `From<&SegmentedParquetReader>`).
  `file`, `bytes`, `file_owned`, `reader` and `reader_labeled` are unchanged.

  `CompositionSource` is opaque rather than a bare `Arc<dyn DataSource>` on
  purpose: `DataSource`'s methods return `Counters`, `Gauges` and
  `HistogramStream`, so making the trait public would bind the crate's internal
  row representations to semver. This is the trade `UnionChild` already makes.

### Changed

- `ParquetSource` reaches the composite through `DataSource` (via a `FileSource`
  newtype — the inherent `histogram_stream` takes `self: &Arc<Self>`, which a
  `&self` trait method cannot supply) rather than through free functions and
  inherent methods. `sample_timestamps` moved down onto the source, since a
  composite has to gather it through the trait instead of reaching past its
  children into row groups. Internal only; no public behavior change.

## [0.20.1]

### Added

- **`referenced_metrics(query)` — the metric names a query names, without a
  data source.** `QueryEngine::columns` answers a related question, but it needs
  the source's column map to expand a selector into its labelled series, so the
  source must already be open. A caller deciding *which* source to open cannot
  have that yet.

  This is for a reader holding many tables: it can now decide which tables a
  query could possibly touch before opening any of them. In a `.rez` archive of
  50 tables and 418 segments, a typical query touches 11% of them — the rest
  were being opened and discarded, at a measured ~1.37 ms per segment.

  Label matchers are ignored deliberately. Routing asks "could this table answer
  the query"; a table holding the metric with no matching series answers with an
  empty result, which is correct, whereas skipping it on a label mismatch would
  route on data the caller does not have.

## [0.20.0]

### Added

- **`QueryOptions::eval_timestamps` — evaluate at an explicit list of instants
  rather than on the uniform grid.** Each rate's averaging window becomes the
  gap to the preceding timestamp, so the uniform grid is simply the special
  case where every gap is equal.

  This exists for cross-cadence queries. The grid walks `start + k·step`, so
  most of its points fall where a slow source has no reading; that source's
  value is then held forward and combined with a fast operand as if the two
  were simultaneous. Nor can the grid be tuned to fix it: a slow source's
  readings are not evenly spaced either — measured on a real recording, one
  sampler's rows fell 30 s apart and then 60 s apart, so no uniform grid sits
  on them at any step or phase. Passing the slow source's own row timestamps
  puts every point where both operands genuinely have data.

  Applies to counter rates and to all four gauge producers, deliberately as one
  unit: a binary op joins its two sides ON TIMESTAMP, so moving one producer
  off the grid while another stayed would make them stop intersecting and yield
  an empty series.

- **`QueryOptions::rate_span_ns` — a rate averaging span separate from the
  step.** A rate's value is `increase / step`, so the step was simultaneously
  the point spacing and the averaging window, and smoothing meant coarsening
  the step. That relocates the evaluation grid, which is destructive exactly
  where smoothing is wanted: on a cross-cadence query the grid moves off the
  slow source's read times, its window is interpolated across the whole gap,
  and the combined uncertainty band explodes (measured 0.85% wide before,
  6.7x after). The span leaves the points where they are and widens only the
  window. Defaults to the step, so nothing moves unless a caller asks.

- **`MetricsSource::snapped_sample_timestamps()`** — the same rows as
  `sample_timestamps()`, snapped to the nominal grid exactly as the query path
  snaps them. This is the form to build `eval_timestamps` from: the query path
  indexes samples by the snapped value, so a row recorded at 1.5 s on a 1 s grid
  is indexed at 2.0 s, and asking for 1.5 s falls before the series' first
  sample and silently yields no point. Defaults to the raw form.

### Changed

- **BREAKING: `QueryOptions` no longer implements `Copy`.** It now holds an
  `Arc<[u64]>` for `eval_timestamps`. It is still `Clone`; callers that relied
  on implicit copies (`let opts = *opts;`) need `.clone()`.

## [0.19.1]

### Changed

- **A binary op whose operands come from different acquisition tables now
  widens their bands before combining them.** Two values read at different
  instants and combined as if simultaneous is an approximation neither
  operand's band accounted for, so cross-table results were too *narrow* —
  the one direction a band must not be wrong in.

  Each `Point` carries the acquisition edges its band came from, and equality
  of those edges is an exact same-read test: an acquisition group is one read
  with one window, so identical edges mean no widening at all. That is the
  common case (`sum(irate(x[5m]))` over 32 CPUs is one group) and it stays
  exactly as tight as before. Differing edges are widened to the union of both
  spans first.

  **Values are unchanged.** This only ever touches bands, and only ever makes
  them wider — so a consumer that displays them will see cross-table plots
  gain visible uncertainty they should always have had, and nothing else move.

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
