# The storage scan: metriken-storage and metriken-query split

**Status:** OPEN. Design 2026-10-09, nothing built. This is path step 2 of
[one recording stack](2026-10-08-one-recording-stack.md). "Step N below"
means this entry's own order of work. Decided 2026-10-09:

- `metriken-query` re-exports every public item that moves to
  `metriken-storage` at its current path for one release, and dependents
  change imports in the release after (the list is under "What moves");
- the three gaps under "Left alone" stay as they are until path step 2 has
  passed its gate, and each is then changed and measured on its own.

## Goal

`metriken-storage` holds the column readers and has no dependency on the
engine. `metriken-query` holds every computation and reads through a trait
that storage defines. Query results are identical, and memory and latency
stay within the gate below.

## Today

`metriken-query`'s `DataSource` trait (`src/lib.rs:211`, `pub(crate)`) has 24
methods. Twenty-three return data or metadata:

- materialized series: `counters`, `gauges` (`Counter`, `Gauge`,
  `src/types.rs`);
- per-series iterators: `counter_streams` (`CounterStream`, `src/types.rs:89`);
- histogram row streams: `histogram_stream` (`HistogramStream`,
  `src/histogram_stream.rs:19`), ordered by `(timestamp, series_idx)`;
- column access used inside the readers: `counter_column_refs`,
  `counter_column`, `batch_columns`, `columns_desc`;
- metadata: `interval`, `time_range`, `*_names`, `*_labels`, `file_metadata`,
  `metadata_get`, `column_map`, `sample_timestamps`, `column_count`,
  `resident_bytes`, `series_count`.

One computes: `counter_grid_rates`. Its only implementation is
`SegmentedSource::grid_rates` (`src/segmented.rs:1129`); `UnionSource`
forwards it to the child holding the metric (`src/union.rs:166`). It does
three things in one function:

1. Plans: which column of which segment holds each matched series
   (`ColPlan`, built from `ColumnTable::by_series`), keeping only segments
   whose catalog span touches the range.
2. Reads: segments in catalog order, in chunks of `batch_threads()` (at most
   8), one decode thread per segment (`read_all`), each a
   `ParquetReader::batch_columns` call that returns arrow `RecordBatch`es of
   `ts`, optional `duration`, optional `occupant`, and per column `values`,
   `begin` and `width`.
3. Computes, by calling the engine's own types: per-partition `SeriesRate`
   accumulators, `Grouped` blocks, `GroupFlush`, `DisplaySink` and
   `BucketReducer`, the partition assignment, and early finish against the
   earliest start of the remaining segments (#241).

The readers are in two halves. `FileSource`, `MultiParquetSource`,
`SegmentedSource` and `UnionSource` implement `DataSource`;
`ParquetReader`, `SegmentedParquetReader`, `UnionMetricsSource` and
`MemoryStore` wrap one and implement `MetricsSource` by owning a
`QueryEngine`. Because of (3), the `DataSource` halves cannot move below the
engine as they are.

## Design

### What moves

As the plan's crate table says, the `DataSource` halves move to storage and
the `MetricsSource` wrappers stay in query. `ParquetReader`,
`SegmentedParquetReader`, `UnionMetricsSource`, `MemoryStore`,
`MemoryStoreBuilder` and `ParquetBuilder` keep their paths and their
inherent methods; each holds a storage source and a `QueryEngine` over it.

The modules `long`, `buffer_pool`, `types`, `histogram_stream`, `labels`,
`lazy` and `memory` move to storage, with the source halves of `parquet`,
`segmented`, `union` and `memory_store`. `metriken-query` re-exports these
public items at their current paths for one release:

- composition: `CompositionSource`, `CompositionCatalog`, `UnionChild`,
  `UnionError`;
- the segment store: `SegmentStore`, `SegmentBytes`, `InMemorySegments`,
  `Handover`, `BufferPool`, `BufferPoolStats`;
- the relabel: `ColumnRelabel`, `Run`, and the module `long`;
- data types and label functions: `Labels`, `is_internal_label`,
  `is_storage_key`, `STORAGE_KEYS`, `CounterSample`, `CounterStream`,
  `HistogramSnapshot`, `ColumnChunk`, `CounterColumnRef`, `ColumnPosition`;
- `fixtures` (feature `fixtures`), which builds parquet files.

`metriken-query`'s `ingest` and `lz4` features forward to storage's for one
release. `ingest` gates `MemoryStore`'s snapshot loading, which reads
`metriken-model` types and moves with the in-memory source.

`metriken-archive` is renamed `metriken-storage` (step 2 below). A new
`metriken-archive` 0.4.0 re-exports `metriken-storage`, forwarding its
`write` and `stream` features; it is a breaking version because
`eval_timestamps_for` leaves it (below). It depends on storage and nothing
depends on it, so there is no cycle.

### The trait

`metriken-storage` exports `Source`: today's `DataSource` without
`counter_grid_rates`, with `counter_scan` added. The types its methods return
move with it: `Labels`, `Counter`, `Gauge`, `Counters`, `Gauges`,
`CounterSample`, `CounterStream`, `ColumnChunk`, `HistogramSnapshot`,
`HistogramStream`, `HistogramStreamMeta`, `HistogramRow`, `ColumnPosition`,
`CounterColumnRef`, `ColDesc`, `BatchColumns`. `BufferPool` moves too; its
blocks hold `HistogramSnapshot`.

`DataSource`, `Counter`, `Gauge`, `Counters`, `Gauges`, `HistogramStream`,
`HistogramStreamMeta`, `HistogramRow`, `ColDesc` (private fields),
`BatchColumns` and `resolve_window` are crate-private today. As storage's
public API they, and the arrow arrays in `ScanChunk`, become part of
storage's semver surface.

### The scan

```rust
fn counter_scan(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64)
    -> Option<CounterScan<'_>>;
```

The engine passes the request's `data_start` (the range start less
`max(range, step)`) as `start_ns`, which is what the plan and the row filter
use today (`src/segmented.rs:1148`).

`None` means the engine uses the per-series path (`counter_streams`), under
the conditions that send it there today: a relabel whose identities are not
fixed, a series with two columns in one segment, a duplicate occupant or wide
column, or nothing matched. A `CounterScan` carries:

- `series()`: the matched series' labels after relabelling, in identity
  order, and a `windowed` flag per series (begin and width columns present at
  its first location);
- `next_chunk(n)`: decodes up to `n` of the remaining touched segments, one
  thread per segment (sequentially on wasm32), and returns a `ScanChunk`, or
  `Ok(None)` when none remain. A segment the store no longer has is skipped,
  as today (`src/segmented.rs:1334`). A read error is `Err`, and the engine
  discards its accumulators and uses the per-series path, as it does on
  `None` today; nothing is emitted before the scan ends, so the result is the
  same;
- `ScanChunk::batches()`: per batch, the arrow arrays `ts` (`UInt64Array`),
  `duration` (optional), and per column `values` (`UInt64Array`), `begin`
  (`Int64Array`, optional) and `width` (`UInt64Array`, optional), with the
  column's rows mapped to series: one series for a wide column, a series
  index per row (`u32::MAX` for an occupant that did not match) for a long
  column. Rows outside `[start_ns, end_ns]` or with a null `ts` or value are
  the engine's to skip, as today;
- `ScanChunk::rest_start()`: the earliest catalog start of every segment not
  yet returned; `None` if one of them has no span (early finish is then off,
  as today) and after the last chunk.

`resolve_window(ts, begin, width, duration)` (today
`metriken-query/src/parquet.rs:2762`) is a public storage function, because
reading a window from those columns is a rule of the storage format.

The engine borrows the arrays, so the accumulators read them directly, as
`grid_rates` does now. `RecordBatch` and `ArrayRef` are `Send + Sync`, so the
partition threads can share one chunk.

The accumulators require each series' samples in increasing time. A series
has at most one column per segment, so this holds when segment order is time
order. Both archive catalogs keep segments in time order, including a live
long-table tail. Each reads unsealed WAL rows `ORDER BY ts` (dendro 0.3.4
`archive.rs:1579`, rezolus `crates/rez/src/rez_sqlite.rs:1038`). Its WAL
table's primary key includes `ts`, so no two rows of one table share a
timestamp. One long WAL row is one tick for every occupant. For
`InMemorySegments` and readers opened from bytes, segment order is the
caller's contract, as today.

### What stays in the engine

The batched rate path becomes an engine function over a `CounterScan`:
`SeriesRate`, `Grid`, the partition assignment (it depends on group sizes),
the per-partition threads over each chunk, `Grouped`/`Slot`/`Compact`,
`GroupFlush`, `earliest_point`/`points_before`, the early-finish lag rule,
`DisplaySink`, `BucketReducer` and `scalar_point`. Everything else the engine
computes (`CounterGridRate`, `CounterPairwiseRate`, the gauge and histogram
operators, aggregation, `display_from_result`) already reads through the
data-returning methods and does not change.

### The archive reader

`ArchiveReader` is a storage type, and its `MetricsSource` implementation
(`metriken-archive/src/reader.rs:1794`) moves to query whole (the trait is
query's, so the orphan rule allows it). Today `ArchiveReader` holds a
`ParquetReader` or `SegmentedParquetReader` per table (`reader.rs:63`) and
builds a `UnionMetricsSource` per query (`reader.rs:1581`). In storage it
holds the sources, and the implementation in query builds the wrapper over
the routed source. I don't know yet what building a `QueryEngine` per query
costs; step 3 below measures it, and if it shows in the gate the archive
keeps one wrapper per table on the query side.

Routing moves with the implementation: it parses the query with
`referenced_metrics` and asks storage for the tables holding those names.
Today `owners` (`reader.rs:1492`) takes the query string and parses it; in
storage it takes the names. Storage exposes what the implementation reads
from `ArchiveReader`'s private state today: each table's name catalog, its
recording, and its source. The cross-cadence choice of evaluation timestamps
(`ArchiveReader::cross_cadence_eval_timestamps`, `reader.rs:1726`) is query
policy and moves with it.

When rezolus's `crates/rez` becomes storage's `rez` feature (step 5 below),
`RezReader` is a storage type, and rezolus can no longer implement
`MetricsSource` for it (E0117). Its implementation only forwards to the
`ArchiveReader` it wraps, so it moves into query behind a `rez` feature.
`LiveReader` (`crates/rez/src/live.rs`) uses only public API and its only
user is rezolus's `src/viewer/follow.rs`, so it stays in rezolus with its
implementation.

### What callers change

These follow from the crate boundary, and the re-exports do not cover them:

- `ArchiveReader::eval_timestamps_for` (`reader.rs:1020`) becomes a function
  in query, `metriken_query::eval_timestamps_for(&ArchiveReader, ..)`.
  rezolus calls it through `RezReader`'s `Deref` four times in two tests
  (`crates/rez/src/reader.rs`).
- Every `metriken_archive::` import becomes `metriken_storage::`, through
  `metriken-archive` 0.4.0 in the meantime.

### Left alone

Kept as they are through path step 2, so the gate compares the same
algorithm:

- a chunk is decoded only after the previous chunk has been computed (no
  prefetch);
- early finish applies only to grouped display queries;
- a plain `ParquetReader`, any `ParquetBuilder` composition (including
  `ArchiveReader::composition_sources`) and `MemoryStore` never take the
  batched path, because only `SegmentedSource` implements it. With the scan,
  `FileSource` can implement `counter_scan` as one segment.

## Order of work

All of it is one release of `metriken-storage` 0.1.0, `metriken-query`
0.35.0 and `metriken-archive` 0.4.0; nothing is published before the last
PR. rezolus builds and tests each step through a `[patch.crates-io]` git pin
on the branch.

1. The scan inside `metriken-query`: define `CounterScan` and `ScanChunk`,
   implement it for `SegmentedSource` and `UnionSource`, and rewrite
   `grid_rates` as an engine function over it. No file moves. Adds the
   gate's query probe as an ignored test
   (`metriken-archive/tests/display_peak.rs`) and the dashboard probe to
   rezolus on the pinned branch. The gate runs here, because this is the only
   step that changes the hot path.
2. `metriken-archive` renamed `metriken-storage`; metriken-segment's tables
   moved in.
3. The `DataSource` halves and the modules under "What moves" moved to
   storage; `ArchiveReader`'s `MetricsSource` implementation and routing in
   query; the re-exports. `display_peak.rs` moves to `metriken-query/tests`.
4. `MsgpackToParquet` from `metriken-exposition`.
5. rezolus's `crates/rez` as feature `rez`, without `caller_rows`.
6. Writers take model rows (`StreamDecoder` stops decoding and re-encoding).
7. The gate again on the result; the new `metriken-archive` crate; the
   release.

Steps 2 to 6 move about 15,000 lines of `metriken-query` and the 5,000 of
`metriken-archive`, mostly without changing them; that size follows from the
crate boundaries the plan chose.

## GO / NO-GO

The archives are under `~/rezolus-bench` on the author's host. They derive
from one 9h37m56s rezolus recording (a `.rez`, converted with
`rezolus recording upgrade --to dendro`):

- `metrics-9.6h.dendro`: the whole recording, 5,874 segments; its per-task
  table is wide;
- `long.dendro` and `wide.dendro`: that per-task table
  (`cpu_usage/cpu_usage_task`, 6,644 tasks) alone, rewritten long and wide by
  `metriken-archive/tests/long_rewrite.rs`.

The queries are #241's. `display_peak.rs` opens the archive's first
recording with a 16 MiB `BufferPool` and runs each through
`query_range_display_opts` over the full range at step 1 s and budget 500:

| archive | query |
|---|---|
| `long.dendro` | `sum by (comm) (irate(task_cpu_usage[5s]))` |
| `metrics-9.6h.dendro` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| `wide.dendro` | `sum by (comm) (irate(task_cpu_usage[5s]))` |

The dashboard load is `rezolus view metrics-9.6h.dendro` with a Playwright
probe that visits every section and scrolls until no request has been issued
for 5 s. It measures the wall time from the first `/api/v1` request to the
last response, the sum over `/api/v1/query_range` requests of response end
less request start as Playwright reports them, and the peak footprint of the
`rezolus view` process.

The candidate and `metriken-query` 0.34.7 run alternately on the same host,
five runs each, on every query and the dashboard load. Every query must return
a bit-identical `DisplayResult` (serialized and compared). For median time and
peak footprint, a measure passes when the two medians are within 5% of each
other, or when the two builds' min-to-max ranges overlap. It fails when the
candidate's median is more than 5% worse and outside the 0.34.7 range. GO for
the release when every measure passes.

Overlap counts as a pass because the 0.34.7 runs on 2026-10-09 spread more
than 5% on three measures: `wide.dendro` peak footprint 1,339 to 1,513 MB,
`long.dendro` peak footprint 384 to 405 MB, and `long.dendro` time 2.52 to
2.92 s. On those measures five runs cannot tell a difference smaller than the
spread from noise, and a regression of that size would pass. Decided
2026-10-09.

NO-GO for the scan if the engine cannot read the arrays without copying them
or cannot keep the per-partition threads over a chunk, or if step 1 below
builds but misses the gate. In that case `counter_grid_rates` stays a storage
method that takes a sink trait defined in `metriken-storage` and implemented
by the engine, which keeps the computation in the engine but leaves a
callback across the crate boundary; it is measured against the same gate.
