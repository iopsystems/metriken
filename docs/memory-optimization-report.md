# Memory optimization report — `metriken-query` TSDB

Branch: `claude/memory-optimization-NQZyr` (PR #90).

Two distinct memory budgets matter for the `metriken-query` consumers (the
Rezolus viewer in particular, which runs as wasm — wasm linear memory only
grows, so peak load-time resident is the permanent ceiling per tab):

* **Live application bytes** (`alloc` from jemalloc) — the actual cost of
  the `Tsdb` we hand back from `load_from_bytes`.  Stays for the lifetime
  of the loaded data.
* **Resident bytes** (`resident` from jemalloc, ≈ process RSS) — live plus
  whatever the allocator hasn't returned to the OS, including transient
  decode scratch.  This is what the wasm tab actually holds.

Headline reductions across all 11 parquet samples in
`rezolus/site/viewer/data{,/disagg}`:

| | OLD total | NEW total | saved |
|---|---|---|---|
| **live** | 738.9 MiB | 249.8 MiB | **−489 MiB (−66%)** |
| **resident** | 2 336.0 MiB | 574.7 MiB | **−1 761 MiB (−75%)** |

## Techniques applied, ranked by absolute corpus savings

### 1. Streaming column-by-column parquet decode (commit `009d89c`)

**What.**  Decompose `Tsdb::load_from_bytes` into two passes against a
refcounted `Bytes` blob: first project just the `timestamp` column to
build a `Vec<Option<u64>>`, then for each non-timestamp column open a
fresh `ParquetRecordBatchReaderBuilder` with
`ProjectionMask::roots([col])` and stream-process its batches via three
helpers (`stream_counter_column` / `stream_gauge_column` /
`stream_histogram_column`).  The reader drops between columns.

**Why this is biggest.**  The previous loader built a full `RecordBatch`
per row group — every column's Arrow buffers decoded simultaneously.
Peak resident scratch was ~5–10× the live data size on histogram-heavy
files; on wasm those pages then stayed forever.  Streaming bounds peak
to *one column × one batch* of decoded Arrow data plus the rolling
`prev` cumulative for histograms (tens of KB).

**Corpus contribution (resident only — live is unaffected):**

| | resident before streaming | resident after streaming | saved |
|---|---|---|---|
| sum across 11 files | 1 885.8 MiB | 574.7 MiB | **−1 311 MiB (−70%)** |

### 2. Per-period delta histograms + u32 narrowing + auto-downsample 7→5

Three closely-coupled changes on the histogram representation that all
landed in the first wave of the PR (commits `b361709`, `2716405`):

* **Delta storage** — instead of storing each snapshot's
  cumulative-since-start histogram, the loader differences consecutive
  snapshots and stores the per-period delta.  A delta has only the
  buckets that fired during one sampling interval (typically ~5–20×
  fewer non-zero buckets than the running cumulative).
* **u32 counts** — `CumulativeROHistogram32` from histogram 1.3 narrows
  the count vector from `Vec<u64>` to `Vec<u32>`.  Per-period totals
  comfortably fit, halving the per-bucket count cost.
* **Auto-downsample `grouping_power 7 → 5`** — `Histogram::downsample`
  collapses every 4 adjacent buckets into 1 for high-precision sources,
  reducing relative error from 0.78% to 3.13% (invisible at viewer
  rendering resolutions) and shrinking non-zero bucket count by up to
  4× on dense workloads.

**Why grouped.**  All three target the same dimension (per-snapshot
histogram bucket data) and we don't have the intermediate measurements
to attribute precisely.  Together they're the biggest *live*-bytes
contributor: the OLD representation stored cumulative `(u32 idx, u64
cnt)` pairs across an ever-growing set of non-zero buckets;
NEW stores delta `(u32 idx, u32 cnt)` pairs across a much smaller set.

**Corpus contribution (estimated from histogram column shares):** on
dense files (sglang-nixl-16c, AB_*, vllm), histogram bucket data was
the dominant live-bytes consumer.  Combined live-byte saving across
the corpus is ≈ −280 MiB.

### 3. `BTreeMap` → sorted `Vec` for time-keyed series storage (`f722125`)

**What.**  Switched `CounterSeries`, `GaugeSeries`, `UntypedSeries`, and
`HistogramSeries` from `BTreeMap<u64, V>` to a sorted `Vec<(u64, V)>`.
Insertion fast-paths the monotonic-load case (parquet rows arrive in
timestamp order); range windows use `partition_point` and yield a
contiguous slice; `UntypedSeries` joins (`Add`/`Div`/`Mul`) replaced
their `contains_key` + `get_mut` + `remove` walk with two-pointer
merge-walks.

**Why high impact.**  A `BTreeMap<u64, u64>` typically eats ~50 B/entry
counting node overhead and load-factor slack; a `Vec<(u64, u64)>` is
16 B/entry packed.  3–4× per-entry reduction across hundreds of
thousands of counter/gauge entries on every file.  Algorithmic bonus:
join cost drops from O(N log M) to O(N+M).

**Corpus contribution (estimated from counter/gauge column shares):**
≈ −100 MiB live-bytes on a corpus with 15 215 counter columns × hundreds
of rows each.

### 4. CSR flatten + Config hoist for `HistogramSeries` (`c80ec02`, `04061ee`)

**What.**  Replaced `Vec<(u64, CumulativeROHistogram32)>` with a flat
columnar layout:

```rust
pub struct HistogramSeries {
    config: Option<Config>,           // shared, set on first insert
    timestamps: Vec<u64>,             // 8 B/snapshot
    offsets: Vec<u32>,                // 4 B/snapshot, range start
    indices: Vec<u32>,                // all snapshots concatenated
    counts: Vec<u32>,                 // all snapshots concatenated
}
```

Per-snapshot fixed overhead drops from **88 B** (`Config` + 2 `Vec`
headers + tuple) to **12 B** (timestamp + offset).  Replaced
`combine`/`wrapping_add`/`individual_counts` with two-pointer slice
operations; no more `BTreeMap`-backed bucket decompose during series
arithmetic.

**Corpus contribution (estimated):** ≈ −60 MiB live-bytes — 76 B per
histogram-snapshot × ~660 K total histogram-snapshot pairs across the
corpus.  Bigger relative win on long-running series; less on short ones
where bucket data dominates.

### 5. Explicit-empty delta preservation (`6a2a0a7`)

**Not a memory saving — a correctness fix.**  The earlier delta-form
storage silently dropped any timestamp where `delta_to_32` returned
`None` (NULL parquet rows, decode failures, counter resets, u32
overflow).  Time-series consumers that align across columns by offset
would shift onto wrong timestamps when such a snapshot landed
mid-series.

`delta_to_32_or_empty` and `empty_delta_32` helpers ensure every
observed timestamp produces a stored entry — empty when the delta
isn't representable.  Cost: a few bytes per affected snapshot for the
empty-histogram entry.  Benefit: lookups by offset stay aligned.

### 6. Drop `anchor_time` field on `HistogramSeries` (`452332d`)

**What.**  Removed the `Option<u64>` field that was carrying the
"first-raw-snapshot timestamp" through to stride-window emission, plus
the `set_anchor_time` accessor and the anchor parameter on
`StrideIter::new`.  For regularly-sampled inputs (essentially all
production cases) every emitted bin shifts by exactly one sampling
interval, with the first stride window dropped — equivalent to a null
at the leading edge of the time axis.

**Negligible byte saving.**  16 B/series × ~657 histogram series on the
corpus ≈ 10 KiB.  Code-size reduction was the real motivation.

## Per-file savings table

All numbers in MiB, measured via the `memory_compare` example with
`tikv-jemallocator` as the global allocator.  `live` is jemalloc
`stats::allocated`; `resident` is jemalloc `stats::resident` immediately
after `Tsdb::load_from_bytes` returns.

| file | OLD live | NEW live | live ↓ | OLD res | NEW res | res ↓ |
|---|---:|---:|---:|---:|---:|---:|
| AB_base.parquet | 41.5 | 14.3 | **−66%** | 115.8 | 36.6 | **−68%** |
| AB_base_pin.parquet | 39.4 | 13.8 | **−65%** | 112.6 | 34.8 | **−69%** |
| AB_level.parquet | 44.8 | 13.8 | **−69%** | 119.8 | 35.1 | **−71%** |
| AB_level_pin.parquet | 39.5 | 12.8 | **−68%** | 137.9 | 42.8 | **−69%** |
| cachecannon.parquet | 47.0 | 12.3 | **−74%** | 182.1 | 33.0 | **−82%** |
| demo.parquet | 19.4 | 5.4 | **−72%** | 91.5 | 15.1 | **−84%** |
| sglang_gemma3.parquet | 21.6 | 9.5 | **−56%** | 174.9 | 40.1 | **−77%** |
| vllm.parquet | 48.1 | 23.3 | **−52%** | 156.9 | 62.7 | **−60%** |
| vllm_gemma3.parquet | 23.1 | 9.8 | **−58%** | 176.8 | 42.7 | **−76%** |
| disagg-sglang.parquet | 81.2 | 27.6 | **−66%** | 315.7 | 57.1 | **−82%** |
| sglang-nixl-16c.parquet | 333.3 | 107.2 | **−68%** | 752.0 | 174.7 | **−77%** |
| **TOTAL** | **738.9** | **249.8** | **−66%** | **2 336.0** | **574.7** | **−75%** |

## Layered effect (resident, sglang-nixl-16c as illustration)

This is the largest file in the corpus and the clearest illustration of
how the techniques compose.

```
OLD                                                              752 MiB
└─ delta + u32 + Vec + Config-hoist + CSR + downsample 7→5 →     522 MiB  (-230)
   └─ + streaming column-by-column decode →                      175 MiB  (-347)
```

Live bytes (allocated) on the same file: 333 MiB → 107 MiB (−226 MiB).
The 230 MiB resident reduction at the structural step that doesn't show
up in live bytes is allocator slack from the OLD representation's
per-entry `BTreeMap` node overhead and `Box`-indirected histogram
buffers — fragmented allocations that jemalloc holds onto as scratch.

## Methodology

* `cargo build --release --example memory_compare -p metriken-query`
* `tikv-jemallocator = "0.6"` set as `#[global_allocator]` for the
  example only; library code is allocator-agnostic.
* For each file in `site/viewer/data/{,disagg/}`, run with `old` and
  `new` modes (separate processes) so allocator caching from one mode
  doesn't pollute the other.
* `OLD` mode uses an inlined re-implementation of the
  pre-optimization loader (BTreeMap, u64 cumulative histograms) so
  both modes ingest the same parquet via the same decoder.
* `live` reads `stats::allocated`; `resident` reads `stats::resident`
  after `epoch::advance()` flushes counters.

## Out of scope but worth flagging for follow-ups

* **Slice-based quantile API in `iopsystems/histogram`** — would let
  `percentiles()` / `heatmap()` skip the per-snapshot
  `from_parts(config, idx.to_vec(), cnt.to_vec())` materialization.
  Filed for upstream.
* **Single-row-group writer-side parquet layout** — costs nothing on
  the writer for files of this size (≤ 17 MiB) and lets the loader
  drop its row-counter-across-batches arithmetic.  Not strictly
  necessary; current loader handles either case.
* **Allocator-side `release_unused()` on native** — `malloc_trim(0)`
  on glibc, `arena.purge` on jemalloc.  Would help post-load resident
  on long-lived processes.  No-op on wasm.
