# Gauge histograms: a distribution that is current state

**Status:** OPEN. Design 2026-10-21; layers (a) to (c) built on
`feat/gauge-histogram` on 2026-10-22 (see "As built"), not reviewed, not
merged, not released.

## Goal

A producer can publish a distribution whose value is the state of something
now (how many free extents of each size a filesystem has, how many tasks are
queued on each CPU) rather than a count of events since start. Every layer
that stores or reads it keeps that reading convention, and every layer that
does not know the convention refuses or skips the column rather than reading
it as a counting histogram.

## Today

Every histogram in metriken is cumulative. `AtomicHistogram` counts
increments; `RwLockHistogram` is bulk-written but its consumers still treat
each row as a running total. Readers difference consecutive rows:

- `histogram_quantile(q, h)`, `histogram_quantiles`, `histogram_count`,
  `histogram_sum`, `histogram_mean` and `histogram_heatmap` take each series'
  delta since its previous row (`delta_walk`,
  `metriken-query/src/promql/streaming/histogram.rs`), clamping a per-bucket
  decrease to zero;
- `histogram_irate` divides that delta by the row interval;
- the strided forms share each row's delta among the windows its interval
  overlaps (`Splitter`).

Some distributions are state, not events:

- ext4's `/proc/fs/ext4/<dev>/mb_groups`: per block group, the number of free
  extents of each order (sizes 2^0 to 2^13 blocks). Summed over groups it is
  the free-space fragmentation of the filesystem, a histogram over powers of
  two (grouping power 0).
- file and directory size distributions of a tree;
- slab object sizes;
- per-CPU runqueue length, in-flight queue depth sampled across queues.

Stored as a counting histogram today, such a distribution gives plausible
numbers and no error. Two rows with the same state give an empty delta, so
`histogram_quantile` emits nothing; a state that shrinks in one bucket and
grows in another gives the growth alone; a state that grows by one extent
reads as one observation. Nothing in the column says the convention is
different, so the convention has to be part of the column's identity.

A column's kind today is:

- in a parquet segment, its Arrow type plus the field metadata:
  `parse_schema` (`metriken-storage/src/parquet.rs`) classifies `UInt64` as a
  counter, `Int64` as a gauge, and `List<UInt64>` with `grouping_power` and
  `max_value_power` as a histogram. `metric_type` (counter, gauge,
  histogram; timestamp, duration on the sidecars) is written by the builders
  (`metriken-storage/src/builder.rs`, `or_insert_with`, so a producer's own
  `metric_type` survives) and read by nothing for histograms;
- in a group schema, the slot list a descriptor sits in
  (`GroupSchema::{counters, gauges, histograms}`, `metriken-model`);
- in a snapshot, the list an entry sits in (`SnapshotV2::histograms`,
  `GroupSnapshot::histograms`).

Prometheus native histograms carry the same distinction: a counter
histogram, and a gauge histogram (OpenMetrics `gaugehistogram`, protobuf
`ResetHint = GAUGE`) whose buckets may go down, which PromQL reads without
`rate()`: `histogram_quantile(0.9, h)` on a gauge histogram is the 90th
percentile of the state at each step. That is the reference used below.

## Design

### Producer API (metriken-core, metriken)

A new variant `Value::GaugeHistogram(&dyn HistogramMetric)` in
`metriken-core`, and a new type `metriken::GaugeHistogram`.

`Value` is `#[non_exhaustive]`, so the variant is an additive change to
metriken-core (a patch release), and every consumer already has a wildcard
arm. That arm is the reason for a variant rather than a flag: a consumer
built before the variant (metriken-exposition 0.24, its Prometheus text
renderer, `Snapshotter`, `GroupBuilder`) reaches `_ => {}` or
`_ => continue` and skips the metric. A defaulted method on
`HistogramMetric` (`fn is_gauge(&self) -> bool { false }`) is equally
additive, but an old exposition ignores it and exposes the metric as a
counting histogram, which is the misread this design exists to prevent.

`GaugeHistogram` is `RwLockHistogram`'s storage with replace semantics:
`set(&[u64])` and `set_with_window(&[u64], Window)` replace every bucket, and
there is no `increment`. `load`, `load_with_window`, `config` as on
`RwLockHistogram`. It implements `HistogramMetric` (the trait is unchanged)
and returns `Value::GaugeHistogram(self)`. A group form
(`GaugeHistogramGroup`, `Value::GaugeHistogramGroup`) is left out; see open
decisions.

### Wire and exposition (metriken-exposition)

Snapshots and WAL rows keep their shapes. A gauge histogram travels as a
histogram entry (`SnapshotV2::histograms`, a group's `histograms` slots, a
`WalValue::Histogram` or a group row's histogram tuple) whose metadata
carries `metric_type = gauge_histogram`. `Snapshotter` and `GroupBuilder`
add the key; nothing else changes on the wire. A new wire variant (a fourth
slot list in `GroupSchema`, a `WalValue::GaugeHistogram`) is the alternative
weighed under open decisions.

Prometheus text (format 0.0.4, the only one `prometheus_text` writes) has no
gauge histogram type, and the Prometheus text parser rejects a `# TYPE` it
does not know, which would fail the whole scrape. A gauge histogram is
written as the series Prometheus itself stores for an OpenMetrics
`gaugehistogram`, each as a gauge family:

```
# TYPE free_extents_bucket gauge
free_extents_bucket{le="1"} 120
free_extents_bucket{le="3"} 151
...
free_extents_bucket{le="+Inf"} 160
# TYPE free_extents_gcount gauge
free_extents_gcount 160
# TYPE free_extents_gsum gauge
free_extents_gsum 1830
```

`histogram_quantile(0.5, free_extents_bucket)` in Prometheus then reads the
state without `rate()`, and a scraper that knows nothing of gauge histograms
reads gauges, which is the correct convention for each line. With
`PrometheusOptions::percentiles` set, the quantiles are written as a gauge
family with a `quantile` label, beside `_gcount` and `_gsum`, rather than as a
`summary` (whose `_count` and `_sum` are counters).

### Stored identity (metriken-storage)

A gauge histogram column is a histogram column (`List<UInt64>`,
`<name>:buckets`, `grouping_power` and `max_value_power` in the field
metadata) whose field metadata has `metric_type = gauge_histogram`. The
builders already keep a producer's `metric_type`, so `TableBuilder`,
`GroupTableBuilder`, the long table builder and WAL replay need no new
value shape. `parse_schema` classifies a list column as a gauge histogram
when `metric_type` is `gauge_histogram`, and as a histogram otherwise, as
today.

Old readers classify by Arrow type and ignore `metric_type`, so the tag alone
would be read as a counting histogram by metriken-storage 0.4.0 and every
metriken-query release on it. A segment holding a gauge histogram column is
therefore written with `metriken.format = 2` in its key-value metadata
(`format.rs`; segments without one stay format 1, so archives without gauge
histograms read in old releases as before). `format::check` in
metriken-storage 0.3.0 and later refuses a segment whose format is newer than
it reads, with "segment format 2 is newer than this reader, which reads up to
1; read it with a newer release". That is the documented use of the key:
"Change `FORMAT_VERSION` when a reader of the previous version would misread
a segment the new writer produces."

What an old reader does with such an archive is verified under "As built"
against the published crates (metriken-storage 0.4.0, metriken-query
0.41.0), not inferred from this source.

The rejected alternative is a different physical type (`List<Int64>`, which
old `parse_schema` skips). It needs a second value shape in every builder,
writer and reader, and old `read_table_parquet` (the `.rez` tar path)
panics on it (`u64_col`'s downcast) instead of refusing.

Reading:

- `DataSource` gets `gauge_histogram_names`, `gauge_histogram_labels`,
  `gauge_histogram_stream` and `gauge_histogram_interval`, defaulted to
  empty, `None` and `interval()`. A gauge histogram is not in
  `histogram_names` and `histogram_stream` never returns one: a caller that
  does not know the convention (an external `DataSource` consumer, a
  composite that does not forward the new methods) loses the metric rather
  than differencing it. Every composite in the crate forwards them.
- The stream type is `HistogramStream` unchanged; the method it came from
  says how to read it. `HistogramStream::merge` (rebucketing to
  `common_config`) applies to gauge histograms as it does to counting ones.
- `MemoryStore` holds gauge histogram series apart from histogram series
  (`insert_gauge_histogram_series`); ingesting a snapshot routes an entry by
  its `metric_type`.
- `ArchiveReader`'s per-table name catalog gets a gauge histogram list.
- `convert` (msgpack to parquet) keeps a gauge histogram's tag and stamps
  format 2, rather than overwriting `metric_type` with `histogram`.

### Query semantics (metriken-query)

A gauge histogram is read like a gauge: at each evaluation time `t` (the
range grid `start + k·step`, or the instant), each series' latest row in
`[t − s, t]`, where `s` is the step or the series' row interval when that is
longer, the same staleness a gauge has. No function takes a difference
between two rows.

- `histogram_quantile(q, h)`, `histogram_quantiles([..], h)`
  (`histogram_percentiles`): the quantiles of the bucket-wise sum of the
  qualifying series' states at `t`. No point at `t` when no series
  qualifies or the summed state holds no items.
- `histogram_count(h)`, `histogram_sum(h)`, `histogram_mean(h)` and their
  `by`/`without` and `sum(...)` forms: per group, over the summed state. An
  empty state gives count 0 and sum 0 (both are true statements about the
  state), and no mean. `histogram_sum` uses bucket midpoints and reports the
  `[Σ count·start, Σ count·end]` band, as for counting histograms.
- `histogram_heatmap(h)`: one column per evaluation time, the summed state's
  buckets.
- Refused, each with an error that names the metric as a gauge histogram and
  the current-state form to use instead: `histogram_irate(h)`; the strided
  forms (`histogram_count(h, 15)`, `histogram_quantiles([..], h, 15)`,
  `histogram_heatmap(h, 15)`), which pool deltas over windows; and `rate`,
  `irate`, `increase` or any other counter or gauge function given the name.
- A name held as a gauge histogram and also as a counting histogram (two
  producers disagreeing) is refused rather than read either way.

Aggregation across series is the bucket-wise sum: the distribution of the
fleet's state. Across bucket configurations the states are rebucketed to the
coarsest common configuration (`common_config`: the lowest grouping power,
the highest max value power) before summing, which `HistogramStream::merge`
already does.

Per-source union (S9 of
[recording identity and views](2026-10-19-recording-identity-and-views.md)):
S9 exists because a value computed from rows of two sources (a rate across a
junction) is wrong; a gauge histogram's value is one row, so no combination
of sources can produce that error. A process group's gauge histogram is
therefore the merge of every source's rows (`GroupSource::gauge_histogram_stream`,
`HistogramStream::merge`, which makes a series held by several sources one
series), read as the latest row at each step. A source and its copy count
once. This differs from S9's rule for a gauge in one case: where two sources
both hold a row in the staleness window, the later row answers rather than
the finer source's. Both are readings of the same state; the later one is the
more recent.

### Grouping power 0

`Config::new(0, 64)` has 65 buckets: 0, 1, then one per power of two,
`[2^k, 2^(k+1) − 1]` for k from 1 to 63. Per-order counts such as ext4's map
one order to one bucket with no loss. Quantiles report a bucket's upper edge
(`bucket.end()`), so a state whose items are all exactly 2^k reads as
2^(k+1) − 1, and `histogram_sum` uses the midpoint with a band to the edges.
A consumer that wants exact per-order counts reads `histogram_heatmap`. A
65-bucket column costs 520 bytes a row before compression.

### A rezolus sampler (later, rezolus)

An `ext4` sampler (`src/agent/samplers/filesystem/ext4_mballoc/`, no BPF)
reads `/proc/fs/ext4/*/mb_groups` at its interval, sums the per-order free
extent counts over block groups into a 65-bucket vector per device, and
writes it with `GaugeHistogram::set_with_window` (the window brackets the
read) into a dynamic metric per device, labelled `device`. The sampler sits
in its own acquisition group, so only that table's segments are format 2.
It belongs in a separate rezolus PR after these crates are released, with
its own sampler review (`docs/principles.md`); rezolus ships agent, recorder
and viewer in one binary, so a fleet on one rezolus release has no old
consumer of its own wire.

## Open decisions

1. **Wire refusal for old consumers.** With the tag on the wire, three paths
   still read a gauge histogram as a counting one: an old recorder
   (metriken-storage 0.4 or older) writes the tagged column as format 1; an
   old reader materializing a live WAL tail builds the column itself; an old
   `MemoryStore::ingest_snapshot` strips `metric_type` as a storage key. A
   new reader reads the first two correctly, because the tag survives. The
   closure is a wire shape old decoders reject: trailing
   `#[serde(default, skip_serializing_if = "Vec::is_empty")]` fields
   (`GroupSchema::gauge_histograms`, `WalGroupRow::gauge_histograms`,
   `LongOccupant::gauge_histograms`) leave every existing encoding and schema
   hash unchanged, and rmp-serde 1.3 refuses an array longer than the struct
   (`Error::LengthMismatch`, `decode.rs:571`), so old consumers refuse the
   row, not misread it. It is a breaking change to metriken-model (new public
   fields; 78 struct literals of the three in this workspace), forcing breaking releases
   of every crate above it. Recommendation: ship the tag now, and add the
   trailing fields before any producer that is not rezolus (whose binary
   carries its own consumers) emits a gauge histogram.
2. **Groups.** `GaugeHistogramGroup` / `Value::GaugeHistogramGroup` for one
   gauge histogram per member (per device, per cgroup). Recommendation:
   add it when the first sampler needs per-member state from one registry
   metric; a dynamic metric per device covers ext4.
3. **Empty state.** count and sum give 0, quantiles and mean give no point.
   Recommendation: keep; it matches Prometheus, where a gauge histogram with
   zero count has no quantile.
4. **Strided forms.** Refused. They could be defined for state (the latest
   row in each window, or a time-weighted mean of the states), but neither is
   what the counting form computes, so the same expression would mean two
   things. Recommendation: refuse until someone needs a downsampled state,
   then add a named function (`histogram_avg_over_time`).
5. **Unknown future `metric_type` on a list column.** Read as a counting
   histogram, as today (`convert` writes `sparse_histogram` on list columns
   that are already misread). Recommendation: leave; the segment format
   version is the mechanism for the next kind.

## Order of work

1. (a) metriken-core `Value::GaugeHistogram`; metriken `GaugeHistogram`;
   metriken-exposition snapshot, group and Prometheus text.
2. (b) metriken-storage: classification, format 2, listings and streams
   through every source, `MemoryStore`, convert; the old-reader test against
   the published crates.
3. (c) metriken-query: current-state evaluation, refusals, names; last, so it
   rebases onto the per-source union work.

## As built

On `feat/gauge-histogram`, rebased onto upstream main 8172a5a (after #306,
the per-source union), one commit per layer:

- (a) metriken-core `Value::GaugeHistogram`; metriken `GaugeHistogram`;
  metriken-model `METRIC_TYPE`, `GAUGE_HISTOGRAM`; metriken-exposition
  `Snapshotter`, `GroupBuilder` (the descriptor carries the tag, and the
  group identity folds a marker for a gauge histogram so a dynamic metric
  replaced by one of the other kind rebuilds the schema) and
  `prometheus_text`. Tests: `metriken-exposition/tests/gauge_histogram.rs`,
  `metriken/src/histogram.rs`.
- (b) metriken-storage: `ColKind::GaugeHistogram` from `metric_type`,
  `DataSource::gauge_histogram_*` through every source, segment format 2
  (`format::format_of`, `stamped_for`, `stamped_if_newer`; the table, long
  table, `convert`, `.rez` parquet ingest and projection writers stamp),
  `convert` keeps the tag. Tests: `metriken-storage/tests/gauge_histogram.rs`
  (sealed, live WAL tail, V2 entry, `convert` in both layouts).
- A fix found by the mixed-configuration test: `HistogramStream::merge`
  summed cumulative counts of buckets that rebucket into one, so 3 + 1
  observations read as 7. It affected counting histograms merged across
  configurations too. Separate commit.
- (c) metriken-query: `promql/streaming/gauge_histogram.rs` (the state
  evaluator), the handlers in `promql/mod.rs`, `histogram_quantile` in
  `promql/streaming/dispatch.rs`, forwarding in
  `promql/streaming/per_source.rs` (`Tap`, `GroupSource`), the
  `MetricsSource` listings, `MemoryStore`. Tests:
  `metriken-query/tests/gauge_histogram.rs` (`MemoryStore`), the evaluator's
  unit tests, and the archive tests in `metriken-storage/tests/gauge_histogram.rs`
  (`ArchiveReader`, a source and its copy, mixed configurations). Raw rate
  mode evaluates at the same grid with the start unsnapped.

What the published releases do, measured 2026-10-22 with metriken-storage
0.4.0 and metriken-query 0.41.0 from crates.io against archives this branch
wrote (`free_extents`, grouping power 0, 12 ticks, a seal every 5 rows; the
state at tick 1 holds 111 items):

| archive | 0.4.0 / 0.41.0 | this branch |
|---|---|---|
| sealed, finalized | open refused: "probing table fs/ext4: segment format 2 is newer than this reader, which reads up to 1" (the whole archive, other tables included) | gauge histogram, 12 rows |
| 3 ticks, all in the live WAL tail | read as a counting histogram: `histogram_count` 2 at each step, `histogram_quantile(0.5)` 7 | gauge histogram, 3 rows |
| recorded by the 0.4.0 writer from tagged snapshots | read as a counting histogram, as above | gauge histogram, 12 rows (the tag survives) |

The second and third rows are open decision 1. The scratch projects are not
in the repository.

Mutation checks, each one foreground run with the file restored and its hash
compared (all caught unless noted):

- the `GroupBuilder` tag removed: the group builder test fails;
- `parse_schema` ignoring `metric_type`: the four storage round-trip tests
  fail;
- `format_of` always 1: the format test, the sealed round trip, the V2 and
  `convert` tests fail;
- the rebucket fix reverted: its unit test and the mixed-configuration test
  fail;
- `reads_gauge_histogram` always false (gauge histograms read on the
  counting path): 9 of the query and archive tests fail;
- staleness never applied: the two staleness tests fail;
- the gauge histogram arm of `nothing_read` removed: the refusal test fails;
- the `histogram_irate` refusal removed: survived at first, because
  `nothing_read` refuses the read as well; the test now asserts each
  refusal's own wording, and fails.
