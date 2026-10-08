# One recording stack for every metriken producer

**Status:** OPEN. Plan opened 2026-10-08, nothing built. Decided 2026-10-08:

- recording, exposition and viewing converge on one stack, and per-project
  recorders and viewer implementations are retired as their projects move;
- the stack is metriken crates (below), named for what they do with metriken
  data rather than for dendro, which stores that data without reading it;
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

## Crates

| Crate | Holds | From |
|---|---|---|
| `metriken-storage` | The archive reader and writer: catalog, encoder, `ArchiveReader` / `ArchiveWriter`, and staging of streamed rows (`StreamDecoder`, `SourceRecorder::stage_streamed`). dendro is today's container; the name leaves room for another. | `metriken-archive`, renamed. One `metriken-archive` release re-exports it so current dependents keep building. |
| `metriken-exposition`, feature `stream` | The producer side of the stream: `FrameProducer` and a route helper for any registry (piece 1). | `metriken-archive`'s `stream` module |
| `metriken-recorder` | Records sources into storage: a remote stream, a Prometheus scrape (as one acquisition group per scrape), and the process's own registry. Reconnects, restart metadata, several sources in one archive. | rezolus `src/recorder` (`stream.rs`, `prometheus.rs`, `restart.rs` and the orchestration in `mod.rs`); `rezolus record` keeps its CLI, `-- cmd` (`child.rs`) and the choice of output format |
| `metriken-dashboard` | The dashboard model: sections, groups and plots from a `metriken_query::MetricsSource`, and templates. | rezolus `crates/dashboard` |
| `metriken-viewer` | The viewer as a library: a server router to mount, the chart and UI assets, the WASM bundle, report saving. Depends on `metriken-query`, `metriken-storage`, `metriken-dashboard`, and `metriken-recorder` for live mode. | rezolus `src/viewer`, `crates/viewer`, `crates/report-save` |

`metriken-segment` and `metriken-query` are unchanged.

Open: whether the stream's producer side is a feature of `metriken-exposition`
or a crate of its own (`metriken-streaming`). The recommendation is the
feature: serving the stream is one more way a process exposes its metrics,
beside Prometheus text and msgpack, and the consumer side belongs to
`metriken-storage` and `metriken-recorder`, so the two halves share only the
frame types that dendro and `metriken-segment` already define. Also open:
whether `metriken-dashboard` and `metriken-viewer` live in this repository
or a repository of their own; the viewer brings a JS build and static assets.

## Pieces

### 1. A stream route for any registry

`stream::FrameProducer` encodes a producer's groups as dendro replication
frames and leaves the transport to the caller. The rezolus agent supplies the
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
`Metric::value_with_window`, whose default returns none
(`metriken-core` `lib.rs`); only `WindowedLazyCounter`, `WindowedLazyGauge` and
`RwLockHistogram` return one. A producer using plain counters gets rows stamped
per pass and no per-metric window. Whether that is enough for a load
generator's counters is not yet measured.

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

A source is detected by its stream: a source whose `/metrics/stream`
handshake opens is streamed, whether or not it is a rezolus agent, and one
that does not is scraped, except a rezolus agent that predates the stream,
which the rezolus entry's decision 1 covers. Today `record` streams only a
source it takes for a rezolus agent from `/metrics/binary` (rezolus entry,
"Endpoint detection").

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

- **Bindings over `metriken-storage` and `metriken-query`** (PyO3), returning
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
systemslab mount the same library. systemslab today vendors the chart layer
(`svelte-ui/src/lib/charts/rezolus-next/upstream/`, pinned at a rezolus
revision) and takes `dashboard` from a rezolus git tag; it takes published
crates and assets instead. The move happens once, after rezolus 6.0.0 is
tagged, because the viewer is the part of rezolus changing fastest.
`cachecannon view` reads `.dendro` before then through its metriken bump
(cachecannon entry).

### 6. What stays

`MsgpackToParquet` stays in `metriken-exposition`, and every reader keeps
reading parquet. A producer's parquet-at-exit path stays as an option for one
release after the producer records through `metriken-recorder` (cachecannon
step 4, llm-perf step 7). New work does not add per-project recorders or
viewers.

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

## Order

1. Rename `metriken-archive` to `metriken-storage` and move the stream's
   producer side.
2. The stream route, with the rezolus agent as its first user and cachecannon
   as its first outside one.
3. `metriken-recorder`, from rezolus's recorder; then llm-perf records in
   process.
4. Templates in the archive: the route helper puts the template in the
   handshake, the recorder keeps handshake metadata, and the viewer reads
   `service_queries` from a source's metadata.
5. Python bindings, before any consumer outside Rust gets `.dendro` by
   default.
6. `metriken-dashboard` and `metriken-viewer`, after rezolus 6.0.0.
