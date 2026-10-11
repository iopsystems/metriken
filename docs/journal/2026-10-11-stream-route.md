# The stream route: any registry serves `/metrics/stream`

**Status:** OPEN. Design 2026-10-11. This is path step 3 of
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
helper on a route in its own HTTP server, with no frame code of its own and no
SQLite in its build. The rezolus agent is the first user, cachecannon the first
outside one.

## Today

Sources: metriken `origin/main` 0e30e75, rezolus `upstream/main` 9ac10de7,
dendro `origin/main` 7bb1e21 (0.3.5-alpha.0; metriken and rezolus pin
0.3.4), cachecannon `origin/main` 809533a, llm-perf `upstream/main` e626b9e.

**The frame encoder is in storage, and uses nothing of storage's.**
`metriken-storage/src/stream.rs` (feature `stream`, 564 lines before its
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
`rows_frames` at :306), `frames.rs`, `clock.rs` and the stream paths of
`snapshot.rs`. Most of it is generic:

- `?interval=` (humantime, default 1s, zero refused) and `?layout=wide|long`;
- a timer per subscription aligned to the Unix epoch
  (`clock::until_next_aligned`), and `seq` from
  `clock::monotonic_interval_index`;
- one build and one encode per pass, shared by every subscriber
  (`CachedSnapshot`'s `OnceLock`s), with a freshness floor (the TTL);
- an empty frame when the stamp has not advanced, and a per-connection filter
  that drops a group whose window equals the last one sent;
- the handshake from `FrameProducer::new({"source": ...}, {})`;
- response headers: `Content-Type: application/vnd.rezolus.replication.v1+dendro`
  (`src/agent/mod.rs:13`), `x-rezolus-frame-interval`,
  `x-rezolus-update-floor`, `x-rezolus-layout`.

What is rezolus's own: `RezolusRouter` and `ReaderGuard` (sampler attribution,
`ACQUISITION_GROUPS`, reader-stamped windows), the sampler refresh before each
pass, the `external/main` group, the V2/V3 switch (`serves_rows`, a 409 for
V2), and the `Subscribers` count reported in `/status`.

The handler's doc comment (:195) describes a generation counter that ends a
stream which falls behind. No such check exists: a reader that stops reading
suspends the generator at `yield` with no lock held, and on resuming it skips
to the current pass and logs a `seq` gap.

**The consumer matches the names exactly.** rezolus's `Subscription::connect`
(`src/recorder/stream.rs`) refuses any `Content-Type` other than
`REPLICATION_CONTENT_TYPE`, and reads `x-rezolus-update-floor` and
`x-rezolus-layout` (wide when absent).

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
(`src/admin/mod.rs:630`) and llm-perf (`src/snapshot.rs:84`).

**`GroupBuilder` drops `HistogramGroup`.** `build_groups` says so
(`group_builder.rs:437`); `Snapshotter` (V2) handles it (`snapshotter.rs:148`).
llm-perf's latencies (`TTFT`, `TPOT`, `ITL`, ...) are `HistogramGroup`s
(`src/metrics.rs:148-192`), so through the stream they would be absent, which
fails the plan's GO criterion for llm-perf.

**The outside producers.** cachecannon (metriken 0.9, exposition 0.16) has a
hand-written admin server (`src/admin/mod.rs:44-121`) that writes whole
responses with `Content-Length`; serving a stream needs a response path that
writes headers and then frames until the client closes. Its metrics have no
acquisition groups, so under `DefaultRouter` they land in one windowless group.
llm-perf (metriken 0.8, a git pin of exposition) serves `/metrics` with warp.

## Design

### dendro 0.4.0: the wire without SQLite

- `WalRow` and `SegmentMeta` move to a module with no SQLite dependency;
  `archive` re-exports them, so their paths do not change.
- A feature `archive`, on by default, gates `archive`, `read`, `rewrite`,
  `seal`, `segment`, `publisher` and `writer`, the `Error::Sqlite` variant and
  its helpers, and the rusqlite, arrow and parquet dependencies. `write`
  implies `archive`.
- `replicate`'s `frame` and `wire` compile without `archive`; `Subscriber` and
  `ArchivePublisher` need it.

A breaking release: a user with `default-features = false` who reads archives
must add `archive`. metriken-storage and rezolus are the users that do.

Check: `cargo tree -p dendro --no-default-features -e normal` lists no
rusqlite, arrow or parquet, and dendro's wire tests pass in that
configuration.

### metriken-exposition 0.22.0: `stream`

Feature `stream` = `msgpack` + `dendro` (no default features) + `metriken`.

**Moved from storage, unchanged:** `FrameProducer`, `SchemaCache`,
`StreamRow`, `EncodedGroup`, `EncodedLongGroup`, `EncodedStreamGroup`,
`encode_groups`, `KeyedLabels`, `SOURCE`, as `metriken_exposition::stream`.

**New: the route helper.** No HTTP framework and no async runtime in the core.

```rust
/// What a producer supplies: one pass of its groups, built once and shared by
/// every subscriber that wakes for it.
pub trait Passes: Send + Sync {
    /// The groups as of `now`, in `layout`. An implementation may return a
    /// pass built earlier when it is younger than its freshness floor.
    fn pass(&self, layout: Layout, now: SystemTime) -> Option<Arc<Pass>>;
    /// The shortest interval a subscription can ask for.
    fn update_floor(&self) -> Duration;
}

/// `Passes` over a `GroupBuilder`: builds and encodes once per pass, under a
/// mutex, with `extra` groups from a callback. What a producer with a plain
/// registry uses.
pub struct RegistryPasses<R: Router> { .. }

/// One stream: the identity in its handshake and the passes it reads.
pub struct StreamRoute<P: Passes> { .. }
impl<P: Passes> StreamRoute<P> {
    pub fn new(passes: P, labels: BTreeMap<String, String>,
               metadata: BTreeMap<String, String>) -> Self;
    /// Parses `interval` and `layout` from a query string. A refusal carries
    /// the HTTP status and message to send.
    pub fn subscribe(&self, query: &str) -> Result<Subscription<'_, P>, Refusal>;
}

pub struct Subscription<'a, P> { .. }
impl<P: Passes> Subscription<'_, P> {
    /// `Content-Type` and the `x-metriken-*` headers, to send before the body.
    pub fn headers(&self) -> Vec<(&'static str, String)>;
    /// The preamble and the handshake frame.
    pub fn opening(&mut self) -> Result<Bytes>;
    /// When the next frame is due: the next multiple of the interval after
    /// `now`, counted from the Unix epoch.
    pub fn next_deadline(&self, now: SystemTime) -> SystemTime;
    /// The frame for the pass at `now`: rows for the groups whose window
    /// moved since the last frame on this subscription, or an empty frame.
    pub fn frame_at(&mut self, now: SystemTime) -> Result<Bytes>;
}
```

Feature `stream-tokio` adds `Subscription::into_stream()`, an
`impl Stream<Item = io::Result<Bytes>>` that sleeps to each deadline. axum's
`Body::from_stream` takes it as is. A server without tokio, such as
cachecannon's admin thread, calls `opening`, then `next_deadline` and
`frame_at` in its own loop.

The timer and `seq` arithmetic (`clock.rs`'s `until_next_aligned`,
`interval_index`, `monotonic_interval_index`) move in as private functions.
`Layout`, the wire names and the query parsing are public, so a consumer
shares them:

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

The query parameters (`interval`, `layout`) keep their names, since they name
nothing.

**`HistogramGroup` in `GroupBuilder`.** `build_groups` and `build_stream`
treat it as they treat `CounterGroup`: one member per populated index, its
labels from the group's metadata, slotted in the long layout. Checked against
`Snapshotter` on the same registry: the same histograms with the same buckets.

**`MsgpackToParquet` leaves.** It goes to metriken-storage with `ParquetOptions`
and the rest of `parquet.rs` and `hashed.rs` (below). Exposition then has no
arrow or parquet dependency, so the default `parquet-conversion` feature goes.
Breaking for its three users; each changes one import.

### metriken-storage 0.2.0

- Gains `MsgpackToParquet` and its options under feature `convert`, which
  needs neither dendro nor the archive.
- dendro becomes optional behind a feature `archive`, on by default, which
  gates the catalog, reader, writer and `rez`. A producer that converts to
  parquet uses `default-features = false, features = ["convert"]` and compiles
  no SQLite. If a module outside those needs a dendro type, it moves under
  `archive` too; `convert` needs none.
- `stream` becomes a re-export of `metriken_exposition::stream` for one release,
  and enables `metriken-exposition/stream`. `rez/wire.rs`'s `AgentRow` keeps its
  `StreamRow` impl here, since `AgentRow` is storage's type.

Breaking, because the exposition types in storage's signatures move to
exposition 0.22. metriken-query 0.36.0 follows for the dependency, and drops
the re-exports of storage items it kept for one release in 0.35.0, as decided
in path step 2. metriken-archive and metriken-segment get no new release; they
stay on storage 0.1, and their users move to `metriken_storage` paths.

There is no cycle: exposition depends on model and dendro; storage depends on
exposition only under `stream`.

### rezolus

- The agent implements `Passes` over its `SnapshotBuilder`, keeping its
  router, refresh, TTL, `external/main` and pass cache. Its route calls
  `StreamRoute::subscribe` and returns `into_stream()` with `headers()`.
  `frames.rs`'s producer and `rows_frames` go. The V2 refusal (409) stays in
  the handler.
- The recorder's `Subscription::connect` takes a `&[WireNames]`. It matches the
  `Content-Type` against each entry's `content_type` and reads that entry's
  headers. rezolus passes `WIRE_NAMES` and its own `REZOLUS_5_WIRE_NAMES`
  (the names agents 5.21 to 5.x serve), so a 6.x recorder records a 5.x agent.
  The 5.x names stay for at least one release; removing them is a later
  decision.
- Imports move from `metriken_archive`/`metriken_segment` to `metriken_storage`.
- The handler's stale backpressure comment is corrected to what happens.

**Compatibility.** A 5.x recorder refuses a 6.x agent's stream, because the
content type differs; it reports the type it got. Its scrape path
(`/metrics/binary`, used for `.rez` and parquet) is unchanged and still
works. This was the cost of option B, accepted 2026-10-11.

### cachecannon, llm-perf

cachecannon serves the stream after its own step 2 (metriken 0.11): a
`RegistryPasses` over `DefaultRouter`, and a streaming response path in its
admin server that writes `headers()`, then `opening()`, then `frame_at` at
each `next_deadline` until the write fails. Its PR is in its own repo.

llm-perf needs the `HistogramGroup` support above; serving the stream is its
own step 4.

## GO / NO-GO

GO for this step when all hold:

1. **Frames unchanged.** For the agent's frame fixtures (`frames.rs` tests and
   `http/mod.rs`'s stream tests), the bytes of the opening and every interval
   frame through `StreamRoute` equal those from the 0.21 `FrameProducer` path,
   wide and long. Only the HTTP headers' names change.
2. **One build per pass.** With three subscribers at the same interval, the
   agent builds and encodes once per pass (a test counts `rebuilds()` and
   encodes).
3. **No SQLite.** `cargo tree -e normal` lists no rusqlite, arrow or parquet
   for `-p metriken-exposition --no-default-features --features stream`, and
   no rusqlite for `-p metriken-storage --no-default-features --features
   convert`.
4. **Agent cost.** The agent's CPU time per pass serving one long-layout
   subscriber, measured as in the storage scan's gate (several alternating
   runs, a measure fails only when its median is more than 5% worse and the
   ranges do not overlap), is within that rule of rezolus at 9ac10de7.
5. **The recorder** records a 5.x agent (old names) and a 6.x agent (new
   names) into one `.dendro`, in rezolus's recorder tests.
6. **`HistogramGroup`** values through `build_groups` match `Snapshotter`'s
   on a registry with one.

NO-GO, as the plan states, if the helper needs an HTTP framework dependency or
a timer per producer rather than per subscription. Then the producers stay on
Prometheus scrape and this entry records why.

## Order of work

1. dendro 0.4.0: the `archive` feature. PR and release in dendro.
2. metriken: exposition's `stream` with the helper, `HistogramGroup`, the
   `MsgpackToParquet` move, storage 0.2.0 and query 0.36.0. One PR, one
   release.
3. rezolus: the agent on the helper, the recorder's name list, the imports.
   Checks 1, 2, 4 and 5 run here.
4. cachecannon, in its repo, after its metriken 0.11 bump.

## Left alone

- The decode-and-re-encode round trip in `StreamDecoder` (storage scan).
- A consumer library: the recorder's `Subscription` and `pump` stay in
  rezolus until path step 4.
- Windows for plain counters in an outside producer: rows are stamped per
  pass with no per-metric window, as piece 1 says. Not measured.
