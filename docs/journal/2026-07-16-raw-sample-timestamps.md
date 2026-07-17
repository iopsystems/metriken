# Raw sample timestamps (sampling-jitter accessor)

**Status:** shipped in **metriken-query 0.13.0** — merged to `main` as
`48e7813` (squash of PR
[#118](https://github.com/iopsystems/metriken/pull/118), off `659b74e`).
Consumer integration (rezolus viewer jitter chart) in progress in a separate
repo. (This entry lands after #118 — the record was missed on the code PR and
is checked in here as a follow-up.)

## Goal

Expose the **raw, un-snapped per-sample `timestamp` column** through the
`MetricsSource` API so a consumer can inspect a recording's actual sampling
cadence — specifically to visualize **sampling jitter** (how far each sample's
inter-arrival gap deviates from the nominal interval).

## Why (context from the rezolus viewer)

The consumer is the rezolus viewer, which is adding a jitter chart: for a
loaded recording, plot the delta between consecutive sample timestamps against
wall-clock time. That requires the *actual* recorded timestamps. Neither
existing path in metriken-query preserves them:

1. **The PromQL range path grids to `step`.** `query_range` evaluates on a
   regular `start + k·step` grid, so its output timestamps are synthetic — any
   jitter is gone before it reaches a caller. (This is the same
   evaluate-on-a-grid property documented in the
   [display-mode decimation entry](2026-07-09-display-mode-decimation.md).)
2. **The eager `Tsdb` path snaps at ingest.** `Tsdb` rounds every timestamp to
   the nominal interval via `snap_timestamp(ts, interval) = ((ts + interval/2)
   / interval) · interval` before storing it, so even its stored timestamps are
   jitter-free.

So neither an existing accessor nor a query can answer "what were the real
sample times." The raw values only survive in the parquet `timestamp` column
itself.

## Design decisions

- **Read the raw column via the lazy `ParquetReader` path, which never snaps.**
  `ParquetReader` (what the viewer holds) resolves through
  `MultiParquetSource`/`ParquetSource` (`metriken-query/src/parquet.rs`) — a
  lazy, per-column reader that is structurally disjoint from both the
  `snap_timestamp` `Tsdb` loader and the gridded PromQL `QueryEngine`. The new
  accessor reads the `timestamp` column directly from that path, so it returns
  exactly what is on disk.
- **New `read_raw_u64_rg` mirrors `read_timestamps` minus the lossy steps.** It
  builds the same `build_batch_reader` + `ProjectionMask` per-row-group read and
  `UInt64Array` downcast that every other per-column reader in the file uses,
  but omits the `snap_timestamp` call **and** the `BufferPool` cache write (the
  raw timestamps are a one-shot read, not a hot query column worth caching).
- **Trait method with an empty default.** `MetricsSource::sample_timestamps(&self)
  -> Vec<u64>` defaults to `Vec::new()`; only `ParquetReader` overrides it.
  `MemoryStore` (live ingest) keeps the default — its ingest path *does* snap,
  and live mode is not the jitter feature's target; retaining raw live
  timestamps is deferred (below).
- **Contract:** nanoseconds since the Unix epoch, ascending, in row order.

## What shipped

All in `metriken-query`, version `0.12.0 → 0.13.0`:

- **`src/lib.rs`**: `MetricsSource::sample_timestamps(&self) -> Vec<u64>` default
  method (returns empty).
- **`src/parquet.rs`**: `MultiParquetSource::sample_timestamps()` +
  `read_raw_u64_rg` (raw per-row-group `timestamp` column read, un-snapped,
  concatenated across files in insertion order); `ParquetReader` inherent method
  + `MetricsSource` impl delegating to it.
- **Test** (`src/parquet.rs`): `sample_timestamps_returns_raw_unsnapped_values`
  writes a parquet whose `timestamp` column holds values deliberately **off**
  the 1 s grid implied by `sampling_interval_ms=1000` (e.g. `2_003_000_000`,
  `2_998_000_000`) and asserts **full-vector equality** against the input — so
  the test fails if any snap/grid path leaks in (hand-checked: `snap_timestamp`
  would map those to `2_000_000_000` / `3_000_000_000`). Plus
  `memory_store_sample_timestamps_is_empty_by_default`.
- CHANGELOG `## [0.13.0]`.

**Verification:** `cargo test -p metriken-query` 160 pass;
`cargo clippy -p metriken-query --no-deps -- -D warnings` clean;
`cargo fmt --check` clean.

**Known pre-existing (not this change):** whole-workspace
`cargo clippy --all-features` trips a deprecated `histogram::percentiles` call
in `metriken-exposition` (`src/prometheus.rs`) and an unused test import in
`memory_store.rs` — both present on `main` before this branch (confirmed via
stash), unrelated to this work. Scope crate-level checks with `-p`/`--no-deps`.

## Release

Single-crate release: metriken-query 0.13.0's only new requirement,
`metriken-exposition 0.16.1`, is **already published** on crates.io, so no
dependency release is needed first. The version bump + CHANGELOG rode in with
PR #118, so after merge the release is just tag + publish (see
`.claude/skills/release` step 9): tag `metriken-query-v0.13.0`, push it, then
`cargo publish -p metriken-query` (irreversible — confirm first).

## Deferred / reopen

- **Raw live timestamps in `MemoryStore`.** Live ingest snaps and
  `sample_timestamps()` returns empty there, so the jitter chart shows "no
  data" in live mode. Reopen if a live consumer needs jitter — retain the raw
  ingest timestamps behind the same accessor.
- **Multi-file ordering.** `sample_timestamps()` concatenates a
  `MultiParquetSource`'s files in insertion order (matching `time_range_ns` and
  the other multi-file accessors); the "ascending" contract holds only if a
  multi-file caller adds sources chronologically. Add a sort (or document the
  precondition) if a caller can't guarantee it.
- **Null handling.** Nulls in the `timestamp` column are dropped via
  `.iter().flatten()`, which would shorten the vector below row count on a
  malformed file (`read_timestamps`, by contrast, preserves `None` at position).
  Harmless today — the recorder writes `timestamp` non-nullable — but a
  divergence to revisit if that invariant ever loosens.
- **Consumer un-patch.** The rezolus viewer bridges to this via a local
  `[patch.crates-io]` on the whole metriken family until 0.13.0 is published;
  it drops the patch and pins `0.13.0` at that point.
