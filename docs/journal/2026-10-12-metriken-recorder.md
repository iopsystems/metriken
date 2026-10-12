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
  cannot stream", which stays a rezolus behaviour (its decision 1).
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
| `mod.rs` | 3,675 | CLI, `probe_endpoint` (:334), agent metadata from `/status`, `/systeminfo`, `/metrics/descriptions`, `/samplers`, epoch tracking, `open_stream`, `refuse_mid_run`, `activate_endpoint`, `adopt_source`, scrape, parquet converter, run events, `RezStream` with `Sink::{Rez, Dendro}`, and `run` (:2333, 1,343 lines, 14 `process::exit` calls, global state for ctrl-c) |
| `config.rs`, `child.rs` | 457, 94 | CLI and TOML parsing, `-- cmd` |

hindsight (`src/hindsight/mod.rs`) and the live viewer (`src/viewer/live.rs`)
each run their own loop over `Subscription::connect` and `pump`, with a
second copy of restart handling (hindsight :838-885) and `source` hard-coded
as `"rezolus"`. hindsight's buffer (`buffer.rs`) adds retention
(`evict_before`).

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

**The handshake is half used.** `adopt_source` (:1041) keeps the uuid and
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
metriken-types, metriken-storage (`write`), dendro (through storage), tokio
(`rt`, `time`, `sync`), tracing, humantime, serde_json and prometheus-parse.
It does not depend on `metriken` or `metriken-exposition`.

- **Wire types move to metriken-model.** `WireNames`, `WIRE_NAMES` and
  `Layout` move to metriken-model; exposition re-exports them at their
  current paths (no release-after removal: producers keep importing from
  exposition). A recorder then shares the producer's names without the
  registry.
- **Transport-free core.** The frame decoder, `StreamSubscriber` and the
  Prometheus converter take bytes and return frames or rows. An `http`
  feature (on by default) adds `Subscription`, `pump` and the scrape client
  over reqwest 0.13 with `rustls-no-provider`; the caller installs the crypto
  provider, as rezolus does (`src/main.rs:60`).
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
- **Local.** A `LocalSource` returns a `metriken_model::Snapshot` per tick,
  stamped by the producer (`GroupBuilder::snapshot`), with the process's
  `clock_anchor_wall_ns`. It is staged with `SourceRecorder::stage`, so there
  is no frame encode or decode, and `HistogramGroup`s stay native.

### Detection

For `.dendro` output, an endpoint whose path is `/` or `/metrics/stream`:

1. Connect to `/metrics/stream`. A handshake in an accepted content type means
   streamed; the opened subscription is kept, not reconnected.
2. Otherwise (404, 409, a foreign content type) ask the caller's probe hook.
   rezolus's hook reads `/status`: a rezolus agent that cannot stream gets
   decision 1 (the run writes `.rez` when the output was defaulted; an
   explicit `-o x.dendro` refuses).
3. Otherwise scrape `/metrics` as Prometheus.

An endpoint with any other path is scraped at that URL (llm-perf's
`server_metrics_url`). `--endpoint url,protocol=stream|prometheus` forces
the choice. `.rez`, parquet and raw output keep scraping `/metrics/binary`;
that path stays in rezolus. A producer then needs `/metrics/binary` only for
those outputs and older recorders.

### Source identity

`source`, in order: the user's (`--endpoint url,source=` or `--metadata
source=`), the handshake's `source` label, `"rezolus"` when the probe hook
identifies a rezolus agent, the name inferred from the URL. The handshake's
metadata merges under the user's `--metadata`. A reconnect's handshake
carries the new process's metadata, so restart patching applies it, and a
producer that puts its identity in the handshake needs no refetch hook. The
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
`Gap`, `Refused`) go to the caller, which decides exit codes and logging.
The library never calls `process::exit` and installs no signal handler.
`commit` is a blocking send to dendro's writer thread; the tick runs it in
`spawn_blocking`. Retention (`retain: Option<Duration>`, `evict_before` each
tick) covers hindsight; the live viewer is a recorder with one source and
retention, read beside it.

### Sinks

A `Sink` trait (`add_source`, `stage`, `stage_streamed`, `commit`,
`patch_metadata`, `maybe_seal`, `finalize`, `discard`) with `DendroSink`
over `ArchiveWriter` in the library. rezolus implements `.rez`, parquet and
raw against it.

### API sketch

```rust
pub struct RecorderConfig {
    pub interval: Duration, pub timeout: Duration,
    pub layout: Layout, pub wire_names: Vec<WireNames>,
    pub writer: metriken_storage::WriterConfig,
    pub retain: Option<Duration>,
}
pub enum Transport { Auto, Stream, Prometheus }
pub struct EndpointSpec {
    pub url: Url, pub transport: Transport,
    pub labels: BTreeMap<String, String>, pub metadata: BTreeMap<String, String>,
}
pub trait LocalSource: Send + 'static {
    fn identity(&self) -> SourceIdentity;
    fn pass(&mut self) -> Option<metriken_model::Snapshot>;
}
#[async_trait]
pub trait Probe: Send + Sync {
    async fn not_streamable(&self, url: &Url) -> Verdict; // Scrape | Refuse | Defer
    async fn enrich(&self, url: &Url, handshake: &Handshake) -> BTreeMap<String, String>;
}
pub struct Recorder<S: Sink = DendroSink> { .. }
impl<S: Sink> Recorder<S> {
    pub async fn start(sink: S, cfg: RecorderConfig, probe: Arc<dyn Probe>,
        endpoints: Vec<EndpointSpec>, local: Vec<Box<dyn LocalSource>>)
        -> Result<(Self, mpsc::Receiver<Event>), StartError>;
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
   and each member's count, p50 bucket and p99 bucket at the last sample
   equal llm-perf's own parquet (the check from llm-perf's step 4).
3. **Detection by stream.** A producer serving `/metrics/stream` and no
   `/metrics/binary` is streamed; a Prometheus endpoint is scraped; a rezolus
   agent older than 5.21 gets decision 1. Each in a recorder test.
4. **Handshake identity.** `rezolus record --url` on cachecannon writes
   `source=cachecannon` and its `version`.
5. **No registry.** `cargo tree -p metriken-recorder -e normal` lists no
   `metriken`, `metriken-core` or `linkme`, with default features.
6. **Recorder cost.** `rezolus record` CPU (utime+stime) and peak RSS over
   300 one-second passes of one agent, five alternating runs per build on one
   Linux host, against rezolus main before the change: a build fails when a
   median is more than 5% worse and the ranges do not overlap (the storage
   scan's rule).

Intended changes, each to be named in its commit: a streamed producer's
`source` and `version` come from its handshake; a stream-only producer is
streamed rather than scraped; decision 1 applies.

NO-GO if the library cannot drive rezolus's three modes without depending on
rezolus agent types (`AgentStatus`, sampler status); then the probe hook is
wrong and this entry records why.

## Order of work

1. **metriken: wire types in metriken-model.** `WireNames`, `WIRE_NAMES` and
   `Layout` to metriken-model, re-exported by exposition. A patch release of
   model and exposition.
2. **rezolus: detection, decision 1 and handshake adoption in its own
   recorder,** as the plan's piece 2 says ("rezolus adds detection by stream
   to its own recorder first"). hindsight and the live viewer move onto the
   same orchestration inside rezolus, so one loop remains. Checks 3 and 4 run
   here.
3. **metriken: `metriken-recorder`** from that code, with rezolus's
   `record_lifecycle` scenarios rewritten as library tests (the fake agent,
   the fake exporter, legacy and metriken names, restart metadata, refusals).
   rezolus builds against the branch through `[patch.crates-io]` and passes
   check 1 before the release; checks 5 and 6 run here.
4. **rezolus on the released crate.**
5. **llm-perf in process** (its plan's step 5), check 2. cachecannon's
   in-process recording (its plan's step 4) follows in its repository.

## Left alone

- Windowless plain counters in llm-perf and cachecannon recorded in process:
  archive growth is not measured. llm-perf streamed grows about 450 KB per
  minute (stream-route entry, Follow-ups).
- Scraping `/metrics/binary` into a `.dendro` (possible: `stage` takes V1 to
  V3). A producer serving msgpack but not the stream is scraped as Prometheus.
