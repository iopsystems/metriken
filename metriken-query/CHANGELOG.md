# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.11.0] - 2026-06-01

This release replaces the monolithic `Tsdb` with a streaming, Arrow-native
architecture. The new design reads parquet row groups on demand, so memory usage
is proportional to active working set rather than file size. A shared
`BufferPool` is available for workloads that query the same files repeatedly.

### Breaking Changes

- **`Tsdb` removed.** Use `ParquetReader` for file-backed data and `MemoryStore`
  for live ingestion. See the Migration Guide below for side-by-side examples.

- **`QueryEngine` is now `pub(crate)`.** Queries are driven through
  `ParquetReader`, `MemoryStore`, or the `MetricsSource` trait. There is no
  longer a separate engine object to construct.

- **`time_range()` returns `Option<(f64, f64)>` in seconds** (was
  `Option<(u64, u64)>` in nanoseconds). Use `time_range_ns()` for exact
  nanosecond timestamps without floating-point precision loss.

- **`counter_names()` / `gauge_names()` / `histogram_names()` return
  `Vec<String>`** (was `Vec<&str>`). The returned strings are owned; lifetime
  coupling to the source is gone.

- **`counter_labels(name)` / `gauge_labels(name)` / `histogram_labels(name)`
  return `Vec<BTreeMap<String, String>>`** (was `Option<Vec<Labels>>` with an
  `.inner` field). Return value is always a `Vec`; empty means the metric is
  unknown. `Labels` is no longer part of the public API.

- **`source()` and `version()` return owned `String`** (was `&str`).

- **`file_metadata()` returns an owned `HashMap<String, String>`** (was a
  reference). For single-key lookups, prefer `metadata_get(key)` to avoid
  cloning the full map.

- **`metriken_query::tsdb` module removed.** `Labels` is gone from the public
  API; use `BTreeMap<String, String>` directly everywhere labels appear.

- **`metriken-exposition` is now an optional dependency** behind the `ingest`
  feature flag. Projects that only read parquet files no longer pull in the
  exposition dependency.

- **`filename()` is not set automatically for bytes-backed or builder-assembled
  readers.** `ParquetReader::open(path)` still derives the filename from the
  path's basename. For other construction paths, set it explicitly with
  `.with_filename(name)` or `ParquetBuilder::filename(name)`.

### Added

- **`ParquetReader`** — file-backed reader with streaming row-group access.
  Memory footprint is O(metadata + active row group), not O(file size).
  Constructors:
  - `ParquetReader::open(path)` — open a path; filename defaults to basename.
  - `ParquetReader::open_bytes(bytes)` — bytes-backed; supports WASM and
    in-memory workflows.
  - `ParquetReader::open_file(file)` — accepts an owned `std::fs::File`; useful
    with the `NamedTempFile::into_file()` unlink pattern.
  - `ParquetReader::open_with_pool(path, pool)` — file-backed with a shared
    `BufferPool` for decoded-block caching.
  - `ParquetReader::open_bytes_with_pool(bytes, pool)` and
    `ParquetReader::open_file_with_pool(file, pool)` — pool variants for the
    other backing types.
  - `ParquetReader::builder()` — returns a `ParquetBuilder` for composing
    multiple sources.
  - `ParquetReader::with_filename(name)` — set the display name after
    construction (builder pattern, returns `Self`).

- **`MemoryStore`** — in-memory queryable store for live-mode workflows. Arc
  clones share state. `MemoryStore::builder()` returns a `MemoryStoreBuilder`
  with `.source()`, `.version()`, `.sampling_interval_ms()`, and `.filename()`
  setters. Post-build mutation via `set_source()`, `set_version()`,
  `set_metadata(key, value)`, and `set_sampling_interval_ms(ms)`.

- **`MetricsSource` trait** — uniform read interface implemented by both
  `ParquetReader` and `MemoryStore`. Has `Send + Sync` supertraits so
  `Arc<dyn MetricsSource>` is usable across threads without additional bounds.
  Methods: `query_range`, `query`, `columns`, `time_range`, `time_range_ns`,
  `interval`, `source`, `version`, `filename`, `metadata_get`, `file_metadata`,
  `counter_names`, `gauge_names`, `histogram_names`, `counter_labels`,
  `gauge_labels`, `histogram_labels`.
  Default methods on the trait: `all_names()`, `total_series_count()`,
  `has_counter(name)`, `has_gauge(name)`, `has_histogram(name)`,
  `all_label_keys()`, `label_values(metric, key)`, `label_values_by_key(metric)`,
  `filename_or_default()`.

- **`ParquetBuilder`** — composable builder for multi-file readers.
  - `.file(path)` / `.file_labeled(path, labels)` — add a path-backed source.
  - `.bytes(bytes)` / `.bytes_labeled(bytes, labels)` — add a bytes-backed source.
  - `.file_owned(file)` / `.file_owned_labeled(file, labels)` — add an
    owned-file source.
  - `.reader(arc_reader)` / `.reader_labeled(arc_reader, labels)` — compose an
    existing `ParquetReader`; reuses the already-opened `Arc<ParquetSource>`
    handles with no additional I/O.
  - `.pool(pool)` — attach a `BufferPool`; all sources opened by this builder
    share it.
  - `.filename(name)` — explicit display name, overrides basename auto-detection.

- **`BufferPool`** — process-shared LRU cache for decoded parquet row groups.
  Wire multiple readers to the same pool so repeated queries (e.g. dashboard
  refresh) hit the cache instead of re-decoding parquet. Cache keys are scoped
  to `(source_id, column_idx, row_group_idx)`; readers with different source IDs
  never collide. API: `BufferPool::new(max_bytes)`, `current_bytes()`,
  `max_bytes()`, `clear()`, `stats()` (returns `BufferPoolStats` with `hits`,
  `misses`, `bytes_used`, `entries`).

- **`time_range_ns()`** — returns `Option<(u64, u64)>` in nanoseconds. Prefer
  this over `time_range()` when exact nanosecond timestamps are required.

- **`metadata_get(key)`** — single-key metadata lookup that avoids cloning the
  full metadata map. Available on `ParquetReader`, `MemoryStore`, and the
  `MetricsSource` trait.

- **`filename()` / `filename_or_default()`** — optional display name for a
  source, exposed on both concrete types and the trait. `filename_or_default()`
  returns an empty string when no name is set.

- **`columns(query)`** — resolves a PromQL expression to the set of physical
  parquet column names it touches, without reading any values. Useful for
  pre-flight column pruning.

- **`query(expr, time)`** — instant PromQL query at a single timestamp. Defaults
  to the latest available timestamp when `time` is `None`.

- **`MatrixSample`** and **`HistogramHeatmapResult`** re-exported from the crate
  root so consumers do not need to reference internal module paths.

- **`ingest` feature** — enables `MemoryStore::ingest_snapshot(snapshot)`.
  Ingests a `metriken_exposition::Snapshot` by value; timestamps are snapped to
  the nearest sampling-interval boundary so samples from multiple collection
  cycles align correctly.

- **`fixtures` feature** — exposes `metriken_query::fixtures` with:
  - `FixtureBuilder` — constructs synthetic parquet files for tests and
    benchmarks, with configurable metric names, label sets, time range, and
    sampling interval.
  - `ParquetAugmentor` — replicates an existing parquet file at scale (adding
    label dimensions, extending time ranges) for realistic benchmark setup.

### Changed

- Schema introspection methods (`counter_names`, etc.) now return `Vec<String>`
  with owned data; see Breaking Changes.
- `counter_labels` / `gauge_labels` / `histogram_labels` return flat
  `Vec<BTreeMap<String, String>>` with injected per-file labels already merged
  in; see Breaking Changes.
- The internal `DataSource` trait is now `pub(crate)`; external crates cannot
  implement it. Use `MetricsSource` for custom backends.

### Removed

- `Tsdb` struct and all associated methods.
- `QueryEngine` from the public API (`pub(crate)` only).
- `metriken_query::tsdb` module (and the `Labels` type it contained).
- The `Labels` type as a public API surface; replaced by `BTreeMap<String, String>`.

### Performance

- **Open time is now constant (~2 ms) regardless of file size.** `ParquetReader`
  reads only the parquet footer at open time. The old `Tsdb::load` decoded the
  entire file: 16 seconds and 856 MB RSS for a 315 MB file. A 2.6 MB file that
  previously cost 25 MB RSS now costs ~48 KB.

- **Memory scales with active row group, not file size.** Row groups are decoded
  on demand during a query and released when no longer needed. A 315 MB file
  held in a `ParquetReader` consumes ~16 KB of RSS at rest; the old `Tsdb` held
  the full decoded dataset in memory.

- **Row-group skipping via parquet statistics.** `time_range()` and query
  execution skip row groups whose min/max timestamp statistics fall entirely
  outside the query window, without decoding those groups.

- **`BufferPool` provides ~1.5x latency improvement for repeated histogram
  queries.** Without the pool, each query decodes the relevant row groups from
  scratch (~10 ms per query in benchmark). With a pool sized to cover the active
  working set, subsequent queries on the same row groups skip parquet decode
  entirely.

- **`metadata_get(key)` walks parquet key-value entries without allocating a
  `HashMap`**, unlike `file_metadata()` which clones the full map.

### Migration Guide

#### 1. Loading a parquet file

```rust
// Before
use metriken_query::{Tsdb, QueryEngine};
use std::sync::Arc;

let tsdb = Arc::new(Tsdb::load(path)?);
let engine = QueryEngine::new(Arc::clone(&tsdb));

// After
use metriken_query::ParquetReader;

let reader = ParquetReader::open(path)?;
```

#### 2. Querying

```rust
// Before
let result = engine.query_range("rate(metric[1m])", start, end, 1.0)?;

// After — query directly on the reader (no engine object needed)
let result = reader.query_range("rate(metric[1m])", start, end, 1.0)?;

// Or, through the MetricsSource trait
use metriken_query::MetricsSource;
fn run_query(src: &dyn MetricsSource) -> Result<_, _> {
    src.query_range("rate(metric[1m])", start, end, 1.0)
}
```

#### 3. Schema introspection and time range

```rust
// Before
let (start_ns, end_ns): (u64, u64) = tsdb.time_range().unwrap();
let names: Vec<&str> = tsdb.counter_names();

// After — time_range() is now in seconds (f64)
let (start_secs, end_secs): (f64, f64) = reader.time_range().unwrap();

// For nanoseconds, use time_range_ns()
let (start_ns, end_ns): (u64, u64) = reader.time_range_ns().unwrap();

// counter_names() now returns Vec<String>
let names: Vec<String> = reader.counter_names();

// counter_labels() now returns Vec<BTreeMap<String, String>> (not Option<Vec<Labels>>)
let label_sets: Vec<BTreeMap<String, String>> = reader.counter_labels("my_counter");
```

#### 4. Live mode ingestion

```rust
// Before
use metriken_query::Tsdb;
use std::sync::{Arc, RwLock};

let tsdb: Arc<RwLock<Tsdb>> = Arc::new(RwLock::new(Tsdb::new()));
tsdb.write().unwrap().set_source(source_name);
// ingest required &mut self
tsdb.write().unwrap().ingest(snapshot);

// After — MemoryStore is cheaply Clone; ingest takes &self (Arc-internal RwLock)
use metriken_query::MemoryStore;

let store = MemoryStore::builder()
    .source("rezolus")
    .version("1.2.3")
    .sampling_interval_ms(1000)
    .build();

// Enable the `ingest` feature in Cargo.toml first:
//   metriken-query = { version = "0.11", features = ["ingest"] }
store.ingest_snapshot(snapshot);  // takes Snapshot by value, &self receiver

// Clone is cheap — both handles share state via Arc
let store2 = store.clone();
store2.ingest_snapshot(next_snapshot);
```

#### 5. Multi-artifact / multi-file queries

```rust
// Before — manual loop, separate engines, manual result merging
use metriken_query::{Tsdb, QueryEngine};

let mut all_series = Vec::new();
for artifact_id in &artifact_ids {
    let tsdb = Arc::new(load_tsdb(*artifact_id)?);
    let engine = QueryEngine::new(Arc::clone(&tsdb));
    let result = engine.query_range(query, start, end, step)?;
    all_series.extend(result.matrix());
}

// After — compose readers, single query, label injection for disambiguation
use metriken_query::{ParquetReader, MetricsSource};
use std::sync::Arc;

let mut builder = ParquetReader::builder();
for artifact_id in &artifact_ids {
    let reader = Arc::new(load_reader(*artifact_id)?);
    builder = builder.reader_labeled(
        reader,
        [("artifact_id", artifact_id.to_string())],
    );
}
let combined = builder.build()?;
let result = combined.query_range(query, start, end, step)?;
```

#### 6. Sharing a BufferPool across readers (dashboard workloads)

```rust
use metriken_query::{ParquetReader, BufferPool};
use std::sync::Arc;

// Create one pool for the process; 500 MB budget
let pool = BufferPool::new(500 * 1024 * 1024);

let reader_a = ParquetReader::open_with_pool(&path_a, Arc::clone(&pool))?;
let reader_b = ParquetReader::open_with_pool(&path_b, Arc::clone(&pool))?;

// Or via the builder
let combined = ParquetReader::builder()
    .pool(Arc::clone(&pool))
    .file(&path_a)
    .file(&path_b)
    .build()?;

// Inspect cache health
let stats = pool.stats();
println!("cache: {} hits / {} misses, {} bytes", stats.hits, stats.misses, stats.bytes_used);
```

## [0.10.8] - prior

Pre-refactor versions based on the `Tsdb` architecture. See git history for
details.
