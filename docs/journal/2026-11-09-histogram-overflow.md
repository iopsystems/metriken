# Histogram overflow: observations above the highest finite bucket

**Status:** OPEN. Design 2026-11-09. Four decisions below need the owner
(D1–D4); nothing is built.
Issue #377.

## Goal

A Prometheus histogram's observations above its highest finite `le` are
counted only in the `+Inf` bucket. Today the converter drops that bucket,
so those observations are absent from the stored histogram and a high
quantile reads low with nothing in the result saying so. After this change
the recording keeps the overflow, and `histogram_quantile` counts it in
the population: a quantile that falls in it reads `+Inf`, with the band
`[top finite bound, +Inf)`.

## Today

- `convert_histogram` (`metriken-recorder/src/prometheus.rs`) keeps
  buckets with `b.less_than.is_finite() && b.count.is_finite()`, scales
  each `le` (×1e9 for a `_seconds` name, else `compute_generic_scale`),
  and adds each bucket's delta at the scaled `le` into a
  `histogram::Histogram` with `grouping_power` 7. The column's metadata is
  `metric`, the exporter's labels, `grouping_power`, `max_value_power`.
  The histogram's `_sum` and `_count` are separate untyped samples, stored
  as float counters since #384. The exposition parser
  (`metriken-recorder/src/exposition.rs`) does keep the `+Inf` bucket as a
  `HistogramCount` with `less_than: f64::INFINITY`.
- Column metadata becomes a series' labels by removing the keys in
  `STORAGE_KEYS` (`metriken-storage/src/labels.rs`: `metric`,
  `metric_type`, `unit`, `grouping_power`, `max_value_power`), in both
  loaders (`Labels::from_metadata`, used by `parquet.rs` and `memory.rs`).
  Every other key is a label. A key added to `STORAGE_KEYS` changes which
  footer keys become labels, which needs a new names index prefix (`MNI2`,
  `metriken-storage/src/names.rs`), since a reader takes a stored index
  over the footer it was built from.
- Nothing but `Labels` and `histogram::Config` reaches the query from a
  histogram column: `HistogramStreamMeta { config, series: Vec<Labels> }`,
  `HistogramRow { series_idx, timestamp, snapshot, resumes }`
  (`metriken-storage/src/histogram_stream.rs`).
- The PromQL surface over a counting histogram is
  `histogram_quantile(q, sel)` (`build_histogram_quantile`,
  `metriken-query/src/promql/streaming/dispatch.rs`), whose second argument
  must be a vector selector, and `histogram_quantiles([qs], sel[, stride])`
  (`handle_histogram_quantiles`, `metriken-query/src/promql/mod.rs`). Both
  call `quantiles_impl` (`streaming/histogram.rs`), which pools every
  series the selector matches into one population, per row-to-row delta
  (no stride) or per stride window. There is no `rate(hist[w])` and no
  `sum by` over a histogram feeding `histogram_quantile`: the "rate" is
  the delta between a series' consecutive rows, or the sum of those
  deltas in a stride window, and the "aggregation" is the pooling of every
  matched series. `delta_walk` clamps a bucket that went down to zero.
- `apply_quantiles` reports a quantile as `bucket.end()` with the band
  `[bucket.start(), bucket.end()]`. The histogram crate (1.6.5) uses
  nearest rank: target `max(1, ceil(q·N))`.
- Two recordings of one histogram are combined by
  `HistogramStream::merge_on_clock` (`recordings.rs`), which re-stamps the
  rows of all but the slowest stream onto the slowest stream's row times.
- `MatrixSample` (`metriken-query/src/promql/mod.rs`) derives `Serialize`
  and `Deserialize`, with values `(f64, f64)` and bands `(f64, f64)`.
  serde_json writes a non-finite f64 as `null` and refuses `null` for an
  `f64` (checked with serde_json 1.x: `(1e10, f64::INFINITY)` writes
  `[10000000000.0,null]`, and reading that back as `(f64, f64)` fails
  with "invalid type: null, expected f64").

## Settled from the code

1. **The overflow series.** The converter writes, for each histogram whose
   scrape has a `+Inf` bucket, a float counter named `<histogram name>_overflow`
   with the histogram's labels, valued `+Inf count − the cumulative count
   at the highest finite le`, clamped at 0. It is a float counter because
   bucket counts are f64 in exposition and every Prometheus scalar is a
   float counter or gauge since #384. It is written every scrape,
   including when it is 0, so a rate over it has rows at the histogram's
   rows. A histogram with no `+Inf` bucket (invalid exposition) gets no
   overflow series and no link, and reads as today.
2. **Name collision.** If the scrape also has a sample named
   `<histogram>_overflow` with the same labels, the converter writes no
   overflow series and no link for that histogram (it reads as today) and
   logs once per converter. The name is found through the link, not by
   convention (D1), so it can change later without a reader change.
3. **The top bound** is the highest finite `le` multiplied by the same
   scale as the buckets, written as an f64 (not truncated to u64), so it
   is in the units the quantile values are in. For a `_seconds` histogram
   with top `le` 10 it is `10000000000`.
4. **The quantile.** Over a window's pooled finite buckets (total `F`) and
   pooled overflow delta (`O`, rounded to an integer), the target rank is
   `max(1, ceil(q·(F+O)))`, the crate's rule over the larger population.
   A target above `F` reads `+Inf` with band `(top, +Inf)`; otherwise the
   quantile is the finite bucket holding that rank, reported as today. The
   rank shift means quantiles below the overflow can move too (more
   population, same buckets); for the issue's test (F 9, O 1)
   `q=0.5` targets rank 5 either way and reads the `le=1` bucket, and
   `q=0.95` targets rank 10 > 9 and reads `+Inf`. When `O` is 0 every
   result is as today.
5. **The overflow is differenced on the histogram's rows.** It must be
   taken over exactly the rows and windows the buckets are, including the
   re-stamping `merge_on_clock` does for a second recording. A query-side
   join of the histogram stream with a separately read counter would
   difference the overflow over different intervals once
   `merge_on_clock` has moved the histogram rows. So the leaf readers
   (parquet, segmented, memory) attach each row's cumulative overflow to
   the `HistogramRow`, and merge and rebucket carry it unchanged. A
   decrease is a reset and contributes 0, as `delta_walk` does per bucket.
   This needs metriken-storage changes, and they are breaking
   (`HistogramRow` and `HistogramStreamMeta` are public structs with public
   fields; `HistogramRow {` appears, as a literal or a pattern, at 15 sites in the workspace). The work is
   therefore recorder, storage and query, not recorder and query. That is
   forced by the re-stamping, not chosen.
6. **Per-series, so per-process and multi-source.** Each histogram series
   has its own overflow series with the same labels, so a selector that
   matches several processes or sources pools each one's overflow with
   that series' buckets.
7. **Absent overflow.** A recording made before this change, or a series
   whose link is missing, contributes `O = 0` and reads exactly as today.
   A query that pools a series with a link and one without (two
   recordings, one older) counts the overflow only of the first.
8. **Only the quantile functions change** (`histogram_quantile`,
   `histogram_quantiles` with or without a stride). See D3 for the others.
9. **Readers that do not know the overflow.** rezolus and any other
   reader list `<histogram>_overflow` as one more counter series (with
   `rate()` it reads observations per second above the top bound). An
   older metriken-query (0.45 and before) reading a new recording ignores
   the link and returns the old, low quantile without an error. This is
   accepted: the only way to refuse instead is a segment format bump,
   which would make an older reader refuse the whole recording to avoid
   one low quantile (D5 below, recommended against). What else an older
   reader shows depends on D1.

## Open decisions (owner)

### D1. Which column metadata keys carry the link and the bound

The histogram column needs two values: the overflow series' metric name
and the top bound. Two ways fit the code:

- **(a) Storage keys `overflow` and `overflow_bound`**, added to
  `STORAGE_KEYS`. This is the existing convention for a value that
  describes the column (`grouping_power`, `max_value_power`). Costs: a
  names index `MNI2` (`names.rs`); the key values must be plumbed from
  column metadata into `HistogramStreamMeta` (per series), since today
  only `Labels` reach it. User-visible: a reader on metriken-storage 0.8
  or earlier (rezolus before its update, metriken-query 0.45) shows
  `overflow="<name>_overflow"` and `overflow_bound="10000000000"` as two
  labels on the histogram, so in such a reader the same histogram
  recorded before and after the change is two series with different
  label sets.
- **(b) Internal labels `__overflow__` and `__overflow_bound__`.** No
  storage key, no index bump; they reach the query in `Labels` already,
  and a consumer that follows `is_internal_label` hides them from listings and legends (rezolus does, per #379). Costs: they are
  series identity (`labels.rs`, `is_internal_label`), so the same
  histogram before and after the change is two series in every reader,
  new ones included, and a change of the top bound alone splits the
  series. It contradicts `labels.rs`'s "No writer in this workspace emits
  a `__` key into column metadata" (#379 plans `__prometheus_name__`, so
  that sentence changes either way).

Recommendation: (a). The bound and the link describe the column, which is
what storage keys are for, and new readers then keep one series across
the change. #379 also adds a column key (the generic scale factor), so the
`MNI2` bump can be shared if the two land in one storage release.

### D2. How `+Inf` reaches a JSON consumer

The issue asks for value `+Inf` and band `[top, +Inf)`. With the derived
`Serialize`, both become `null` in JSON: the value reads as a gap in a
chart, the band as `[top, null]`, and `MatrixSample`'s own `Deserialize`
refuses the whole result ("invalid type: null, expected f64"). Options:

- **(a) Keep the derived encoding** and accept `null`. A consumer that
  deserializes `MatrixSample` breaks on any result with an overflow
  quantile.
- **(b) Write non-finite values as the strings `"+Inf"`, `"-Inf"`,
  `"NaN"`** (Prometheus's HTTP API spelling) in `values`, `intervals` and
  `bands`, and read both numbers and those strings. Finite values encode
  as today. This also changes what a NaN result (a binary op's
  `unwrap_or(f64::NAN)` in `dispatch.rs`) encodes to, which is `null`
  today. The rezolus frontend must handle a string in a numeric slot.
- **(c) Report the top bound as the value** with band `(top, +Inf)`, so
  only the band is non-finite. Still `null` in the band; and the value
  then claims the observation finished at the bound, the reading the
  issue rejected.

Recommendation: (b), with the rezolus frontend change listed as a
follow-up there.

### D3. Whether the other histogram functions count the overflow

`histogram_count`, `histogram_sum`, `histogram_mean`, `histogram_irate`
and the heatmap read the same buckets. Options:

- **(a) Quantiles only** (the issue's scope). The others exclude the
  overflow, documented; `histogram_count` still differs from `_count` by
  the overflow.
- **(b) Also `histogram_count`**, which is exact (no band). Then
  `histogram_sum` and `histogram_mean`, which `reduce` computes from the
  same pooled buckets, either stay finite-only (and disagree with count)
  or become unbounded above (band upper end `+Inf`).

Recommendation: (a) here, with (b) as a separate issue, since it changes
three functions' results and their bands.

### D4. Versions

With the recommendations for D1–D3: metriken-recorder 0.10.0 (a new series per histogram
is a change to what every recording holds, and a breaking change to any
reader that enumerates a scrape's counters), metriken-storage 0.9.0
(`HistogramRow`/`HistogramStreamMeta` fields, `STORAGE_KEYS` contents),
metriken-query 0.46.0 (requires storage 0.9.0, and `histogram_quantile`
results change). D2 (b) is a further breaking change to query's JSON.

### D5. (Recommended against) a segment format bump

A format bump would make metriken-storage 0.8 refuse a segment holding an
overflow link, so an older reader could not return a low quantile. It
would also make that reader refuse every other column in the recording.
Not recommended; settled item 9 states the silent old answer instead.

## Order of work (after D1–D4)

1. metriken-recorder: the overflow series, the column keys, collision
   handling.
2. metriken-storage: the keys (if D1 (a)), the per-series overflow on
   `HistogramRow` from the parquet, segmented and memory readers, carried
   through merge, `merge_on_clock` and rebucket.
3. metriken-query: the rank rule and the open band in `apply_quantiles`'
   callers; JSON encoding per D2.
4. Tests through exposition text → `PrometheusConverter` → `ArchiveWriter`
   → archive → PromQL: the issue's scrape; a stride window; two processes
   pooled; two recordings, one without the overflow; an older recording
   alone.
