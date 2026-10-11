# The stream route: any registry serves `/metrics/stream`

**Status:** OPEN. Design 2026-10-11. Step 2 built 2026-10-11 on metriken
branch `feat/stream-route` ("As built"); checks 1, 3 and 6 pass in metriken.
Steps 3 and 4 and checks 2, 4 and 5 remain. This is path step 3 of
[one recording stack](2026-10-08-one-recording-stack.md) (piece 1). "Step N
below" means this entry's own order of work. Decided 2026-10-11:

- the wire takes neutral names (`application/vnd.metriken.replication.v1+dendro`,
  `x-metriken-*`). A producer serves only those. A consumer is built with the
  list of names it accepts, and rezolus's recorder adds the 5.x agent's
  `vnd.rezolus.replication` names to it for at least one release;
- the gaps the [storage scan](2026-10-09-storage-scan.md) left alone wait
  until after this step.

## Goal

A producer with a metriken registry serves `/metrics/stream` by mounting one
helper on a route in its own HTTP server, with no frame code of its own. The
helper adds no SQLite to the producer's build. (A producer that also reads
archives, such as cachecannon's viewer through metriken-query, still compiles
SQLite for that.) The rezolus agent is the first user, cachecannon the first
outside one.

## Today

Sources: metriken `origin/main` 0e30e75, rezolus `upstream/main` 9ac10de7,
dendro `origin/main` 7bb1e21 (0.3.5-alpha.0; metriken and rezolus pin
0.3.4), cachecannon `origin/main` 809533a, llm-perf `upstream/main` e626b9e.

**The frame encoder is in storage, and uses nothing of storage's.**
`metriken-storage/src/stream.rs` (feature `stream`, 566 lines before its
tests) holds `FrameProducer`, `SchemaCache`, `StreamRow`, `EncodedGroup`,
`EncodedLongGroup` and `EncodedStreamGroup`. It needs metriken-model's row and
occupant encoders, `metriken::epoch`, metriken-exposition's
`StreamGroup`/`LongGroupSnapshot`/`GroupSnapshot`, and dendro's `Frame`,
`wire`, `WalRow` and `keys::PRODUCER_EPOCH`. The `storage` paths it imports
(`occupants`, `schema`, `wal`) are re-exports of metriken-model. It stayed in
storage in path step 2 only because exposition could not depend on storage
without a cycle (storage scan, "As built").

**The rest of the route is rezolus's.** About 770 production lines across
`src/agent/exposition/http/mod.rs` (the `stream` handler at :204 and
`rows_frames` at :306), `http/frames.rs`, `src/agent/clock.rs` and the stream
paths of `http/snapshot.rs`. Most of it is generic:

- `?interval=` (humantime, default 1s, zero refused) and `?layout=wide|long`;
- a timer per subscription aligned to the Unix epoch
  (`clock::until_next_aligned`), and `seq` from
  `clock::monotonic_interval_index`;
- one build and one encode per pass, shared by every subscriber
  (`CachedSnapshot`'s `OnceLock`s), with a freshness floor (the TTL). The pass
  is async: `rows_at` and `stream_long_at` (`snapshot.rs:241`, `:296`) await
  the samplers' refresh, behind a `tokio::sync::Mutex<SnapshotBuilder>`
  (`http/mod.rs:34`). The wide pass is storage's `AgentRows` (shared with
  `/metrics/rows`), the long pass `EncodedStreamGroup`s;
- an empty frame when the stamp has not advanced, and a per-connection filter
  that drops a group whose window equals the last one sent;
- the handshake from `FrameProducer::new({"source": ...}, {})`;
- response headers: `Content-Type: application/vnd.rezolus.replication.v1+dendro`
  (`src/agent/mod.rs:13`), `x-rezolus-frame-interval`,
  `x-rezolus-update-floor`, `x-rezolus-layout`.

What is rezolus's own: `RezolusRouter` and `ReaderGuard` (sampler attribution,
`ACQUISITION_GROUPS`, reader-stamped windows), the sampler refresh before each
pass, the `external/main` group, the V2/V3 switch (`serves_rows`, a 409 for
V2), and the `clock::Subscribers` guard each stream holds, which `/status`
counts.

The handler's doc comment (:195) describes a generation counter that ends a
stream which falls behind. No such check exists: a reader that stops reading
suspends the generator at `yield` with no lock held, and on resuming it skips
to the current pass and logs a `seq` gap.

**The consumer matches the names exactly.** rezolus's `Subscription::connect`
(`src/recorder/stream.rs:463`) refuses any `Content-Type` other than
`REPLICATION_CONTENT_TYPE`, and reads `x-rezolus-update-floor` and
`x-rezolus-layout` (wide when absent: 5.x agents do not send it). Agents from
5.21.0 through 6.0.0-alpha.42 serve these names.

**dendro always compiles SQLite.** rusqlite (bundled), arrow and parquet are
unconditional dependencies, and `default-features = false` only turns off
`write`. The wire needs none of them, but `replicate/wire.rs:13` and
`replicate/frame.rs:9` import `SegmentMeta` and `WalRow` from `archive.rs`,
which imports rusqlite, and `Error` has a `Sqlite` variant. A producer serving
the stream would compile SQLite, arrow and parquet.

**`MsgpackToParquet` is in exposition.** `convert.rs`, `parquet.rs` and
`hashed.rs` (about 900 lines), behind the default `parquet-conversion`
feature, are what put arrow and parquet into exposition. Users: rezolus
(`recording convert`, the recorder's parquet output), cachecannon
(`src/admin/mod.rs:630`) and llm-perf (`src/snapshot.rs:4`, `:91`).

**`GroupBuilder` drops `HistogramGroup`.** `build_groups` says so
(`group_builder.rs:437`); `Snapshotter` (V2) handles it (`snapshotter.rs:148`).
llm-perf's latencies (`TTFT`, `TPOT`, `ITL`, ...) are `HistogramGroup`s
(`src/metrics.rs:148-192`), so through the stream they would be absent, which
fails the plan's GO criterion for llm-perf.

**The outside producers.** cachecannon (metriken 0.9, exposition 0.16) has an
admin server on tokio with no framework (`src/admin/mod.rs:121-231`) that
writes whole responses with `Content-Length`; serving a stream needs a response
path that writes headers and then frames until the client closes. It already
depends on axum 0.8 for its viewer. Its `request_target` strips the query
string, which a stream request needs. Its metrics have no
acquisition groups, so under `DefaultRouter` they land in one windowless group.
llm-perf (metriken 0.8 and exposition 0.14, both from git rev 98db358) serves
`/metrics` with warp.

## Design

### dendro 0.4.0: the wire without SQLite

- `WalRow` and `SegmentMeta` move to a module with no SQLite dependency;
  `archive` re-exports them, so their paths do not change.
- A feature `archive`, on by default, gates `archive`, `read`, `rewrite`,
  `segment`, `writer` and `replicate::publisher`, the `Error::Sqlite`
  variant and its helpers, and the rusqlite, arrow and parquet dependencies.
  `write` implies `archive`. `Error` is `#[non_exhaustive]`, so gating a
  variant breaks no match.
- `replicate`'s `frame` and `wire` compile without `archive`; `Subscriber` and
  `ArchivePublisher` need it.
- CI: the `reader-only` and `msrv` jobs and `scripts/check-wasm.sh`, which run
  `--no-default-features` today, switch to `--no-default-features --features
  archive`, and a new job builds and tests the wire alone with
  `--no-default-features`.
- rusqlite stays at 0.40, so a graph holding metriken-archive 0.4 (storage 0.1,
  dendro 0.3) beside storage 0.2 still resolves `libsqlite3-sys`'s
  `links = "sqlite3"`. Types from the two do not interoperate.

A breaking release: a user with `default-features = false` who reads archives
must add `archive`. metriken-storage and rezolus are the users that do.

Check: `cargo tree -p dendro --no-default-features -e normal` lists no
rusqlite, arrow or parquet, and dendro's wire tests pass in that
configuration.

### metriken-exposition 0.22.0: `stream`

Feature `stream` = `msgpack` + `dep:dendro` (no default features). `metriken`
is already a dependency.

**Moved from storage:** `FrameProducer`, `SchemaCache`, `StreamRow`,
`EncodedGroup`, `EncodedLongGroup`, `EncodedStreamGroup`, `encode_groups`,
`KeyedLabels`, `SOURCE`, as `metriken_exposition::stream`. One addition:
`StreamRow` gains `fn window(&self) -> Option<Window>`, which the
per-connection filter reads (today rezolus reads `AgentRow.window` and the
encoded groups' fields directly, `http/mod.rs:403-425`).

**New: the route helper.** No HTTP framework and no async runtime in the core.
The subscription does not fetch passes: the caller fetches each pass however
its producer builds one (rezolus awaits a refresh; a plain registry builds
synchronously) and hands it in. The subscription owns its state, so it can move
into a `'static` body stream or a spawned task. Named `StreamSubscription`,
since rezolus already has two types called `Subscription`.

```rust
/// What a pass is stamped with: the arguments `FrameProducer::interval` takes,
/// and the wall time the empty-frame rule compares.
pub struct PassStamp { pub wall_ns: u64, pub ts: i64, pub wall_offset: i64 }

/// One pass of a producer's groups, in one layout.
pub trait Pass: Send + Sync + 'static {
    type Row: StreamRow;
    fn stamp(&self) -> PassStamp;
    fn rows(&self) -> &[Self::Row];
}
// exposition implements it for `(PassStamp, Vec<R>)` with
// `R: StreamRow + Send + Sync + 'static`, and for `Arc<P: Pass>`;
// storage for `AgentRows`; rezolus for its `StreamRows`.

/// `interval` and `layout` from a request's query string. Refuses a zero or
/// malformed interval and an unknown layout; an interval below the update
/// floor is accepted and gets empty frames, as rezolus does today. A refusal
/// carries the HTTP status and message.
pub struct StreamRequest { pub interval: Duration, pub layout: Layout }
impl StreamRequest {
    pub fn parse(query: &str) -> Result<Self, Refusal>;
}

pub struct StreamSubscription { .. }
impl StreamSubscription {
    pub fn new(request: StreamRequest, update_floor: Duration,
               labels: BTreeMap<String, String>,
               metadata: BTreeMap<String, String>) -> Self;
    /// `Content-Type` and the `x-metriken-*` headers, to send before the body.
    pub fn headers(&self) -> Vec<(&'static str, String)>;
    /// The preamble and the handshake frame.
    pub fn opening(&mut self) -> Result<Bytes>;
    /// The next multiple of the interval after `now`, counted from the Unix
    /// epoch.
    pub fn next_deadline(&self, now: SystemTime) -> SystemTime;
    /// The frame for one pass when its stamp advanced: rows whose window moved
    /// since the last frame on this subscription, and every windowless row.
    /// An empty frame when the stamp did not advance or `pass` is `None`.
    /// `seq` comes from `now`, read on waking and before the pass was fetched;
    /// a skipped `seq` is logged (exposition gains a `tracing` dependency).
    pub fn frame_at<P: Pass>(&mut self, pass: Option<&P>, now: SystemTime)
        -> Result<Bytes>;
    /// `frame_at` with no pass, without naming a `Pass` type.
    pub fn empty_frame_at(&mut self, now: SystemTime) -> Result<Bytes>;
}

/// Passes over a `GroupBuilder` for a producer with a plain registry: built and
/// encoded once per pass under a mutex, shared by every subscription, with a
/// freshness floor and `extra` groups from a callback.
pub struct RegistryPasses<R: Router> { .. }
impl<R: Router> RegistryPasses<R> {
    pub fn pass(&self, layout: Layout, now: SystemTime)
        -> Option<Arc<(PassStamp, Vec<EncodedStreamGroup>)>>;
}
```

Feature `stream-tokio` adds:

```rust
impl StreamSubscription {
    /// Sleeps to each deadline, reads `now` from `wall_now`, awaits
    /// `fetch(now)` and yields `frame_at(pass, now)`. `keep_alive` is dropped
    /// with the stream, which is how a producer ties a registration
    /// (rezolus's `Subscribers` guard) to the stream's lifetime. `wall_now` is
    /// `SystemTime::now` outside tests; rezolus's tests inject a clock, since
    /// `tokio::time::pause` does not control `SystemTime`
    /// (`http/mod.rs:772`, `:858`).
    pub fn into_stream<P, F, Fut>(self, fetch: F, keep_alive: impl Send + 'static,
                                  wall_now: impl Fn() -> SystemTime + Send + 'static)
        -> impl Stream<Item = io::Result<Bytes>> + Send + 'static
    where P: Pass, F: FnMut(SystemTime) -> Fut + Send + 'static,
          Fut: Future<Output = Option<P>> + Send;
}
```

`fetch` returns a `Send` future rather than being an async closure, because an
`AsyncFnMut` bound cannot require its future to be `Send` on stable Rust, and
axum's `Body::from_stream` needs a `Send` stream. The wide and long layouts
have different row types, so a producer calls `into_stream` once per layout
and turns each into its body. cachecannon's fetch is
`move |now| { let p = passes.clone(); async move { p.pass(layout, now) } }`;
its admin server can use `into_stream` or write the bytes from `opening`,
`next_deadline` and `frame_at` to the socket itself.

The timer and `seq` arithmetic (`src/agent/clock.rs`'s `until_next_aligned`,
`interval_index`, `monotonic_interval_index`) move in as private functions.
`Layout`, `StreamRequest` and the wire names are public, so a consumer shares
them:

```rust
pub struct WireNames {
    pub content_type: &'static str,
    pub frame_interval: &'static str,
    pub update_floor: &'static str,
    pub layout: &'static str,
}
pub const WIRE_NAMES: WireNames = WireNames {
    content_type: "application/vnd.metriken.replication.v1+dendro",
    frame_interval: "x-metriken-frame-interval",
    update_floor: "x-metriken-update-floor",
    layout: "x-metriken-layout",
};
```

The query parameters (`interval`, `layout`) keep their names, since they
contain no project name.

**`HistogramGroup` in `GroupBuilder`.** `build_groups` and `build_stream` emit
one member per index that `metadata_snapshot()` lists and `load_histogram`
returns, as `Snapshotter` does (`snapshotter.rs:148-152`), with labels from the
index's metadata. In the long layout this is more than a match arm:
`LongMember` (public fields) gains `histograms`, the eligibility test at
`group_builder.rs:1078` admits `HistogramGroup`, and `EncodedLongGroup::encode`
fills the histogram columns it leaves empty today (`stream.rs:235`). Storage's
long table already reads histogram columns (`long_table.rs:143-154`).

**`MsgpackToParquet` leaves.** It goes to metriken-storage with `ParquetOptions`
and the rest of `parquet.rs` and `hashed.rs` (below). Exposition then has no
arrow or parquet dependency, so the default `parquet-conversion` feature goes.
Breaking for its three users: each adds `metriken-storage` with `convert` and
changes its imports (rezolus at `src/main.rs:5`, `parquet_tools/convert.rs:11`,
`recorder/mod.rs:1239`).

### metriken-storage 0.2.0

- Gains `MsgpackToParquet` and its options as `metriken_storage::convert`
  (storage already has a `parquet` module, `CompositionSource`'s), under
  feature `convert`, which needs neither dendro nor the archive.
- dendro becomes optional behind a feature `archive`, on by default, which
  gates the modules that use dendro: the catalog, reader and writer,
  `encoder.rs` (`dendro::segment`), `filter.rs` (`dendro::rewrite`), the
  private `archive.rs`, `rez`, and the `lib.rs` re-exports of `Encoder`,
  `KeepMetrics`, `Catalog`, `DendroCatalog` and `ArchiveReader`, and the rest
  of the `lib.rs` re-exports from those modules (`lib.rs:74-78`: `archive::*`,
  `SegmentIndex`, `ENCODER_VERSION`, `READABLE_ENCODERS`,
  `LabeledRecordings`). A producer
  that converts to parquet uses `default-features = false, features =
  ["convert"]` and compiles no SQLite. The existing `rez` feature implies
  `archive`.
- `stream` becomes a re-export of `metriken_exposition::stream` for one release,
  and enables `metriken-exposition/stream`. `rez/wire.rs`'s `AgentRow` keeps its
  `StreamRow` impl here, since `AgentRow` is storage's type.

Breaking, because the exposition types in storage's signatures move to
exposition 0.22. metriken-query 0.36.0 follows for the dependency. It depends
on storage with `default-features = false` and implements `MetricsSource` for
`ArchiveReader` (`metriken-query/src/archive.rs:371`), so it gets a feature
`archive`, on by default, that enables `metriken-storage/archive` and covers
`archive.rs` and `eval_timestamps_for`. A query user that reads only parquet
can turn it off and compile no SQLite. Query drops the re-exports of storage
items it kept for one release in 0.35.0, as decided in path step 2.

metriken-archive and metriken-segment leave the workspace's `members` in this
PR: as members on storage 0.1 they fail `cargo metadata` once storage is 0.2,
and pointing them at crates.io storage 0.1 would bring a second `metriken-core`,
which declares `links`. Their last releases (0.4.0, 0.2.0) stay on storage 0.1
and get no new release; their users move to `metriken_storage` paths. Their
directories are deleted; the published sources stay in git history.

There is no cycle: exposition depends on model and dendro; storage depends on
exposition only under `stream`.

### rezolus

- The agent's handler parses a `StreamRequest`, keeps the V2 refusal (409),
  registers with `Subscribers`, and on each layout's arm returns
  `into_stream(fetch, guard)` with `headers()`. `fetch` locks the
  `SnapshotBuilder` and awaits `rows_at` (wide, `Arc<AgentRows>`) or
  `stream_long_at` (long, `Arc<StreamRows>`) as today, so the router, refresh,
  TTL, `external/main` and the pass cache shared with `/metrics/rows` stay.
  `frames.rs`'s producer and `rows_frames` go.
- `metriken-query` stays `default-features = false` in rezolus's workspace, so
  `crates/rez` declares it with `features = ["archive"]`, unconditionally: the
  read path, the wasm viewer's included, needs `MetricsSource for
  ArchiveReader`. `metriken-storage` is already `default-features = false`;
  storage's `rez` feature implies `archive`.
- The recorder's `Subscription::connect` takes a `&[WireNames]`. It matches the
  `Content-Type` against each entry's `content_type` and reads that entry's
  headers. rezolus passes `WIRE_NAMES` and its own `REZOLUS_LEGACY_WIRE_NAMES`
  (agents 5.21.0 through 6.0.0-alpha), so a 6.x recorder records those
  agents. The legacy names stay for at least one release; removing them is a
  later decision.
- Every metriken crate moves in this one PR, since exposition 0.21 and 0.22
  types cannot mix. Imports move from `metriken_archive`, `metriken_segment`
  and query's dropped re-exports (about 125 sites) to `metriken_storage`.
- The handler's stale backpressure comment is corrected to what happens.

**Compatibility.** A 5.x recorder refuses a new agent's stream, because the
content type differs; it reports the type it got. So do 6.0 alpha recorders,
hindsight and live viewers. The scrape path (`/metrics/binary`, used for
`.rez` and parquet) is unchanged and still works. Renaming without serving the
old names was decided 2026-10-11 with this cost.

### cachecannon, llm-perf

cachecannon serves the stream after its own step 2 (metriken 0.11): a
`RegistryPasses` over `DefaultRouter`, and a streaming response path in its
admin server that writes the subscription's bytes until the write fails. Its
PR is in its own repo.

llm-perf needs the `HistogramGroup` support above; serving the stream is its
own step 4.

## GO / NO-GO

GO for this step when all hold:

1. **Frames unchanged.** Step 0 below makes `rows_frames`' per-pass body a
   sync function from a pass (or none) and a wall time to a frame, with no
   behaviour change. A rezolus test records that function's frames for one
   sequence of passes (a repeated pass, a window that does not advance, a
   missing pass, wide and long) into a committed fixture, using
   `FrameProducer::for_source` (`stream.rs:388`) with a fixed uuid and anchor.
   The same sequence through `StreamSubscription` built the same way (a
   `#[doc(hidden)] pub` constructor taking the uuid and anchor, since the test
   is in rezolus) gives byte-equal frames. Only the
   HTTP headers' names change.
2. **One build per pass.** A rezolus test runs three `into_stream`
   subscriptions on the real clock (not `start_paused`) at one interval
   (50 ms, TTL at least the interval) against
   one `SnapshotBuilder` for N ticks and asserts `samples()` is at most N. One
   encode per pass holds by the `OnceLock`s on `rows_full` and `stream_long`
   (`snapshot.rs:98-101`), which this change does not touch.
3. **No SQLite.** `cargo tree -e normal` lists no rusqlite, arrow or parquet
   for `-p metriken-exposition --no-default-features --features stream`, and
   no rusqlite for `-p metriken-storage --no-default-features --features
   convert`.
4. **Agent cost.** Agent utime+stime from `/proc/<pid>/stat` over 300 passes
   with one `record -o x.dendro` long subscriber, five alternating runs per
   build on one Linux host, against rezolus at 9ac10de7. Both builds are
   recorded by the new recorder, which accepts both names. A build fails when its
   median is more than 5% worse and the ranges do not overlap, the storage
   scan's rule.
5. **The recorder** records an agent serving the legacy names and one serving
   the new names into one `.dendro`, in rezolus's recorder tests.
6. **`HistogramGroup`** values through `build_groups` match `Snapshotter`'s on
   a registry with one, and through `build_stream`'s long layout decoded by
   `StreamDecoder` give the same buckets.

NO-GO, as the plan states, if the helper needs an HTTP framework dependency or
a timer per producer rather than per subscription. Then the producers stay on
Prometheus scrape and this entry records why.

## Order of work

0. rezolus, on main: `rows_frames`' per-pass body becomes a sync function, and
   check 1's fixture is recorded from it.
1. dendro 0.4.0: the `archive` feature and the CI jobs. PR and release in
   dendro, since crates.io needs it before exposition publishes.
2. metriken: exposition's `stream` with the helper, `HistogramGroup`, the
   `MsgpackToParquet` move, storage 0.2.0 and query 0.36.0, and
   metriken-archive and metriken-segment out of the workspace, in one PR.
   metriken's tests pass checks 3 and 6. Before the release, rezolus builds
   against the branch through `[patch.crates-io]` and passes checks 1, 2 and
   5, as the storage scan did.
3. metriken release, then the rezolus PR on it; check 4 runs here.
4. cachecannon, in its repo, after its metriken 0.11 bump.

## As built

dendro 0.4.0 as merged (8cc03ee, 0.4.0-alpha.0, not yet released) leaves
`seal` ungated, since it has no dependencies; the line above said
otherwise. metriken depends on it as `0.4.0-alpha.0` through a workspace
`[patch.crates-io]` to that commit, which goes before metriken's release.

Step 2 is on metriken branch `feat/stream-route`, off c2b939c. Where it
differs from the design, or the design left a choice open:

- `RegistryPasses` has a second type parameter, `N: MemberNames =
  Positional`, so it takes a `GroupBuilder` with either naming;
  `RegistryPasses<R>` still names the design's type. It is built with
  `new(builder, floor)`, and `with_extra(f)` adds the extra groups;
  `floor()` returns the floor to pass as a subscription's update floor.
  The cache is per layout and keyed on the `now` given to `pass`: a pass is
  reused while `now` is less than the floor after the `now` it was built
  for, and rebuilt when `now` is earlier than that. A pass is stamped with
  `Stamp::now()` when it is built. A group that fails to encode logs a
  warning, drops the cached pass and returns `None`.
- `pass` returns `Option<RegistryPass>`, an alias for the design's
  `Arc<(PassStamp, Vec<EncodedStreamGroup>)>`.
- `opening`, `frame_at` and `empty_frame_at` return `std::io::Result<Bytes>`,
  the item type of `into_stream`.
- `StreamRequest::parse` refuses a parameter given twice with 400, as
  axum's `Query` extractor did for rezolus's handler, and decodes `%XX` and
  `+` in values. `StreamRequest` defaults to one second, wide.
- `StreamRow::window` has no default, so an implementation cannot leave
  every row windowless by omission.
- `StreamSubscription::for_source(request, update_floor, uuid, anchor,
  labels, metadata)` is the hidden constructor. `request()` returns the
  request; `Layout` has `as_str` and `Display`; `Refusal` implements
  `Error` and has public `status` and `message`.
- `into_stream` reads `wall_now` before sleeping, to compute the deadline,
  and again on waking for `seq`. Rezolus read `SystemTime::now` for the
  sleep and the injected clock once; with `SystemTime::now` they are the
  same.
- New exposition dependencies: `bytes`, `humantime` and `tracing` under
  `stream`; `tokio` (`time`), `futures-core` and `futures-util` under
  `stream-tokio`. dendro is a dev-dependency with its default features, for
  the test that applies frames into an archive; `cargo tree -e normal`
  excludes it.
- A histogram group's members ignore the router's `Membership`: each index
  its metadata lists whose histogram loads, sorted. Each member carries the
  metric's metadata, `id`, the slot's metadata, then `grouping_power` and
  `max_value_power`, which win over a slot key of the same name.
  `Snapshotter` adds no `id` and adds `description`; check 6 compares member
  names, buckets and slot labels. The identity pass loads each member's
  histogram to decide membership, so a build loads it twice. Histogram
  groups have no window and do not call `ReadGuard::mark_end`. In the long
  layout a histogram column's descriptor carries the configuration.
- The exposition default features are `msgpack` and `segment`
  (`parquet-conversion` enabled `msgpack`).
- Storage's `stream` no longer enables `metriken`; nothing in it used the
  registry. Storage's `stream` integration tests stay in storage and import
  `metriken_storage::stream`, which tests the re-export.
- Query's module `long` became test-only, and its tests import the relabel
  from `metriken_storage`. Five rustdoc warnings in query (two links to
  dropped re-exports, three that predate this change) are fixed, so
  `cargo doc -D warnings` passes for exposition, storage, query, model and
  types. It still fails for metriken-core and metriken, unchanged from
  main.

Checks, run 2026-10-11:

- **1.** `metriken-exposition/tests/stream_frames.rs` reproduces
  `tests/fixtures/stream-frames/{wide,long}.bin` (copied from rezolus
  f55709b8; sha1 `19caeafe…` and `7d1a6cf8…`) byte for byte through
  `StreamSubscription::for_source` with rezolus's uuid, anchor, labels and
  metadata and its pass sequence. The wide passes hold `EncodedGroup`s
  there; `metriken-storage/tests/stream_frames.rs` runs the wide sequence
  with `Arc<AgentRows>` and matches too. Setting `advanced` to `true` in
  `frame_at` fails both fixtures.
- **3.** `cargo tree -e normal` lists no rusqlite, arrow or parquet for
  `-p metriken-exposition --no-default-features --features stream` (and
  `stream-tokio`), and no rusqlite or dendro for `-p metriken-storage
  --no-default-features --features convert` or for `-p metriken-query
  --no-default-features --features lz4,ingest`.
- **6.** `metriken-exposition/tests/histogram_group.rs`: `build_groups` on a
  registry with a `HistogramGroup` gives the members, slot labels and
  buckets `Snapshotter` gives, and reuses the schema until a slot's
  metadata changes. `metriken-storage/tests/stream_histogram.rs`:
  `build_stream` sends it long, `StreamDecoder` decodes each member's
  buckets equal to `load(idx)` under its labels, the writer stages the rows,
  and `ArchiveReader` lists one histogram series per member.

Checks 2 and 5 need rezolus built against this branch; check 4 runs on the
rezolus PR.

## Left alone

- The decode-and-re-encode round trip in `StreamDecoder` (storage scan).
- A consumer library: the recorder's `Subscription` and `pump` stay in
  rezolus until path step 4.
- Windows for plain counters in an outside producer: rows are stamped per
  pass with no per-metric window, as piece 1 says. Whether that is enough for
  a load generator is not measured.
