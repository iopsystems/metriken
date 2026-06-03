# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.11.0] - 2026-06-03

Replace the materialized `Tsdb` with a streaming, Arrow-native query path.
Resident memory is now O(parquet metadata + active row group) instead of
O(file size). See `README.md` for migration examples.

### Added
- `ParquetReader` (file / bytes / owned-file) and `MemoryStore`, both
  implementing the new `MetricsSource` trait.
- `ParquetBuilder` for multi-file queries with optional per-file label
  injection.
- Optional shared `BufferPool` — LRU-evicted cache of decoded blocks for
  dashboards that re-query the same windows.
- `ingest` feature flag — gates `MemoryStore::ingest_snapshot` and the
  `metriken-exposition` dependency.
- `fixtures` feature flag — synthetic `FixtureBuilder` and real-file
  `ParquetAugmentor` for tests and benchmarks.
- `histogram_sum` PromQL function; `histogram_percentiles` accepted as an
  alias for `histogram_quantiles`.

### Changed
- `time_range()` returns `Option<(f64, f64)>` in seconds. Raw nanoseconds
  via the new `time_range_ns()`.
- Schema introspection (`counter_names`, `counter_labels`, …) returns
  owned `Vec<String>` and `Vec<BTreeMap<String, String>>`.

### Removed
- `Tsdb` and `QueryEngine` from the public API.
- The `metriken_query::tsdb` module and the public `Labels` type. Use
  `BTreeMap<String, String>` for labels.

### Fixed
- Decode panics from malformed parquet (apache/arrow-rs#8885) are caught
  and surfaced as errors instead of crashing the host.
- File reads use positional `pread` (Unix) / `seek_read` (Windows) to
  avoid `f_pos_lock` contention when several queries hit the same file
  concurrently.

## [0.10.8] - prior

Pre-refactor versions based on the `Tsdb` architecture. See git history.
