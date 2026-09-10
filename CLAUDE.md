# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

metriken is a fast, lightweight Rust metrics library that uses compile-time registration via proc macros and `linkme` distributed slices. Metrics are declared as statics with `#[metric]`, collected into a global array at link time, and can be exposed via multiple serialization formats or queried with PromQL.

## Build Commands

```bash
cargo build                                  # build all crates
cargo nextest run --all-targets --all-features --locked  # run tests (CI uses nextest)
cargo test --doc --all-features --locked     # run doctests separately
cargo test -p metriken-query                 # test a single crate
cargo fmt --all -- --check                   # check formatting
cargo clippy --all-targets --all-features    # lint
cargo hack --feature-powerset check --locked # check all feature combinations (CI)
```

## Formatting

rustfmt.toml settings: `wrap_comments = true`, `imports_granularity = "module"`, `group_imports = "stdexternalcrate"`.

## Workspace Crates

Five crates with this dependency flow:

```
metriken-core  <──  metriken-derive
      │                    │
      └───── metriken ─────┘
                │
       metriken-exposition
                │
         metriken-query
```

- **metriken-core** — `Metric` trait, `Value` enum, `MetricEntry`, global `METRICS` distributed slice (linkme), dynamic metrics registry, type-based request/provide system. Uses `links = "metriken-core"` to enforce single version at compile time.
- **metriken-derive** — Proc macro for `#[metric]` attribute. Generates `declare_metric_v1!` invocations and PHF static maps for metadata.
- **metriken** — User-facing crate re-exporting everything. Defines metric types: `Counter` (AtomicU64), `Gauge` (AtomicI64), `AtomicHistogram` (OnceLock-backed), `RwLockHistogram`, `Lazy<T>`.
- **metriken-exposition** — Snapshot building and serialization. Feature-gated formats: `json`, `msgpack`, `parquet`. Includes `Snapshotter` (builder for filtering metrics), `ParquetWriter`, and msgpack-to-parquet conversion.
- **metriken-query** — PromQL query engine and in-memory TSDB loaded from parquet files. Feature-gated: `ingest` (from metriken-exposition), `lz4`, `http` (axum API routes). HTTP API is Prometheus-compatible (`/api/v1/query`, `/api/v1/query_range`, `/api/v1/labels`, etc.).

## Key Design Patterns

- **Compile-time metric registration**: `#[metric]` generates a `MetricEntry` added to a linkme distributed slice. No runtime registration cost for static metrics.
- **Type-erased storage**: All metrics stored as `*const dyn Metric` in a single global array, with type recovery via `as_any()` or the provide/request system.
- **Dual registration**: Static metrics via linkme + dynamic metrics via `DynBoxedMetric<T>`/`DynPinnedMetric<T>` in a `RwLock<BTreeMap>`. Iterators combine both.
- **Provider pattern** (metriken-core/src/provide.rs): Type-keyed request/response modeled after `std::error::Request`. Metrics can provide metadata, formatters, or custom types without downcasting.

## Testing

- CI runs on Ubuntu, macOS, and Windows.
- **metriken**: UI compile-failure tests via `trybuild` (tests/ui/), integration tests for naming, description, dynamic metrics, formatters.
- **metriken-query**: Tests build a TSDB from programmatic snapshots via `create_test_tsdb()` helper, then assert query results.
- **metriken-exposition**: Uses `tempfile` for snapshot serialization round-trip tests.
