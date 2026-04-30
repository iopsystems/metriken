# Histogram series memory optimization — implementation plan

This document captures the design for the next two commits on
`claude/memory-optimization-NQZyr` (PR #90).  Everything described here is
scoped to `metriken-query` and does not change the recorder API, the
`metriken-core` traits, or the parquet wire format.

## Status of this branch

Already landed:

1. `histogram = "1.3.0"` workspace bump and switch to `CumulativeROHistogram32`
   for stored deltas.  Recorder and wire format unchanged.
2. BTreeMap → sorted Vec for all four time-keyed series types
   (`CounterSeries`, `GaugeSeries`, `UntypedSeries`, `HistogramSeries`).
3. `anchor_time` removal — stride alignment relies on natural offset.
4. Explicit-empty preservation: every observed histogram timestamp produces a
   stored entry, with empty deltas for null parquet rows, decode failures,
   resets, and u32 overflow.  Tests in `tsdb/series/histogram.rs::tests` and
   `tsdb::ingest_tests`.
5. `examples/memory_compare.rs` reports the RSS delta of OLD vs NEW
   representations against a parquet input.

Cachecannon parquet (3.79 MiB, 1227 series): OLD +127.93 MiB, NEW
+96.41 MiB — ≈25% saving.  Header overhead dominates the remainder.

## Why the savings stalled at ~25%

Per-snapshot footprint of the current representation:

| | bytes |
|---|---|
| `Config` | 32 |
| `Vec<u32>` index header | 24 |
| `Vec<u32>` count header | 24 |
| `(u64, CumulativeROHistogram32)` total | **88 B** |

`Config` is duplicated across every snapshot of every series (1227 series ×
~1000 snapshots ≈ 38 MiB of redundant Config alone).  The two `Vec` headers
are also duplicated per snapshot.  Bucket counts only narrow from u64 to
u32 — that single arithmetic win is diluted by the fixed overhead.

## Plan: cheap Config-hoist (next commit)

Hoist `Config` to the series level.  Keep per-snapshot `Vec<u32>`s for
indices and counts.  Per-snapshot footprint drops to **56 B (was 88 B)** —
saves 32 B × N_snapshots × N_series.

### New types in `tsdb/series/histogram.rs`

```rust
pub struct HistogramSeries {
    config: Option<Config>,            // shared, set on first insert
    inner: Vec<(u64, BucketsDelta)>,
}

pub(crate) struct BucketsDelta {
    index: Vec<u32>,                   // sorted ascending non-zero buckets
    count: Vec<u32>,                   // cumulative running sum
}

impl BucketsDelta {
    fn is_empty(&self) -> bool { self.index.is_empty() }
    fn total_count(&self) -> u64 {
        self.count.last().copied().unwrap_or(0) as u64
    }
}
```

### Method-by-method changes

* **`HistogramSeries::insert(ts, value: CumulativeROHistogram32)`** —
  - First call: set `self.config = Some(value.config())`.
  - Subsequent: silently drop on config mismatch (loader/ingest already
    guarantee a single Config per series; mismatch is "should never happen").
  - Decompose `value` into `BucketsDelta { index: value.index().to_vec(),
    count: value.count().to_vec() }` and push at the sorted position
    (same fast/slow path as today).

* **`HistogramSeries::iter()`** — return type changes from
  `slice::Iter<'_, (u64, CumulativeROHistogram32)>` to
  `impl Iterator<Item = (u64, CumulativeROHistogram32)> + '_`.  Each yield
  materializes via `from_parts(self.config?, idx.clone(), cnt.clone())`.
  Empty deltas use `empty_delta_32(config)`.

* **`time_bounds()`**, **`is_empty()`** — unchanged.

* **`iter_strided`** — `StrideIter` now consumes
  `slice::Iter<'a, (u64, BucketsDelta)>` plus the series' `Config`.
  Internally walks slices (`idx.iter()` / `cnt.iter()`) instead of
  `hist.index()` / `hist.count()` accessors; flush still calls
  `from_parts(config, ...)` to emit a stride-bin `CumulativeROHistogram32`.

* **`StrideIter::new(iter, stride, config: Config)`** — new third arg.

* **`percentiles`**, **`heatmap`** — call sites unchanged; both consume
  `iter_strided(stride)` which still yields owned
  `(u64, CumulativeROHistogram32)`.

* **`Add<&HistogramSeries>`** — merge-walk both `inner` Vecs:
  - On `<` / `>`: clone the side's `BucketsDelta` into `out`.
  - On `==`: combine the two `BucketsDelta`s via `combine_slices` (below).
  - Configs must match — return `self` on mismatch (historical fallback).

* **`combine`/`wrapping_add`/`individual_counts`** — drop.  Replace with:

  ```rust
  fn combine_slices(
      a_idx: &[u32], a_cnt: &[u32],
      b_idx: &[u32], b_cnt: &[u32],
      op: impl Fn(u32, u32) -> u32,
  ) -> BucketsDelta;
  ```

  Two-pointer merge over already-sorted indices; on-the-fly decompose by
  tracking `a_prev`/`b_prev` running cumulatives; rebuild output cumulative
  via `wrapping_add` on the running sum.  No `BTreeMap` / `BTreeSet`.
  O(N+M) instead of O((N+M) log (N+M)).

* **`empty_delta_32`/`delta_to_32`/`delta_to_32_or_empty`** — keep as-is.
  Loader and ingest still produce `CumulativeROHistogram32` and call
  `series.insert(ts, that)`.  The decompose into `BucketsDelta` happens
  inside `insert`.

### Tests

Existing tests in `tsdb/series/histogram.rs::tests` access `series.inner`
directly; switch the entry-shaped checks to `series.iter()` so they work
against owned `CumulativeROHistogram32`s.  The end-to-end
`tsdb::ingest_tests` already uses `series.iter()` and is unaffected.

### Verify

`cargo build`, `cargo test --workspace`, `cargo fmt --check`,
`cargo clippy -p metriken-query --all-features`, then re-run
`memory_compare new /tmp/cachecannon.parquet` to record the new RSS delta
in the commit message.

### Commit message shape

`perf(query): hoist Config out of per-snapshot histogram storage`

Captures the 88 B → 56 B per-snapshot footprint reduction and the
combine_slices algorithmic improvement.

## Plan: flatten to series-wide parallel arrays (follow-up commit)

After the cheap hoist lands, replace per-snapshot `Vec<u32>`s with three
series-level flat buffers in CSR form:

```rust
pub struct HistogramSeries {
    config: Option<Config>,
    timestamps: Vec<u64>,              // 8 B per snapshot
    offsets: Vec<u32>,                 // 4 B per snapshot, start of snapshot's range
    indices: Vec<u32>,                 // all snapshots concatenated
    counts: Vec<u32>,                  // all snapshots concatenated
}

// snapshot i covers indices/counts in
//   [offsets[i], offsets.get(i+1).copied().unwrap_or(indices.len() as u32))
```

Per-snapshot fixed overhead: 8 (ts) + 4 (offset) = **12 B** (was 56 B
after the cheap hoist, 88 B before any of this).

### Method-by-method changes (delta from the cheap version)

* **`insert(ts, value)`** — set/check config; push `ts` to `timestamps`,
  push `indices.len()` to `offsets`, then `indices.extend_from_slice(...)`,
  `counts.extend_from_slice(...)`.  Out-of-order insert (rare) still
  works but requires shifting all four buffers — keep correct, optimize
  for the append path.

* **`iter()`** — for snapshot `i`, slice
  `&indices[offsets[i] as usize .. end_of_i]`, materialize
  `from_parts(config, idx_slice.to_vec(), cnt_slice.to_vec())`.  Helper:

  ```rust
  fn snapshot_range(&self, i: usize) -> Range<usize> { /* offsets[i]..end */ }
  fn snapshot_at(&self, i: usize) -> Option<(u64, CumulativeROHistogram32)>;
  ```

* **`StrideIter`** — switch input to `&HistogramSeries` + `range: Range<usize>`
  (or just an index counter).  Walks slices via `snapshot_range`; flush
  unchanged.

* **`Add<&HistogramSeries>`** — merge-walk by snapshot index; on equal
  timestamps call `combine_slices` against the two CSR ranges, then
  `out.indices.extend_from_slice(&combined.index); out.counts.extend...;
  out.offsets.push(...)` etc.

### Materialization cost

Every borrowed `CumulativeROHistogram32` in the query path becomes two
small `Vec<u32>` clones via `from_parts`.  Filed
[iopsystems/histogram issue](https://github.com/iopsystems/histogram/issues/new)
for a slice-based / borrowed quantile API
(`CumulativeROHistogram32Ref<'a>` or `quantiles_from_parts`); when that
lands the materialization can drop entirely.

### Verify

Same as the cheap version, plus an in-binary structural-size check (or a
`size_of_val` walk) in `memory_compare` to confirm the per-snapshot
header overhead delta.

### Expected savings

For a 1k-snapshot series the per-snapshot fixed overhead drops from
56 B → 12 B (≈4.7×).  On the cachecannon dataset this should bring NEW
RSS well below 60 MiB (vs 96 MiB today).

## Out of scope for this branch

- Slice-based quantile API in `iopsystems/histogram` — tracked upstream.
- Lazy materialization from Arrow `RecordBatch`es (would skip the
  in-memory copy entirely, at the cost of keeping the parquet decode
  resident).  Worth revisiting after the structural changes land, since
  the Arrow representation already mirrors the CSR layout we'll converge
  to.
- Counter / gauge byte-narrowing (u64 → u32 values).  Risky for byte-rate
  counters that exceed 2^32 per sampling interval (saturating high-rate
  NICs); intentionally not pursued.
