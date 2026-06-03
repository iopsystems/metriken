# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.11.0] - 2026-06-03

### Added

- `ParquetReader`, an Arrow-native streaming parquet source. Resident memory is O(parquet metadata + active row group) instead of O(file size). (#113)
- `MemoryStore`, an in-memory queryable source for live agent ingestion. (#113)
- `MetricsSource` public trait implemented by both sources, with `Send + Sync` supertraits. (#113)
- `ParquetBuilder` for multi-file composition with optional per-file label injection. (#113)
- `BufferPool`, an LRU-evicted shared cache of decoded blocks for dashboards that re-query the same windows. (#113)
- `ingest` cargo feature (default-on) gating `MemoryStore::ingest_snapshot` and the `metriken-exposition` dependency. (#113)
- `fixtures` cargo feature exposing `FixtureBuilder` (synthetic) and `ParquetAugmentor` (real-file replication) for tests and benchmarks. (#113)
- PromQL: `histogram_sum(metric)` function. (#112)
- PromQL: `histogram_percentiles(...)` accepted as an alias for `histogram_quantiles(...)`. (#113)
- `time_range_ns()` for raw-nanosecond timestamps. (#113)
- `metadata_get(key)` for single-key file-metadata lookups without cloning the full map. (#113)
- `ParquetFile` and range-scoped query engine work that preceded the full streaming refactor. (#110, #111)

### Changed

- `time_range()` now returns `Option<(f64, f64)>` in seconds. Use `time_range_ns()` for the exact nanosecond pair. (#113)
- `counter_names()`, `gauge_names()`, `histogram_names()` return owned `Vec<String>` instead of `Vec<&str>`. (#113)
- `counter_labels()`, `gauge_labels()`, `histogram_labels()` return `Vec<BTreeMap<String, String>>` instead of `Option<Vec<Labels>>`. (#113)

### Removed

- `Tsdb` and the public `QueryEngine` — query directly through `ParquetReader`, `MemoryStore`, or `dyn MetricsSource`. (#113)
- `metriken_query::tsdb` module and the public `Labels` type. Use `BTreeMap<String, String>` for labels. (#113)

### Fixed

- Parquet decode panics (apache/arrow-rs#8885) are caught and surfaced as `QueryError` instead of crashing the host process. (#113)
- File reads use positional `pread` (Unix) / `seek_read` (Windows) to avoid kernel `f_pos_lock` contention when multiple queries hit the same file concurrently. (#113)
- Parquet metadata is loaded through a `File` handle rather than reading the whole file into a `Vec<u8>`. (#109)

## [0.10.8] - prior

Pre-refactor versions based on the `Tsdb` architecture. See git history.

[Unreleased]: https://github.com/iopsystems/metriken/compare/metriken-query-v0.11.0...HEAD
[0.11.0]: https://github.com/iopsystems/metriken/compare/metriken-query-v0.10.8...metriken-query-v0.11.0
