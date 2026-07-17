# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
