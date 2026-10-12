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
- rezolus 6.x writes `.dendro` only: `record`, hindsight, the live viewer,
  Save as Report and every `recording` subcommand that writes a new file.
  `.rez`, parquet and raw output are removed from 6.x; reading them stays.
  The library therefore writes `.dendro` only and has no sink trait. An
  agent older than 5.21.0, which cannot stream, is refused by 6.x with a
  pointer to a 5.x recorder. This replaces the rezolus 6.0 entry's decision 1
  (record it to `.rez`) and the plan's "`.rez` output for agents that cannot
  stream"; the rezolus 6.0 entry is to be updated to match (decision 1
  withdrawn, and the removal of the Save as Report and `recording` writers
  added to its plan).
- rezolus 6.0.0 is released after every open step of the plan (4 to 7) and
  its follow-ups (the plan's step 7 said "after rezolus 6.0.0" until
  2026-10-12).

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
`.rez` and dendro writers, :266-290); hindsight records each pass for its
`/status` and dump logic (`record_tick`, `set_at_retention_bound` in
`hindsight/state.rs:57,65`, called from `hindsight/mod.rs:493,521`). The
live viewer's buffer has no retention, and the viewer refreshes its reader
after every interval it writes (`live.rs:318-327`).

**Generic and rezolus-only.** Generic: the frame decoder, subscription and
pump, the Prometheus conversion, epoch and restart tracking, the `.dendro`
sink (one commit per tick, seal checks, late activation, finalize and
discard), refusing one endpoint mid-run, run events. rezolus-only: `/status`
and the other agent routes, `STREAM_SINCE` and its refusal text, the legacy
wire names, the CLI, signals and exit codes. The `.rez`, parquet and raw
sinks and `--separate` (one file per endpoint) go with 6.x's `.dendro`-only
writing.

**Detection is not built.** `probe_endpoint` still classifies an endpoint by
`/metrics/binary` (a msgpack `Snapshot` means a rezolus agent, else
Prometheus at `/metrics`), and probes only when the URL's path is `/`. The
rezolus 6.0 entry's "Endpoint detection" and its decision 1 (an agent that
cannot stream is recorded to `.rez` when the output was not named, refused
for an explicit `-o x.dendro`) are not built either; `demote_from_rez`
(:2301) only handles `--separate`. This is why cachecannon and llm-perf serve
`/metrics/binary`.

**The handshake's labels and metadata are dropped.** `adopt_source` (:1041)
keeps the uuid and clock anchor and drops the labels and metadata. The
recording's `source` is the user's `--endpoint url,source=`, else `"rezolus"`
for anything that answered `/metrics/binary`, else a name inferred from the
URL. So `--url` records cachecannon as `source=rezolus` (stream-route entry,
Follow-ups), and a streamed producer gets no `version` (its `/status` 404s).

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

### Detection

Detection runs before the archive is opened, so a run that cannot record an
endpoint fails before writing anything. `detect` takes the subscription
settings (`DetectConfig { interval, timeout, layout, wire_names }`, so the
caller's legacy names reach the connection) and returns a `Detected` per
endpoint. An endpoint whose path is `/` or `/metrics/stream`:

1. Connect to `/metrics/stream`. A handshake in an accepted content type means
   streamed (`Detected::Streamed`, with the open subscription and handshake,
   handed to `start`).
2. Otherwise (404, 409, a foreign content type) ask the caller's probe hook.
   A 5xx or a handshake timeout is `Pending`, retried each tick, as today
   (`ConnectError::Unreachable`, `mod.rs:999-1006`).
   rezolus's hook identifies a rezolus agent by `/status`, else the `/`
   banner (agents before 5.16.0), else a `/metrics/binary` body that decodes
   as a `Snapshot`, and refuses one (`Verdict::Refuse`), naming its version
   and pointing at a 5.x recorder.
3. Otherwise scrape `/metrics` as Prometheus (`Detected::Scraped`).

A refused endpoint at startup makes rezolus exit 1 before the archive is
opened, naming the agent, as today (`mod.rs:2431-2468`). An unreachable
endpoint is `Pending` and detected on each tick until it answers; one that
turns out to be an agent that cannot stream after the run has started is
refused mid-run, as `refuse_mid_run` does today. A streamed source whose
reconnect is refused is `Refused` without asking the hook, so it is never
switched to a scrape mid-recording (`mod.rs:1137-1143`). The hook is told
whether it is asked at startup, on late activation or on a reconnect, so it
can word each refusal as today (`RefusedAt`, `mod.rs:904-925`). The stream
route is pull-based, so a subscription opened by `detect` and read later by
`start` costs at most a `Gap`. An endpoint with any other path is scraped at
that URL (llm-perf's `server_metrics_url`). `--endpoint url,protocol=` keeps
`msgpack` (streamed, the agent's meaning today for a `.dendro`) and
`prometheus`, and gains `stream`. A producer then needs `/metrics/binary` only
for older recorders.

### Source identity

Labels, highest first: the user's (`--endpoint url,source=`, `--label`), the
probe hook's (rezolus's: `host` from the agent's `/systeminfo`, as
`rez::build_labels` does today, `mod.rs:1543`), the handshake's. `source`, in
order: the user's, the handshake's `source` label, `"rezolus"` when the probe
hook identifies a rezolus agent, the name inferred from the URL. Labels are
fixed when the source is added to the archive, so `start` awaits the hook's
first `enrich` for each streamed endpoint before adding it, bounded by
`timeout`; after that, only the handshake's labels are used.

Metadata, highest first: the user's `--metadata`; keys the recorder writes
(`sampling_interval_ms`, `producer_epoch`, `producer_epochs`); the probe
hook's; the handshake's. A reconnect's handshake carries the new process's
metadata, so restart patching applies it, and a producer that puts its
identity in the handshake needs no refetch hook. The hook's `enrich(url,
epoch)` runs after the handshake, which reverses today's order (`/status`
before the stream opens, `open_stream`'s doc, `mod.rs:727`), so it never sets
the epoch or anchor, and a reply whose epoch differs from the handshake's uuid
is discarded, as `fetch_restarted_agent` does. The rezolus agent sends labels
`{source}` and empty metadata today; its hook keeps fetching `/status`,
`/systeminfo` and `/metrics/descriptions` until it puts those in its
handshake. Piece 3's template (`service_queries` in the handshake metadata)
then reaches the archive with no recorder change.

### Orchestration

`Recorder` owns the sink and the sources: an aligned tick on multiples of the
interval from the Unix epoch; one commit per tick; late activation of an
unreachable endpoint; reconnect after max(interval, 1 s); refusing one
endpoint mid-run; seal checks; finalize at each source's last stamp, then
join the writer. The library never calls `process::exit` and installs no
signal handler. `commit` is a blocking send to dendro's writer thread; the
tick runs it in `spawn_blocking`. Retention (`retain: Option<Duration>`,
`evict_before` each tick) covers hindsight; the live viewer is a recorder
with one source and no retention, read beside it. hindsight's dumps open the
buffer by path (`buffer::dump`, `buffer.rs:511`, `:574`), as today, so
`Recorder` has no accessor for its sink.

Events go to the caller, which decides exit codes and logging: `Active {
source, transport: ActiveTransport }`, `Pending`, `Dropped`, `Reconnected`,
`Gap`, `Refused`, `Failed { error }` (the sink failed; the recorder stops
ticking and closes the channel, as a writer failure ends `record` and
hindsight today), `Frame { source, seq, stamp: Option<Stamp> }` for every
frame including empty ones, emitted after the frame is staged, with `seq`
the recorder's per-source frame count, monotonic across reconnects, and
`Committed { source, passes_total, first_ts, ts, wall_ns }` after each
commit, cumulative so a dropped event loses nothing (`ts` is the newest pass
staged). hindsight's pass count and
`at_retention_bound` (newest minus `first_ts`) and the live viewer's reader
refresh are built on `Committed`.

`finish(self, stop: Stop)` stops the tick first, so no pass is taken while
it waits, then:

- `Stop::AfterExit { wall_ns, bound }`, for `record`'s `-- cmd`: takes one
  more pass of every local and scraped source at once, unless the exit
  landed during the pass just taken (the recorder tracks pass times, as
  `scraped_last_pass` does today, `mod.rs:2740-2757`), then waits, up to
  `bound`, for each streamed source's frame, empty or not, stamped at or
  after `wall_ns` (`mod.rs:3273-3305`). The barrier is inside the library,
  so it does not depend on events reaching the caller.
- `Stop::Grace(duration)`, for `--duration`, ctrl-c and every other stop:
  waits `duration` (`record` passes the shorter of one interval and 2 s when
  a source streams, as `mod.rs:3281-3285` does) and takes no extra pass.
- `Stop::Now`, for a producer recording itself after its last pass
  (`LocalSource::pass` is called once more first).

It then stages every frame already received, writes any pending restart
without further retries, finalizes each source and joins the writer, which is
the order today's shutdown follows after its last pass (`drain_stream_events`,
then `apply_restarts(.., last = true)`, `mod.rs:3350-3365`). Dropping a
`Recorder` stops its tasks and leaves the archive unfinalized on disk.

A caller that ignores events drops the receiver. The channel is bounded.
`Active`, `Refused` and `Failed` are never dropped (the recorder awaits their
send); `Frame`, `Gap` and `Committed` are dropped when it is full and counted,
and the next delivered event is `Lagged { dropped }`. `Summary` also carries
each source's final state and transport, so exit codes do not depend on
events. `start` returns the `SourceId` of each detected endpoint and local
source, which `patch_metadata` and the events use; `patch_metadata` is
`async` and returns once the patch has reached the writer, which it follows
with `sync` (storage's `update_metadata` logs and skips a patch
it cannot apply, `writer.rs:1296-1306`).

Restart patches keep today's semantics (`restart::metadata_patch`): the
recorder writes the epoch history, and the probe hook's `merge(key, old,
new)` combines values it owns (rezolus's `descriptions` is a union of old
and new; `version` goes into each `producer_epochs` entry). After three
`Retry`s the restart is written without the hook's metadata, as
`FETCH_ATTEMPTS` does today.

### Archive

`Recorder` writes one `.dendro` through `ArchiveWriter`, created with
`WriterConfig` in `RecorderConfig`. There is no sink trait: 6.x writes no
other container.

### API sketch

```rust
pub struct RecorderConfig {
    pub path: PathBuf, pub writer: metriken_storage::WriterConfig,
    pub interval: Duration, pub timeout: Duration,
    pub layout: Layout, pub wire_names: Vec<WireNames>,
    pub retain: Option<Duration>,
}
pub enum Transport { Auto, Stream, Prometheus }
pub struct EndpointSpec {
    pub url: Url, pub transport: Transport,
    pub labels: BTreeMap<String, String>, pub metadata: BTreeMap<String, String>,
}
pub trait LocalSource: Send + 'static {
    fn identity(&self) -> SourceIdentity;          // labels, metadata, anchor
    fn pass(&mut self) -> Option<(metriken_model::Snapshot, Stamp)>;
}
pub enum Verdict { Scrape, Refuse { reason: String }, Pending }
pub enum At { Startup, Activation, Reconnect }
pub enum Stop { AfterExit { wall_ns: u64, bound: Duration }, Grace(Duration), Now }
pub enum ActiveTransport { Stream, Prometheus, Local }
pub enum Enrich {
    Metadata { epoch: Option<String>, labels: BTreeMap<String, String>,
               metadata: BTreeMap<String, String> },
    Retry,
}
#[async_trait]
pub trait Probe: Send + Sync {
    async fn not_streamable(&self, url: &Url, at: At) -> Verdict;
    // `epoch`: the handshake's uuid. A reply whose epoch differs is
    // discarded; Retry asks again next tick.
    async fn enrich(&self, url: &Url, epoch: Option<&str>) -> Enrich;
    fn merge(&self, key: &str, old: &str, new: &str) -> String { new.to_string() }
}
pub struct NoProbe; // Scrape, nothing to enrich: for producers recording themselves

pub enum Detected {
    Streamed { spec: EndpointSpec, subscription: Subscription, handshake: Handshake },
    Scraped { spec: EndpointSpec },
    Refused { spec: EndpointSpec, reason: String },
    Pending { spec: EndpointSpec },
}
pub struct DetectConfig { pub interval: Duration, pub timeout: Duration,
                          pub layout: Layout, pub wire_names: Vec<WireNames> }
pub async fn detect(client: &Client, cfg: &DetectConfig, endpoints: Vec<EndpointSpec>,
    probe: &dyn Probe) -> Vec<Detected>;
pub struct Recorder { .. }
impl Recorder {
    pub async fn start(cfg: RecorderConfig, client: Client, probe: Arc<dyn Probe>,
        detected: Vec<Detected>, local: Vec<Box<dyn LocalSource>>)
        -> Result<(Self, Vec<SourceId>, mpsc::Receiver<Event>), StartError>;
    pub async fn patch_metadata(&self, source: SourceId, patch: BTreeMap<String, String>)
        -> Result<()>;
    pub async fn finish(self, stop: Stop) -> Result<Summary>;
    pub fn discard(self);
}
```

Names and shapes are settled in step 3 below, against rezolus's callers.

### What stays in rezolus

The clap and TOML CLI, `-- cmd`, signals and exit codes; the probe hook
(`/status`, `/systeminfo`, `/samplers`, descriptions) and the refusal text
for an agent that cannot stream; the legacy wire names, passed through the
config; hindsight's dumps and the live reader.

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
   agent older than 5.21 is refused at startup (exit 1, nothing written,
   pointer to a 5.x recorder) and mid-run (refused, the rest recorded). Each
   in a recorder test.
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
streamed rather than scraped; `protocol=stream` is accepted; `record` and
hindsight write `.dendro` only (the live viewer already does): `record -o
x.rez`, `.parquet`, `.raw`, `--format` other than dendro and `--separate`
are refused with a pointer to a 5.x recorder or `rezolus recording upgrade
--to dendro`, and hindsight refuses an `output` ending in `.rez` or
`.parquet` at startup (`config/hindsight.toml` drops its `.rez` comment); an
agent older than 5.21 is refused.

Callers that ask `record` for parquet or `.rez` are refused by 6.x and must
ask for `.dendro` before they run it: llm-perf's server metrics with an
`output` not ending in `.dendro` (`src/server_metrics.rs:22,46-53`), and
systemslab's runner, which asks a rezolus from 5.19 for `-o x.rez`
(`crates/runner/src/metrics.rs:276-300`). Each change lands before rezolus
6.0.0 (systemslab's in its 6.0 branch, iopsystems/systemslab#6371).

NO-GO if the library cannot drive rezolus's three modes without depending on
rezolus agent types (`AgentStatus`, sampler status); then the probe hook is
wrong and this entry records why.

## Order of work

1. **metriken: wire types in metriken-model.** `WireNames`, `WIRE_NAMES` and
   `Layout` to metriken-model, re-exported by exposition. A patch release of
   model and exposition.
2. **rezolus: `.dendro`-only writing, detection and handshake adoption in
   its own recorder,** as the plan's piece 2 says ("rezolus adds detection by
   stream to its own recorder first"). This removes the `.rez`, parquet and
   raw writers from `record` and hindsight (the live viewer already writes
   `.dendro` only), and the old-agent refusal replaces decision 1. Checks 3 and 4 run here. hindsight and the
   live viewer are not restructured here; they move once, onto the library,
   in step 4. The other writers of `.rez` and parquet (Save as Report,
   `recording` subcommands) are the rezolus 6.0 entry's to remove, before
   6.0.0.
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
  agent (refused), is scraped as Prometheus.
