# Plan: slice-based quantile API in `iopsystems/histogram`

This document describes a proposed addition to the
[`iopsystems/histogram`](https://github.com/iopsystems/histogram) crate
needed by `metriken-query`'s columnar histogram storage.  It is intended
as the source-of-truth for an upstream issue/PR — paste sections into a
GitHub issue body or PR description as needed.

## Why

`metriken-query` stores per-period delta histograms in a columnar
"CSR" layout — one shared `Config` per series plus three flat buffers
(`timestamps: Vec<u64>`, `offsets: Vec<u32>`, `indices: Vec<u32>`,
`counts: Vec<u32>`).  See [`HistogramSeries`] in
`metriken-query/src/tsdb/series/histogram.rs`.

For every percentile or heatmap query, we currently materialize a
`CumulativeROHistogram32` per snapshot just to call its quantile
methods:

```rust
fn snapshot_at(&self, i: usize) -> Option<CumulativeROHistogram32> {
    let r = self.snapshot_range(i);
    let idx: Vec<u32> = self.indices[r.clone()].to_vec();   // alloc 1
    let cnt: Vec<u32> = self.counts[r].to_vec();            // alloc 2
    CumulativeROHistogram32::from_parts(self.config?, idx, cnt).ok()
    // ↑ validates the inputs (linear pass) — work we just did at load time
}
```

Per call we pay two small `Vec<u32>` allocations + memcpy + a
revalidation pass.  For a `histogram_percentiles([p50, p90, p99],
metric)` query against a histogram-heavy file in the Rezolus viewer
(e.g. `sglang-nixl-16c.parquet`: 143 histogram series × 800 snapshots ≈
114 K snapshots), that's:

* ≈ 230 K small `Vec<u32>` allocations
* ≈ 2.5 MB of transient memcpy traffic
* ≈ 230 K revalidation passes

Wall-time: ~5–20 ms on native, more on wasm.  The bigger problem is
**wasm linear memory only grows** — each transient allocation may
trigger `memory.grow`, permanently expanding the tab's resident set.
This is the same class of leak the streaming-decode change in
`metriken-query` PR #90 just stopped doing for the loader; we'd like
to avoid re-introducing it on the query path.

A slice-based API avoids the entire round-trip: caller already owns
the `Vec<u32>` buffers (built once at load time and reused), the crate
runs quantile binary search directly on the borrowed slices.

## Proposed API

Two shapes that achieve the same goal.  Either is acceptable; the
borrowed-view form is more idiomatic and parallels how
`std::path::Path` relates to `PathBuf`.

### Option A: borrowed-view type (preferred)

```rust
/// Borrowed view into a `CumulativeROHistogram32`-shaped histogram
/// without owning the `index` / `count` buffers.
///
/// All inherent methods of `CumulativeROHistogram32` that don't mutate
/// or take ownership are available on the borrowed view.
#[derive(Clone, Copy, Debug)]
pub struct CumulativeROHistogram32Ref<'a> {
    config: Config,
    index: &'a [u32],
    count: &'a [u32],
}

impl<'a> CumulativeROHistogram32Ref<'a> {
    /// Build a view, validating the same invariants `from_parts`
    /// validates.  Use `from_parts_unchecked` for the (common) case
    /// where the caller already guarantees these.
    pub fn from_parts(
        config: Config,
        index: &'a [u32],
        count: &'a [u32],
    ) -> Result<Self, Error>;

    /// Build a view without validation.  The caller asserts that:
    ///   - index.len() == count.len()
    ///   - every index is in range for the config
    ///   - indices are strictly ascending
    ///   - counts are strictly non-decreasing
    ///   - no count is zero
    /// Used by columnar consumers that already validated at construction.
    pub fn from_parts_unchecked(
        config: Config,
        index: &'a [u32],
        count: &'a [u32],
    ) -> Self;

    pub fn config(&self) -> Config;
    pub fn index(&self) -> &[u32];
    pub fn count(&self) -> &[u32];
    pub fn total_count(&self) -> u64;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;

    pub fn iter(&self) -> CumulativeIter32<'_>;       // yields Bucket
    pub fn iter_with_quantiles(&self) -> QuantileRangeIter32<'_>;
    pub fn bucket_quantile_range(&self, bucket_idx: usize) -> Option<(f64, f64)>;
}

impl<'a> SampleQuantiles for CumulativeROHistogram32Ref<'a> {
    fn quantiles(&self, quantiles: &[f64])
        -> Result<Option<QuantilesResult>, Error>;
    fn quantile(&self, quantile: f64)
        -> Result<Option<QuantilesResult>, Error>;
}

// Cheap downgrade from owned to borrowed
impl<'a> From<&'a CumulativeROHistogram32> for CumulativeROHistogram32Ref<'a> {
    fn from(h: &'a CumulativeROHistogram32) -> Self;
}
```

Equivalent for `SparseHistogram32` (sparse non-cumulative) is similarly
useful but lower priority — `metriken-query` only consumes the
cumulative form on the hot path.

### Option B: free function

```rust
/// Compute quantiles directly from the parts of a cumulative-RO
/// histogram, without materializing one.  Same input contract as
/// `CumulativeROHistogram32::from_parts` followed by `.quantiles(...)`,
/// without the intermediate allocation.
pub fn cumu32_quantiles_from_parts(
    config: Config,
    index: &[u32],
    count: &[u32],
    quantiles: &[f64],
) -> Result<Option<QuantilesResult>, Error>;
```

Smaller surface, less idiomatic, but lower-friction to add.  Doesn't
help heatmap rendering (which calls `iter()` / `iter_with_quantiles`
on the histogram); we'd want a parallel `cumu32_iter_from_parts`,
which starts pointing back toward Option A.

## Implementation sketch

The existing `CumulativeROHistogram32` already stores its data as an
inline `Config` plus two `Vec<u32>` fields.  The borrowed view is the
same struct with `&'a [u32]` instead of `Vec<u32>`.  Most inherent
methods are already implemented as walks over `self.index` /
`self.count` slices internally, so the implementation is largely
duplicating the impl block with the borrowed type — or, more
elegantly, generic over a trait that abstracts "thing that has a
`config`, `&[u32]` index, and `&[u32]` count":

```rust
trait CumulativeRoView {
    fn config(&self) -> Config;
    fn index(&self) -> &[u32];
    fn count(&self) -> &[u32];
}

impl CumulativeRoView for CumulativeROHistogram32 { ... }
impl<'a> CumulativeRoView for CumulativeROHistogram32Ref<'a> { ... }

impl<T: CumulativeRoView> SampleQuantiles for T { /* shared impl */ }
```

Either approach works; the trait route avoids code duplication and is
forward-compatible with adding a similar view to other variants
(`CumulativeROHistogram` u64, `SparseHistogram32`).

## Testing

The existing test suite for `CumulativeROHistogram32::quantiles` etc.
should be parameterized to also exercise the borrowed view, ensuring
the two paths produce bit-for-bit identical `QuantilesResult` for the
same inputs.  The unsafe `from_parts_unchecked` constructor needs
debug-assertion-only validation under `cfg(debug_assertions)`.

## `metriken-query` consumer-side changes

After the upstream API lands, `metriken-query`'s changes are roughly:

```rust
// metriken-query/src/tsdb/series/histogram.rs
impl HistogramSeries {
    fn snapshot_view(&self, i: usize)
        -> Option<CumulativeROHistogram32Ref<'_>>
    {
        let cfg = self.config?;
        let r = self.snapshot_range(i);
        Some(CumulativeROHistogram32Ref::from_parts_unchecked(
            cfg,
            &self.indices[r.clone()],
            &self.counts[r],
        ))
    }
}
```

Then `percentiles()` and `heatmap()` iterate snapshots calling
`.quantiles(percentiles)` / `.iter()` on the borrowed view directly.
Zero allocations on the query path.

`StrideIter::flush()` continues to call `from_parts(...)` for the
emitted bin — that's an owned histogram (the accumulated
cross-snapshot sum), and there's no slice it could borrow against.
The slice API doesn't displace this use case.

## Versioning

Adding a new public type and a new trait impl is additive — no
breaking changes.  Could ship in `histogram = "1.4"`.  The
`from_parts_unchecked` constructor is `unsafe`-equivalent in the API
contract (caller asserts invariants) but compiles as a safe `pub fn`
since the cost of misuse is incorrect quantile output rather than
memory unsafety.  We could prefix with `unsafe` to be conservative.

## Estimated effort

* Borrowed view + trait abstraction + `From<&Owned>` impl: ~150 LOC
  in `iopsystems/histogram`.
* Test parameterization: ~50 LOC.
* `metriken-query` consumer-side: ~30 LOC, drops a similar amount of
  materialization scaffolding.

## Out of scope

* Mutable borrowed view (would require more careful invariant
  threading; not needed by any current consumer).
* Borrowed view over the dense `Histogram32` form — its data layout
  is a single `Vec<u32>` indexed by bucket position, so a view is just
  `&[u32]`; no separate type needed.
* Async / streaming quantile computation across many snapshots — could
  be useful for the rezolus viewer's animated time-range selector, but
  needs the borrowed-view foundation first.
