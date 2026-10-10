# One recording stack for every metriken producer

**Status:** OPEN. Plan opened 2026-10-08, nothing built. Decided 2026-10-08:

- recording, exposition and viewing converge on one stack, and per-project
  recorders and viewer implementations are retired as their projects move;
- the stack is metriken crates (below), named for what they do with metriken
  data rather than for dendro, which stores that data without reading it;
- only the producer side depends on the `metriken` registry; everything
  downstream depends on a data model crate with no `metriken` dependency
  ("Structure" below);
- applications own their dashboard templates, and the viewer loads a template
  from the archive;
- consumers outside Rust get readers in their own languages, not a parquet
  export with one schema.

Related entries opened the same day: rezolus
`docs/journal/2026-10-08-6-0-release-readiness.md`, systemslab
`docs/journal/2026-10-08-dendro-artifacts.md`, cachecannon and llm-perf
`docs/journal/2026-10-08-dendro-recording.md`. This continues
[a high-cardinality metrics stack](2026-09-28-high-cardinality-stack.md),
which moved the segment format, the archive reader and writer, and the
stream's frame encoder out of rezolus.

## Goal

A program instrumented with metriken records, exposes and is viewed the same
way as the rezolus agent: it serves dendro's replication stream, a recorder
records it into the same `.dendro` as the hosts it runs on, or it records
itself, and one viewer reads the result with the dashboard the program
supplied.

## Today

Each producer that records carries its own copy of the same pipeline: a
snapshot timer that appends msgpack to a temporary file and converts it to
parquet at exit through `metriken_exposition::MsgpackToParquet`, a `/metrics`
Prometheus endpoint, and sometimes its own viewer.

| Producer | metriken / exposition | Records | Serves | Views |
|---|---|---|---|---|
| rezolus agent | 0.11 / 0.21, metriken-archive 0.3 `stream` | — (the recorder records it) | `/metrics/stream`, `/metrics/binary` | `rezolus view` |
| cachecannon | 0.9 / 0.16 | parquet at exit (`src/admin/mod.rs`, `run_parquet_recorder`) | `/metrics`, `/metrics/binary` | `cachecannon view` (metriken-query 0.9, parquet only) |
| llm-perf | 0.8 / 0.14 (git rev `98db358`) | parquet at exit (`src/snapshot.rs`); runs `rezolus record` to capture the server under test (`src/server_metrics.rs`) | `/metrics` with percentile gauges (`src/admin.rs`) | rezolus template `llm-perf.json` |
| rpc-perf | 0.8 / 0.14 | parquet (`src/output/mod.rs`) | `/metrics` with percentile gauges | — |
| an internal service | 0.9 / 0.16 | — | `/metrics` | — |

Three things follow from this:

- A load generator's recording is a separate file with no acquisition
  windows, joined to the host recordings afterwards by
  `rezolus recording combine`.
- A Prometheus scrape of these producers gets histograms as quantile gauges,
  which the viewer cannot turn back into distributions.
- `metriken-core` declares `links`, so a dependency graph holds one
  metriken-core version. metriken 0.11, which metriken-archive 0.3 uses, is on
  core 0.3; metriken 0.9 is on core 0.2 and 0.8 on core 0.1. A producer cannot
  write or serve the stream with metriken-archive 0.3 (its `write` and
  `stream` features) until it moves to metriken 0.11: cachecannon from
  0.9, rpc-perf from 0.8, llm-perf off its git dependency (0.8).

## Structure

Decided 2026-10-08, starting from the question of how the crates would be
laid out if only `metriken`, `metriken-core` and `metriken-derive` had to stay.
Each step below is a breaking release where it needs to be: compatibility with
current dependents is not a constraint.

### The rule

Only the producer side depends on the `metriken` registry. Everything
downstream depends on `metriken-model`, which has no `metriken` dependency.
Three workarounds in the current tree come from the absence of this rule:

- `metriken-core`'s registry declares a `linkme` distributed slice, which has
  no wasm32 implementation. rezolus's `crates/rez` therefore has a `write`
  feature that the browser viewer turns off, and rezolus CI runs
  `cargo check -p rez --no-default-features --target wasm32-unknown-unknown`
  so that no `metriken` type reaches the read path.
- systemslab depends on rezolus's `rez` crate with `default-features = false`
  to avoid linking `metriken`.
- A group's schema and acquisition window are defined twice: as
  metriken-exposition's snapshot types and as metriken-segment's copies
  (`schema.rs`, `window.rs`), with a test in metriken-exposition
  (`mirrors_the_producers_encoding_byte_for_byte`) pinning them together.

`metriken-core` declares `links`, so a build holds one `metriken-core`
version. Under the rule only programs that link `metriken`, through
`metriken-exposition`, are held to it. Readers (systemslab, the WASM viewer,
the Python bindings) are not.

### Dependency graph

Each crate and what it depends on:

- `metriken-types`: nothing in metriken.
- `metriken-core`: `metriken-types`. `metriken`: `metriken-core`,
  `metriken-types` (for `UID_LABEL`).
- `metriken-model`: `metriken-types`.
- `metriken-exposition`: `metriken`, `metriken-model`, dendro (feature
  `stream`).
- `metriken-storage`: `metriken-model`, arrow, parquet; dendro (feature
  `dendro`, the default) and rusqlite (feature `rez`), with no `dendro?/…`
  references, so a producer using only the `parquet` feature (parquet at exit)
  does not compile SQLite.
- `metriken-query`: `metriken-storage`, `metriken-model`.
- `metriken-recorder`: `metriken-storage`, `metriken-model`, dendro.
- `metriken-dashboard`: `metriken-query`.
- `metriken-viewer`: `metriken-dashboard`, `metriken-query`,
  `metriken-storage` (report saving), `metriken-recorder` (live mode).

Four of these edges need explaining:

- `metriken-types` sits below `metriken-core` and holds what the registry
  writes and readers interpret: the acquisition window (`Window`, moved from
  `metriken-core`'s `src/window.rs`) and the slot-identity label key
  (`UID_LABEL`, `__uid__`, moved from `metriken`'s `group::identity`, and today
  also written as a string literal in metriken-archive's writer (`writer.rs`)
  and in metriken-segment's and metriken-query's tests). `metriken-core`
  re-exports `Window` and `metriken` re-exports `UID_LABEL`, so
  `metriken::Window` keeps its path and there is one type with no conversions.
  `Window` takes the union of today's derives (core's, with optional serde, and
  metriken-segment's `PartialOrd`, `Ord`, `Hash`).
- `metriken-model` must not depend on `metriken-core`: core declares
  `links = "metriken-core"` and its registry's `linkme` slice, which has no
  wasm32 implementation. A breaking model release needs a new
  `metriken-exposition` but not a new `metriken-core`, so it does not force a
  producer onto a new `metriken`.
- `metriken-storage` is below `metriken-query`. Today the order is reversed:
  `metriken-archive`'s reader implements `metriken_query::MetricsSource`, uses
  `ParquetReader`, `UnionMetricsSource` and the long-table relabel from
  metriken-query, and parses PromQL (`referenced_metrics`) to route a query to
  tables; `ParquetReader` holds a `QueryEngine`; and the segmented reader's
  `grid_rates` computes rates, `by()` groups and display decimation while it
  reads each segment (#237, #241). The fix keeps that streaming and puts the
  computation in the engine. Storage defines a public scan: for a metric, a
  label filter and a time range it returns the matched series with their
  labels, a per-segment plan (which columns, and which occupants of a long
  table, hold each series, and which segments each series appears in), and a
  time-ordered stream of decoded column chunks (arrow arrays: timestamps,
  values, the acquisition-window columns, `duration`). Every container
  implements the scan. The engine runs the rate, grouping and display
  accumulators over the stream and ends a series once the plan says no later
  segment holds it, so neither side materializes a whole series. Routing a
  query to tables moves into the engine. The scan must not return whole
  series, as today's `pub(crate) DataSource` methods such as `counters(name)`
  do; that would undo #241. `metriken-query` is the
  engine over that trait, and `MetricsSource` (engine plus data) is assembled
  there: a consumer opens a file through `metriken-query` (`open(path)`, or
  from a storage reader) and gets something it can run PromQL on, so
  consumers depend on both crates.
- `metriken-exposition` depends on dendro only behind a `stream` feature,
  for the frame types. dendro bundles SQLite (`rusqlite` with `bundled`,
  `links = "sqlite3"`). An optional dependency that no feature enables stays
  out of the lockfile, so producers that do not serve the stream are not
  affected. A weak feature reference (`dendro?/feature`) would bring it into
  resolution and is not used; that is how systemslab came to stub
  `sqlx-sqlite`. dendro should also make SQLite optional, so the wire codec
  builds without it for producers that do serve the stream.

### Crates

| Crate | Holds | From |
|---|---|---|
| `metriken-types` | What the registry writes and readers interpret, and nothing else: `Window` (the acquisition window) and `UID_LABEL` (`__uid__`). No dependencies beyond optional serde; builds for wasm32. | `metriken-core`'s `Window`; `metriken`'s `group::identity::UID_LABEL` |
| `metriken-model` | Everything about rows, as plain types with no `metriken` dependency: source identity (labels, producer epoch), group snapshots and their schemas, metric descriptions, histogram configuration, occupants (labels keyed by `metriken-types`' `UID_LABEL`), the row types (`WalGroupRow`, `WalLongRow`, `LongOccupant`, `Occupant`, `WalCell`, `WalValue`) and their msgpack encoding and decoding (split out of metriken-segment's `wal.rs` and `occupants.rs`, which also hold parquet materialization), which are both the WAL and the stream rows, and decoding of the V1, V2 and V3 snapshots that raw recordings hold. Re-exports `metriken-types`. Builds for wasm32. Tables (arrow, parquet) are not here, so producers do not compile them. | metriken-exposition's snapshot types; metriken-segment's `schema` and row types (its `window.rs` copy is deleted in favour of `metriken-types`' `Window`) |
| `metriken-exposition` | The producer side: registry to model (the snapshotter and group builder), Prometheus text, msgpack, and the stream route behind feature `stream` (piece 1). The only crate here that depends on `metriken`. | itself; `metriken-archive`'s `stream` module |
| `metriken-storage` | Everything about tables and files: the arrow/parquet layouts (wide, long, occupant streams), turning rows into segments (`materialize_wal_tail`), the containers as features (`dendro`, the default; `rez`; `parquet`, single files, including `MsgpackToParquet`), the public data-access trait, the column readers, the long-table relabel, writers behind `write` (including staging streamed rows, `StreamDecoder`), and opening a file by its content. | `metriken-archive` (renamed); metriken-segment's tables; rezolus's `crates/rez` without its `caller_rows` identity index (`indexed.rs`), which only 5.x's preview `record --stream -o .rez` wrote; `metriken-query`'s column readers (the `DataSource` halves of `ParquetReader`, `SegmentedParquetReader` and `UnionMetricsSource`: `FileSource`, `MultiParquetSource`, `LazySource`, `SegmentedSource`, `UnionSource`, `MemoryStoreInner`) and `long`; the `MetricsSource` wrappers stay in `metriken-query`; metriken-exposition's `MsgpackToParquet` |
| `metriken-query` | The PromQL engine over storage's data-access trait, including routing a query to tables. `ingest` forwards to storage's, where `MemoryStore`'s snapshot loading reads model types, so `metriken-query` no longer depends on `metriken`. | itself, without the column readers |
| `metriken-recorder` | Sources to storage: a stream subscription, a Prometheus scrape (one acquisition group per scrape), the process's own snapshots handed over directly. Reconnects, restart metadata, several sources in one archive, `.rez` output for agents that cannot stream. Serves both rezolus release lines; 5.x's preview `record --stream -o .rez` is dropped when 5.x adopts it. | rezolus `src/recorder` (`stream.rs`, `prometheus.rs`, `restart.rs` and the orchestration in `mod.rs`) |
| `metriken-dashboard` | The dashboard model: sections, groups and plots from a source, and templates read from source metadata. | rezolus `crates/dashboard` |
| `metriken-viewer` | The viewer as a library: a server router to mount, the chart and UI assets, the WASM bundle, report saving. | rezolus `src/viewer`, `crates/viewer`, `crates/report-save` |
| `metriken-py` (later) | Python bindings over storage and query (piece 4). | new |

`metriken-segment` and `metriken-archive` are retired. A 5.x `--stream` `.rez`
(preview, unstable) still opens, with each slot column carrying the labels its
segment was built with rather than split by occupant. `rezolus record` becomes
the CLI over `metriken-recorder`, keeping `-- cmd` (`child.rs`) and the choice
of output format. dendro is unchanged apart from making SQLite optional.

The formats are features of one storage crate rather than three crates
(`metriken-dendro`, `metriken-rez`, `metriken-parquet`) because they share
the table layouts, the data-access trait, the column readers and the
occupant relabel, and opening a file by its content needs all of them in one
place. Removing `.rez` later is deleting a module and a feature in a major
release.

The stream has no crate of its own. Its producer half is in
`metriken-exposition` (feature `stream`), its consumer half in
`metriken-storage`'s writer (`StreamDecoder`) driven by `metriken-recorder`,
and the frame format is dendro's (`replicate::Frame`) carrying
`metriken-model` rows.

Open: whether `metriken-dashboard` and `metriken-viewer` live in this
repository or a repository of their own; the viewer brings a JS build and
static assets.

## Pieces

### 1. A stream route for any registry

metriken-archive's `stream::FrameProducer` encodes a producer's groups as
dendro replication frames and leaves the transport to the caller. It moves to
`metriken-exposition` (path step 3). The rezolus agent supplies the
rest itself (`src/agent/exposition/http`): the HTTP route, the pass timer, the
handshake fields, which groups a subscriber asked for, the long layout for
groups with slots. Each producer that wants the stream would write that again.

Add a helper beside `FrameProducer` that takes a metriken registry, an
interval and the producer's identity (source name, version, dashboard
template) and yields the frames for one subscription, so a producer mounts it
on a route in its own HTTP server. cachecannon's admin server is hand-written,
so the helper cannot depend on an HTTP framework. The rezolus agent moves onto
the helper first.

Acquisition windows: the snapshotter takes a window from
`Metric::value_with_window`, whose default returns none (`metriken-core`
`lib.rs`); only `WindowedLazyCounter`, `WindowedLazyGauge` and
`RwLockHistogram` return one from `value_with_window`, and windowed counter
and gauge groups supply one per entry (`load_with_window`). A producer using
plain counters gets rows stamped per pass and no per-metric window. Whether
that is enough for a load generator's counters is not yet measured.

### 2. A recorder library

`metriken-recorder` records any of three sources into one archive: a process
that serves the stream (the rezolus agent, or any producer using piece 1), a
Prometheus endpoint by scrape, and the calling process's own registry.

- `rezolus record` becomes its CLI.
- llm-perf records the server under test and itself in process, instead of
  running `rezolus record` (`src/server_metrics.rs`), and needs no rezolus
  binary.
- cachecannon records itself in process (cachecannon/cachecannon#176) through
  the same library rather than its own writer.

A source whose `/metrics/stream` handshake opens is streamed, rezolus agent
or not. A rezolus agent that predates the stream identifies itself on
`/status` and is covered by decision 1 of the rezolus entry. Any other source
is scraped at `/metrics`. Today `record` streams only a source it takes for a
rezolus agent from `/metrics/binary`; rezolus adds detection by stream to its
own recorder first (rezolus entry, "Endpoint detection"), and step 4 of the
path moves that code.

### 3. Templates travel in the archive

Applications own their dashboard templates, and the viewer loads a source's
template from the archive. Today the templates for cachecannon, llm-perf,
vllm, sglang and valkey live in rezolus (`crates/dashboard/templates/`), so a
change to one needs a rezolus release.

- A producer that serves the stream passes its template to the route helper,
  which puts it in the handshake's `metadata`. dendro carries that map
  verbatim (`Frame::Handshake`), and dendro's own `Subscriber` stores it as the
  source's metadata. rezolus's recorder does not use that subscriber
  (`src/recorder/stream.rs`): it keeps only the handshake's uuid and clock
  anchor (`adopt_source`, `src/recorder/mod.rs`) and reads metadata from the
  agent's `/status`, `/systeminfo` and `/metrics/descriptions`. The recorder
  therefore has to copy handshake metadata into the source's metadata,
  including on a reconnect. No dendro change is needed.
- The key is `service_queries`, which `rezolus recording annotate --queries`
  already writes into a `.rez` or `.dendro` source's metadata and
  `rezolus recording check` already reads. Templates are the same
  `ServiceExtension` JSON.
- A program recording itself writes its template into its own source's
  metadata.
- A scraped Prometheus endpoint has no handshake. The recorder takes a
  template per endpoint (for example `--endpoint url,template=<file>`) and
  writes it into that source's metadata.
- The viewer reads a source's template from its metadata. Today it chooses a
  template from its built-in registry by source name (for archives, e.g. the
  WASM viewer's `crates/viewer/src/lib.rs`), and that is what changes.
  Parquet recordings carry `service_queries` in their file metadata, and that
  path stays.

### 4. Readers for other languages

A `.dendro` holds wide tables, long tables (one row per tick and occupant)
and the occupant streams that name occupants. Flattening that into one wide
parquet schema would undo the long layout, so it is not offered. Python is the
first language: internal duckdb and polars notebooks, the h2histogram-py
documentation and llm-sim's export all assume rezolus parquet today.

- **Bindings over `metriken-storage` and `metriken-query`** (PyO3, the
  `metriken-py` crate), returning
  Arrow tables. Two calls cover the uses found: a PromQL range query returning
  long-form `(series labels, timestamp, value, lo, hi)`, which is what
  `rezolus mcp`'s `export_query` writes, and a table read returning a table's
  rows with occupant labels joined. The archive logic stays in one place.
- **A reader written in Python** against dendro's `FORMAT.md` (SQLite plus
  parquet segment blobs) has no Rust dependency, but has to reimplement the
  long-table relabel, the occupant join and the WAL tail.

Recommended: bindings.

### 5. One viewer

`metriken-viewer` and `metriken-dashboard` replace rezolus's viewer crates and
`cachecannon view`'s own dashboards. `rezolus view`, `cachecannon view` and
systemslab mount the same library; systemslab, which today vendors rezolus's
chart code at a rezolus revision and takes `dashboard` from a rezolus git tag,
takes published crates and assets instead. The move happens once, after
rezolus 6.0.0 is tagged, because the viewer is the part of rezolus changing
fastest. `cachecannon view` reads `.dendro` before then through its metriken
bump (cachecannon entry).

### 6. What stays

`MsgpackToParquet` moves to `metriken-storage` (feature `parquet`), and every
reader keeps reading parquet. A producer's parquet-at-exit path stays as an
option for one release after the producer records through `metriken-recorder`
(cachecannon step 4, llm-perf step 7). New work does not add per-project
recorders or viewers.

## GO / NO-GO

GO for each piece when:

1. **Stream route:** the rezolus agent, cachecannon and llm-perf serve the
   stream through the helper with no frame code of their own.
2. **Recorder library:** `rezolus record` is a CLI over `metriken-recorder`
   with its tests passing, and one llm-perf run records the server under test
   and llm-perf into one `.dendro` in process, with llm-perf's latencies as
   native histograms.
3. **Templates:** `rezolus view` on an archive holding an agent and a
   producer shows the producer's dashboard, for the producer's recording, from
   the template in the archive, with no template for it in the viewer.
4. **Python bindings:** the bindings return the same values as
   `rezolus mcp query` on rezolus's compatibility fixtures (rezolus entry).
5. **One viewer:** `cachecannon view` and `rezolus view` show the same
   dashboard for the same archive from `metriken-viewer`.

NO-GO for the stream route if it cannot be written without an HTTP framework
dependency or without a per-producer timer. In that case it is recorded here
with the reason, and the producers stay on Prometheus scrape, which the
recorder supports.

## Path

Each step is a release of the crates it touches; rezolus (its 5.x and 6.x
lines), systemslab, cachecannon and llm-perf move when they adopt it. For
users of the `rezolus` binary the CLI, the files it writes and reads, and the
wire stay the same, apart from 5.x's preview `record --stream -o .rez`, which
is dropped; rezolus's own tests (including the compatibility fixtures in the
rezolus entry) check that across each step. Library users (systemslab's
`rez` and `dashboard` pins) change imports when they bump, or, where a
step re-exports what it moves, in the release after.

1. **`metriken-types` and `metriken-model`.** Move `Window` from
   `metriken-core` and `UID_LABEL` from `metriken` into `metriken-types` (a
   minor release of each, which re-export them). Move the snapshot types, the
   row types and their encoding from `metriken-exposition` and
   `metriken-segment` into `metriken-model`. The pinning test goes, because
   there is one type. The V1/V2/V3 snapshot decoding moves without a behaviour
   change, checked against metriken-exposition's snapshot tests and rezolus's
   raw-recording tests (V2 today; the rezolus entry adds V3).
2. **`metriken-storage` and `metriken-query` over it, in one release.** Rename
   `metriken-archive`; move in metriken-segment's tables, metriken-query's
   column readers and long-table relabel, `MsgpackToParquet`, and rezolus's
   `crates/rez` (feature `rez`, without `caller_rows`). Define the scan, move
   the rate, grouping and display accumulators onto it in the engine, and move
   query routing into the engine; `ingest` reads model types. The writer
   depends on model types, not the `metriken` registry; the
   decode-and-re-encode round trip in `StreamDecoder` is deferred (see the
   storage scan entry). The two crates release together because the readers
   cannot leave the engine's crate until the engine reads through the scan.
   Design and gate: [the storage scan](2026-10-09-storage-scan.md).
3. **The stream route** in `metriken-exposition` behind `stream` (piece 1),
   and dendro with SQLite optional. The rezolus agent is the first user,
   cachecannon the first outside one.
4. **`metriken-recorder`** from rezolus's recorder (piece 2), with detection
   by stream. rezolus's `record`, `hindsight` and live viewer use it from both
   release lines; then llm-perf records in process.
5. **Templates in the archive** (piece 3): the route helper puts the template
   in the handshake, the recorder keeps handshake metadata, and the viewer
   reads `service_queries` from a source's metadata.
6. **`metriken-py`** (piece 4), before any consumer outside Rust gets
   `.dendro` by default.
7. **`metriken-dashboard` and `metriken-viewer`** (piece 5), after rezolus
   6.0.0, because the viewer is the part of rezolus changing fastest.
