# Display-mode query decimation (boxplot envelopes)

**Status:** milestone 1 shipped on branch `feat/display-reducer` (off `main`
@ `550e4c2`, metriken-query 0.11.0). Engine side complete and tested;
consumer integration (rezolus viewer) not started. This is the first entry
in this repo's journal — see the engineering-journal convention it follows.

## Goal

Give a metrics source a way to answer a range query **for display** that is
decimated to a bounded point budget *without* losing short-lived spikes, and
without corrupting query semantics. The standard `QueryResult::Matrix`
(a scalar per timestamp) cannot express this: you get either full resolution
(unbounded payload) or a lossy scalar-per-bucket (spikes averaged away).

## Why (context from the rezolus viewer)

The consumer is the rezolus viewer's chart-fetch path. Two failure modes were
observed there and drove this design:

1. **Spike averaging.** The viewer originally queried at a coarse PromQL
   `step` (a point-count grid). metriken evaluates a range query by
   bucket-aggregating at `step`, which *averages away* a 1-in-N spike before
   it ever reaches the chart. Sub-second/short-lived events vanished.
2. **Decimating via `step` is the wrong lever — proven, not assumed.** A
   rezolus interim tried bounding payload by widening `step`. Measured
   against a real parquet: histogram queries (`histogram_quantiles` /
   `histogram_heatmap`) *ignore* `step` entirely (their resolution is a
   `stride` arg), so a coarse step bounds nothing for them; counters *do*
   honor `step` but then need their rate window rewritten to match. Widening
   `step` corrupts queries (mismatched per-type resolution) and still
   averages spikes. So the viewer reverted to querying whole-range at native
   step (rezolus PR #995, tag v5.16.1) — correct but payload-unbounded.

Conclusion carried into this design: **separate evaluation from decimation.**
Evaluate at native resolution (faithful rate/aggregation), then decimate the
*result* — as a distinct, presentation-only step that carries enough
distribution information to keep spikes visible.

## Design decisions

- **Richer-than-PromQL response.** PromQL stays the query *language*; the
  display *response* need not be PromQL-compliant. Precedent already exists:
  `QueryResult::HistogramHeatmap` is a non-Prom shape. So a parallel
  `DisplayResult` is idiomatic, not a departure.
- **Per-bucket boxplot, not a scalar+band.** Each decimated point is
  `EnvPoint { t, min, lo, median, hi, max }`:
  - `median` is the line — **robust**, so a spike stays in `max` instead of
    dragging the line up. (Rejected `last`-value line: arbitrary, jittery,
    not centered in the band, and a boundary outlier corrupts it. Rejected
    `mean`: a spike drags it, double-counting the spike in line *and* band.)
  - `min`/`max` are the hard extremes — the spike-preservation **invariant**.
  - `lo`/`hi` are the typical-spread inner band.
- **Inner band configurable, outer band invariant.** `lo`/`hi` are computed
  at caller-chosen quantiles (`DisplayOptions.band`, default IQR
  `[0.25, 0.75]`). `min`/`max` are **not** configurable: a custom outer bound
  (e.g. p99) would silently reintroduce the spike-hiding bug. The real desire
  behind "custom outer bounds" (one outlier blowing out the y-axis) is a
  view-layer clipping concern — clamp what the chart *renders*, never discard
  the extreme from the data.
- **Only `Matrix` is decimated.** Heatmap/scalar/vector pass through
  unchanged (heatmaps are already resolution-limited server-side; decimation
  is a per-series-time-points concept that doesn't apply to a 2D grid).
- **Default trait method, post-evaluation.** `query_range_display` is a
  default method on `MetricsSource` that post-processes `query_range`, so it
  works for every backend with zero per-impl code. This captures all
  Matrix-producing paths (streaming + histogram scalar/quantile/irate) at one
  point without threading through engine internals.

## What shipped (milestone 1)

All in `metriken-query`, branch `feat/display-reducer`:

- **`src/display.rs`** (new): `EnvPoint`, `DisplaySeries` (+ provenance:
  `nativeInterval`, `rawPoints`, `reducer`, `band`, `decimated`),
  `DisplayResult` (serde tag `resultType`: `series` / `histogram_heatmap` /
  `scalar` / `vector`), `Reducer` enum (`Boxplot`, serializes `"boxplot"`),
  `DisplayOptions { budget, reducer, band }` (`Default` = budget 0 /
  no-decimation, Boxplot, band `[0.25, 0.75]`), and `reduce_boxplot`:
  time-uniform buckets over contiguous time-sorted slices; per-bucket
  min/lo/median/hi/max via linear-interpolation (numpy type-7) quantiles;
  gap-preserving (empty buckets emit nothing); identity when `budget == 0`
  or the series already fits the budget.
- **`src/lib.rs`**: `MetricsSource::query_range_display(expr, start_s, end_s,
  step_s, &DisplayOptions) -> Result<DisplayResult, QueryError>` default
  method; re-exports of the new public types.
- **`src/memory_store.rs`**: two ingest-gauge integration tests (spike
  preserved in `max` while median stays on baseline; identity under budget).

**Verification:** 158 tests pass (`cargo test -p metriken-query --all-features`),
`metriken-query` clippy-clean, fmt-clean.

**Known pre-existing (not this change):** `cargo clippy --all-features` on the
workspace fails on a deprecated `histogram::Histogram::percentiles` call in
`metriken-exposition` (`src/prometheus.rs:168`) — unrelated to this work.

## GO/NO-GO

This is engine plumbing with no runtime sampler cost. The GO gate is
**milestone 2's payload measurement**: on a real long recording (e.g. 24h @
1s), display mode at a ~2–4k budget must cut per-series payload by >10x vs
native-resolution `query_range` while keeping spikes visible. If the win
isn't there, the whole envelope response is not worth the consumer
complexity. Not yet measured — see below.

## Milestone 2 — rezolus integration (not started)

The plan, for whoever picks this up:

1. **Local dev wiring.** `[patch.crates-io]` in rezolus `Cargo.toml` →
   local `metriken-query` (+ `metriken`, `metriken-exposition` to avoid a
   duplicate `metriken-exposition` 0.16.0/0.16.1). Keep the patch
   uncommitted / local-only. All local versions already satisfy rezolus's
   requirements (metriken 0.9.2 exact, metriken-exposition ^0.16.0 ⊇ 0.16.1,
   metriken-query 0.11.0 exact).
2. **Route.** Add `format=display&points=N&band=lo,hi` to rezolus's
   `/api/v1/query_range` (`src/viewer/routes.rs`), dispatching to
   `query_range_display` with a `DisplayOptions`. Absent `format=display`,
   keep today's `QueryResult` (MCP/analysis path must stay compliant).
3. **Frontend adapter** (`src/viewer/assets/lib/data.js`): consume
   `DisplayResult::Series`; render median line + filled `[lo,hi]` inner band
   + fainter `[min,max]` outer band (echarts: line series + two `areaStyle`
   bands, or a custom renderer). When `decimated:false`, all five values are
   equal so it collapses to a plain line — one code path for full-res and
   decimated. Use the `nativeInterval` / `decimated` provenance to badge
   downsampling and decide zoom-refetch.
4. **Measure** the payload delta (the GO gate above) and record it here.

## Deferred / reopen

- **Engine-level pre-materialization.** The default `query_range_display`
  post-processes a fully-materialized `Matrix`. A future override could
  decimate *before* materializing (inside the streaming pipeline at
  `src/promql/streaming/`), saving memory on long recordings. Reopen if
  memory on the metriken-query side (esp. the WASM viewer) shows up as a
  bottleneck.
- **Second reducer (LTTB).** `Reducer` is a one-variant enum today. LTTB
  (visual-fidelity single line, no band) is the obvious second algorithm.
  The `DisplayOptions`/`Reducer` shape already allows adding it.
- **Multi-band / configurable outer.** Deliberately out of scope — see the
  "outer band invariant" decision. Reopen only with a distinct, explicitly
  lossy mode, never as a tweak to the boxplot's outer bound.
- **Publish + un-patch.** When milestone 2 proves out: publish a new
  `metriken-query` version, bump rezolus's dependency, drop the local
  `[patch]`.
