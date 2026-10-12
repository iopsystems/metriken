# metriken-recorder: one recorder for every metriken producer

**Status:** OPEN. Design 2026-10-12. This is path step 4 of
[one recording stack](2026-10-08-one-recording-stack.md) (piece 2). "Step N
below" means this entry's own order of work. Decided 2026-10-12:

- rezolus's 5.x line does not adopt the library. 5.x is on an older
  dependency graph (metriken-exposition 0.21, metriken-archive 0.2, dendro
  0.3); adopting would backport the storage 0.2 / exposition 0.22 / dendro
  0.4 move and 6.x's recorder into a 5.x minor. 5.x's scrape paths keep
  working against every producer; 5.x `record --stream` refuses a producer
  serving the metriken wire names (stream-route entry, Compatibility).
- the library writes `.dendro` only. `.rez`, parquet and raw stay in rezolus,
  implemented against the library's sink trait, so the library carries no
  5.x-era container. This changes the plan's "`.rez` output for agents that
  cannot stream", which stays a rezolus behaviour (the rezolus 6.0 entry's
  decision 1).
- rezolus 6.0.0 is released after every open step of the plan (4 to 7) and
  its follow-ups; the plan's "after rezolus 6.0.0" for step 7 no longer holds.

## Goal

`rezolus record` is a CLI over `metriken-recorder`, and a metriken producer
records itself and the server it drives into one `.dendro` in process, with
no rezolus binary. The plan's GO criterion 2: `rezolus record` passes its
tests over the library, and one llm-perf run records the server under test
and llm-perf into one `.dendro` in process, with llm-perf's latencies as
native histograms.

## Today

Sources: metriken origin/main 9241fbc, rezolus upstream/main 1e00fec0
(6.0.0-alpha.46), llm-perf upstream/main edf0f20, cachecannon upstream/main
74056cb, dendro origin/main ed9b4a4.

**rezolus's recorder.** `src/recorder/`, all `pub(crate)`, production lines:

| module | lines | what |
|---|---|---|
| `stream.rs` | 747 | frame decoder, `StreamSubscriber`, `Subscription::connect` (reqwest), `pump` (reconnect after max(interval, 1 s), silence timeout), the legacy wire names, `wide_snapshot` for `.rez` |
| `prometheus.rs` | 490 | `PrometheusConverter`: one V3 group `prometheus/scrape` per scrape, windowed by the round trip |
| `restart.rs` | 152 | restart metadata patches (`producer_epochs`, version, systeminfo, descriptions) |
| `endpoint.rs` | 285 with tests | `EndpointConfig`, `EndpointState`, `infer_source_name` |
| `mod.rs` | 3,675 | CLI, `probe_endpoint` (:334), agent metadata from `/status` (else the `/` banner, before 5.16.0), `/systeminfo`, `/metrics/descriptions`, `/samplers`, epoch tracking, `open_stream`, `refuse_mid_run`, `activate_endpoint`, `adopt_source`, scrape, parquet converter, run events, `RezStream` with `Sink::{Rez, Dendro}`, and `run` (:2333-3674, 1,342 lines, 10 `process::exit` calls, global state for ctrl-c) |
| `config.rs`, `child.rs` | 457, 94 | CLI and TOML parsing, `-- cmd` |

hindsight (`src/hindsight/mod.rs`) and the live viewer (`src/viewer/live.rs`)
each run their own loop over `Subscription::connect` and `pump`, with
`source` hard-coded as `"rezolus"`. hindsight has a second copy of restart
handling (`record_restarts`, :836-872); the live viewer only logs a restart.
hindsight's buffer (`buffer.rs`) adds retention (`evict_before`, on both its
`.rez` and dendro writers, :266-290) and records each pass for its dump
logic (`record_tick`, `set_at_retention_bound`); the live viewer's buffer has
no retention and refreshes its reader after every interval it writes
(:318-327).

**Generic and rezolus-only.** Generic: the frame decoder, subscription and
pump, the Prometheus conversion, epoch and restart tracking, the `.dendro`
sink (one commit per tick, seal checks, late activation, finalize and
discard), refusing one endpoint mid-run, run events. rezolus-only: `/status`
and the other agent routes, `STREAM_SINCE` and its refusal text, the legacy
wire names, the `.rez`, parquet and raw sinks, `--separate`, the CLI, signals
and exit codes.

**Detection is not built.** `probe_endpoint` still classifies an endpoint by
`/metrics/binary` (a msgpack `Snapshot` means a rezolus agent, else
Prometheus at `/metrics`), and probes only when the URL's path is `/`. The
rezolus 6.0 entry's "Endpoint detection" and its decision 1 (an agent that
cannot stream is recorded to `.rez` when the output was not named, refused
for an explicit `-o x.dendro`) are not built either; `demote_from_rez`
(:2301) only handles `--separate`. This is why cachecannon and llm-perf serve
`/metrics/binary`.

**The handshake's labels and metadata are dropped.** `adopt_source` (:1041) keeps the uuid and
clock anchor and drops the labels and metadata. The recording's `source` is
the user's `--endpoint url,source=`, else `"rezolus"` for anything that
answered `/metrics/binary`, else a name inferred from the URL. So `--url`
records cachecannon as `source=rezolus` (stream-route entry, Follow-ups), and
a streamed producer gets no `version` (its `/status` 404s).

**What metriken provides.** metriken-storage (`write`): `ArchiveWriter`,
`WriterConfig`, `SourceRecorder::{stage, stage_streamed, maybe_seal,
update_metadata, sync, evict_before, finalize}`, `StreamDecoder`.
metriken-exposition: `WIRE_NAMES`, `Layout`, `GroupBuilder::snapshot`. dendro:
the wire (`decode_payload`, `MAX_FRAME_BYTES`). Missing: any HTTP consumer, a
Prometheus client, detection, reconnect and refusal policy, multi-source
orchestration, an epoch-aligned tick (exposition's is private), restart
patches, a source for the process's own registry.

**The wire types link the registry.** `WireNames`, `WIRE_NAMES` and `Layout`
are in `metriken-exposition` (`src/stream/route.rs:62,215`), which depends on
`metriken` unconditionally. A recorder importing them would link the
registry, against the plan's rule that only producers do.

## Design

### Crate

`metriken-recorder`, in this repository. It depends on metriken-model,
metriken-types, metriken-storage (`write`), dendro directly at storage's
version (the frame decoder needs `replicate::{Frame, wire}`, and
`StreamDecoder::decode` takes dendro's `WalRow`; storage does not re-export
dendro), tokio (`rt`, `time`, `sync`), async-trait, tracing, humantime,
serde_json and prometheus-parse; `http` adds reqwest and url. It does not
depend on `metriken` or `metriken-exposition`.

- **Wire types move to metriken-model.** `WireNames`, `WIRE_NAMES` and
  `Layout` move to metriken-model; exposition re-exports them at their
  current paths (no release-after removal: producers keep importing from
  exposition). A recorder then shares the producer's names without the
  registry.
- **Transport-free core.** The frame decoder, `StreamSubscriber` and the
  Prometheus converter take bytes and return frames or rows. An `http`
  feature (on by default) adds `Subscription`, `pump` and the scrape client
  over a `reqwest::Client` the caller builds and passes in, so TLS, auth
  headers and the crypto provider are the caller's (rezolus installs one at
  `src/main.rs:60`). reqwest 0.13 is the version; llm-perf is on 0.12 and
  gets a second copy until it moves.
- **A `registry` feature** adds a `LocalSource` over the process's own
  registry, depending on metriken-exposition. Off, a caller supplies its
  snapshots itself.

### Sources

- **Streamed.** `/metrics/stream?layout=long` with the caller's accepted
  `WireNames` (default `[WIRE_NAMES]`; rezolus adds its legacy names). The
  handshake's uuid and anchor are kept as today, and its labels and metadata
  are adopted (below).
- **Scraped.** Prometheus text at the URL, one acquisition group
  `prometheus/scrape` per scrape, windowed by the round trip, as today.
- **Local.** A `LocalSource` returns a `metriken_model::Snapshot` and its
  stamp (`ts`, `wall_offset`) per tick, taken by the producer
  (`GroupBuilder::snapshot`); its identity carries the process's
  `clock_anchor_wall_ns`. It is staged with `SourceRecorder::stage(snapshot,
  ts, wall_offset)`, so there is no frame encode or decode, and
  `HistogramGroup`s stay native.
- **Polled.** A `PolledSource` the caller implements, polled once per tick:
  `async fn poll(&mut self) -> Result<Option<(Snapshot, Stamp)>>`. rezolus's
  `/metrics/binary` scrape for `.rez`, parquet and raw output is one, so
  those runs go through the same `Recorder`.

### Detection

Detection runs before the archive is opened: `detect(endpoints, probe)`
returns each endpoint's transport, and the caller chooses the sink from the
result, since decision 1 makes a defaulted run's output depend on every
endpoint. For `.dendro` output, an endpoint whose path is `/` or
`/metrics/stream`:

1. Connect to `/metrics/stream`. A handshake in an accepted content type means
   streamed; the opened subscription is kept, not reconnected.
2. Otherwise (404, 409, a foreign content type) ask the caller's probe hook.
   rezolus's hook identifies a rezolus agent by `/status`, else the `/`
   banner (agents before 5.16.0), else a `/metrics/binary` body that decodes
   as a `Snapshot`. An agent that cannot stream gets decision 1: the run
   writes `.rez` when the output was defaulted, and an explicit `-o x.dendro`
   refuses it.
3. Otherwise scrape `/metrics` as Prometheus.

An unreachable endpoint is `Pending` and detected on each tick until it
answers; one that turns out to be an agent that cannot stream after the run
has started is refused mid-run, as `refuse_mid_run` does today. An endpoint
with any other path is scraped at that URL (llm-perf's
`server_metrics_url`). `--endpoint url,protocol=` keeps `msgpack` (streamed
into a `.dendro`, scraped otherwise) and `prometheus`, and gains `stream`.
`.rez`, parquet and raw output keep scraping `/metrics/binary`, through a
polled source in rezolus. A producer then needs `/metrics/binary` only for
those outputs and older recorders.

### Source identity

`source`, in order: the user's (`--endpoint url,source=` or `--metadata
source=`), the handshake's `source` label, `"rezolus"` when the probe hook
identifies a rezolus agent, the name inferred from the URL. The handshake's
metadata merges under the user's `--metadata`. A reconnect's handshake
carries the new process's metadata, so restart patching applies it, and a
producer that puts its identity in the handshake needs no refetch hook.
Metadata precedence, highest first: the user's `--metadata`; keys the
recorder writes (`sampling_interval_ms`, `producer_epoch`,
`producer_epochs`); the probe hook's; the handshake's. The hook's
`enrich(url, handshake)` runs after the handshake, which reverses today's
order (`/status` before the stream opens, `open_stream`'s doc, `mod.rs:727`),
so it never sets the epoch or anchor, and a reply whose epoch differs from
the handshake's uuid is discarded, as `fetch_restarted_agent` does. The
rezolus agent sends labels `{source}` and empty metadata today; its hook keeps
fetching `/status`, `/systeminfo` and `/metrics/descriptions` until it puts
those in its handshake. Piece 3's template (`service_queries` in the
handshake metadata) then reaches the archive with no recorder change.

### Orchestration

`Recorder` owns the sink and the sources: an aligned tick on multiples of the
interval from the Unix epoch; one commit per tick; late activation of an
unreachable endpoint; reconnect after max(interval, 1 s); refusing one
endpoint mid-run; seal checks; finalize at each source's last stamp, then
join the writer. Events (`Active`, `Pending`, `Dropped`, `Reconnected`,
`Gap`, `Refused`, and `Committed { source, ts, wall_ns }` after each commit)
go to the caller, which decides exit codes and logging. `Committed` is what
hindsight's per-pass bookkeeping, the live viewer's reader refresh and
`record`'s `-- cmd` end barrier (wait for each streamed source's frame
stamped at or after the child's exit, `mod.rs:3273-3305`) are built on.
The library never calls `process::exit` and installs no signal handler.
`commit` is a blocking send to dendro's writer thread; the tick runs it in
`spawn_blocking`. Retention (`retain: Option<Duration>`, `evict_before` each
tick) covers hindsight; the live viewer is a recorder with one source and no
retention, read beside it. hindsight's dumps need the archive handle the
sink owns; the sink trait exposes it to the caller.

### Sinks

A `Sink` trait (`add_source`, `stage`, `stage_streamed`, `commit`,
`patch_metadata`, `maybe_seal`, `evict_before`, `sync`, `finalize`,
`discard`) with `DendroSink::create(path, WriterConfig)` over
`ArchiveWriter` in the library; the writer configuration belongs to the
sink, not the recorder. rezolus implements `.rez`, parquet and
raw against it.

### API sketch

```rust
pub struct RecorderConfig {
    pub interval: Duration, pub timeout: Duration,
    pub layout: Layout, pub wire_names: Vec<WireNames>,
    pub retain: Option<Duration>,
}
pub enum Transport { Auto, Stream, Prometheus, Polled(Box<dyn PolledSource>) }
pub struct EndpointSpec {
    pub url: Url, pub transport: Transport,
    pub labels: BTreeMap<String, String>, pub metadata: BTreeMap<String, String>,
}
pub trait LocalSource: Send + 'static {
    fn identity(&self) -> SourceIdentity;          // labels, metadata, anchor
    fn pass(&mut self) -> Option<(metriken_model::Snapshot, Stamp)>;
}
#[async_trait]
pub trait PolledSource: Send {
    async fn poll(&mut self) -> Result<Option<(metriken_model::Snapshot, Stamp)>>;
}
#[async_trait]
pub trait Probe: Send + Sync {
    async fn not_streamable(&self, url: &Url) -> Verdict; // Scrape | Refuse(reason) | Pending
    async fn enrich(&self, url: &Url, handshake: &Handshake) -> BTreeMap<String, String>;
}
pub struct NoProbe; // Scrape, nothing to enrich: for producers recording themselves

pub async fn detect(client: &Client, endpoints: Vec<EndpointSpec>, probe: &dyn Probe)
    -> Vec<Detected>;
pub struct Recorder<S: Sink = DendroSink> { .. }
impl<S: Sink> Recorder<S> {
    pub async fn start(sink: S, cfg: RecorderConfig, client: Client, probe: Arc<dyn Probe>,
        detected: Vec<Detected>, local: Vec<Box<dyn LocalSource>>)
        -> Result<(Self, mpsc::Receiver<Event>), StartError>;
    pub fn sink(&self) -> &S;
    pub fn patch_metadata(&mut self, source: SourceId, patch: BTreeMap<String, String>)
        -> Result<()>;
    pub async fn finish(self) -> Result<Summary>;
    pub fn discard(self);
}
```

Names and shapes are settled in step 3 below, against rezolus's callers.

### What stays in rezolus

The clap and TOML CLI, `-- cmd`, signals and exit codes; the probe hook
(`/status`, `/systeminfo`, `/samplers`, descriptions); decision 1 and the
`STREAM_SINCE` refusal text; the legacy wire names, passed through the
config; the `.rez`, parquet and raw sinks; hindsight's dumps and the live
reader.

## GO / NO-GO

GO when all hold:

1. **rezolus over the library.** `rezolus record`, hindsight and the live
   viewer use `metriken-recorder`, with no loop of their own.
   `tests/record_lifecycle.rs`, `tests/hindsight_dump.rs` and the live
   viewer's tests pass. Every test whose expectation changes is one of the
   intended changes below, named in its commit.
2. **llm-perf in process.** One llm-perf run, with no rezolus binary, records
   the server under test (Prometheus) and llm-perf (local source) into one
   `.dendro`. `ttft`, `ttft_content`, `itl` and `tpot` are histograms there,
   and each member's count, p50 bucket and p99 bucket at the last sample,
   with the `request_latency` histogram as the control, equal llm-perf's own
   parquet, both last samples taken after the last request completed (the
   check from llm-perf's step 4).
3. **Detection by stream.** A producer serving `/metrics/stream` and no
   `/metrics/binary` is streamed; a Prometheus endpoint is scraped; a rezolus
   agent older than 5.21 gets decision 1. Each in a recorder test.
4. **Handshake identity.** `rezolus record --url` on cachecannon writes
   `source=cachecannon` and its `version`.
5. **No registry.** `cargo tree -p metriken-recorder -e normal` lists no
   `metriken`, `metriken-core` or `linkme`, with default features.
6. **Recorder cost.** `rezolus record` and `rezolus hindsight` CPU
   (utime+stime from `/proc/<pid>/stat`) and peak RSS (`VmHWM` from
   `/proc/<pid>/status`) over 300 one-second passes of one agent, five
   alternating runs per build on one Linux host, against rezolus main at the
   commit before step 2: a build fails when a median is more than 5% worse
   and the ranges do not overlap (the storage scan's rule). The harness is
   committed in rezolus with the change.

Intended changes, each to be named in its commit: a streamed producer's
`source` and `version` come from its handshake; a stream-only producer is
streamed rather than scraped; decision 1 applies (a defaulted run with an
agent that cannot stream writes `.rez`); `protocol=stream` is accepted.

NO-GO if the library cannot drive rezolus's three modes without depending on
rezolus agent types (`AgentStatus`, sampler status); then the probe hook is
wrong and this entry records why.

## Order of work

1. **metriken: wire types in metriken-model.** `WireNames`, `WIRE_NAMES` and
   `Layout` to metriken-model, re-exported by exposition. A patch release of
   model and exposition.
2. **rezolus: detection, decision 1 and handshake adoption in its own
   recorder,** as the plan's piece 2 says ("rezolus adds detection by stream
   to its own recorder first"). Checks 3 and 4 run here. hindsight and the
   live viewer are not restructured here; they move once, onto the library,
   in step 4.
3. **metriken: `metriken-recorder`** from that code, with rezolus's
   `record_lifecycle` scenarios rewritten as library tests (the fake agent,
   the fake exporter, legacy and metriken names, restart metadata, refusals).
   rezolus builds against the branch through `[patch.crates-io]` and passes
   check 1 before the release; checks 5 and 6 run here.
4. **rezolus on the released crate,** with `record`, hindsight and the live
   viewer all on `Recorder` (check 1).
5. **llm-perf in process** (its plan's step 5), check 2. cachecannon's
   in-process recording (its plan's step 4) follows in its repository.

## Left alone

- Archive growth for llm-perf and cachecannon recorded in process is not
  measured. llm-perf streamed grows about 450 KB per minute, from its
  histograms sent whole every pass (stream-route entry, Follow-ups).
- Scraping `/metrics/binary` into a `.dendro` (possible: `stage` takes V1 to
  V3). A producer serving msgpack but not the stream, other than a rezolus
  agent (decision 1), is scraped as Prometheus.
