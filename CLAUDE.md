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

Nine crates. Each depends (normal dependencies, optional ones included) on the crates after its arrow:

```
metriken-types
metriken-derive
metriken-core        -> metriken-types
metriken-model       -> metriken-types
metriken             -> metriken-core, metriken-derive, metriken-types
metriken-exposition  -> metriken, metriken-model, metriken-types
metriken-storage     -> metriken, metriken-exposition, metriken-model, metriken-types
metriken-query       -> metriken-model, metriken-storage
metriken-recorder    -> metriken, metriken-exposition, metriken-model, metriken-storage, metriken-types
```

`metriken-storage` also has a path-only dev-dependency on `metriken-query`, which `cargo publish` strips. Regenerate the list with `cargo metadata --format-version 1 --no-deps` when a dependency changes (filter `.packages[].dependencies` to workspace members whose `kind` is null).

- **metriken-types** — Types shared by the metriken registry and the readers of what it records.
- **metriken-model** — The observation model: snapshots, group schemas, the rows a recording stores, and the stream's wire names.
- **metriken-storage** — Storage for recordings: the segment format, column readers, and the dendro and `.rez` archives.
- **metriken-core** — `Metric` trait, `Value` enum, `MetricEntry`, global `METRICS` distributed slice (linkme), dynamic metrics registry, type-based request/provide system. Uses `links = "metriken-core"` to enforce single version at compile time.
- **metriken-derive** — Proc macro for `#[metric]` attribute. Generates `declare_metric_v1!` invocations and PHF static maps for metadata.
- **metriken** — User-facing crate re-exporting everything. Defines metric types: `Counter` (AtomicU64), `Gauge` (AtomicI64), `AtomicHistogram` (OnceLock-backed), `RwLockHistogram`, `Lazy<T>`.
- **metriken-exposition** — Snapshot building and serialization. Feature-gated formats: `json`, `msgpack`. Includes `Snapshotter` (builder for filtering metrics), `GroupBuilder` (acquisition groups), and with `stream` the dendro replication frames and the `/metrics/stream` route helper (`stream-tokio` adds a tokio body stream). Msgpack-to-parquet conversion is `metriken-storage`'s `convert` feature.
- **metriken-recorder** — Records producers into one `.dendro`: `detect` (stream, probe hook, Prometheus), `Recorder` (aligned tick, one commit per tick, reconnects, restart metadata, retention, `Event`s and a `Summary`). Features: `http` (default; reqwest over a caller-built client), `registry` (`RegistrySource`, the only feature that links `metriken`). Design: `docs/journal/2026-10-12-metriken-recorder.md`.
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

## Pull Requests

- Feature PRs do not bump crate versions; versions are bumped per crate in release PRs.
- A user-facing change adds a bullet to `CHANGELOG.md` under `## Unreleased`, inside the affected crate's `### <crate> <next-version>` sub-section (created if absent).
- Commit subjects are conventional, `type(scope): description`, with the crate area as the scope (`feat(query): …`, `fix(storage): …`), except release PRs (see Releases).
- Open PRs as drafts.

## Releases

Cut releases with `/cs:release`. What it cannot read from the repository:

- Each crate is versioned and tagged on its own. Tags are `<crate>-v<version>` (`metriken-query-v0.41.0`).
- A release PR covers one crate, or a crate and the dependents whose requirement on it changed, titled `release: <crate> v<version>[, <crate> v<version>…]` in the order of the list under Workspace Crates, from a branch named `release-<crate>-<version>`. It bumps each crate's version and the workspace requirements on it, and updates `Cargo.lock`.
- The root `CHANGELOG.md` keeps released `### <crate> <version>` blocks under `## Unreleased`; a release does not move them. A crate with its own `CHANGELOG.md` (metriken-core, metriken-query, metriken-storage) has its `## [Unreleased]` renamed to `## [<version>] - <date>` in the release PR.
- Nothing is automated: CI (`cargo.yml`) builds and tests only. After the PR merges, tag the merge commit `<crate>-v<version>` for each crate by hand and push the tags to `origin`. Then stop and ask before `cargo publish -p <crate>`, in the order of the list; a crates.io publish cannot be undone, only yanked.
