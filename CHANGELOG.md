# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### metriken-query 0.29.0

- **Changed:** the streaming producers (`CounterGridRate`,
  `CounterPairwiseRate`, `GaugeStepGrid`, `GaugeAvgOverTime`, `GaugeIdelta`,
  `GaugeDeriv`) own their samples instead of borrowing them, and the
  dispatcher hands each series its producer as the series' iterator rather
  than collecting the producer's points into a `Vec` first. An aggregate
  over many series now holds one buffered point per series while it runs.
  Measured on a ten-hour archive's 6,644-task table, `sum(rate())` over
  every series: the collected points were most of a 10 GB query.

### metriken-query 0.28.0

- **Added:** `ColumnRelabel`, identity that varies with time, and
  `SegmentedParquetReader::open_relabeled_with_pool` to open a table with
  one. At open a column contributes every label set it can present as; at
  query time its samples are cut into runs by occupant (`split`), histogram
  rows relabelled by timestamp (`at`), and a filter on a key the relabelling
  supplies is turned into one the columns can answer (`segment_filter`) and
  applied to the relabelled runs afterwards. For a reader whose archive says
  who held each slot and when, without decoding the table to split it.
  `Labels` is public, since the hook takes it.

### metriken-query 0.27.0

- **Added:** `SegmentedParquetReader::open_with_pool` takes a `SegmentStore`,
  a trait that supplies a table's segment bytes on demand
  (`InMemorySegments` is the store over bytes already in hand, and what
  `open_bytes_with_pool` wraps). Open reads each segment's footer once to
  build the identity indexes, a per-segment time span and the column map,
  and keeps neither the bytes nor the parsed footer; a query fetches only
  the segments whose span it touches, through a cache bounded by the pool's
  byte budget. A segment the store no longer has is skipped. Measured on a
  1.3 GB, ten-hour archive whose task table had 159 segments of up to 2,851
  columns: building the dashboard held 4.1 GB resident, of which the
  segment bytes were 1.27 GB and parsed footers most of the rest.
- **Fixed:** a `MultiParquetSource` reached through `dyn DataSource` reported
  no sample timestamps; the inherent method had them and the trait method
  returned its empty default.

### metriken-query 0.26.0

- **Added:** `MemoryStore` takes whole series: `insert_counter_series`,
  `insert_gauge_series` and `insert_histogram_series` add a series with its
  timestamps, values and — for counters and gauges — the per-sample
  acquisition windows that `rate()`/`irate()` turn into uncertainty bounds,
  which the ingest path could not carry. `set_sample_timestamps` declares
  the rows a store assembled from a table stands for; without it the store
  reports the union of its series' timestamps. `UnionChild` composes a
  `MemoryStore` beside parquet readers, `HistogramSnapshot` is public, and
  `Labels` converts from a `BTreeMap<String, String>`. For a reader that
  builds series itself — one that splits a table's columns by occupant
  through an identity index — and hands them to the engine.
- **Fixed:** a `MemoryStore` counter or gauge series that carried windows had
  them dropped on read; they are sliced with the samples now.

### metriken-core 0.3.2

- **Added:** `metadata_version()` on `CounterGroupMetric`, `GaugeGroupMetric`
  and `HistogramGroupMetric`: a value that changes whenever any entry's
  metadata is set, added to or removed, and never otherwise. It lets a
  reader that caches something derived from the metadata (a schema, a hash
  of every entry's labels) decide in O(1) whether the cache is current,
  instead of re-reading every entry every tick. Defaulted to a hash of the
  metadata snapshot — correct for any implementor, but a full read per call,
  so a type that can count its own mutations should override it. The reader
  must take the version before reading the metadata it validates; the doc
  says why.

### metriken 0.11.1

- **Added:** `metadata_version()` on `CounterGroup`, `GaugeGroup`,
  `HistogramGroup` and `ShardedCounterGroup`, backed by a counter in the
  shared metadata store that every mutation path bumps under the write lock.
  Overrides the `metriken-core` default with an O(1) load.

### metriken-query 0.25.0

- **Added:** `is_internal_label(name)`, `is_storage_key(key)` and
  `STORAGE_KEYS` are public. A label whose name begins with `__` is internal,
  following Prometheus: part of a series' identity and matchable in a
  selector, and dropped by `without` and by default binary-op matching
  alongside `__name__`. This crate emits internal labels on every result;
  hiding them from listings and legends is the consumer's contract, which is
  why the predicate is public. The engine's own internal labels are
  `__name__` and `__run__`; a reader above this crate may add its own under
  the same rule. Consumers that check for `__name__` by name should use the
  predicate.
- **Changed (breaking):** `without (...)` and `ignoring (...)` drop every
  label whose name begins with `__`; `__name__` was the only one dropped
  before. Default binary-op matching ignores the same set. The observable
  difference today is on a histogram whose bucket configuration changed
  mid-recording: `histogram_mean without (cpu) (latency{__run__="1"})`
  returned series labelled `{__name__, __run__="1"}` and now returns
  `{__name__}`. The values are the same, because a run is selected before
  any aggregation and runs are never merged; a consumer keying on the full
  result label set sees the change. It is what lets a reader split a series
  by incarnation without breaking `a / sum(a)`.
- **Fixed (breaking):** the `ingest` loader and the parquet loader derive
  labels from one function (`Labels::from_metadata`) and one storage-key
  list. They had drifted: the live path kept `grouping_power` and
  `max_value_power` as labels where the parquet path stripped them, so a
  recording viewed live carried two extra labels per histogram series that
  the same recording read from disk did not. Those two labels are gone from
  the live path. `STORAGE_KEYS` is pinned by a test, and each loader has a
  test that a histogram column carrying every storage key yields no label
  from them.

### metriken-query 0.24.0

- **Fixed (breaking):** the parquet read path no longer rounds timestamps to a
  nominal sampling grid. Every timestamp it decoded used to be replaced with
  `round(ts / sampling_interval_ms)`, which discarded the one thing the file
  states exactly — when each row was read — in favour of a value it merely
  declares. A file whose declaration was wrong, or absent (the reader assumed
  1000 ms), therefore lost data rather than being described oddly: at a 100 ms
  cadence all ten rows of a second landed on one instant and nine of the ten
  values were dropped, so a sub-second query returned held-forward copies of
  each second's survivor. The declared interval is still read, and is still the
  staleness hint, where being approximate costs nothing. `MemoryStore` likewise
  ingests a snapshot at the timestamp it carries.
- **Changed (breaking):** `MetricsSource::snapped_sample_timestamps` and
  `ParquetReader::snapped_sample_timestamps` are removed. They existed to
  expose what the rounding did to a caller deciding where a series has data;
  `sample_timestamps` is now that answer for every source. The internal
  `DataSource::counters`/`gauges` lost their `raw` parameter for the same
  reason — it selected between the two forms, and there is one. `RateMode::Raw`
  is unaffected: it still selects point placement in the streaming layer.

### metriken-core 0.3.1

- **Added:** `with_metadata`/`for_each_metadata` default methods on
  `CounterGroupMetric`, `GaugeGroupMetric`, and `HistogramGroupMetric` —
  object-safe metadata visiting reachable through `&dyn` (unlike the
  generic `with_metadata<R>` inherent methods below, a generic method can't
  live on a trait used as a trait object; these use `&mut dyn FnMut(..)`
  instead so they can). `with_metadata(idx, f)` visits one entry's metadata;
  `for_each_metadata(f)` visits every populated entry's metadata in one
  pass, in unspecified order. Both default to the allocating
  `load_metadata`/`metadata_snapshot` so any existing implementor keeps
  compiling; `metriken`'s `CounterGroup`/`GaugeGroup`/`HistogramGroup`
  override both to route through their internal metadata store without
  cloning. Implementations may hold an internal read lock for the callback's
  duration — callers must not block, await, or re-enter the group inside it.
  Additive: existing implementors keep compiling on the defaults.

### metriken 0.11.0

- **Changed (breaking):** an owned `CounterGroup` entry that has never been
  written now reads back as `None` rather than `Some(0)`, matching what
  `GaugeGroup` has always done with its `i64::MIN` sentinel. The value array is
  allocated whole on first touch, so previously writing *any* index made *every*
  index report an honest-looking zero — a sampler populating part of its group
  (one GPU of two, the CPUs it was allowed) published a phantom zero series for
  the rest, and a consumer could not tell those from real measurements. Owned
  backing is now filled with a `u64::MAX` sentinel, `add` replaces it rather
  than wrapping onto it (a compare-exchange loop, as `GaugeGroup::add` already
  uses), and `value`/`load_with_window` report it as absent. An honest measured
  zero is still `Some(0)`.
  **Externally-backed groups are deliberately unaffected**: that memory belongs
  to the caller and a BPF mmap is kernel zero-filled, so it cannot carry a
  sentinel — and a zero there is a real starting value, with membership derived
  from the map's registered entries rather than from value presence. `load()`
  still returns values raw, sentinel included, to stay index-aligned; use
  `value()` for a per-entry `Option`.
### metriken 0.10.1

- **Added:** `with_metadata` on `CounterGroup`, `GaugeGroup`, `HistogramGroup`,
  `WindowedCounterGroup`, and `WindowedGaugeGroup` — runs a closure against
  `Option<&HashMap<String, String>>` while holding the group's metadata read
  lock for the closure's duration, instead of cloning the whole per-index map
  the way `load_metadata` does. Intended for hot paths (e.g. per-member,
  per-tick identity checks) that only need to inspect metadata, not own a
  copy of it. Callers must not block, await, or re-enter the group's methods
  inside the closure. Additive — `load_metadata` is unchanged and still
  available. `CounterGroup`/`GaugeGroup`/`HistogramGroup` also override the
  new object-safe `with_metadata`/`for_each_metadata` from
  `metriken-core`'s `*GroupMetric` traits (see that entry) so allocation-free
  access works both from concrete callers and through `&dyn`.
  `WindowedCounterGroup`/`WindowedGaugeGroup` gain a matching inherent
  `for_each_metadata` that forwards to the inner group. Additive throughout —
  `load_metadata` and every existing signature are unchanged. Requires
  metriken-core 0.3.1 for the trait defaults it overrides.

### metriken-exposition 0.20.0

- **Changed (breaking):** requires `metriken` 0.11. No API change of its own —
  but `metriken` 0.11 changes `CounterGroup::value()` for unwritten entries, and
  a consumer cannot hold both 0.10 and 0.11 in one tree and still have the group
  types unify. Consumers move in lockstep.

### metriken-exposition 0.19.0

- **BREAKING:** `GroupSnapshot::schema` is now `Option<Arc<GroupSchema>>` (was
  `Option<GroupSchema>`), with serde's `rc` feature enabled so `Arc<T>`
  serializes exactly as a bare `T` — wire bytes are unchanged (proved by the
  `arc_schema_wire_compat` test, which compares the encoding byte-for-byte
  against a mirror struct with a bare `GroupSchema` field). Lets a producer
  that caches schemas by `(name, schema_hash)` hand out another reference to
  the same allocation on a cache hit (an `Arc` clone, i.e. a refcount bump)
  instead of deep-cloning every `MetricDesc` on every tick — measured 54% of
  V3-builder allocations at 2k members before this change.
  `GroupSnapshot::validate()` and `GroupSchema::hash()` are unaffected (the
  hash is computed over the schema's content, not its storage). The wire
  format is untouched, but the field's Rust type changes, so anything that
  constructs or destructures `GroupSnapshot::schema` must adapt — hence a
  breaking bump rather than a patch.

### metriken-exposition 0.18.0

- **Added:** `SnapshotV3` — the acquisition-group snapshot format. Metric
  readings are organized into `GroupSnapshot`s (e.g. `cpu_usage/percpu`) that
  share one acquisition `Window` per group instead of one per metric, with
  membership driven by producer registration rather than value sentinels.
  Group membership is described by a `GroupSchema` (`MetricDesc` entries for
  counters/gauges/histograms) that is content-hashed (`schema_hash`, FNV-1a-128
  as `(hi, lo)`) so receivers can cache parsed schemas across restarts instead
  of re-parsing every tick. `Snapshot::V1`/`Snapshot::V2` still decode exactly
  as before.
- **BREAKING:** `Snapshot` gained a new `V3` variant, which breaks any
  exhaustive `match` on `Snapshot` in downstream consumers. This is
  deliberate — `Snapshot` stays intentionally *not* `#[non_exhaustive]` so
  that a new wire version is a compile-time event for every consumer rather
  than something a wildcard arm could silently swallow (see the `Snapshot`
  rustdoc).
- **Added:** `GroupSnapshot::validate()` — checks the cross-field invariants
  a decoded `GroupSnapshot` cannot express on the wire (per-kind schema/value
  arity, and `schema_hash` agreement with the transmitted `GroupSchema`).
  Receivers that cache parsed schemas by `(name, schema_hash)` must call this
  before inserting into the cache. The `Snapshot::counters()`/`gauges()`/
  `histograms()` accessors now silently skip a group that fails these checks
  instead of asserting on it — a malformed but structurally valid V3 payload
  could previously trigger a `debug_assert_eq!` panic on decoded wire data in
  debug builds.
- **Added:** `Snapshot::from_msgpack()` — decodes with a nesting-depth cap
  and rejects trailing bytes, unlike a bare `rmp_serde::from_slice`, which
  silently ignores trailing bytes and has no depth limit.
- **Fixed:** histograms decoded through `Snapshot::histograms()` (all of
  V1/V2/V3) are now rebuilt through `histogram::Histogram::from_buckets`,
  the validating constructor, and dropped if that fails. Raw
  `Deserialize` on `histogram::Histogram` cannot enforce its invariants
  (e.g. `grouping_power < max_value_power`), so a malformed decoded
  histogram could previously panic downstream in `iter()`/`quantiles()`.
  For V3, the expanded histogram's `grouping_power`/`max_value_power`
  metadata is also overwritten from the canonicalized config, restoring
  the V2 invariant that the metadata copy cannot disagree with the
  embedded config.

### metriken-query 0.23.0

- **Changed (breaking):** `EnvPoint` is `#[non_exhaustive]`, constructed with
  `EnvPoint::new` plus `with_band`/`with_interpolated` (the shape `MatrixSample`
  already uses). Literal construction from another crate no longer compiles.
  Taken in the same release as the field addition below, since that addition is
  breaking only because the struct could be built by literal — paying it once
  makes the next field additive.
- **Changed (breaking):** display-mode decimation carries the interpolated
  flag and the per-point bands. `EnvPoint` gains `interpolated: bool`, and
  `Reducer::reduce` takes `(points, bands, interpolated, budget, band)` where it
  took `(points, intervals, budget, band)`. The added field breaks literal
  construction, which is what the entry above makes a one-time cost.

  The reducer read `MatrixSample::intervals`, the all-or-nothing band field,
  which goes absent for a whole series as soon as one point lacks a band — and a
  hole does exactly that. So a decimated series with one unobserved stretch
  showed NO uncertainty band anywhere, despite having one almost everywhere. It
  now reduces from `bands`, the lossless per-point form added in 0.22.

  A decimated bucket is interpolated if ANY of its samples was — it is only as
  observed as its least observed member, so this ORs rather than votes — and a
  bucket's band now skips the samples that have none rather than counting them
  as zero-width.

### metriken-query 0.22.0

- **Changed (breaking):** requires `metriken-exposition` 0.20 (see that entry).
- **Fixed (performance):** a parquet source parses its schema once instead of
  per lookup. `parse_schema` walks every field in the file and eight call sites
  re-ran it, so any "for each metric name" loop was O(names x columns) with
  nothing memoised — `MetricsSource::total_series_count` is exactly that loop
  and nothing overrides the default, so every consumer paid it. Measured on a
  950-column, 14,210-series recording: `total_series_count()` 542 ms -> 12.9 ms
  (5.6 ms warm), and a dashboard section render 598 ms -> 5.3 ms. The parse
  depends only on `meta`, which never changes after construction, so the cache
  needs no invalidation. Costs one `Vec<ColDesc>` per open source for its
  lifetime.

- **Fixed:** `rate`/`irate` no longer fabricate an acquisition window at a
  timestamp the producer never read. `interp_window` interpolates a window
  between the two bracketing samples, which is the right reading when a grid
  edge merely falls between adjacent reads — but across a HOLE (a counter that
  was null for a stretch: a device that appears at runtime, a partially
  populated group, a failed read) it invented a read that never happened, and
  the band then claimed a precision nobody measured. Such a point now carries
  its value and no band, flagged by the new `Point::interpolated`. The
  hole-spanning value itself is unchanged — the total across the hole is known
  even though its distribution inside it is not.
- **Added:** `MatrixSample::bands` — the per-value uncertainty band, present
  when ANY value has one, with `None` at the values that do not.
  `MatrixSample::intervals` is all-or-nothing by construction and cannot carry
  a partial set, so on a series with a hole it reports `None`; `bands` is the
  lossless view. Additive: `intervals` keeps its type and its documented
  behaviour, and `MatrixSample` is `#[non_exhaustive]` with builder
  construction, so nothing downstream needs to change to keep compiling.
- **Added:** `MatrixSample::interpolated` — which values span a stretch the
  producer did not read, parallel to `values`. This is what a renderer needs to
  distinguish an interpolated point from a measured one (a desaturated
  connector, a dashed segment); an uncertainty band cannot express it, because
  the honest bound on an unobserved interval is not a number. Propagates
  through scalar ops, aggregation, and series-op-series the way bands do: any
  operand being interpolated makes the result interpolated.

### metriken-query 0.18.0

- **Changed:** built against `metriken-exposition` 0.18.0. `Snapshot::V3`
  payloads flow through the ingest path via the exposition accessors
  (expansion), so live-agent ingest of a V3 producer works without engine
  changes. No API changes in metriken-query itself.
- **Added:** table-level acquisition-window columns. A bare
  `:window_begin`/`:window_width` pair (no metric prefix) is read as one
  acquisition window shared by every metric in the table. Resolved as an
  atomic pair — never a begin from one source mixed with a width from
  another: a metric's own `<m>:window_begin`/`<m>:window_width` sidecar
  takes precedence where BOTH are present; the table-level pair is the
  fallback where BOTH are present; otherwise no window (unchanged). Both
  bare names are reserved and never surface as metrics.
  `SegmentedParquetReader` splices table-level windows across segments the
  same way it splices per-metric sidecars.
- **Added:** `UnionMetricsSource` — presents several `ParquetReader`/
  `SegmentedParquetReader` readers with DISJOINT metric-name sets as one
  logical `MetricsSource`, for a caller that has split one logical table
  into several physical ones (e.g. rezolus's `.rez` V3 container, which
  tables a sampler's acquisition groups separately). A per-name accessor
  call dispatches to whichever single child owns that name; catalog methods
  (`counter_names()`/etc.) and `time_range()` are the union across
  children; `interval()` is the FINEST (minimum) across children, since a
  child that skipped ticks (window-advance dedup) has a coarser apparent
  cadence than the sampler's true poll rate. No timestamp splicing or join:
  each child keeps its own samples and acquisition windows exactly as
  before, so a query combining two children's metrics (`a / b`) resolves
  through the same grid-alignment the PromQL engine already does for any
  two independently-sampled series, and a `rate()` band still comes from
  whichever child's own table-level/per-metric window the metric belongs
  to — no fan-out or reconstruction step to lose precision in. Identity
  must be disjoint across children by construction (a caller decision, not
  something derived from untrusted archive bytes); a name seen in more than
  one child deterministically keeps its first owner rather than panicking.
  Built via the new `UnionChild` (`From<&ParquetReader>`/
  `From<&SegmentedParquetReader>`, borrowing — the original reader stays
  usable standalone after contributing to a union). Consumed by rev, not
  published — no version bump.

### metriken-query 0.17.0

- **Added:** `SegmentedParquetReader` — presents an ordered list of parquet
  byte blobs (segments of one logical table) as a single `MetricsSource`.
  Splicing happens at the `DataSource` level, *below* PromQL evaluation, so a
  `rate()` window spanning a segment boundary computes on complete data (unlike
  `MultiParquetSource`, which duplicates same-identity series across files
  rather than splicing one timeline). Open is footer-only — no row-group decode
  — and an identity index built once at open keeps splice cost linear.
  Cross-segment identity conflicts (an agent restart remapping a column id, or
  histogram bucket-power drift) split into distinct `__run__`-labelled series
  with a warning rather than erroring or silently coercing.
- **Behavior change:** `:wall_offset` is now a reserved column name, skipped by
  `parse_schema` alongside `:window_begin` / `:window_width`. A parquet file
  carrying a column literally named `:wall_offset` no longer surfaces it as a
  metric.

### metriken-query 0.14.1

- Fleet fallback acquisition window: a windowless file with a `duration` column
  now gets a coarse per-snapshot window `[timestamp, timestamp + duration]`, so
  `rate()`/`irate()` carry uncertainty bands on older/plain-parquet recordings.
  Per-observation `:window_*` sidecars still take precedence.

### Measurement uncertainty: acquisition windows + rate() error bars (#117)

A cross-crate, breaking release. Metrics can now carry a per-observation
**acquisition window** `[begin_ns, end_ns]`, which the query engine turns into
honest measurement-uncertainty bands on `rate()`/`irate()` and histogram
queries. Pre-1.0, so each breaking crate takes a minor bump.

#### metriken-core 0.3.0
- **Added:** `Window` acquisition-window type (opt-in serde); default
  `Metric::load_window` / `value_with_window` accessors; `MetricEntry::module()`.
- **BREAKING:** `MetricEntry` gained a `module` field, and its constructor takes
  a `module` argument (the `#[metric]` definition's `module_path!()`).

#### metriken 0.10.0
- **Added:** torn-safe windowed wrappers — `WindowCell`, `WindowedLazyCounter` /
  `WindowedLazyGauge`, `WindowedCounterGroup` / `WindowedGaugeGroup`, a per-index
  window API on `CounterGroup`/`GaugeGroup`, and `set_with_window` /
  `load_with_window`.
- **BREAKING:** re-exports metriken-core 0.3.0 (the changed `MetricEntry`);
  `#[metric]` now records the defining module path.

#### metriken-derive 0.6.0
- **BREAKING:** `#[metric]` emits the definition's `module_path!()` into the
  `MetricEntry` (requires metriken 0.10.0 / metriken-core 0.3.0).

#### metriken-exposition 0.17.0
- **Added:** optional per-observation `window` on `Counter`/`Gauge`/`Histogram`
  (serde `default` + `skip_serializing_if`, so it is wire-compatible with older
  snapshots); `new()` + `with_window()` constructors.
- **BREAKING:** `Counter`/`Gauge`/`Histogram` are now `#[non_exhaustive]` — build
  them with `new()` / `with_window()`, not struct literals.

#### metriken-query 0.14.0
- **Added:** reads per-metric `:window_begin` / `:window_width` sidecar columns;
  `rate()`/`irate()` derive interval bounds from acquisition windows (widened to
  contain the nominal), propagated through scalar ops, sum/avg aggregation, and
  series-op-series binary ops; histogram value bands from bucket resolution
  (`histogram_quantile` / `histogram_sum` / `histogram_mean`); `QueryResult`
  carries optional `intervals`; `new()` / `with_interval(s)` constructors.
- **BREAKING:** `Sample` / `MatrixSample` are now `#[non_exhaustive]` (use the
  constructors); `Point` is a struct (was a `(u64, f64)` tuple).

### metriken-query 0.12.0

- Add `MetricsSource::query_range_display` and a `display` module: a
  range query evaluated at native resolution and then decimated to a
  bounded point budget for *display*, without losing spikes. Each point
  is a per-bucket boxplot `EnvPoint { t, min, lo, median, hi, max }` —
  a robust median line, a hard min/max envelope so a 1-in-N spike
  survives the downsample, and a configurable inner band (`lo`/`hi` at
  the `DisplayOptions.band` quantiles, default IQR; the min/max outer
  band is invariant). Returns a `DisplayResult` (a richer, non-PromQL
  shape); only `Matrix` results are decimated, heatmap/scalar/vector
  pass through. The default trait method post-processes `query_range`,
  so every backend gets it with no per-impl code. Analysis consumers
  that recompute on the data should keep using `query_range`.

### metriken-query 0.10.6

- Add `histogram_irate(m)` — per-step rate of a histogram's
  cumulative sample count, returned as an instant vector. Lets
  dashboards derive a fallback event-rate line for histograms
  that have no standalone counter (`scheduler_runqueue_latency`,
  `scheduler_offcpu`, `scheduler_running`, `tcp_packet_latency`).
  Replaces the `sum(irate(histogram_count(m)[5m]))` idiom
  suggested for `histogram_count` in 0.10.5, which never parsed —
  PromQL disallows range vectors on function-call results.
- Add an optional `by (..)` / `without (..)` aggregation modifier
  to `histogram_irate`, `histogram_count`, and `histogram_mean`,
  matching standard PromQL aggregation-operator syntax. With no
  modifier, every matching series collapses into one
  `{__name__: metric_name}` output (today's behaviour); with
  `by`/`without`, one series per distinct projected-label tuple.
  Lets `histogram_mean by (source) (m)` return one mean per
  source in a single query — previously required N filtered
  queries.

### metriken-query 0.10.2

- Restore the matcher-less single-right binary broadcast. Queries
  shaped like `sum(rate(x[..])) / y` (where the aggregate strips
  labels and `y` carries some) were silently empty in 0.10.0/0.10.1
  on single-host parquets — the rezolus viewer's CPU-utilization
  tiles relied on this fallback. Now `matrix_matrix_op` materialises
  the lone unmatched right series into a shared timestamp lookup
  and broadcasts it across every unmatched left series, mirroring
  the eager engine's per-left fallback.

### metriken-query 0.10.1

- Cache the parquet footer once per load and decode columns one at a
  time within each row group. Restores load performance on wide files
  that regressed in 0.9.6's per-column projection rewrite — 5–28×
  faster than 0.10.0 across the rezolus dashboard fixtures
  (vllm.parquet 21.0s → 0.74s; sglang-nixl-16c 130s → 6.0s).

### metriken-query 0.10.0

Breaking — collapses the PromQL evaluator to streaming-only and
narrows the supported surface to the subset rezolus actually uses.

- All eager evaluation removed. `evaluate_expr` now forwards every
  expression to the streaming dispatcher; any AST shape the
  dispatcher doesn't recognise becomes `QueryError::Unsupported`.
- `histogram_heatmap` now streams its input — peak transient heap
  drops ~54% per query versus the eager merge-then-walk path.
- `histogram_quantile`, `histogram_quantiles`, counter `deriv`
  (the 2nd-derivative case), gauge `deriv`, and the binary
  operators (`+`, `-`, `*`, `/`) all flow through the streaming
  pipeline.
- The instant `query()` entry point now routes through
  `query_range` with `start = end = time` and collapses the
  resulting matrix to a vector by taking each series's latest
  point. Inherits the full streaming PromQL surface.
- Removed PromQL features (none used by the only known consumer):
  `scalar(...)`, `vector(...)`, `group_left` / `group_right`
  one-to-many binary matching, the matcher-less single-right
  binary broadcast, and the eager `sum(scalar(x))` passthrough.
- Removed crate features: `http` (along with the `axum` dep and
  the `promql::routes` axum router that lived behind it).
- Removed Tsdb / Collection / Series API surface that had no
  remaining callers: `Tsdb::counters` / `gauges` / `histograms`
  (cloning variants — use `*_ref` instead),
  `CounterCollection::filter` / `rate` / `filtered_rate`,
  `GaugeCollection::filter` / `filtered_sum`,
  `HistogramCollection::filter` / `sum`,
  `CounterSeries::rate` / `windowed_rate` / `windowed_irate`,
  `GaugeSeries::untyped`,
  `HistogramSeries::heatmap` / `percentiles` (the eager
  multi-quantile walker; streaming pipeline replaces it),
  `UntypedCollection`.
- Cumulative cachecannon-bench peak transient heap across 43
  representative queries: 12.82 MiB → 7.53 MiB (−41%).

### metriken-query 0.9.5

- Store histograms in the TSDB as `CumulativeROHistogram`, which only retains
  non-zero buckets in columnar form. This substantially reduces memory usage
  for sparse distributions and lets quantile queries run as a binary search on
  the cumulative counts. Delta and sum between two `CumulativeROHistogram`s
  are computed via a shared `combine()` helper.

### metriken-query 0.9.4

- Support PromQL `on(...)` and `ignoring(...)` label-matching modifiers on
  binary operators, allowing expressions whose operands carry mismatched label
  sets (e.g. `tx_bytes / ignoring(direction) link_bandwidth`) to combine
  correctly.

### 0.5.1
Metriken versions older than 0.5.1 did not have changelogs.
