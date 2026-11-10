# Histogram overflow: observations above the highest finite bucket

**Status:** OPEN. Design 2026-11-09, owner decisions the same day (below).
Built on branch `feat/histogram-overflow` (see "As built"); not merged, not
released.
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
   scrape has a `+Inf` bucket, a float counter named
   `<histogram name>_overflow` with the histogram's labels, valued
   `+Inf count − the cumulative count at the highest finite le`, clamped
   at 0. It is a float counter because
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
   (the parquet reader, which the segmented reader uses per segment, and
   `Memory`) attach each row's cumulative overflow to the `HistogramRow`,
   and merge and rebucket carry it unchanged. A decrease is a reset and
   contributes 0, as `delta_walk` does per bucket. This needs a breaking
   metriken-storage change: `HistogramRow` is a public struct with public
   fields, and gains one. (The design first named `HistogramStreamMeta`
   too; as built, the bound travels on each row and the meta is unchanged.)
   The work is therefore recorder, storage and query, not recorder and
   query. That is forced by the re-stamping, not chosen.
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
   one low quantile (D4). An older metriken-storage (0.8.0 and before)
   also shows the histogram's two new keys as labels (D1).

## Decisions (owner, 2026-11-09)

All four as recommended.

- **D1. Storage keys `overflow` and `overflow_bound`** in `STORAGE_KEYS`,
  with names index `MNI2`. The alternative, internal labels
  `__overflow__` and `__overflow_bound__`, was rejected: they would be
  series identity, so the same histogram before and after the change, or
  after a change of its top bound, would be two series in every reader.
  `MNI2` is defined so that #379's scale-factor key joins it if it is added
  before the metriken-storage release that first writes `MNI2` (0.9.0);
  a key added after that release needs `MNI3`. Cost, stated: a reader on
  metriken-storage 0.8.0 or earlier shows the two keys as labels on the
  histogram.
- **D2. Non-finite values in a query result's JSON are the strings
  `"+Inf"`, `"-Inf"`, `"NaN"`**, as the Prometheus HTTP API writes them,
  and either form is read. This covers `Sample::value` and `interval`,
  `MatrixSample::values`, `intervals` and `bands`, and a scalar result.
  Finite values encode as before. A NaN result, `null` before, is now
  `"NaN"`. Rejected: keeping the derived encoding (`null`, which
  `MatrixSample`'s `Deserialize` refuses), and reporting the top bound as
  the value (the reading the issue rejected, and the band still non-finite).
  The rezolus frontend must handle a string in a value slot (follow-up
  there).
- **D3. Quantile functions only.** `histogram_count`, `histogram_sum`,
  `histogram_mean`, `histogram_irate` and the heatmap read the finite
  buckets as before, so `histogram_count` still differs from `_count` by
  the overflow. Follow-up: count the overflow in `histogram_count` (exact,
  no band), and decide what `histogram_sum` and `histogram_mean` do with an
  unbounded part.
- **D4. No segment format bump.** An older reader returns the old, low
  quantile without an error (settled item 9). Versions: metriken-recorder
  0.10.0, metriken-storage 0.9.0, metriken-query 0.46.0, all breaking. The
  CHANGELOG says that older readers show one extra counter per histogram
  and return the old quantile.

## As built

- **Recorder** (`metriken-recorder/src/prometheus.rs`): `convert_histogram`
  returns the overflow and writes the two keys; `convert` writes the
  overflow counters after every sample is read, so a scrape's own sample
  of the same name and labels, of any scalar kind, is known first. On such
  a collision the histogram loses both keys and the converter logs once at
  `warn`, then at `debug`. The highest finite bucket is the one with the
  highest bound, whatever the line order.
- **Storage**: `OVERFLOW_KEY` and `OVERFLOW_BOUND_KEY` (`labels.rs`) are in
  `STORAGE_KEYS`, and in the writer's list of keys that describe a column
  rather than an occupant (`writer.rs`). `HistogramRow` gains
  `overflow: Option<Overflow>` (`histogram_stream.rs`); merge, re-stamping
  and rebucketing move rows whole, so they carry it unchanged. The parquet
  reader (`ParquetHistogramCursor`) reads each counting histogram's
  overflow counter, the counter column of the named metric with the same
  labels (so, in a long segment, the same occupant), from the same row
  group pages, and attaches its value to the histogram row of the same row.
  `Memory` (the live path) records the link from the ingested metadata
  (`set_histogram_overflow`) and attaches the counter's sample at the
  histogram row's timestamp. The names index is written as `MNI2`; an
  `MNI1` index is still read, and its columns frame's label sets lose any
  key that is a storage key now, which is what a read of the segment's
  footer gives.
- **Query**: a series' overflow delta between rows is
  `round(curr − prev)`, 0 when either row has none or the count went down
  (`overflow_delta`). It is pooled per tick, or shared among stride
  windows in proportion to overlap as the bucket deltas are
  (`Splitter::row_with_overflow`). A window's band uses the lowest bound
  among the series whose overflow it holds. The process-group combine
  (`per_source.rs`, `Combined`) sums the overflow deltas of the pairs it
  takes as it sums bucket deltas. `apply_quantiles_with_overflow` applies
  the rank rule (settled item 4). JSON: `promql/json_f64.rs`.

## Order of work (done)

1. metriken-recorder: the overflow series, the column keys, collision
   handling.
2. metriken-storage: the keys, `MNI2`, the per-row overflow from the
   parquet reader and `Memory`.
3. metriken-query: the rank rule and the open band; the process-group
   combine; the JSON encoding.
4. Tests through exposition text → `PrometheusConverter` →
   `ArchiveWriter` → archive → PromQL
   (`metriken-recorder/tests/histogram_overflow.rs`): the issue's scrape; a
   stride window; two series pooled; an older recording alone, pooled with
   a newer one, and as the earlier half of one process; the name
   collision; the live `MemoryStore`; the JSON of a `+Inf` result.
