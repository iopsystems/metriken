# metriken-py: reading recordings from Python

**Status:** OPEN. Design 2026-10-12, not started. This is path step 6 of
[one recording stack](2026-10-08-one-recording-stack.md) (piece 4, whose
recommendation, bindings over `metriken-storage` and `metriken-query`, this
entry takes). "Step N below" means this entry's own order of work. The
owner decided the package name, versioning, histogram conversion and
platforms on 2026-10-12; the design below follows those decisions, and the
questions still open are under "Questions for the owner".

## Goal

A Python program opens any recording metriken reads (`.dendro`, `.rez`,
parquet) and gets PromQL results and table rows as Arrow data that pyarrow,
polars and duckdb use without a copy. The archive logic stays in Rust, in
one place. The plan's GO criterion 4: the bindings return the same values as
`rezolus mcp query` on rezolus's compatibility fixtures.

The deadline is the plan's: before any consumer outside Rust gets `.dendro`
by default. rezolus 6.x writes `.dendro` only
([metriken-recorder](2026-10-12-metriken-recorder.md), decided 2026-10-12).
Once systemslab's metrics artifacts are `.dendro`, the scripts below fail on
them.

## Today

Sources: metriken origin/main d2bf74d (metriken-storage 0.2.1,
metriken-query 0.36.0), rezolus upstream/main 15c77844.

**What Python consumers read.** The plan names internal duckdb and polars
notebooks, the h2histogram-py documentation and llm-sim's export. A search
of the author's checkouts on 2026-10-12 found no notebook that imports
duckdb or polars against rezolus data, and llm-sim is not checked out there,
so what the notebooks and llm-sim read is not known from source.
h2histogram-py is public (iopsystems/h2histogram-py, read at 1f494cb;
published on PyPI as `h2histogram` 0.1.0, import `h2histogram`, pure Python,
no required dependencies, `requires-python >= 3.9`):

- **What it reads.** `h2histogram.arrow.read_histograms(source, name)` takes
  a parquet path or a pyarrow `Table`/`RecordBatch`, finds the column
  `"{name}:buckets"` (or the sparse `:bucket_indices`/`:bucket_counts`
  pair), and returns one `Histogram` per row, built with
  `Histogram.from_buckets(grouping_power, max_value_power, buckets)` for
  the dense column and `SparseHistogram.from_parts(...).to_dense()` for the
  sparse pair (`arrow.py:217-240`). The
  configuration comes from the field metadata keys `grouping_power` and
  `max_value_power` (the same keys metriken-storage writes,
  `long_table.rs:165`), else from the dense bucket count, else (3, 64). It
  does not open a `.rez` or a `.dendro`.
- **What its documentation does with the rows.** The README prints each row's
  p99 and merges all rows into a recording total. A rezolus histogram row is
  cumulative since the producer started (the `.rez` script below differences
  consecutive rows for that reason, and the engine does the same,
  `accumulate_delta_into` in `metriken-query/src/promql/streaming/histogram.rs:252`).
  So a row's percentile is the since-start distribution, and the merge
  counts early samples once per later row. The example reports a
  percentile's `.midpoint`, where the engine reports the bucket's end. It
  looks the metric up as `"syscall/read/latency"`, a column name current
  rezolus files do not have: their wide columns are named by id
  (`269:buckets`), with the metric name in the field metadata. Whether any
  caller of h2histogram-py relies on that example was not checked.

Three other patterns were found, in private experiment repositories:

- **A script that reads a `.rez` v3 directly.** It opens the file with
  Python's `sqlite3`, reads `segments.bytes` for one table in `seq` order,
  decodes each blob with `pyarrow.parquet`, picks histogram columns by the
  `:buckets` suffix, reads `grouping_power` from the field metadata,
  reimplements the H2 bucket upper bound in Python, and differences
  consecutive rows because the buckets are cumulative. It ignores the WAL
  tail, so it misses the unsealed end of the recording, and it does not read
  a `.dendro` (a different catalog schema) or a long table (no occupant
  relabel). This is the plan's "reader written in Python" alternative, built
  ad hoc for one question.
- **Scripts that read a rezolus parquet with pandas.** They select columns by
  ids copied by hand from one file's field metadata (`74x0` for GPU power and
  so on). Nothing guarantees a metric the same id in another file, so the
  script is correct only for files like the one it was written against.
- **Scripts that run PromQL through `systemslab promql`.** These go through
  systemslab's server, which reads the archive itself. They do not need the
  bindings, and they keep working when the artifact becomes a `.dendro`
  provided systemslab reads it (systemslab's change to read rezolus 6.0
  output).

**What metriken provides.** Opening by content:
`metriken_storage::rez::open::{open_recordings, open_recordings_from_bytes}`
(feature `rez`) returns one `ArchiveReader` per recording with its labels,
for a `.dendro`, a `.rez` v3 or a tar `.rez`; `metriken_query::ParquetReader`
opens a parquet file. `ArchiveReader` implements `MetricsSource`
(`metriken-query/src/archive.rs:371`): `query_range_opts`, `query`, names,
labels, metadata, `time_range`. Raw access differs by container:

- **`.dendro` and `.rez` v3** are catalogs, opened lazily: the `Catalog`
  trait (`metriken-storage/src/catalog.rs`) gives `sources`, `tables`,
  `segment_meta`, `segment_bytes` and `live_wal`. A long table's occupant
  stream is the table `<table>/occupants`; `long::OccupantLabels` maps an
  occupant number to its labels, and an occupant never changes labels
  (`long.rs:163`). A WAL tail is decoded by one of three functions, chosen
  by what the table is (`reader.rs`, `live_tail` at `:295-308` and the
  occupant read at `:452-453`): a wide table's tail by
  `wal::materialize_wal_tail`, a long table's (one that has a
  `<t>/occupants` table) by `wal::materialize_long_wal_tail`, and an
  occupant stream's rows by `occupants::decode_wal_row`.
- **A tar `.rez`** (v1/v2) is read eagerly into an `InMemorySource`
  (`rez/open.rs:4-6`, `archive.rs:54`): per recording, `(table, segments
  oldest first)` as parquet bytes. It has no WAL and no long tables
  (`reader.rs:414`).
- **A parquet file** is opened by `metriken_query::ParquetReader` for
  queries. It is one wide table: no segments, no WAL, no occupants.
  `ParquetReader` exposes no row groups or record batches
  (`metriken-query/src/parquet.rs:30-292`), so the bindings read a parquet
  file's rows with the parquet crate's `ParquetRecordBatchReaderBuilder`
  and take its row-group count from the file's parquet metadata.

**Query results are not Arrow.** `QueryResult::Matrix` holds `MatrixSample`s:
labels as a `HashMap<String, String>`, `values: Vec<(f64, f64)>` with the
timestamp in f64 seconds, and `bands: Option<Vec<Option<(f64, f64)>>>`
(`metriken-query/src/promql/mod.rs:135`). The bindings convert this to
Arrow. f64 seconds lose up to about 130 ns at the current epoch (the doc
comment on `RANGE_SLACK_NS`, `promql/mod.rs:231-238`; the constant itself,
`:239`, is 1,000 ns, the slack the engine reads beyond a range's bounds).

**Wide tables do not have one schema.** In the 9.6 h recording below, the
per-task table `cpu_usage/cpu_usage_task` has 159 segments and 159 distinct
column sets: at most 2,851 columns in one segment, 6,648 in their union.
Every other table (48 of 49) has one schema across its segments. A wide
column is named by an id (`43x1`, `269:buckets`); its metric name, type,
unit and labels are in the Arrow field metadata.

## Measured

Host: Apple M4 Max, macOS 26.6.2. Archive:
`metrics-9.6h.dendro`, one 9h37m56s rezolus recording, 1,272 MB in 5,874
segments over 49 tables, no WAL rows (the storage scan entry's gate
archive). `long.dendro` is its per-task table rewritten long. Probe programs
were scratch code, not committed: a Rust binary over the crates at d2bf74d,
and a PyO3 extension (pyo3 0.28.3, arrow and arrow-pyarrow 58.2.0, built by
maturin 1.15.0 as an abi3 wheel) with two functions, a table read and a range
query. Python 3.12, pyarrow 26.0.0, polars 2.0.0.

**Full read of every table, in Rust.** Every sealed segment of every table
fetched from the catalog and decoded to Arrow record batches, plus each
table's WAL tail, batches dropped after counting:

| run | total | fetch | decode | peak RSS |
|---|---|---|---|---|
| 1 | 6.30 s | 0.77 s | 5.34 s | 218 MB |
| 2 | 5.85 s | 0.26 s | 5.40 s | 223 MB |
| 3 | 7.05 s | 0.36 s | 6.42 s | 232 MB |

1,665,118 rows. Decoded Arrow (`get_array_memory_size`) is 9,684 MB from
1,272 MB on disk; holding every batch instead of dropping it peaked at 8,539
MB RSS. Histograms are most of it: the five largest tables are histogram
tables (2,206 MB for `syscall_latency`). Each histogram cell is a
`List<UInt64>` of 496 per-bucket counts, each cumulative since the producer
started (grouping power 3, max value power 64, from the field metadata); in
one mid-recording segment the rows averaged 0 to 315 non-zero buckets, by
histogram column. A full read of `long.dendro` (the long table's 76,446,307
rows and its occupant stream) took 1.53 s in one run, and its occupant
stream held 264,286 rows. A Python consumer that reads whole tables gets
these sizes; the API reads one table, and a time range, at a time.

**Queries, in Rust, against `rezolus mcp query`.** Full range, step 1 s, one
process per run, three runs (open is 17 ms):

| query | probe query | probe to Arrow | points | `rezolus mcp query` wall |
|---|---|---|---|---|
| `sum by (id) (irate(cpu_usage[5m])) / 1000000000` | 62-66 ms | 2.6 ms | 554,816 | 0.09 s, 0.08 s |
| `sum(irate(network_bytes[1m]))` | 10-11 ms | 0.15 ms | 34,676 | 0.03 s |
| `histogram_quantiles([0.5, 0.99], syscall_latency)` | 5.87-7.93 s | 0.3 ms | 69,354 | 5.40 s, 5.44 s |
| `sum(irate(task_cpu_usage[5s]))` | 4.39-4.47 s | 0.15 ms | 34,676 | 3.86 s, 3.90 s |
| `sum by (comm) (irate(task_cpu_usage[5s]))` | 1.13-1.19 s | 30.4-30.7 ms | 5,326,720 | 4.08 s, 4.16 s |

The `rezolus` binary is 6.0.0-alpha.41. The column is for reference; that
binary uses an older metriken-query than d2bf74d. The probe ran the last
two queries in one process, so the per-task table was already open and
cached for the last one; the CLI opens it for each. Converting a result to
long-form Arrow costs at most 31 ms, at 5.3 million points.

**Through Python.** The prototype's `query_range` opens the archive, runs the
query with the GIL released, converts to Arrow, and hands a
`RecordBatchReader` to pyarrow; two runs each:

| query | Python wall | rows |
|---|---|---|
| `sum by (id) (irate(cpu_usage[5m])) / 1000000000` | 0.11 s, 0.12 s | 554,816 |
| `sum(irate(network_bytes[1m]))` | 0.02 s, 0.03 s | 34,676 |
| `sum by (comm) (irate(task_cpu_usage[5s]))` (table not cached) | 4.72 s, 4.42 s | 5,326,720 |

Joining 179 series' labels onto 5.3 million rows in polars took 13-18 ms.
Two of the first query on two Python threads took 0.10-0.11 s against
0.16-0.17 s run one after the other, so the GIL is released during the
query.

**Zero-copy handoff, verified.** Reading `tcp_connect_latency`'s table
(34,678 rows, 116 segments, 139 MB as pyarrow counts it) grew RSS by 148-150
MB and pyarrow's allocator by 0.0 MB: the buffers pyarrow holds are the
ones Rust allocated, passed through the Arrow C stream interface. For the
`blockio_latency` total-latency table (552 MB) RSS grew 570 MB and
pyarrow's allocator 0.0 MB. `polars.from_arrow(table, rechunk=False)` on
that pyarrow `Table` (from the reader's `read_all()`) then took 2-3 ms and
grew RSS by 7-9 MB, keeping all 116 chunks; on a one-batch table
the timestamp column's buffer address in polars equals pyarrow's. Field
metadata (metric name, labels, histogram configuration) reaches pyarrow's
schema. polars 2.0.0 has no field metadata on a DataFrame's schema, so a
polars user cannot recover which metric a wide column is from the table
alone.

**The wheel.** macOS arm64, abi3 for CPython 3.9 and later: 4.5 MB
compressed, an 11.5 MB stripped extension, with metriken-storage (`rez`),
metriken-query, arrow-pyarrow and pyo3. The extension exports one symbol,
`PyInit_metriken_proto`; none of the bundled SQLite's `sqlite3_*` symbols
are exported. Python's own `sqlite3` module (SQLite 3.49.1) imports in the
same process.

**Not measured:** Linux wheels (size, the manylinux build, symbol
visibility); a live archive's WAL tail cost at open; duckdb's reading of an
Arrow stream from the bindings; whether `polars.DataFrame(x)` on the
bindings' capsule object is zero-copy (the prototype went through a pyarrow
`Table`); Windows; free-threaded CPython; any consumer's workflow end to
end.

## Design

### Scope

Read-only. The bindings open a file and read it; they do not record, write,
annotate or convert. A Python program that wants to record runs
`rezolus record` or a producer that records itself (path step 4,
[metriken-recorder](2026-10-12-metriken-recorder.md)).

**Containers: `.dendro`, `.rez` (v2 tar and v3) and parquet, by content.**
Recordings that exist today are parquet and `.rez`: systemslab artifacts
from 5.x agents, and the files the scripts above read. A `.dendro`-only
reader would leave those scripts with a second reader for the files they
already have, which is the situation the plan is removing. The cost is
storage's `rez` feature (tar, tempfile, serde_json), since SQLite is already
compiled for dendro; the prototype carried it in its 4.5 MB wheel. A file is
recognized by checking for parquet's `PAR1` magic first, then in the order
`open_recordings` uses (dendro, then `.rez` v3, then tar `.rez`). The
parquet check comes first because `open_recordings` hands any file it does
not recognize to the tar reader (`rez/open.rs:115-127`), so a parquet file
passed to it fails with a tar error.

**Two kinds of call, as the plan says,** plus the catalog that makes the
second usable:

1. **PromQL**, range and instant, returning long-form rows. These are the
   columns `rezolus mcp`'s `export_query` writes (`src/mcp/export.rs`:
   series, timestamp, value, lo, hi), with series as an id into
   `res.series` and timestamps in nanoseconds; `export_query` writes the
   rendered label string and f64 seconds.
2. **Table rows**, a table's stored columns for a time range, with a long
   table's occupant labels available as a join.
3. **The catalog:** recordings and their labels and metadata; tables, with
   layout (wide or long), row count, segment count and span; and for each
   table the column map (column name to metric, type, unit, labels),
   which is the field metadata as a table, because polars drops field
   metadata.

### Python API sketch

```python
import metriken

a = metriken.open("run.dendro")        # any container, by content
a.recordings                           # [{"source": "rezolus", "host": ...}, ...]
r = a.recording(source="rezolus")      # exactly one match, else ValueError listing them
r.metadata                             # dict
r.time_range                           # (start_ns, end_ns)
r.complete                             # False for an unfinalized archive

res = r.query_range("sum by (id) (irate(cpu_usage[5m]))",
                    start=None, end=None, step="1s",
                    rate_mode="grid", rate_span=None)
res.samples                            # Arrow: series u32, timestamp, value, lo, hi
res.series                             # Arrow: series u32, one utf8 column per label key
res.to_polars()                        # samples joined to series
r.query("cpu_cores", time=None)        # instant: same two tables, one row per series

r.metrics()                            # Arrow: name, type, unit, table, series count
r.tables()                             # Arrow: table, layout, rows, segments, first_ts, last_ts
r.columns("cpu_usage/cpu_usage_task")  # Arrow: column, metric, type, unit, labels...
r.read_table("syscall_latency/syscall_latency_latencies", start=None, end=None)
                                       # iterator of Arrow record batches, WAL tail last
r.occupants("cpu_usage/cpu_usage_task")  # long tables: occupant u64, one column per label key
r.read_table(t, occupant_labels=True)  # long tables: the occupant's labels joined in Rust

metriken.histogram.bucket_bounds(grouping_power=3, max_value_power=64)
                                       # Arrow: index, lower, upper

hs = r.histograms('syscall_latency{op="read"}', start=None, end=None, stride=None)
hs.grouping_power, hs.max_value_power  # from the stream's histogram::Config
hs.buckets                             # Arrow: timestamp, index list<u32>, count list<u64>
                                       # per tick, per-bucket counts in that tick

# with `pip install metriken[h2histogram]`
hs.to_h2()                             # [(timestamp_ns, h2histogram.SparseHistogram), ...]
hs.to_h2(dense=True)                   # ... h2histogram.Histogram instead
metriken.h2.from_column(r.read_table(t), "269:buckets", delta=True)
                                       # one stored column, row by row:
                                       # [(timestamp_ns, SparseHistogram | None), ...]
```

Every Arrow object implements the Arrow PyCapsule interface
(`__arrow_c_stream__`, `__arrow_c_schema__`), so `pyarrow.table(x)`,
`polars.DataFrame(x)` and duckdb take it without the bindings depending on
any of them. `polars.from_arrow(x)` on such an object returns a `Series` of
`Struct`, not a `DataFrame` (polars 2.0.0, tried on a pyarrow table wrapped
to expose only `__arrow_c_stream__`). pyarrow's acceptance without a copy
is measured above; polars' and duckdb's through the capsule are not. The
prototype used arrow-rs's `pyarrow` feature, which calls into pyarrow and
so requires it; the capsule exporter needs only `arrow::ffi_stream`, either
written here or taken from the `pyo3-arrow` crate. pyarrow becomes an
optional dependency for the `to_pyarrow`/`to_polars` conveniences.

Timestamps are Arrow `timestamp[ns, UTC]` in query results, converted from
the engine's f64 seconds, and the stored `UInt64` nanoseconds in table rows.
Selectors (`recording(**labels)`) have the semantics of `rezolus mcp
--recording`: ANDed, subset match, exactly one recording or an error naming
the candidates.

### Long tables and occupants

The plan rules out flattening an archive into one wide schema, and the 9.6 h
recording shows why for wide tables too: the per-task table's segments
share no single schema, and their union is 6,648 columns where a segment has
at most 2,851. So:

- `read_table` returns the segments' batches as stored. A table whose
  segments share one schema (48 of 49 tables in the 9.6 h recording) is one
  Arrow stream. A table whose segments differ is returned as an iterator of
  batches with their own schemas, never null-filled to a union. A caller that
  wants one shape for such a table asks for a metric (`query_range` with no
  function, or the series read in "Left alone").
- A long table is read as stored: `timestamp`, `occupant`, one column per
  metric. Its occupant stream is a dimension table (an occupant never changes
  labels), so `occupants(t)` returns one row per occupant, and
  `occupant_labels=True` joins it in Rust as dictionary-encoded columns.
  264,286 stream rows named the long table's occupants in `long.dendro`.
- PromQL over a long table relabels as the engine does today; the bindings
  add nothing.

### Tables by container

The three containers expose rows differently (see "What metriken
provides"), so `tables()`, `read_table` and `occupants` are defined per
container:

| container | `tables()` | `read_table(t)` | `occupants(t)` |
|---|---|---|---|
| `.dendro`, `.rez` v3 | the `Catalog`'s tables for the recording, occupant streams included | sealed segments in sequence order, then the WAL tail through `materialize_wal_tail` (wide) or `materialize_long_wal_tail` (long) | the table `<t>/occupants`, sealed segments and WAL rows (`occupants::decode_wal_row`), one row per occupant |
| tar `.rez` | the `InMemorySource`'s table names | its segments, oldest first; no WAL tail | `ValueError`: a tar `.rez` has no long tables |
| parquet | one table, `parquet`, layout wide | the file's row groups, through the parquet crate's `ParquetRecordBatchReaderBuilder` | `ValueError`: a parquet file has no long tables |

Segment and span counts in `tables()` come from `segment_meta` for a
catalog and from the segment list for a tar `.rez`. For a parquet file the
segment count is the row-group count from the file's parquet metadata.

### Histograms

- **In table rows:** the stored `List<UInt64>` of per-bucket counts, each
  cumulative since the producer started, zero-copy, with `grouping_power`
  and `max_value_power` in the column map.
  `bucket_bounds` computes each bucket's bounds with the `histogram` crate,
  so no consumer reimplements the H2 index arithmetic, as the `.rez` script
  above does. Differencing consecutive rows is the caller's, in polars or
  numpy; a helper is added only if a consumer needs it.
- **In queries:** the engine's histogram functions, through the same
  long-form result (`histogram_quantiles` gives one series per quantile).
  `QueryResult::HistogramHeatmap` maps to an Arrow table of (timestamp,
  bucket bound, count) from its `timestamps`, `bucket_bounds` and `data`.
- **As h2histogram objects** (decided 2026-10-12: 0.1.0 converts, behind an
  optional extra). Below.

### Histograms as h2histogram objects

h2histogram-py is an optional dependency: `pip install metriken[h2histogram]`
adds `h2histogram` (the PyPI name; the repository is
iopsystems/h2histogram-py). Without it, everything above works and
`to_h2`/`metriken.h2` raise `ImportError` naming the extra. The bindings
build no Python histogram type of their own.

**Configuration mapping.** metriken's histograms are the `histogram`
crate's (1.4.1 in metriken's lockfile). A stream carries a
`histogram::Config` (`HistogramStreamMeta::config`,
`metriken-storage/src/histogram_stream.rs:7`) and a stored column carries
`grouping_power` and `max_value_power` in its field metadata. Both map
one to one onto `h2histogram.Config.new(grouping_power, max_value_power)`,
which enforces the crate's constraints (`max_value_power <= 64`,
`grouping_power < max_value_power`) and produces the same bucket count (496
for (3, 64)) and the same index-to-range arithmetic. A bucket index in an
Arrow `index` list is the crate's index and is passed through unchanged.

**Two conversions, for the two kinds of read:**

1. **`r.histograms(selector, start, end, stride)`**, the per-tick histograms
   the engine computes quantiles from. For each series matching the
   selector the engine takes the per-bucket difference between consecutive
   rows (saturating at zero, so a bucket that went down contributes
   nothing), sums those differences across series per tick, or per stride
   when one is given, and answers `histogram_quantiles` from the sum
   (`quantiles_impl`, `histogram.rs:270`). `histogram_heatmap` does the same
   accumulation in a second copy (`heatmap_impl`, `:685`), and `reduce`
   (`:435`; count, sum and mean, grouped) in a third. The heatmap
   then subtracts
   the lowest occupied bucket's index from every index and returns no
   configuration, so its output cannot be passed to h2histogram as it is.
   Step 4 below adds a public function to metriken-query that takes
   `(source, selector, start, end, stride)`, the selector as a PromQL
   selector string, and yields `(timestamp_ns, index, count)` per tick;
   `quantiles_impl`, `heatmap_impl` and the bindings all use its per-tick
   accumulation. The selector parser (`QueryEngine::parse_metric_selector`,
   private, `promql/mod.rs:591`) and the per-tick accumulation are
   metriken-query's; a caller could fetch the stream from metriken-storage
   (`DataSource::histogram_stream`, public through
   `ArchiveReader::owners_of`, `SamplerReader::reader()` and
   `TableReader::source()`) but would reimplement the accumulation, which
   check 8 must test one copy of. `heatmap_impl` disagrees with the other
   two when a stride is given.
   After a stride with no counts, `flush_accum` clears the accumulator and
   returns false. `quantiles_impl` moves `stride_last_emit` to that tick
   anyway (`histogram.rs:352-369`), so its next point is a full stride
   later. `heatmap_impl` moves it only when `flush_accum` returns true
   (`:799-822`), so it emits at the next tick with any counts, a point
   covering less than a stride, and its strides restart from there. The
   shared function takes `quantiles_impl`'s rule, which changes
   `histogram_heatmap` with a stride after an empty stride; that is a
   behaviour change. It still ships in a 0.36.x release after 0.36.1: no
   public type changes, the change makes a heatmap and the quantiles of the
   same selector and stride emit at the same timestamps, and a point that
   covers less than a stride is the defect being removed. rezolus's viewer
   requests a strided `histogram_heatmap` whenever the range has more ticks
   than its pixel budget (`src/viewer/assets/lib/data.js:1396-1402` at
   15c77844), so its zoomed-out heatmaps move to `histogram_quantiles`'
   timestamps after an empty stride, matching the quantile charts beside
   them. cachecannon (metriken-query 0.9) and systemslab (0.30.0) do not
   take 0.36.x. The changelog entry lists it under fixes and says
   which timestamps move. That release adds a test of `histogram_heatmap`
   with a stride over a series with an empty stride, asserting that its
   timestamps equal `histogram_quantiles`'; the existing histogram tests in
   metriken-query guard the rest of the refactor. Then check 8
   below tests the conversion and h2histogram's arithmetic, and not two
   copies of the accumulation. The result is Arrow first
   (`hs.buckets`), with one row per tick (or stride) that has at least one
   non-zero bucket after the difference, the same ticks at which the engine
   emits a point: `flush_accum` returns false for an empty tick and the
   engine emits nothing for it. A tick with no difference has no row, so
   `hs.buckets` and `to_h2()` never contain an empty histogram.
   `hs.to_h2()` builds one
   `h2histogram.SparseHistogram.from_parts(config, index, count)` per tick,
   or with `dense=True` a `Histogram` via `to_dense()`. Sparse is the
   default because every bucket becomes a Python `int`, and a stored row in
   the 9.6 h recording averaged 0 to 315 non-zero buckets of 496, by
   column; a tick's difference has no more non-zero buckets than the later
   row.
2. **`metriken.h2.from_column(batches, column, delta=True)`**, one stored
   column from `read_table`. Each cell is a `List<UInt64>` of 496 counts
   cumulative since the producer started. With `delta=True` (the default)
   each non-null cell is differenced per bucket from the last non-null cell
   before it, saturating at zero, which is what the engine does: the
   histogram stream skips a null cell
   (`metriken-storage/src/parquet.rs:2916-2918`) and `quantiles_impl` keeps
   each series' previous snapshot (`prev_per_series`), so it differences
   across the gap. A null cell, and the first non-null cell, give `None`; a
   difference with no non-zero bucket gives an empty `SparseHistogram`. The
   engine emits no point for either (`flush_accum` returns false for an
   empty tick), and check 8 counts a missing engine point as equal to
   `None` or an empty histogram at that timestamp.
   `h2histogram.Histogram.subtract` is not used for this, because it raises
   on a negative bucket, which a producer restart produces. With
   `delta=False` each cell becomes the since-start histogram, which is what
   h2histogram-py's own `read_histograms` returns. The differencing and the
   non-zero scan run in Rust over the Arrow buffers; Python receives
   `(index, count)` lists. A long table's histogram column is per occupant,
   so `from_column` differences within an occupant, never across occupants.
   A batch whose schema lacks the column (a wide table's segments do not
   share one schema) is read as a null cell in each of its rows: each gives
   `None`, and the next non-null cell is differenced from the last non-null
   cell before the batch. The engine does the same: a segment without the
   metric contributes no rows to the spliced histogram stream
   (`segmented.rs:1604-1609`), so `prev_per_series` (`histogram.rs:298`)
   carries across it. A column whose `grouping_power` or `max_value_power`
   changes between batches raises `ValueError`; the engine splits such a
   metric into runs labelled `__run__`, and a selector without `__run__`
   reads the first run only (`segmented.rs:1612-1637`).

The conversion copies: an h2histogram object holds Python lists. A caller
who wants zero-copy stays with `hs.buckets` or the stored column. The cost
per tick is not measured.

**Percentile conventions agree.** The engine's value for quantile q is the
end of the bucket at rank `max(1, ceil(q * total))` over the cumulative
counts, with the band `[start, end]` of that bucket
(`apply_quantiles`, `histogram.rs:914`; rank in histogram 1.4.1
`cumulative.rs:599`). h2histogram's `percentile(q)` returns a `Bucket` with
inclusive `start`/`end`, at rank 1 for q = 0, `total` for q = 1, and
`max(1, ceil(q * total))` otherwise (`_analytics.py:55`), which is the same
bucket for totals up to 2^53, from metriken-query 0.36.1 on, which stops
capping counts at u32::MAX (below). So the value from h2histogram is
`bucket.end`, and the band is `(bucket.start, bucket.end)`.

**metriken-query 0.36.0 caps counts at u32::MAX.** `flush_accum`
(`histogram.rs:893-912`) clips each running cumulative count to u32::MAX
in a `u32` scratch buffer, and three paths read that buffer:
`apply_quantiles` builds a `CumulativeROHistogram32Ref` from it (`:923`);
the reduce path behind `histogram_count`, `histogram_sum` and
`histogram_mean` does the same (`reduce`, `:493`, `:533`, `:557`, `:571`);
and `heatmap_impl` builds no histogram type, but differences consecutive
entries of the clipped `u32` buffer to get each bucket's count (its
`emit_tick` closure, `:720-746`). Any tick or stride whose summed count
exceeds 2^32 gets the wrong answer, from Python or not. Reproduced on
2026-10-12 against metriken-query at d2bf74d (a scratch binary over
`MemoryStore`): one series, an empty row then a row with 4e9 observations
in bucket 100 and 1e9 in bucket 200, config (3, 64).
`histogram_quantiles([0.85], ...)` returns the band [24576, 26623] (bucket
100) where the counts give [134217728, 150994943] (bucket 200);
`histogram_count` returns 4,294,967,295 for 5e9; the heatmap gives bucket
200 a count of 294,967,295 for 1e9. The same counts through histogram
1.4.1's u64 `CumulativeROHistogramRef` (`cumulative.rs:644-652`) give
bucket 200.

The fix is a separate patch release, metriken-query 0.36.1, not part of
this entry's step 4: `flush_accum` keeps the running count as `u64`,
`apply_quantiles` and the reduce path read it through
`CumulativeROHistogramRef`, and `heatmap_impl` differences the `u64`
buffer. No public type changes. It adds a test with a tick of 5e9
observations, the case above. As of 2026-10-12 it is merged
(iopsystems/metriken#261) and not yet released. Check 8 is exact equality only against 0.36.1 or
later.

### Binding technology

PyO3 0.28 and maturin, a `cdylib` built as one abi3 wheel per platform (the
prototype built `cp39-abi3`). wasm32 is not a target. Four wheels (decided
2026-10-12):

- Linux x86_64 and aarch64, manylinux_2_28, built by `PyO3/maturin-action`
  (SQLite and zstd are C, compiled by `cc` in the manylinux container);
- macOS arm64 and x86_64, deployment target 11.0 (the prototype linked
  SQLite and zstd objects built for the host's 26.4 SDK with warnings until
  `MACOSX_DEPLOYMENT_TARGET=11.0` was set; CI sets it).

Windows, musllinux and free-threaded CPython (3.13t, 3.14t, which abi3 does
not cover) get no wheel in 0.1.0. The release also uploads an sdist, which
needs a Rust toolchain to install; on a platform without a wheel, pip builds
from it, and that build is not tested.

`requires-python >= 3.10`: 3.9 has been end-of-life since October 2025.
macOS's `/usr/bin/python3` is 3.9.6, so a user of the system Python
installs another one first.

The crate is `metriken-py/` in this repository with its own workspace
(`exclude` from the root workspace) and lockfile. Linking is not the
reason: with a build script that calls
`pyo3_build_config::add_extension_module_link_args()`, plain `cargo build`
and `cargo test` link a PyO3 extension on macOS (tried on a one-function
crate, pyo3 0.28.3). The reasons are the root CI's jobs, which run
`cargo nextest --all-targets --all-features` on Linux, macOS and Windows
and `cargo hack --feature-powerset`:

- a Rust test that calls into Python needs libpython linked, which the
  `extension-module` feature leaves out, so under `--all-features` such
  tests fail to link or need a feature the wheel build must not have;
- pyo3's build script needs a Python interpreter on every runner and every
  feature combination `cargo hack` builds;
- on Windows, an abi3 extension links against an import library for
  `python3.dll`, which comes from a Python install or from pyo3's
  `generate-import-lib` feature; the root Windows job has neither, and
  0.1.0 builds no Windows wheel.

A separate workspace keeps those jobs as they are and gives the bindings
their own CI. The root `cargo fmt --all` and `cargo clippy` do not cover an
excluded crate either, so the bindings' CI runs both itself. It depends on
`metriken-storage` and `metriken-query` by path, so it always builds
against the crates on the same commit, and it is not published to
crates.io.

### Threads, the GIL, and processes

- Every call that reads or queries releases the GIL (`Python::detach`).
  Measured above: two queries on two threads overlap.
- An open recording is a `#[pyclass]` that is `Send + Sync`
  (`MetricsSource` requires both), so Python threads can share it. From
  the code: a file-backed archive gives each table its own catalog
  connection behind a `Mutex` (`DbHandle::Reopen`,
  `metriken-storage/src/reader.rs:225-245`), so segment fetches for one
  table serialize and different tables read in parallel. An archive opened
  from bytes shares one `Mutex` across all its tables
  (`DbHandle::Shared`, `reader.rs:1195`), so its fetches serialize. Not
  measured; the measurement above used two separate opens.
- `read_table` cannot use the engine's connection: the per-table store
  that holds it (`DbSegmentStore`) is private. It follows the same rule
  with its own connections. For a file, each table gets its own catalog
  connection behind its own `Mutex`, opened on that table's first
  `read_table` and kept by the recording, so `read_table` on two tables
  runs in parallel and on one table serializes. For an archive opened from
  bytes, the engine's shared `Arc<Mutex<Box<dyn Catalog>>>` is private: the
  public `ArchiveReader::from_catalog(db: Box<dyn Catalog>, reopen, pool,
  index)` wraps the catalog it is given in that `Mutex` itself
  (`reader.rs:1189-1196`). The bindings therefore open the catalog
  themselves (`DendroCatalog::open_bytes` or `RezDb::open_bytes`, chosen by
  `Container::recognizes_bytes`, as `open_recordings_from_bytes` does), hold
  it in their own `Arc<Mutex<…>>`, and pass `from_catalog` a small `Catalog`
  that locks that `Mutex` and delegates each method. The engine and
  `read_table` then share one connection and one copy of the bytes, and
  `read_table` on every table serializes with the engine's fetches, as the
  engine's tables already do with each other. A second `open_bytes` for
  `read_table` would avoid the delegating type but hold a second in-memory
  copy of the archive, as large as the file, for as long as the recording
  is open. Locks are always taken in one order (the engine's, then the
  bindings'; `read_table` takes only the bindings'), so the two cannot
  deadlock. Calls release the GIL before taking either lock and do not
  reacquire it while holding one, so the GIL cannot form a cycle with
  them.
- After `fork`, a SQLite connection opened in the parent must not be used in
  the child. Python's `multiprocessing` forks by default on Linux before
  3.14. A recording remembers the pid that opened it, and a call from another
  pid raises an error saying to open the file in the child. The check is on
  every object that holds a reference to a reader or a connection, not only
  the recording: a `read_table` iterator, a view returned by `reopen()`, a
  histogram result if it reads lazily. Each checks the pid when called and
  when dropped. Drop in another pid leaks what the object holds: it takes
  no lock, because a per-table `Mutex` held by another parent thread at the
  fork stays locked in the child, and it closes no connection. Recordings
  are not picklable (`__reduce__` raises `TypeError` saying to pass the
  path), so `multiprocessing` workers open the file by path.
- Arrow buffers handed to Python are owned by Rust allocations released when
  the last Python reference goes; they outlive the recording object.

### SQLite in the wheel

dendro bundles SQLite (rusqlite `bundled`), so the wheel carries its own copy
next to whatever Python's `sqlite3` module links. The extension exports only
its init symbol on macOS (measured), so the two do not resolve to each other.
SQLite's documentation warns that two copies of the library in one process
open on the same database file can break POSIX advisory locks, because
closing one copy's file descriptor releases the other's locks. The bindings
only read, and the risk has two forms:

- a writer in the same process: a script that also opens the archive with
  `sqlite3` for writing, or a recorder embedded in the same Python process;
- a writer in another process (a recorder, hindsight) while this process's
  `sqlite3` module opens and closes the same file, even read-only. The
  close drops every POSIX lock this process holds on the file, including
  the bindings' SQLite's lock on its WAL read mark. The writer then sees no
  reader, and a checkpoint can overwrite database pages the bindings are
  reading.

The bindings' documentation says not to open a file with `sqlite3` while
the bindings have it open.

### A file still being written

A recording opened while a recorder or hindsight writes it is a fixed view.
A recording's table list and spans are read at open. Each table's segment
list and WAL tail are read the first time that table is queried, and are
fixed from then on (`SamplerReader::reader()` builds them once, in a
`OnceLock`, through `DbSegmentStore::build`, `reader.rs:311-341`). Rows
written later are not seen. `read_table` keeps its own view per table by
the same rule: on a table's first `read_table` it reads `segment_meta` and
`live_wal` inside one SQLite read snapshot, and every later `read_table` of
that table reuses that list and that tail. Two separate statements lose
rows: a seal that commits between them is missing from the `segment_meta`
read, and the watermark it installed hides its rows from the `live_wal`
read (dendro 0.4.0 `read.rs:376-381`; dendro's own readers use
`Archive::read_snapshot`). The `Catalog` trait has no read transaction,
and `DendroCatalog::segment_meta` and `live_wal` are separate statements
(`catalog.rs:207-247`). So either the trait gains a method that runs a
closure inside one snapshot, or the bindings hold the concrete catalog and
call its snapshot: `DendroCatalog`'s `Archive` is a public field with
`read_snapshot`, and `RezDb` has its own (`rez_sqlite.rs:878`). The
engine's view of a table and `read_table`'s are taken at different
moments, so on a file being written a query and a `read_table` of the same
table can differ by the rows written in between. `columns()` reads the
field metadata of the segments in `read_table`'s view, so it is fixed with
that view. It cannot use the engine's per-table metadata: that is the
private `SamplerReader::metric_metadata` (`reader.rs:669`), keyed by metric
rather than by column. That function builds a new segment store on every
call (`SegmentSource::store`, `reader.rs:382-408`), so the public
`ArchiveReader::metric_metadata` built on it is read at each call and is
not fixed at the table's first query. `metrics()` takes units from
`ArchiveReader::metric_metadata`, so they are read at each call. `r.reopen()` returns a new view, using
`ArchiveReader::keep_handover` and `reuse_from` so segments already read
are not read again, as rezolus's `crates/rez/src/live.rs` does.
`r.complete` is false until the writer finalizes. The open-time cost of a
large WAL tail is not measured.

A held view can lose rows. The view lists a table's segments when the table
is first read and fetches their bytes later, so a segment that hindsight's
retention deletes in between is gone. Queries already handle this: an
occupant stream skips a missing segment (`reader.rs:431-446`), and a table
with no segments and no tail left is reported as absent with a warning
(`reader.rs:726-770`). `read_table` does the same: it skips a segment whose
bytes are gone, and the iterator's `missing_segments` count, read after it
is exhausted, says how many were skipped, so a caller can tell a short read
from a short table.

A held view can also read the wrong rows, and `missing_segments` does not
show it. A sequence number in the view's list can name different bytes by
the time they are fetched: dendro's compaction gives a merged segment the
sequence number of the first segment it replaced (`compact_stream`, dendro
0.4.0 `rewrite.rs:196`, and the comment on `segment_key`,
`reader.rs:276-283`), and numbering restarts for a stream whose segments
were all evicted. In a single pass in sequence order a merge cannot repeat
rows already read, because the merged segment keeps the lowest number of its
run. It can still return more than was listed: the merged segment holds the
whole run where the list expected only the run's first segment, and the
run's later numbers then read as missing although their rows arrived. A
restarted number can return a segment sealed from WAL rows that the view's
materialized tail already holds, and those rows are returned twice.

The engine has the same exposure: its `DbSegmentStore::bytes` fetches by
the listed number and does not compare what comes back with the listed
`segment_key`, and nothing in `splice_histogram_streams` drops overlapping
rows. Whether a table read by two queries, one before a merge and one
after, can combine a cached decode of a segment with the merged segment's
copy of the same rows was not checked. The engine can also drop a
concurrent seal's rows: `DbSegmentStore::build` calls `segment_meta` and
then `live_wal` as separate statements (`reader.rs:311-341`), the race
dendro documents above. Both are from reading the code, not reproduced.
Both are metriken-storage bugs, independent of the bindings, and both are
tracked in the plan entry's step 7 follow-ups
([one recording stack](2026-10-08-one-recording-stack.md)).

`read_table` checks each fetched segment against the `segment_meta` it
listed. Those `rows`, `first_ts` and `last_ts` describe the WAL rows the
seal consumed, not the segment's parquet rows (except for a segment that
dendro 0.4.0's compaction produced, whose `rows` is its parquet row count;
dendro 0.4.1 writes the WAL count, and the wide check's "≤ rows" holds for
either): metriken-storage's encoder
records the WAL row count and the first and last WAL row's timestamps
(`encoder.rs:117-119`, `:146-149`), and the seal hands it the stream's
live WAL ordered by timestamp (dendro 0.4.0 `archive/sqlite.rs:1540`). A
long segment has one parquet row per occupant per tick, and an occupant
stream one per occupant decoded from each WAL row. An acquisition group's
wide segment skips WAL rows that have no schema anchor
(`wal.rs:314-370`), and a long one skips those and rows with no occupants
(`:425-429`). A long segment written with `sort_long` is sorted by
(occupant, timestamp), so its first parquet row is not its earliest. The
check therefore uses the minimum and maximum timestamp over the decoded
rows: both must lie in `[first_ts, last_ts]`,
and they equal those bounds unless a leading or trailing WAL row was
skipped. For a wide table the parquet row count must also be at most
`rows`. It equals `rows` when no row was skipped, but a segment does not
record whether one was, so equality is not required. Long tables and
occupant streams are checked on timestamps only. A failed check raises an
error saying to `reopen()`.

The check catches every merge dendro's compaction makes. A merged segment
would pass only if all its rows lay inside the first listed segment's
span. `compact_stream` merges at least two adjacent segments (dendro 0.4.0
`rewrite.rs:161`), and a seal encodes only rows later than the stream's
newest sealed row (`LIVE_WAL_PREDICATE`, `archive/sqlite.rs:465`), so a
stream's segments do not overlap in time and the merged segment's latest
row is later than the listed `last_ts`. A restarted number is caught the
same way, because the new segment's rows are later than the evicted
segments the view listed. A segment replaced by one with the same rows
over the same span would pass and return those rows; dendro's compaction
leaves a run of one alone. This is from reading the code, not tested.
Nothing in metriken origin/main outside a test, or in rezolus 15c77844,
calls dendro's compaction.

### Packaging and publishing

- **Name** (decided 2026-10-12): `metriken` on PyPI and as the import name.
  It was unclaimed on 2026-10-12 (as were `metriken-py` and `rezolus`;
  `dendro` is taken). The Rust producer crate has the same name; the
  package's description says it reads recordings and does not record.
- **Extras:** `h2histogram` (adds `h2histogram`, for `to_h2` and
  `metriken.h2`), `pyarrow` and `polars` (for `to_pyarrow`/`to_polars`).
  The base install depends on nothing.
- **Version** (decided 2026-10-12): its own semver, starting at 0.1.0, not
  tied to the crates'.
  `metriken.build_info()` reports the `metriken-storage` and
  `metriken-query` versions it was built with. A change to the Python API is
  a minor version while 0.x. A storage or query release that changes what a
  recording reads as (a fix, a new container) is followed by a bindings
  release; one that does not, is not.
- **Release:** a tag `metriken-py-vX.Y.Z` (this repository's tag form,
  decided 2026-10-12)
  triggers a GitHub Actions workflow that builds the wheels and sdist,
  installs each wheel in a clean environment, runs the Python tests, and
  publishes with PyPI trusted publishing (no token in the repository).
- **CI on pull requests:** build the Linux x86_64 wheel, run the Python
  tests, and run `cargo fmt --check` and `cargo clippy` in `metriken-py/`
  (the root jobs do not cover an excluded crate) when `metriken-py/`,
  `metriken-storage/`, `metriken-query/`, `metriken-model/`,
  `metriken-types/` or the root `Cargo.toml` (its
  `[workspace.dependencies]`) changes.

## GO / NO-GO

GO for release 0.1.0 when all of these hold. Each runs in CI except 5,
which runs on one host and is recorded here, and 7, which runs in the
release workflow on each wheel's platform.

1. **Same values as the engine.** Two fixture sets:
   - (a) metriken-storage's own test archives (wide, long with an occupant
     stream, a WAL tail);
   - (b) rezolus's compatibility fixtures (rezolus
     `docs/journal/2026-10-08-6-0-release-readiness.md`, § "Compatibility
     (blocks 6.0.0)": a tar `.rez`, a v3 `.rez`, a 5.25 `.dendro`, a 5.x
     parquet and a 6.0 recording, not yet checked in as of 15c77844).

   On each, `query_range` returns the same series, timestamps (to the
   nanosecond after conversion) and values and bands (bit for bit) as
   `MetricsSource::query_range_opts` called from Rust on the same commit,
   for the golden queries. (a) gates 0.1.0. (b) gates 0.1.0 if the owner
   answers question 6 "wait", and gates the first 0.1.x after the fixtures
   are checked in otherwise; until question 6 is answered, (b) gates 0.1.0.
   Against `rezolus mcp query` of a rezolus release built on the same
   metriken-query version: that command prints a summary per series
   (`format_query_result`, `src/mcp/mod.rs:1024` at 15c77844), so the
   comparison is per series, of the point count, the first and last
   timestamp and value, and the minimum and maximum, all exact (it prints
   f64 with `{}`, which round-trips). It prints a series' `intervals`
   `[lo, hi]` only beside the First and Last lines, with `{:.6}`, so the
   bands compared are those two points', after rounding to six decimals;
   the bands at every other point are compared only against Rust in (a)
   and (b). This is the plan's criterion 4.
2. **Tables complete.** For every table of every fixture, the rows from
   `read_table` equal what the container holds: for a `.dendro` and a `.rez`
   v3, the sealed segments from the `Catalog` plus the WAL tail from
   `materialize_wal_tail` for a wide table or `materialize_long_wal_tail`
   for a long one, and for an occupant stream its segments plus its WAL rows
   through `occupants::decode_wal_row`; for a tar `.rez`, the
   `InMemorySource`'s segments; for a parquet, the rows
   `pyarrow.parquet.read_table` gives for the file. `read_table(t,
   occupant_labels=True)` on a long table gives each row the labels
   `OccupantLabels` gives its occupant, and `occupants(t)` on a tar `.rez`
   or a parquet raises the documented error.
3. **Zero copy.** In CI, reading a table into pyarrow grows
   `pyarrow.total_allocated_bytes()` by 0, and `polars.from_arrow(table,
   rechunk=False)` on that pyarrow `Table`, for a one-batch result, keeps
   the column's buffer address (both measured on the prototype above, with
   that call). Whether `polars.DataFrame(x)` on the capsule object keeps it
   is measured here for the first time; if it does not, the documented
   polars path is the one through pyarrow.
4. **Containers by content.** A `.dendro`, `.rez` v3, tar `.rez` and parquet,
   each renamed to a wrong extension, open and give the same results as
   under their own names.
5. **Cost against Rust.** On `metrics-9.6h.dendro`, the five queries in
   "Measured", five runs each alternating with a Rust example built from the
   same commit that does the same open, query and Arrow conversion: each
   query's Python median within 10% or 50 ms (whichever is larger) of the
   Rust median, and peak RSS within 10%. The prototype's first query took
   0.11-0.12 s in Python against 0.08 s for the same steps in the Rust probe
   (two and three runs, not alternated): 30-40 ms, inside the 50 ms floor.
   Where the difference comes from has not been looked into.
6. **Workflows reproduced.** The three patterns under "Today" written against
   the bindings, as examples with tests on the fixtures: per-interval
   percentiles of one histogram from a `.rez` and from a `.dendro` (including
   the WAL tail the current script misses); GPU columns found by metric name
   and labels through `columns()` rather than by id, from a parquet and a
   `.dendro`; and a PromQL range query exported to a polars DataFrame and
   queried by duckdb. h2histogram-py's README example (per-row p99 and a
   recording total) is reproduced from a `.dendro` through `hs.to_h2()`,
   with per-tick histograms in place of since-start rows, the metric found
   by name rather than by the README's `"syscall/read/latency"` column, and
   each percentile reported as `bucket.end`, the engine's representative,
   where the README prints `.midpoint`. If the owner supplies the notebooks
   and the llm-sim export the plan names, each is reproduced the same way.
7. **Wheels install.** Each of the four wheels (manylinux_2_28 x86_64 and
   aarch64, macOS arm64 and x86_64) installs into a clean virtual
   environment on its platform with no Rust toolchain, no pyarrow and no
   h2histogram, and `import metriken; metriken.open(fixture)` works, on
   CPython 3.10 and 3.14 (the oldest supported and the newest, since one
   abi3 wheel serves both); the tests then run with the `pyarrow`,
   `polars` and `h2histogram` extras installed.
8. **Histograms round-trip through h2histogram.** Against metriken-query
   0.36.1 or later (the u32::MAX cap fixed) with step 4's per-tick function.
   For every histogram metric in every fixture, `histogram_quantiles([0,
   0.5, 0.9, 0.99, 0.999, 1], m)` from `query_range` and `[h.percentile(q)
   for each tick's h in r.histograms(m).to_h2()]` agree within bucket error:
   at every timestamp, h2histogram's bucket `(start, end)` equals the
   engine's band and `float(bucket.end)` equals the engine's value (the
   engine's values are f64, which rounds a bucket end above 2^53). The same
   holds with a stride and with `dense=True`, and the two sides have the
   same timestamps (`hs.buckets` has a row exactly where the engine has a
   point). For `from_column(..., delta=True)` on a single-series column
   against the query with that series' labels, a row whose timestamp has no
   engine point must be `None` or an empty histogram, and every other row
   must match the engine's point. Equality of the bucket is the test, not a
   tolerance of one bucket width, because both sides use the same rank (see
   "Percentile conventions agree"); a disagreement is a bug in the
   conversion. The fixtures include a series whose producer restarted
   mid-recording, so the saturating difference is exercised; a column with a
   null cell and a segment without the column, so the difference across a
   gap is; and a tick whose total exceeds 2^32, so a cap like 0.36.0's fails
   the check. Step 4 builds these fixtures. Check 8 on rezolus's fixtures
   is gated as 1(b) is.

If 5 fails, 0.1.0 waits until 5 passes: the difference and its mechanism
are recorded here first.
NO-GO for bindings as a whole if the extension cannot be built for Linux
aarch64 under manylinux_2_28; then the Python reader against dendro's
`FORMAT.md`, the plan's alternative, is priced instead.

## Order of work

1. **Skeleton and CI.** `metriken-py/` with its own workspace, `open`,
   `recordings`, `recording(**labels)`, metadata, time range; the capsule
   export; the Linux x86_64 wheel built and tested on pull requests. Checks
   4 and 7 (Linux x86_64) run here.
2. **Queries.** `query_range` and `query` with rate mode and rate span
   (`QueryOptions`); the cross-cadence policy comes with `ArchiveReader`'s
   `MetricsSource` and needs nothing here; histogram results;
   `to_polars`. Check 1(a) runs here.
3. **Tables.** `metrics`, `tables`, `columns`, `read_table` with time range
   and WAL tail, `occupants`, the occupant join, `bucket_bounds`, `reopen`,
   the fork check. Checks 2 and 3 run here (3 reads a table through
   `read_table`).
4. **Histograms.** Needs metriken-query 0.36.1, the separate patch release
   that removes the u32::MAX cap (described under "Histograms as
   h2histogram objects"). In metriken-query, a later 0.36.x release: the
   per-tick function, shared by `quantiles_impl` and `heatmap_impl`, with
   `quantiles_impl`'s stride rule (which `reduce` already follows), and the
   test that a strided
   `histogram_heatmap` emits at `histogram_quantiles`' timestamps after an
   empty stride. Then
   `r.histograms`, `to_h2`, `metriken.h2.from_column` and the `h2histogram`
   extra. Step 4 builds check 8's fixtures: a series whose producer
   restarted mid-recording, a column with a null cell, a wide table where
   one segment lacks the column, and a tick whose total exceeds 2^32. They
   are written by a Rust test helper in `metriken-py/` through
   metriken-storage's writer. Check 8 runs here, on
   those fixtures and storage's archives.
5. **Fixtures and workflows.** Checks 1(b) and 8 on rezolus's
   compatibility fixtures once they are checked in (they are copied into
   `metriken-py/tests/`, so this repository does not depend on rezolus's
   tree); check 6 in CI; check 5 on the author's host.
6. **Release.** The four-wheel matrix and sdist, trusted publishing,
   `metriken` 0.1.0 on PyPI. Then the consumers move: the patterns above,
   and whatever the owner names under "Questions for the owner".

## Callers

Nothing in this repository or in rezolus calls the bindings. What moves to
them, outside this repository:

- the scripts under "Today" that read `.rez` segments through `sqlite3` or a
  rezolus parquet by column id, which break on a `.dendro` artifact;
- h2histogram-py's README, whose parquet example would read a `.dendro`
  through the bindings and `to_h2` (a change in that repository, proposed
  there after 0.1.0);
- the duckdb and polars notebooks and llm-sim's export, which the plan names
  and which were not found on disk.

Scripts that query through `systemslab promql` stay as they are.

## Questions for the owner

1. **Which notebooks, and where.** Open. The plan names duckdb and polars
   notebooks and llm-sim's export; none are in the author's checkouts.
   Where are they, and which workflows must check 6 reproduce?
   (h2histogram-py's documentation was found; see "Today".)
2. **h2histogram-py.** Should a histogram row convert to an h2histogram-py
   object (a dependency, or an optional extra), or is Arrow plus
   `bucket_bounds` enough? Decided 2026-10-12: 0.1.0 converts, behind the
   optional extra `metriken[h2histogram]` ("Histograms as h2histogram
   objects", check 8).
3. **PyPI name.** `metriken` (proposed) is also the Rust producer library's
   name, and the Python package only reads. `metriken-py` or another name
   avoids that. Decided 2026-10-12: `metriken`, import `metriken`.
4. **Versioning.** Independent semver (proposed), or the same version as
   `metriken-query` so a user can tell which engine they have from the
   package version? Decided 2026-10-12: independent semver, tagged
   `metriken-py-vX.Y.Z`; `build_info()` names the engine.
5. **Platforms.** Windows (the Rust crates' CI tests it), musllinux, and
   free-threaded CPython (3.13t, 3.14t, which abi3 does not cover): in 0.1.0
   or later? Decided 2026-10-12: the four wheels only (manylinux_2_28
   x86_64 and aarch64, macOS arm64 and x86_64); the others are not in
   0.1.0.
6. **Gating on rezolus's fixtures.** Open. Check 1(b) depends on fixtures
   rezolus has not checked in yet. Release 0.1.0 on metriken-storage's own
   archives and add rezolus's in 0.1.x, or wait? Until answered, check 1
   treats it as "wait".
7. **PyPI ownership.** Open, owner action. Trusted publishing needs the
   PyPI project `metriken` created and linked to this repository's release
   workflow by an owner of both.

## Left alone

- **Writing.** Recording, annotating and converting from Python. A program
  that needs to record runs a recorder.
- **A series read.** Reading one metric's stored samples as long form
  (timestamp, series, value, window), wide or long alike, would serve a
  caller that wants raw counters without PromQL. The storage scan provides it
  for counters (`DataSource::counter_scan`); gauges and histograms are read
  as whole series (`gauges`, `histogram_stream`), which the plan says the
  scan must not do. Added when the scan covers them and a consumer asks.
- **Query results as Arrow in the engine.** The bindings convert
  `MatrixSample`s (at most 31 ms for 5.3 million points, measured). An
  engine API that returns Arrow and nanosecond timestamps directly would
  remove the conversion and the f64 rounding; not needed for 0.1.0.
- **The viewer and dashboards** from Python.
