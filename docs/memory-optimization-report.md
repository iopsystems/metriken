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
| **live** | 738.9 MiB | 262.1 MiB | **−477 MiB (−65%)** |
| **resident** | 2 342.1 MiB | 570.7 MiB | **−1 771 MiB (−76%)** |

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

### 2. Per-period delta histograms + u32 narrowing

Two coupled changes on the histogram representation that landed in
commit `b361709`.  Both target the same dimension — per-snapshot
histogram bucket data — but have very different magnitudes.

#### 2a. Delta storage (vs cumulative-since-start)

Instead of storing each snapshot's cumulative histogram, the loader
differences consecutive snapshots and stores the per-period *delta*.
A delta carries only the buckets that fired during one sampling
interval; cumulative form carries every bucket that has ever fired
since process start, growing monotonically.

For a series sampled at 1 Hz over an hour:

* Cumulative: by the end, every bucket the metric has ever observed
  is non-zero.  Storage grows like the *union* of all per-period
  bucket sets — ~50–200 non-zero buckets per snapshot for typical
  latency distributions.
* Delta: each snapshot has only the buckets that fired in the last
  second — typically 5–15 non-zero buckets, regardless of how long
  the process has been running.

**Best estimate of corpus contribution: −180 to −220 MiB live.**  The
dominant single contributor among the structural changes.

#### 2b. `u32` counts (vs `u64`)

`CumulativeROHistogram32` from histogram 1.3 narrows the count vector
from `Vec<u64>` to `Vec<u32>`.  Per-period totals comfortably fit
(2³² events ≈ 4.3 G; even per-second deltas on a 1 GHz syscall
counter clear that by 7×), so this is a clean halving of the count
data.

Index vectors stay u32 in both representations, so this only narrows
the count side.  Per-snapshot cost drops from `4·N (idx) + 8·N
(cnt) = 12N` bytes of bucket data to `4·N + 4·N = 8N` — a 33%
reduction on bucket-data only, multiplied across all stored
snapshots.

**Best estimate of corpus contribution: −60 to −80 MiB live.**

#### 2c. Auto-downsample `grouping_power 7 → 5` — *measured, then reverted*

The hypothesis was that high-precision histograms (gp=7, the Rezolus
default, ≈0.78% relative error) could be cheaply collapsed to gp=5
(≈3.13% relative error) via `Histogram::downsample`, dropping
non-zero bucket count by up to 4× on dense workloads.

We measured this directly by running the example with
`TARGET_GROUPING_POWER` set to both 5 and 7 across the full corpus.

Auto-downsample contribution to NEW live, by file:

| file | with downsample (5) | without (7) | DS saves | DS share |
|---|---:|---:|---:|---:|
| AB_base | 14.3 | 17.1 | −2.8 | 16% |
| AB_base_pin | 13.8 | 16.0 | −2.2 | 14% |
| AB_level | 13.8 | 15.9 | −2.1 | 13% |
| AB_level_pin | 12.8 | 13.9 | −1.1 | 8% |
| cachecannon | 12.3 | 15.0 | −2.7 | 18% |
| demo | 5.4 | 5.4 | 0 | 0% |
| sglang_gemma3 | 9.5 | 9.9 | −0.4 | 4% |
| vllm | 23.3 | 23.3 | 0 | 0% |
| vllm_gemma3 | 9.8 | 10.2 | −0.4 | 4% |
| disagg-sglang | 27.6 | 27.8 | −0.2 | 1% |
| sglang-nixl-16c | 107.2 | 107.5 | −0.3 | 0.3% |
| **TOTAL** | **249.8** | **262.1** | **−12.3** | **−4.7%** |

**Conclusion: not worth the precision cost.**  Auto-downsample buys
~12 MiB of live bytes (4.7%) at the cost of a permanent 4×
relative-error inflation on every quantile query.  The hypothesis
that delta-form histograms would still benefit substantially from
bucket collapsing was wrong: deltas are already sparse (5–15 non-zero
buckets per snapshot), so collapsing 4 adjacent buckets into 1
mostly merges already-zero buckets and saves nothing.  Counter and
gauge series dominate the live-byte budget on most files anyway.

**Removed in a follow-up commit.**  Original commit was `2716405`.
This branch keeps the source `grouping_power` from the parquet
column metadata as-is.  Programmatic consumers (SLO checks,
alerting) keep their tight quantile bounds; viewer rendering is
unaffected (renders below pixel granularity either way).

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
| AB_base.parquet | 41.5 | 17.1 | **−59%** | 115.8 | 36.2 | **−69%** |
| AB_base_pin.parquet | 39.4 | 16.0 | **−59%** | 121.6 | 40.2 | **−67%** |
| AB_level.parquet | 44.8 | 15.9 | **−65%** | 119.8 | 38.1 | **−68%** |
| AB_level_pin.parquet | 39.5 | 13.9 | **−65%** | 121.4 | 34.1 | **−72%** |
| cachecannon.parquet | 47.0 | 15.0 | **−68%** | 182.6 | 32.4 | **−82%** |
| demo.parquet | 19.4 | 5.4 | **−72%** | 91.5 | 15.1 | **−84%** |
| sglang_gemma3.parquet | 21.6 | 9.9 | **−54%** | 176.7 | 44.3 | **−75%** |
| vllm.parquet | 48.1 | 23.3 | **−52%** | 156.9 | 61.0 | **−61%** |
| vllm_gemma3.parquet | 23.1 | 10.2 | **−56%** | 176.8 | 39.5 | **−78%** |
| disagg-sglang.parquet | 81.2 | 27.8 | **−66%** | 314.1 | 57.9 | **−82%** |
| sglang-nixl-16c.parquet | 333.3 | 107.5 | **−68%** | 764.9 | 171.7 | **−78%** |
| **TOTAL** | **738.9** | **262.1** | **−65%** | **2 342.1** | **570.7** | **−76%** |

## Layered effect (resident, sglang-nixl-16c as illustration)

This is the largest file in the corpus and the clearest illustration of
how the techniques compose.

```
OLD                                                              765 MiB
└─ delta + u32 + Vec + Config-hoist + CSR →                      522 MiB  (-243)
   └─ + streaming column-by-column decode →                      172 MiB  (-350)
```

Live bytes (allocated) on the same file: 333 MiB → 108 MiB (−225 MiB).
The ~243 MiB resident reduction at the structural step that doesn't show
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

## Already-landed slice-based quantile path

`histogram = "1.3.1"` ships `CumulativeROHistogram32Ref<'a>` with
`from_parts_unchecked(config, &[u32], &[u32])`, `quantiles`, `iter`,
`iter_with_quantiles`, and a `SampleQuantiles` impl.
`HistogramSeries::iter()` returns
`(u64, CumulativeROHistogram32Ref<'_>)` borrowing into the series'
flat buffers, and `percentiles()` / `heatmap()` dispatch through a
`DeltaSnapshot::{Borrowed, Owned}` enum so the no-stride query path
is zero-alloc.  Stride windows still materialize once per emitted
bin (the accumulator sums across snapshots into a fresh buffer —
nothing borrowable).

## Out of scope but worth flagging for follow-ups

* **Single-row-group writer-side parquet layout** — costs nothing on
  the writer for files of this size (≤ 17 MiB) and lets the loader
  drop its row-counter-across-batches arithmetic.  Not strictly
  necessary; current loader handles either case.
* **Allocator-side `release_unused()` on native** — `malloc_trim(0)`
  on glibc, `arena.purge` on jemalloc.  Would help post-load resident
  on long-lived processes.  No-op on wasm.
