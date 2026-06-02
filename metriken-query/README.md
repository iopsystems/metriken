# metriken-query

[![Crates.io](https://img.shields.io/crates/v/metriken-query)](https://crates.io/crates/metriken-query)
[![License](https://img.shields.io/crates/l/metriken-query)](LICENSE)

PromQL query engine for parquet metric captures produced by [`metriken-exposition`](https://crates.io/crates/metriken-exposition). Supports both file-backed streaming reads (low memory) and in-memory ingestion (live mode).

## Overview

`metriken-query` lets you run PromQL queries against parquet files containing time-series metrics. Two reader types implement a shared `MetricsSource` trait:

- **`ParquetReader`** — streams data from parquet files. Memory usage is bounded by the active row group, not the file size. Ideal for query servers holding many captures.
- **`MemoryStore`** — in-memory ingestion. Live-mode polling from a running agent writes snapshots into the store; queries run against the growing data.

A process-shared `BufferPool` caches decoded row groups across all readers, restoring fast query performance for dashboard workloads.

## Quick start

### Query a parquet file

```rust,ignore
use std::path::Path;
use metriken_query::{ParquetReader, MetricsSource};

let reader = ParquetReader::open(Path::new("metrics.parquet"))?;
let (start, end) = reader.time_range().expect("non-empty file");
let result = reader.query_range(
    "rate(network_rx_bytes[1m])",
    start, end, 1.0
)?;
# Ok::<_, Box<dyn std::error::Error>>(())
```

### Compose multiple files

```rust,ignore
use metriken_query::{ParquetReader, MetricsSource};

let reader = ParquetReader::builder()
    .file_labeled("baseline.parquet", [("run", "baseline")])
    .file_labeled("experiment.parquet", [("run", "experiment")])
    .build()?;

// Filter by injected label
let result = reader.query_range(
    r#"histogram_quantile(0.99, latency_ns{run="experiment"})"#,
    0.0, 3600.0, 1.0,
)?;
# Ok::<_, Box<dyn std::error::Error>>(())
```

### Live ingestion (with `ingest` feature)

```rust,ignore
use metriken_query::{MemoryStore, MetricsSource};

let store = MemoryStore::builder()
    .source("rezolus")
    .version("1.0")
    .sampling_interval_ms(1000)
    .build();

// In a background loop:
loop {
    let snapshot = fetch_from_agent().await?;
    store.ingest_snapshot(snapshot);
}

// Query from any thread (MemoryStore is Send + Sync and cheaply Clone via Arc):
let result = store.query_range("cpu_usage", 0.0, 3600.0, 1.0)?;
```

### Browser / WASM

```rust,ignore
use metriken_query::{ParquetReader, MetricsSource};

let bytes: Vec<u8> = read_uploaded_file();
let reader = ParquetReader::open_bytes(bytes)?
    .with_filename("uploaded.parquet");
```

## Shared cache for hot data

For workloads with repeated queries against the same files (dashboards, A/B compare views), share a `BufferPool` across readers:

```rust,ignore
use std::path::Path;
use std::sync::Arc;
use metriken_query::{BufferPool, ParquetReader};

let pool = Arc::new(BufferPool::new(500 * 1024 * 1024));  // 500 MB budget

let reader_a = ParquetReader::open_with_pool(Path::new("a.parquet"), Arc::clone(&pool))?;
let reader_b = ParquetReader::open_with_pool(Path::new("b.parquet"), Arc::clone(&pool))?;

// First query: cache miss, slower
let _ = reader_a.query_range("rate(metric[1m])", 0.0, 3600.0, 1.0)?;
// Second query against same range: cache hit, faster
let _ = reader_a.query_range("rate(metric[1m])", 0.0, 3600.0, 1.0)?;

println!("Pool stats: {:?}", pool.stats());
# Ok::<_, Box<dyn std::error::Error>>(())
```

The pool follows the standard layered-cache pattern used by ClickHouse, DuckDB, and Postgres. LRU eviction keeps active data hot; cold readers contribute only their parquet metadata to RAM.

## Schema introspection

Both reader types implement `MetricsSource`:

```rust,ignore
use std::path::Path;
use metriken_query::{ParquetReader, MetricsSource};

let reader = ParquetReader::open(Path::new("metrics.parquet"))?;

// Metric enumeration
let counters: Vec<String> = reader.counter_names();
let all: Vec<String> = reader.all_names();
let labels = reader.counter_labels("cpu_usage");

// Cross-metric introspection
let dimensions: std::collections::BTreeSet<String> = reader.all_label_keys();
let cpus = reader.label_values("cpu_usage", "cpu");

// File metadata
let source: String = reader.source();
let version: String = reader.version();
let filename: Option<String> = reader.filename();
let interval_s: f64 = reader.interval();
# Ok::<_, Box<dyn std::error::Error>>(())
```

## Tradeoffs

| Feature | `ParquetReader` (streaming) | Old `Tsdb` (pre-0.11) | `MemoryStore` |
|---------|----------------------------|----------------------|---------------|
| Open / load time | constant (~2ms) | O(file size) | constant |
| Open / load RSS | small constant (~50 KB) | O(file size) × ~3-5 | O(ingested data) |
| Repeat query latency | slower without pool | very fast (materialized) | very fast |
| Repeat query latency (with `BufferPool`) | ~1.5× faster than cold | n/a | n/a |
| Multi-file merging | yes (k-way merge in builder) | no | n/a |
| Live ingestion | no | yes | yes |

For a 315 MB capture:
- Old `Tsdb`: 16 seconds to load, 856 MB RSS
- New `ParquetReader`: 2 ms to open, 16 KB RSS (data decoded on demand)

## Features

```toml
[dependencies]
metriken-query = { version = "0.11", features = ["ingest"] }
```

- `ingest` (off by default) — pulls in `metriken-exposition` and enables `MemoryStore::ingest_snapshot`. Without this feature, `MemoryStore` is still usable for query routing but cannot accept new snapshots.
- `lz4` (on by default) — enables LZ4 decompression support for parquet files.
- `fixtures` (off by default, test/bench-only) — exposes the `fixtures` module with `FixtureBuilder` and `ParquetAugmentor` for generating test parquets.

Default features are minimal so consumers (including WASM viewers) can opt in.

## Cargo dependency

```toml
metriken-query = "0.11"
```

To consume the workspace path during development:

```toml
metriken-query = { path = "../metriken/metriken-query" }
```

## License

MIT OR Apache-2.0
