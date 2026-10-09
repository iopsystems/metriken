# The storage scan: metriken-storage and metriken-query split

**Status:** OPEN. Design 2026-10-09, nothing built. This is path step 2 of
[one recording stack](2026-10-08-one-recording-stack.md). Decided 2026-10-09:

- `metriken-query` re-exports the readers that move to `metriken-storage`
  (`ParquetReader`, `SegmentedParquetReader`, `UnionMetricsSource`,
  `MemoryStore`, `SegmentStore`, `BufferPool`, `Labels`, the long-table
  relabel) at their current paths for one release, so dependents bump the
  version without changing imports, and change imports in the release after;
- the three gaps below ("Left alone") stay as they are until step 2 has
  passed its gate, and each is then changed and measured on its own.

## Goal

`metriken-storage` holds the column readers and has no dependency on the
engine. `metriken-query` holds every computation and reads through a trait
that storage defines. Query results, memory and latency do not change.

## Today

`metriken-query`'s `DataSource` trait (`src/lib.rs:211`, `pub(crate)`) has 20
methods. Nineteen return data or metadata:

- materialized series: `counters`, `gauges` (`Counter`, `Gauge`, `src/types.rs`);
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

Because of (3), and because `ParquetReader`, `SegmentedParquetReader` and
`UnionMetricsSource` implement `MetricsSource` by owning a `QueryEngine`, the
readers cannot move below the engine as they are.

## Design

### The trait

`metriken-storage` exports `Source`: today's `DataSource` without
`counter_grid_rates`, with `counter_scan` added. The types its methods return
move with it: `Labels`, `Counter`, `Gauge`, `Counters`, `Gauges`,
`CounterSample`, `CounterStream`, `HistogramSnapshot`, `HistogramStream` and
its rows, `ColumnPosition`, `CounterColumnRef`, `ColDesc`, `BatchColumns`.
`BufferPool` moves too; its blocks hold `HistogramSnapshot`.

### The scan

```rust
fn counter_scan(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64)
    -> Option<CounterScan<'_>>;
```

`None` means the engine uses the per-series path (`counter_streams`), under
the conditions that send it there today: a relabel whose identities are not
fixed, a series with two columns in one segment, a duplicate occupant or wide
column, or nothing matched. A `CounterScan` carries:

- `series()`: the matched series' labels after relabelling, in identity
  order, and a `windowed` flag per series (begin and width columns present at
  its first location);
- `next_chunk(n)`: decodes up to `n` of the remaining touched segments, one
  thread per segment (sequentially on wasm32), and returns a `ScanChunk`, or
  `Ok(None)` when none remain. A read error is `Err`, and the engine falls
  back to the per-series path as it does on `None` today;
- `ScanChunk::batches()`: per batch, the arrow arrays `ts` (`UInt64Array`),
  `duration` (optional), and per column `values` (`UInt64Array`), `begin`
  (`Int64Array`, optional) and `width` (`UInt64Array`, optional), with the
  column's rows mapped to series: one series for a wide column, a series
  index per row (`u32::MAX` for an occupant that did not match) for a long
  column. Rows outside `[start_ns, end_ns]` or with a null `ts` or value are
  the engine's to skip, as today;
- `ScanChunk::rest_start()`: the earliest catalog start of every segment not
  yet returned, or `None` if one of them has no span (early finish is then
  off, as today).

`resolve_window(ts, begin, width, duration)` (today
`metriken-query/src/parquet.rs:2762`) is a public storage function, because
reading a window from those columns is a rule of the storage format.

The engine borrows the arrays, so the accumulators read them directly, as
`grid_rates` does now. Within one series, samples arrive in increasing time:
segments come in catalog order and a series has at most one column per
segment. A live long-table tail keeps that order too: both catalogs read
unsealed WAL rows `ORDER BY ts` (dendro 0.3.4 `archive.rs:1579`, rezolus
`crates/rez/src/rez_sqlite.rs:1038`), and one long WAL row is one tick for
every occupant.

### What stays in the engine

The batched rate path becomes an engine function over a `CounterScan`:
`SeriesRate`, `Grid`, the partition assignment (it depends on group sizes),
the per-partition threads over each chunk, `Grouped`/`Slot`/`Compact`,
`GroupFlush`, `earliest_point`/`points_before`, the early-finish lag rule,
`DisplaySink`, `BucketReducer` and `scalar_point`. Everything else the engine
computes (`CounterGridRate`, `CounterPairwiseRate`, the gauge and histogram
operators, aggregation, `display_from_result`) already reads through the
data-returning methods and does not change.

`MetricsSource` stays in `metriken-query`, and its implementations for the
storage readers move there (the trait is query's, so the orphan rule allows
it). `ArchiveReader`'s routing moves into its implementation: it parses the
query with `referenced_metrics` and asks storage for the tables holding those
names. Today `owners` (`metriken-archive/src/reader.rs:1492`) takes the query
string and parses it; in storage it takes the names. The cross-cadence choice of evaluation
timestamps (`ArchiveReader::cross_cadence_eval_timestamps`,
`metriken-archive/src/reader.rs:1726`) is query policy and moves with it.

### Left alone

Kept as they are through step 2, so the gate compares the same algorithm:

- a chunk is decoded only after the previous chunk has been computed (no
  prefetch);
- early finish applies only to grouped display queries;
- a plain `ParquetReader`, any `ParquetBuilder` composition (including
  `ArchiveReader::composition_sources`) and `MemoryStore` never take the
  batched path, because only `SegmentedSource` implements it. With the scan,
  `FileSource` can implement `counter_scan` as one segment.

## Order of work

All of it is one release of `metriken-storage` 0.1.0 and `metriken-query`
0.35.0; nothing is published before the last PR.

1. The scan inside `metriken-query`: define `CounterScan` and `ScanChunk`,
   implement it for `SegmentedSource` and `UnionSource`, and rewrite
   `grid_rates` as an engine function over it. No file moves. The gate runs
   here, because this is the only part that changes the hot path.
2. `metriken-archive` renamed `metriken-storage`; metriken-segment's tables
   moved in.
3. The column readers, `long`, `buffer_pool`, `types`, `histogram_stream`,
   `labels`, `lazy` and `MemoryStore` moved to storage; `MetricsSource`
   implementations and routing in query; the re-exports.
4. `MsgpackToParquet` from `metriken-exposition`.
5. rezolus's `crates/rez` as feature `rez`, without `caller_rows`.
6. Writers take model rows (`StreamDecoder` stops decoding and re-encoding).
7. The gate again on the result, then the release.

Steps 2 to 6 move about 15,000 lines of `metriken-query` and the 5,000 of
`metriken-archive`, mostly without changing them; that size follows from the crate
boundaries the plan chose.

## GO / NO-GO

GO for the release when, on `~/rezolus-bench/metrics-9.6h.dendro` (9h37m56s,
5,874 segments, a 6,644-occupant task table; made from a `.rez` with
`rezolus recording upgrade --to dendro`), #241's three queries at a budget of
500 and a full `rezolus view` dashboard load, five runs each, have median time
and peak memory within 5% of `metriken-query` 0.34.7 on the same host, and
every query returns the same result as 0.34.7.

NO-GO for the scan if the engine cannot read the arrays without copying them
or cannot keep the per-partition threads over a chunk. In that case
`counter_grid_rates` stays a storage method that takes an engine-defined sink
trait, which keeps the computation in the engine's types but leaves a
callback across the crate boundary.
