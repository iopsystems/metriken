# metriken-py: reading recordings from Python

**Status:** OPEN. Design 2026-10-12, not started. This is path step 6 of
[one recording stack](2026-10-08-one-recording-stack.md) (piece 4, whose
recommendation, bindings over `metriken-storage` and `metriken-query`, this
entry takes). "Step N below" means this entry's own order of work. Nothing
here is decided by the owner yet; the questions that need the owner are
under "Open questions".

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
duckdb or polars against rezolus data, and neither h2histogram-py nor llm-sim
is checked out there, so what those three read is not known from source. Three
other patterns were found, in private experiment repositories:

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
`rezolus record` or a producer that records itself (step 4).

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
- No dependency on h2histogram-py. Whether its objects should be constructible
  from a row is an open question.

### Binding technology

PyO3 0.28 and maturin, a `cdylib` built as one abi3 wheel per platform (the
prototype built `cp39-abi3`). wasm32 is not a target. Wheels:

- Linux x86_64 and aarch64, manylinux_2_28, built by `PyO3/maturin-action`
  (SQLite and zstd are C, compiled by `cc` in the manylinux container);
- macOS arm64 and x86_64, deployment target 11.0 (the prototype linked
  SQLite and zstd objects built for the host's 26.4 SDK with warnings until
  `MACOSX_DEPLOYMENT_TARGET=11.0` was set; CI sets it);
- an sdist, which needs a Rust toolchain to install.

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

- **Name:** `metriken` on PyPI and as the import name. It was unclaimed on
  2026-10-12 (as were `metriken-py` and `rezolus`; `dendro` is taken).
- **Version:** its own semver, starting at 0.1.0, not tied to the crates'.
  `metriken.build_info()` reports the `metriken-storage` and
  `metriken-query` versions it was built with. A change to the Python API is
  a minor version while 0.x. A storage or query release that changes what a
  recording reads as (a fix, a new container) is followed by a bindings
  release; one that does not, is not.
- **Release:** a tag `metriken-py-vX.Y.Z` (this repository's tag form)
  triggers a GitHub Actions workflow that builds the wheels and sdist,
  installs each wheel in a clean environment, runs the Python tests, and
  publishes with PyPI trusted publishing (no token in the repository).
- **CI on pull requests:** build the Linux x86_64 wheel and run the Python
  tests when `metriken-py/`, `metriken-storage/` or `metriken-query/`
  changes.

## GO / NO-GO

GO for release 0.1.0 when all of these hold. Each runs in CI except 5 and 6,
which run on one host and are recorded here.

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
   queried by duckdb. If the owner supplies the notebooks, h2histogram-py
   documentation and llm-sim export the plan names, each is reproduced the
   same way.
7. **Wheels install.** Each wheel installs into a clean virtual environment
   on its platform with no Rust toolchain and no pyarrow, and `import
   metriken; metriken.open(fixture)` works; the tests then run with pyarrow
   and polars installed.

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
4. **Fixtures and workflows.** Check 1 on rezolus's compatibility fixtures
   once they are checked in (they are copied into `metriken-py/tests/`, so
   this repository does not depend on rezolus's tree), check 6, check 5 on
   the author's host.
5. **Release.** The wheel matrix, trusted publishing, `metriken` 0.1.0 on
   PyPI. Then the consumers move: the three patterns above, and whatever the
   owner names under "Open questions".

## Callers

Nothing in this repository or in rezolus calls the bindings. What moves to
them, outside this repository:

- the scripts under "Today" that read `.rez` segments through `sqlite3` or a
  rezolus parquet by column id, which break on a `.dendro` artifact;
- the duckdb and polars notebooks, h2histogram-py's documentation and
  llm-sim's export, which the plan names and which were not found on disk.

Scripts that query through `systemslab promql` stay as they are.

## Open questions for the owner

1. **Which notebooks, and where.** The plan names duckdb and polars
   notebooks, h2histogram-py's documentation and llm-sim's export; none are
   in the author's checkouts. Where are they, and which workflows must
   check 6 reproduce?
2. **h2histogram-py.** Should a histogram row convert to an h2histogram-py
   object (a dependency, or an optional extra), or is Arrow plus
   `bucket_bounds` enough?
3. **PyPI name.** `metriken` (proposed) is also the Rust producer library's
   name, and the Python package only reads. `metriken-py` or another name
   avoids that.
4. **Versioning.** Independent semver (proposed), or the same version as
   `metriken-query` so a user can tell which engine they have from the
   package version?
5. **Platforms.** Windows (the Rust crates' CI tests it), musllinux, and
   free-threaded CPython (3.13t, 3.14t, which abi3 does not cover): in 0.1.0
   or later?
6. **Gating on rezolus's fixtures.** Check 1 depends on fixtures rezolus has
   not checked in yet. Release 0.1.0 on metriken-storage's own archives and
   add rezolus's in 0.1.x, or wait?
7. **PyPI ownership.** Trusted publishing needs the PyPI project created and
   linked to this repository's workflow by an owner of both.

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
