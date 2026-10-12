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
by default. rezolus 6.x writes `.dendro` only (recorder entry, decided
2026-10-12), so the date a systemslab experiment's metrics artifact becomes
a `.dendro` is the date the scripts below stop working.

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
  `Histogram.from_buckets(grouping_power, max_value_power, buckets)`. The
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
  counts early samples once per later row. Whether any caller of
  h2histogram-py relies on that example was not checked.

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
  provided systemslab reads it (iopsystems/systemslab#6371).

**What metriken provides.** Opening by content:
`metriken_storage::rez::open::{open_recordings, open_recordings_from_bytes}`
(feature `rez`) returns one `ArchiveReader` per recording with its labels,
for a `.dendro`, a `.rez` v3 or a tar `.rez`; `metriken_query::ParquetReader`
opens a parquet file. `ArchiveReader` implements `MetricsSource`
(`metriken-query/src/archive.rs:371`): `query_range_opts`, `query`, names,
labels, metadata, `time_range`. Raw access is the `Catalog` trait
(`metriken-storage/src/catalog.rs`): `sources`, `tables`, `segment_meta`,
`segment_bytes`, `live_wal`, and `wal::materialize_wal_tail` turns a WAL
tail into a segment. A long table's occupant stream is the table
`<table>/occupants`; `long::OccupantLabels` maps an occupant number to its
labels, and an occupant never changes labels (`long.rs:163`).

**Query results are not Arrow.** `QueryResult::Matrix` holds `MatrixSample`s:
labels as a `HashMap<String, String>`, `values: Vec<(f64, f64)>` with the
timestamp in f64 seconds, and `bands: Option<Vec<Option<(f64, f64)>>>`
(`metriken-query/src/promql/mod.rs:135`). The bindings convert this to
Arrow. f64 seconds lose up to about 130 ns at the current epoch
(`RANGE_SLACK_NS`, `promql/mod.rs:231`).

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
1,272 MB on disk; holding every batch instead of dropping it peaked at
8,539 MB RSS. Histograms are most of it: the five largest tables are
histogram tables (2,206 MB for `syscall_latency`). Each histogram cell is a
`List<UInt64>` of 496 cumulative buckets (grouping power 3, max value power
64, from the field metadata); in one mid-recording segment the rows averaged 0
to 315 non-zero buckets, by histogram column. A full read of `long.dendro` (the long
table's 76,446,307 rows and its occupant stream) took 1.53 s in one run, and its occupant stream held 264,286
rows. A Python consumer that reads whole tables gets these sizes; the API
reads one table, and a time range, at a time.

**Queries, in Rust, against `rezolus mcp query`.** Full range, step 1 s, one
process per run, three runs (open is 17 ms):

| query | probe query | probe to Arrow | points | `rezolus mcp query` wall |
|---|---|---|---|---|
| `sum by (id) (irate(cpu_usage[5m])) / 1000000000` | 62-66 ms | 2.6 ms | 554,816 | 0.09 s, 0.08 s |
| `sum(irate(network_bytes[1m]))` | 10-11 ms | 0.15 ms | 34,676 | 0.03 s |
| `histogram_quantiles([0.5, 0.99], syscall_latency)` | 5.87-7.93 s | 0.3 ms | 69,354 | 5.40 s, 5.44 s |
| `sum(irate(task_cpu_usage[5s]))` | 4.39-4.47 s | 0.15 ms | 34,676 | 3.86 s, 3.90 s |
| `sum by (comm) (irate(task_cpu_usage[5s]))` | 1.13-1.19 s | 30.4-30.7 ms | 5,326,720 | 4.08 s, 4.16 s |

The `rezolus` binary is 6.0.0-alpha.41, built against an earlier
metriken-query than d2bf74d, so this column is a reference, not a
comparison of the same engine. The probe ran the last two queries in one
process, so the per-task table was already open and cached for the last one;
the CLI opens it for each. Converting a result to long-form Arrow costs at
most 31 ms, at 5.3 million points.

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
pyarrow's allocator 0.0 MB. `polars.from_arrow(table, rechunk=False)` then took
2-3 ms and grew RSS by 7-9 MB, keeping all 116 chunks; on a one-batch table
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
Arrow stream from the bindings; Windows; free-threaded CPython; any
consumer's workflow end to end.

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
recognized in the order `open_recordings` uses (dendro, then `.rez` v3, then
tar `.rez`), and parquet by its `PAR1` magic.

**Two kinds of call, as the plan says,** plus the catalog that makes the
second usable:

1. **PromQL**, range and instant, returning long-form rows. This is what
   `rezolus mcp`'s `export_query` writes (`src/mcp/export.rs`: series,
   timestamp, value, lo, hi).
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
`polars.from_arrow(x)` and duckdb take it without a copy and without the
bindings depending on any of them. The prototype used arrow-rs's
`pyarrow` feature, which calls into pyarrow and so requires it; the
capsule exporter needs only `arrow::ffi_stream`, either written here or
taken from the `pyo3-arrow` crate. pyarrow becomes an optional
dependency for the `to_pyarrow`/`to_polars` conveniences.

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

### Histograms

- **In table rows:** the stored `List<UInt64>` of cumulative bucket counts,
  zero-copy, with `grouping_power` and `max_value_power` in the column map.
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
   accumulation in a second copy (`heatmap_impl`, `:685`), then subtracts
   the lowest occupied bucket's index from every index and returns no
   configuration, so its output cannot be passed to h2histogram as it is.
   Step 4 below adds a public function to metriken-query that yields
   `(timestamp_ns, index, count)` per tick from a `HistogramStream`, and
   `quantiles_impl`, `heatmap_impl` and the bindings all call it. Then
   check 8 below tests the conversion and h2histogram's arithmetic, and not
   two copies of the accumulation. The function needs a metriken-query
   release before the bindings can use it. The result is Arrow first
   (`hs.buckets`); `hs.to_h2()` builds one
   `h2histogram.SparseHistogram.from_parts(config, index, count)` per tick,
   or with `dense=True` a `Histogram` via `to_dense()`. Sparse is the
   default because every bucket becomes a Python `int`, and a stored row in
   the 9.6 h recording averaged 0 to 315 non-zero buckets of 496, by
   column; a tick's difference has no more non-zero buckets than the later
   row.
2. **`metriken.h2.from_column(batches, column, delta=True)`**, one stored
   column from `read_table`. Each cell is a `List<UInt64>` of 496 counts
   cumulative since the producer started. With `delta=True` (the default)
   consecutive non-null cells are differenced per bucket, saturating at zero
   as the engine does, and the first row and any row after a null give
   `None`. `h2histogram.Histogram.subtract` is not used for this, because it
   raises on a negative bucket, which a producer restart produces. With
   `delta=False` each cell becomes the since-start histogram, which is what
   h2histogram-py's own `read_histograms` returns. The differencing and the
   non-zero scan run in Rust over the Arrow buffers; Python receives
   `(index, count)` lists. A long table's histogram column is per occupant,
   so `from_column` differences within an occupant, never across occupants.

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
bucket for totals up to 2^53. So the value from h2histogram is
`bucket.end`, and the band is `(bucket.start, bucket.end)`.

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
(`exclude` from the root workspace) and lockfile. The root CI runs
`cargo nextest --all-targets --all-features` on Linux, macOS and Windows and
`cargo hack --feature-powerset`; a PyO3 extension module does not link
under plain `cargo build` (the prototype failed with undefined Python
symbols until built by maturin), so it cannot be a member those jobs build.
It depends on `metriken-storage` and `metriken-query` by path, so it always
builds against the crates on the same commit, and it is not published to
crates.io.

### Threads, the GIL, and processes

- Every call that reads or queries releases the GIL (`Python::detach`).
  Measured above: two queries on two threads overlap.
- An open recording is a `#[pyclass]` that is `Send + Sync`
  (`MetricsSource` requires both), so Python threads can share it. Whether
  queries on one recording from several threads serialize anywhere in the
  reader is not measured; the measurement above used two separate opens.
- After `fork`, a SQLite connection opened in the parent must not be used in
  the child. Python's `multiprocessing` forks by default on Linux before
  3.14. A recording remembers the pid that opened it, and a call from another
  pid raises an error saying to open the file in the child.
- Arrow buffers handed to Python are owned by Rust allocations released when
  the last Python reference goes; they outlive the recording object.

### SQLite in the wheel

dendro bundles SQLite (rusqlite `bundled`), so the wheel carries its own copy
next to whatever Python's `sqlite3` module links. The extension exports only
its init symbol on macOS (measured), so the two do not resolve to each other.
SQLite's documentation warns that two copies of the library in one process
open on the same database file can break POSIX advisory locks, because
closing one copy's file descriptor releases the other's locks. The bindings
only read, and the risk is to a writer in the same process: a script that
also opens the archive with `sqlite3` for writing, or a recorder embedded in
the same Python process. The bindings' documentation says so.

### A file still being written

A recording opened while a recorder or hindsight writes it is a fixed view:
the WAL tail is materialized into a segment at open, and later rows are not
seen (the same as `ArchiveReader` everywhere). `r.reopen()` returns a new
view, using `ArchiveReader::keep_handover` and `reuse_from` so segments
already read are not read again, as rezolus's `crates/rez/src/live.rs`
does. `r.complete` is false until the
writer finalizes. The open-time cost of a large WAL tail is not measured.

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
- **CI on pull requests:** build the Linux x86_64 wheel and run the Python
  tests when `metriken-py/`, `metriken-storage/` or `metriken-query/`
  changes.

## GO / NO-GO

GO for release 0.1.0 when all of these hold. Each runs in CI except 5 and 6,
which run on one host and are recorded here, and 7, which runs in the
release workflow on each wheel's platform.

1. **Same values as the engine.** On rezolus's compatibility fixtures
   (rezolus 6.0 entry, "Compatibility": a tar `.rez`, a v3 `.rez`, a 5.25
   `.dendro`, a 5.x parquet and a 6.0 recording, not yet checked in as of
   15c77844) and on metriken-storage's own test archives (wide, long with an
   occupant stream, a WAL tail), `query_range` returns the same series,
   timestamps (to the nanosecond after conversion) and values and bands
   (bit for bit) as `MetricsSource::query_range_opts` called from Rust on
   the same commit, for the golden queries. Against `rezolus mcp query` of a
   rezolus release built on the same metriken-query version, the same
   values. This is the plan's criterion 4.
2. **Tables complete.** For every table of every fixture, the rows from
   `read_table` equal the rows the Rust catalog yields (segments plus WAL
   tail); `read_table(t, occupant_labels=True)` on a long table gives each
   row the labels `OccupantLabels` gives its occupant.
3. **Zero copy.** In CI, reading a table into pyarrow grows
   `pyarrow.total_allocated_bytes()` by 0, and `polars.from_arrow(...,
   rechunk=False)` on a one-batch result keeps the column's buffer address
   (both measured on the prototype above).
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
   with per-tick histograms in place of since-start rows. If the owner
   supplies the notebooks and the llm-sim export the plan names, each is
   reproduced the same way.
7. **Wheels install.** Each of the four wheels (manylinux_2_28 x86_64 and
   aarch64, macOS arm64 and x86_64) installs into a clean virtual
   environment on its platform with no Rust toolchain, no pyarrow and no
   h2histogram, and `import metriken; metriken.open(fixture)` works; the
   tests then run with the `pyarrow`, `polars` and `h2histogram` extras
   installed.
8. **Histograms round-trip through h2histogram.** For every histogram
   metric in every fixture, `histogram_quantiles([0, 0.5, 0.9, 0.99, 0.999,
   1], m)` from `query_range` and `[h.percentile(q) for each tick's h in
   r.histograms(m).to_h2()]` agree within bucket error: at every timestamp,
   h2histogram's bucket `(start, end)` equals the engine's band and its
   `end` equals the engine's value. The same holds with a stride, with
   `dense=True`, and for `from_column(..., delta=True)` on a single-series
   column against the query with that series' labels. Equality of the
   bucket is the test, not a tolerance of one bucket width, because both
   sides use the same rank (see "Percentile conventions agree"); a
   disagreement is a bug in the conversion. The fixtures include a series
   whose producer restarted mid-recording, so the saturating difference is
   exercised.

NO-GO if 5 fails because of the binding itself (conversion, capsule export,
GIL handling) and not because of the engine; then the cost and its mechanism
are recorded here and the slow call is fixed before release. NO-GO for
bindings as a whole if the extension cannot be built for Linux aarch64
under manylinux_2_28; then the Python reader against dendro's `FORMAT.md`,
the plan's alternative, is priced instead.

## Order of work

1. **Skeleton and CI.** `metriken-py/` with its own workspace, `open`,
   `recordings`, `recording(**labels)`, metadata, time range; the capsule
   export; the Linux x86_64 wheel built and tested on pull requests. Checks
   4 and 7 (Linux x86_64) run here.
2. **Queries.** `query_range` and `query` with rate mode and rate span
   (`QueryOptions`); the cross-cadence policy comes with `ArchiveReader`'s
   `MetricsSource` and needs nothing here; histogram results; `to_polars`. Checks 1 (on storage's archives) and 3 run here.
3. **Tables.** `metrics`, `tables`, `columns`, `read_table` with time range
   and WAL tail, `occupants`, the occupant join, `bucket_bounds`, `reopen`,
   the fork check. Check 2 runs here.
4. **Histograms.** The per-tick function in metriken-query, shared by
   `quantiles_impl` and `heatmap_impl` (a metriken-query release), then
   `r.histograms`, `to_h2`, `metriken.h2.from_column` and the
   `h2histogram` extra. Check 8 runs here, on storage's archives.
5. **Fixtures and workflows.** Checks 1 and 8 on rezolus's compatibility
   fixtures once they are checked in (they are copied into
   `metriken-py/tests/`, so this repository does not depend on rezolus's
   tree), check 6, check 5 on the author's host.
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
6. **Gating on rezolus's fixtures.** Open. Check 1 depends on fixtures
   rezolus has not checked in yet. Release 0.1.0 on metriken-storage's own
   archives and add rezolus's in 0.1.x, or wait?
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
