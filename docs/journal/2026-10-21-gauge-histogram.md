# Gauge histograms: a distribution that is current state

**Status:** OPEN. Design 2026-10-21; built on `feat/gauge-histogram`
2026-10-22 (see "As built"). Owner decisions 2026-10-22: the kind is on the
wire (open decision 1, option b); the counting histogram types and stored
string are renamed (`CounterHistogram`, `counter_histogram`); the rebucket
fix ships on its own (#308, merged); metriken writes no standalone parquet
(`convert` writes an archive). Reviewed once (fixes applied, "As built");
not merged, not released.

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

`GaugeHistogram` is `RwLockCounterHistogram`'s storage with replace
semantics: `set(&[u64])` and `set_with_window(&[u64], Window)` replace every
bucket, and there is no `increment`. `load`, `load_with_window`, `config` as
on the counting types. It implements `HistogramMetric` (the trait is
unchanged) and returns `Value::GaugeHistogram(self)`. A group form
(`GaugeHistogramGroup`, `Value::GaugeHistogramGroup`) is left out; see open
decisions.

Names. metriken names a metric type by how it is stored and then what it
is (`AtomicHistogram`, `RwLockHistogram`; `CounterGroup`, `GaugeGroup`,
`HistogramGroup`), and pairs `Counter` with `Gauge`. The counting types
become `AtomicCounterHistogram`, `RwLockCounterHistogram` and
`CounterHistogramGroup`, which keeps the storage prefix (both counting
types exist, for per-event and bulk writes) and says the kind as
Prometheus's counter and gauge histograms do. The gauge kind needs one
storage form (a state is replaced whole, so there is nothing to make
atomic), so it is `GaugeHistogram`, as `Gauge` is. The old names stay as
`#[deprecated]` type aliases. metriken-core keeps `Value::Histogram` and
`Value::HistogramGroup` for the counting kind, with a doc alias; renaming a
variant would break every consumer's match.

### Wire and exposition (metriken-model, metriken-exposition)

A gauge histogram's kind is the list it travels in. `GroupSchema`,
`GroupSnapshot`, `WalGroupRow`, `LongOccupant`, `SnapshotV1` and `SnapshotV2`
gain a last field, `gauge_histograms`, and `WalValue` a last variant,
`GaugeHistogram`. Each field has `#[serde(default, skip_serializing_if =
"Vec::is_empty")]`:

- without a gauge histogram it is left out, so the encoding is the 0.1.x
  one (the schema hash's known-answer test is unchanged) and 0.1.x
  decoders read it;
- with one the array is an element longer. rmp-serde refuses an array
  longer than the struct (`LengthMismatch`), and under the untagged
  `Snapshot` no variant matches, so an 0.1.x consumer refuses the row,
  frame or snapshot instead of reading the state as counts. A
  `WalValue::GaugeHistogram` cell is an unknown variant index to it.

The untagged `Snapshot` still tells versions apart: `SnapshotV1` with the
field (6 elements) is tried as V1 first and decodes; a V2 (6 or 7) fails V1
at `metadata` (a `Duration` is not a map); a group's arity does not change
`SnapshotV3`'s. `tests/gauge_histogram_wire.rs` decodes every carrier with
copies of the 0.1.3 types, with and without a gauge histogram.

`Snapshotter` puts a `GaugeHistogram` in the snapshot's `gauge_histograms`
and `GroupBuilder` in the group's `gauge_histograms` slots. No entry carries
a `metric_type`; the list is the kind. This is a breaking change to
metriken-model (new public fields; struct literals across the workspace
gained the field) and so to every crate above it.

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
metadata) whose field metadata has `metric_type = gauge_histogram`. A
counting histogram column is written with `metric_type =
counter_histogram`; releases up to metriken-storage 0.4 wrote `histogram`.
One constant per kind lives in metriken-model (`COUNTER_HISTOGRAM`,
`GAUGE_HISTOGRAM`, and `LEGACY_HISTOGRAM` for the old string), with
`is_counter_histogram` accepting both counting spellings.

The builders take the kind from the value: `CellValue::GaugeHistogram`
(V1/V2 `gauge_histograms`, `WalValue::GaugeHistogram` cells) and
`GroupTableBuilder::push_row_with_gauge_histograms` (a group's
`gauge_histograms` slots) write `gauge_histogram`; the counting lists write
`counter_histogram`, replacing any `metric_type` a producer's metadata
carried. A group holding a gauge histogram is written wide, since the long
layout has no gauge histogram columns.

`parse_schema` classifies a list column by its `metric_type`: absent,
`histogram` or `counter_histogram` is counting; `gauge_histogram` is a gauge
histogram; anything else (the sparse layout's halves, which were read as
garbage buckets, or a later kind) is not read.

Old readers classify list columns by Arrow type and ignore `metric_type`, so
a gauge histogram column alone would be read as counting by
metriken-storage 0.4.0. A segment holding one is therefore written with
`metriken.format = 2` (`format.rs`; segments without one stay format 1).
`format::check` in metriken-storage 0.3.0 and later refuses a newer format,
with "segment format 2 is newer than this reader, which reads up to 1". A
`counter_histogram` column is read correctly by those readers, since they
never look at the string.

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
  (`insert_gauge_histogram_series`); `ingest_snapshot` stores a snapshot's
  `gauge_histograms` as gauge histograms.
- `ArchiveReader`'s per-table name catalog gets a gauge histogram list.
- metriken writes no standalone parquet. A parquet file has no format
  version its readers check, so a reader from before a column kind reads
  the new kind with nothing to stop it; an archive's segments carry
  `metriken.format`, which metriken-storage 0.3.0 to 0.4.0 check. Readers
  on metriken-segment 0.1.4 / metriken-archive 0.2.7 (rezolus 5.25 to 5.28)
  do not check it and read any list column as a counting histogram; they
  are kept from a new archive by dendro, whose 0.3.3 opens only schema 4,
  while dendro 0.6 writes schema 5. `convert` is `MsgpackToArchive` (a raw msgpack
  recording into a `.dendro`, one source, through `ArchiveWriter`), and
  `MsgpackToParquet`, `ParquetOptions`, `ParquetHistogramType` (with its
  sparse layout), `ParquetSchema` and `ParquetWriter` are removed. Reading
  parquet stays. metriken-query's `fixtures` feature still writes parquet
  in the legacy layout (`metric_type = histogram`, no gauge histograms), to
  test reading it.
- `.rez` is legacy (pre-6.0) and frozen: nothing new is written into one,
  so gauge histograms are recorded in `.dendro` only. `record -o out.rez`
  and a hindsight `.rez` buffer still write `.rez`, and every writer path
  leaves gauge histograms out and keeps the rest: `rez::group_by_sampler`
  (V1/V2 snapshots, `RezStream` and the v3 writer's flat path), the v3
  writer's `ingest_v3` (the group's schema without its gauge histograms,
  rehashed; a group holding nothing else is not written), and
  `parquet_ingest` (a `gauge_histogram` column is skipped). Each calls
  `rez::gauge_histograms_left_out`, which warns once per process "gauge
  histograms are recorded in .dendro only; this .rez recording leaves them
  out". `.rez` segments are sealed through `table::write_rez_table_parquet`
  (and `wal::materialize_rez_wal_tail`), which writes counting histogram
  columns as `histogram`, since a 5.x viewer passes the column's
  `metric_type` string into its metric catalog, and refuses a
  `gauge_histogram` column; a `.rez` segment is therefore always format 1.
  Reading `.rez` is unchanged. Mutations checked: skipping the `ingest_v3`
  strip, writing `counter_histogram` into a `.rez`, keeping the parquet
  ingest column, and dropping the V1/V2 warning each fail a test.

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
read as one part per source (`GroupSource::gauge_histogram_parts`), each
source's rows held for that source's own interval (or the step), and a
series several sources hold is one series, answered at each step by the
latest of its rows still held. A source and its copy count once; a fine
source's row is not held into a gap past its own interval (as a gauge's is
not), and a coarse source's rows answer after a fine one ends. This differs
from S9's rule for a gauge in one case: where two sources both hold a row,
the later row answers rather than the finer source's. Both are readings of
the same state; the later one is the more recent.

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
its own sampler review (`docs/principles.md`). Producers and recorders must
be upgraded together: a recorder from before this change ends the whole
recording when the agent emits a gauge histogram (see "What the published
releases do"), and rezolus records through metriken-recorder
(`src/recorder/mod.rs`, `src/hindsight/mod.rs`), so an agent with the
sampler needs a rezolus `record` and hindsight built on these releases. Its
histograms are recorded only to `.dendro`; `record -o out.rez` and a `.rez`
hindsight buffer leave them out with the warning above.

## Open decisions

1. **Wire refusal for old consumers.** Decided 2026-10-22: option (b), the
   trailing `gauge_histograms` fields (see "Wire and exposition"). The tag
   alone left three paths misreading (an old recorder, an old reader of a
   live WAL tail, an old `MemoryStore` ingest), measured below under the
   first build.
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
5. **Unknown future `metric_type` on a list column.** Decided with the
   string change: not read.

## Order of work

1. (a) metriken-core `Value::GaugeHistogram`; metriken `GaugeHistogram`;
   metriken-exposition snapshot, group and Prometheus text.
2. (b) metriken-storage: classification, format 2, listings and streams
   through every source, `MemoryStore`, convert; the old-reader test against
   the published crates.
3. (c) metriken-query: current-state evaluation, refusals, names; last, so it
   rebases onto the per-source union work.

## As built

Branches, both local, on upstream main 8172a5a (after #306):

- The rebucket fix (`HistogramStream::merge` summed cumulative bucket
  counts when it rebucketed), found by this work's mixed-configuration
  test, merged on its own as #308.
- `feat/gauge-histogram`, on top of it: the design note; (a) producer and
  exposition; (b) storage; the counting histogram rename; the wire fields
  and `counter_histogram`; (c) the query engine; the legacy-artifact tests.
  The first three are the tag-based build, which the wire commit reworks.
- Review fixes: the stream decoder keeps a group's gauge histogram slots
  (it dropped them, and with them the whole group, for every
  `/metrics/stream` recording); a long row or long-written group carrying
  gauge histograms is written without their values, warned once; a read is
  parts (`gauge_histogram_parts`), each row held for its own source's
  interval; an empty gauge histogram read is empty rather than
  `MetricNotFound`; `convert` writes an archive, carrying the snapshots'
  metadata and leaving no partial archive on failure.

Query (c): `promql/streaming/gauge_histogram.rs` (the state evaluator), the
handlers in `promql/mod.rs`, `histogram_quantile` in
`promql/streaming/dispatch.rs`, forwarding in
`promql/streaming/per_source.rs` (`Tap`, `GroupSource`). Raw rate mode
evaluates at the same grid with the start unsnapped.

### What the published releases do

Measured 2026-10-22 with metriken-storage 0.4.0, metriken-query 0.41.0 and
metriken-exposition 0.24.0 (metriken-model 0.1.3) from crates.io, against
artifacts this branch wrote (`free_extents` a gauge histogram, `io_latency`
a counting one, `mem_free` a gauge, `ops` a counter; 12 ticks a second
apart, a seal every 5 rows).

| artifact (this branch) | published releases |
|---|---|
| archive with a gauge histogram, sealed | open refused: "probing table fs/ext4: segment format 2 is newer than this reader, which reads up to 1" (the whole archive) |
| archive with a gauge histogram, 3 ticks all in the live WAL tail | open refused: "failed to decode a group WAL row: array had incorrect length, expected 3" |
| V3 msgpack snapshots from `GroupBuilder` with a gauge histogram | decode refused: "data did not match any variant of untagged enum Snapshot", so the 0.4.0 writer records nothing |
| V1 msgpack snapshots from `Snapshotter` with one | the same refusal |
| archive without a gauge histogram (counter, gauge, `counter_histogram`; sealed and live tail) | opens; `histogram_count(io_latency)` 1 a tick, `mem_free`, `rate(ops)` 10/s: correct |
| archive with no histogram at all | opens; `mem_free` and `rate(ops)` correct |
| V3 msgpack snapshots without a gauge histogram | decoded (12 of 12); the 0.4.0 writer records them and its reader answers correctly |
| `/metrics/stream` passes from `GroupBuilder::build_stream` (groups `fs/ext4` with a gauge histogram, `fs/lat`, `fs/mem`) through the 0.4.0 `StreamDecoder` | every pass refused: "stream fs/ext4: failed to decode a group WAL row: array had incorrect length, expected 3". In metriken-recorder 0.4.0 (and `rezolus record` and hindsight, built on it) that error ends the whole recording on the first such frame: `on_stream_event` returns it (`recorder.rs:2025`), and `fail` (`:1195`, `:1285`) aborts every endpoint's task and drops the archive. Reconnecting does not help. Upgrade producers and recorders together; the owner kept this break (2026-10-22), since recording is mostly local |
| the same passes without `fs/ext4` | decoded (12 of 12); recorded and read back correctly |
| a standalone parquet file | none written: `convert` writes an archive |

Every row is a refusal or a correct read. What the published releases still
read: any archive, live tail, snapshot or row from this branch that holds
no gauge histogram, counting histogram columns included. What they refuse:
a sealed archive with a gauge histogram anywhere (the whole archive, since
the open probes every table), a live tail or stream row of a group holding
one (the whole stream pass), and any snapshot holding one. This branch's
own stream decoder records the pass, gauge histogram included
(`tests/stream_gauge_histogram.rs`).

This branch against artifacts the published releases wrote
(`tests/fixtures/legacy/`, `metric_type = histogram`): the archive through
`ArchiveReader`, the parquet file through the parquet reader and
`ParquetReader`, msgpack snapshots through `MemoryStore::ingest_snapshot`,
and `convert` of a raw recording (into an archive whose column says
`counter_histogram`) all read `lat` as a counting histogram with one
observation a tick.

The first, tag-based build was measured the same way and misread two paths:
an archive whose gauge histogram was still in the live WAL tail
(`histogram_count` 2 where the state held 111) and one the 0.4.0 writer
recorded from tagged snapshots.

### Mutation checks

One foreground run each, the file restored and its hash compared; all were
caught unless noted:

- `GroupBuilder` writing the gauge histogram to the counting list: 5 tests
  fail;
- `parse_schema` reading a `gauge_histogram` column as counting: 8 fail;
  `format_of` always 1: 4; the rebucket fix reverted: 1;
- `skip_serializing_if` removed from `GroupSchema::gauge_histograms`: the
  schema hash known-answer test and two wire tests fail;
- `is_counter_histogram` without the legacy spelling: four legacy tests fail;
- the builder keeping a producer's `metric_type` for histograms: the V2 test
  (a counting entry claiming `gauge_histogram`) fails;
- WAL replay reading `WalValue::GaugeHistogram` as counting: two archive
  tests fail;
- an unknown `metric_type` on a list column read as counting: the kind test
  fails;
- `reads_gauge_histogram` always false (gauge histograms read on the
  counting path): 10 fail; staleness never applied: 2; the gauge-histogram
  arm of `nothing_read` removed: 1; the `histogram_irate` refusal removed: 1
  (in the first build this survived, because `nothing_read` refuses the
  read too, until the test asserted each refusal's own wording).
- Review fixes: the stream decoder dropping gauge histogram slots again: the
  stream round-trip test fails; a group's interval back to its finest
  source's: the group test fails; `held_somewhere` without gauge
  histograms: the empty-read test fails; each of the two long-row refusal
  arms, and the long-written-group refusal, removed: the refusal tests fail
  (the schema arm survived until the test sent columns naming a gauge
  histogram with no value).
- Delta review: a long row, or a long-written group, carrying gauge
  histograms skipped whole instead of written without them: the two
  long-row tests fail; a group read as one merged part at its longest
  interval: the gap test fails; the evaluator holding every part's rows
  20 s: 4 tests fail; `MsgpackToArchive` not carrying the snapshots'
  metadata, and keeping a partial archive: the convert test fails each
  time; `_gsum` at upper edges in the percentile mode: the Prometheus test
  fails.
