# Members that come and go: slot identity, the group builder and families in metriken

**Status:** DONE in metriken — 5a, 5b and 5c built; 5b measured GO against rezolus
(2026-09-29, "5b: measured" below). Phase 5 of
[the high-cardinality stack](2026-09-28-high-cardinality-stack.md). Both
decisions below were made on 2026-09-29: `.rez` stops being a `--stream`
target in rezolus 6.0, and all three parts land before rezolus 6.0.0, in
the order 5a, 5b, 5c.

## Goal

A group whose members come and go should be something metriken models, not
something each producer builds around metriken. Today rezolus does all of it:

- which occupant holds a slot, and its `__uid__`;
- walking the registry into acquisition groups (`SnapshotV3`);
- serving those groups as dendro replication frames on `/metrics/stream`.

metriken has the storage side already: `metriken-segment` writes a group
with slots long, `metriken-archive` keys occupants by `__uid__` (else by
label set, `metriken-archive/src/writer.rs:311-314`). The producer side is
missing, so a service with per-tenant metrics has no way to produce what the
writer stores long.

Phase 5 moves the producer side into metriken, in three parts that can land
separately:

- **5a. Slot identity for fixed-capacity groups.** Assigning labels to a
  slot, minting `__uid__`, releasing a slot.
- **5b. The group builder and the stream frame producer.** Walking the
  registry into `SnapshotV3` groups, and turning group snapshots into dendro
  replication frames.
- **5c. Families in the dynamic registry.** A metric per member, registered
  and dropped at runtime, snapshotted as one group whose occupants are
  registration ids.

## What exists

References are to rezolus `main` (6.0.0-alpha) and metriken `main`.

### In rezolus

**Slot identity** is `SlotIdentity` (`src/agent/identity.rs:277`). It holds
the groups a slot space covers (one cgroup id reaches three
`scheduler_runqueue` streams, `:259-263`) and the live assignments,
`BTreeMap<usize, (labels, uid)>` (`:290-294`).

- `set(slot, labels)` (`:311-339`) returns early when the slot already has
  those labels. Otherwise it mints a uid, writes the labels plus `__uid__`
  to every metric of every group through metriken's per-slot metadata
  (`set_metadata`), and publishes the change once per group.
- `clear(slot)` (`:343-357`) removes the assignment, clears the metadata and
  publishes a removal.
- `mint_uid` (`:160-171`) is FNV-1a-64 over the process's `producer_epoch`
  and a process-wide generation counter (`GENERATION`, `:77`), as 16 hex
  digits. Unique per process by the counter, across processes by the
  random epoch (`src/agent/epoch.rs`).

Writers: per-task `cpu_usage` (`set` on `task_info`, `clear` on
`task_exit`, `src/agent/samplers/cpu/linux/usage/mod.rs:129,141`); the
cgroup samplers through `process_cgroup_info`
(`src/agent/bpf/mod.rs:716,763`, `set` only); the filesystem slot registry's
consumers (`src/agent/bpf/counters.rs:411-463`); and samplers that re-set
labels every refresh (filesystem, drivehealth, ethtool, GPU), which the
early return makes free.

Known gaps, carried into this design:

- A dropped `task_exit` leaves the old occupant as a phantom member at 0
  (`usage/mod.bpf.c:489-506`).
- No cgroup removal reaches `clear`. A removed cgroup's slot keeps its
  labels until its css id is reused.
- `set` publishes a generation read after minting (`:332`), so a concurrent
  `set` can stamp a later number than the uid was minted from.

**The group builder** is `create_v3` (`src/agent/exposition/http/snapshot.rs:2037`).

- It walks `metriken::metrics()` (`:2070`) and routes each metric to a
  group by rezolus's module-to-sampler attribution (`:2093`) and its
  `acq_group` metadata against rezolus's `ACQUISITION_GROUPS` registry
  (`:2108-2128`).
- Membership has three modes (`:2095-2114`): a dense prefix
  (`member_bound`), an explicit set (`member_set`), or the slots with
  metadata (`for_each_metadata`, `:2305-2307`).
- Each member's `MetricDesc` is the metric's metadata plus `id` and the
  slot's labels (`:2323-2334`), named `{metric_id}x{idx}` by the metric's
  position in the registry.
- A skeleton cache reuses a group's schema and hash while
  `metadata_version()` and the member set are unchanged (`:1660`,
  `:2854-2860`).

What is rezolus's: sampler attribution, the acquisition-group registry
(windows, member modes), the `log_` filter, external metrics. What is
generic: walking the registry into groups, the member descriptors, the
schema, its hash and the cache.

**The stream** is `/metrics/stream` (`src/agent/exposition/http/mod.rs:75`,
`rows_frames` at `:277`). It sends dendro's preamble and handshake
(uuid = `producer_epoch`), then per interval `Frame::Index` entries (the
identity index, `crates/rez/src/index.rs`) and `Frame::Rows` whose rows are
encoded `WalGroupRow`s, the schema inserted when it changed for that
subscriber (`frames.rs:131-202`).

The recorder (`src/recorder/stream.rs`) checks each Rows frame's
`index_state` against the index it has accumulated and skips rows that do
not match (`:285-292`, `:341-365`). For a `.dendro` output it rebuilds
snapshots from the schemas in the rows and writes no index (occupants come
from `__uid__` in the schema). For a `.rez` output it writes the index
entries beside the rows. The index, its broadcast
(`identity.rs:185-206`) and the `Demand` refcount that gates it
(`:93-111`) exist for `.rez --stream` and for that check.

### In metriken

- **Groups** have fixed capacity (`CounterGroup::new(entries)`,
  `metriken/src/group/counter.rs:77-93`; gauge and histogram alike) and a
  per-slot metadata API: `set_metadata`, `insert_metadata`,
  `clear_metadata`, `with_metadata`, `metadata_version`
  (`counter.rs:225-272`), backed by a lazily created
  `RwLock<HashMap<usize, HashMap<String, String>>>` with a version counter
  (`group/metadata.rs:18-121`). There is no uid, no notion of an
  occupant, and no change notification.
- **The dynamic registry** keys a metric by its address (`key_for`,
  `metriken-core/src/dynmetrics.rs:24-37`), which is reused after a free;
  `MetricBuilder` registers (`:63`) and dropping a `DynPinnedMetric`
  unregisters (`:276-281`). `metrics()` iterates static entries, then
  dynamic ones in address order, under one global read guard
  (`metriken-core/src/metrics.rs:16-80`).
- **The snapshotter** produces `Snapshot::V1` only
  (`metriken-exposition/src/snapshotter.rs:186`) and names columns by
  position (`:68-72`). metriken-exposition defines `SnapshotV3`,
  `GroupSnapshot`, `GroupSchema` and its hash, but builds none of them
  outside tests.

## Design

### 5a. Slot identity for fixed-capacity groups

A type in `metriken` that owns a slot space shared by one or more groups:

```rust
static TASKS: SlotIdentity = SlotIdentity::new(&[&TASK_CPU, &TASK_SWITCHES]);

TASKS.assign(pid, labels);   // new occupant, or the same one relabelled
TASKS.release(pid);          // the occupant left
```

- **Semantics are rezolus's, moved:** `assign` with unchanged labels is a
  no-op; otherwise it mints a uid and writes labels plus `__uid__` to every
  group's slot metadata. `release` clears it. One generation per process,
  taken once per assignment and used for both the uid and anything
  published (fixing the double read).
- **`producer_epoch` moves too.** The uid needs it, and so do snapshot
  metadata and the stream handshake. metriken mints it once per process,
  and the name and semantics stay dendro's `keys::PRODUCER_EPOCH`.
- **No change broadcast.** The identity index is gone (decision 1); the
  occupant stream is built from `__uid__` in the schema, and nothing else
  consumes a change feed.
- **A liveness hook**, since both known phantom cases are a missed release:
  `retain(|slot, labels| bool)` walks the live assignments at the caller's
  cadence and releases those the caller reports gone. rezolus would check a
  task's start time against `task_start_times` and a cgroup's css serial;
  metriken only provides the walk. This is the check the changes-only entry
  left open (rezolus `docs/journal/2026-09-28-changes-only-stream.md`).

rezolus then deletes `identity.rs`'s identity half and its `epoch.rs`, and
its samplers call the same methods on the metriken type.

### 5b. The group builder and the stream frame producer

In `metriken-exposition`, behind a feature:

- **`GroupBuilder`**, the generic half of `create_v3`: walk the registry,
  ask a caller-supplied router which group a metric belongs to (and the
  group's window and membership mode), and build `GroupSnapshot`s with the
  member descriptors, schema, hash and skeleton cache. rezolus supplies the
  router (sampler attribution, `ACQUISITION_GROUPS`) and keeps its external
  metrics and filters.
- **Member names stop depending on registry position** where a producer
  can say otherwise. `{metric_id}x{idx}` changes when a metric registers
  before another; the schema hash changes with it, and so does every column
  name in a wide table. A metric's name plus its static metadata is stable;
  the builder can name members by that when every metric in a group has a
  distinct name. Kept as an option, since today's names are what existing
  archives carry.
- **`FrameProducer`**, the transport-free half of `rows_frames`: preamble,
  handshake, and per interval the Rows frames of the groups a subscriber
  asked for, schema inserted on change. rezolus keeps the axum route and its
  per-connection filtering.

The first check of 5b is equivalence: rezolus's snapshot and stream tests
pass unchanged against the moved builder, and a snapshot built both ways
has the same groups, schemas and hashes.

### 5c. Families in the dynamic registry

For a service, members are not BPF map entries but metrics created when a
tenant appears and dropped when it leaves. What that needs:

- **A registration id** that is never reused: a process-wide counter taken
  at `register`, stored in the registry entry beside the address key. It is
  the member's occupant, and `__uid__` is minted from it as in 5a.
- **A family**: one name, one group, a member per label set.

  ```rust
  static REQUESTS: CounterFamily = CounterFamily::new("requests");
  let tenant = REQUESTS.member([("tenant", "acme")]); // registers
  tenant.increment();
  drop(tenant);                                        // unregisters
  ```

  The group builder snapshots a family as one group whose members are its
  live registrations, so it is written long like any group with slots.
- **Cost at scale, measured before it is relied on.** Each member is a boxed
  allocation registered under the global `RwLock`, and every snapshot walks
  the whole registry. The entry records, at 1k, 10k, 100k and 1M members:
  register and drop latency, snapshot build time, and bytes per member. If
  the global lock or the walk is the limit, a family keeps its own member
  table instead of one registry entry per member; that is the design
  alternative, and the measurement chooses.

**Measured (2026-09-29).** One counter per member, registered through
`MetricBuilder` with two metadata keys, on one thread of an Apple-silicon
laptop (macOS); the snapshot is metriken-exposition's `Snapshotter` (V1):

| members | register | drop | memory | registry walk | V1 snapshot | snapshot msgpack | register, worst, during snapshots |
|---|---|---|---|---|---|---|---|
| 1k | 392 ns | 110 ns | ~1 KB | 0.03 ms | 0.35 ms | 0.0 MB | 0.4 ms |
| 10k | 207 ns | 98 ns | 488 B | 0.15 ms | 1.7 ms | 0.5 MB | 1.3 ms |
| 100k | 188 ns | 144 ns | 642 B | 1.9 ms | 17.7 ms | 5.3 MB | 11.7 ms |
| 1M | 201 ns | 150 ns | 651 B | 21 ms | 178 ms | 57 MB | 121 ms |

Register and drop are per member; memory is the resident-set growth per
member. The median register during snapshots stayed at 0.2 µs; the worst
equals one snapshot, because a snapshot holds the registry's global read
guard for its whole walk and a registration waits for it. The walk itself is
an eighth of the snapshot: the rest is building each member's name and
metadata map.

So a registry entry per member meets the bar at 100k (an 18 ms snapshot)
and does not at 1M (178 ms, and a 121 ms stall for a member created
mid-snapshot). **Decision: a family keeps its own member table** instead of
one registry entry per member. It holds registration ids, never reused, each
member's labels once, and the values in a slab, under the family's own lock.
The family is one registry entry. Its snapshot reuses a cached schema while
membership is unchanged and copies values, so neither the global guard nor a
per-member map is on the snapshot path. The measurement is repeated against
the family table as its GO.

**Built and measured (2026-09-29): GO.** `CounterFamily` and `GaugeFamily`
(`metriken/src/family.rs`), same machine and members:

| members | create | drop | memory | value read | label read | worst create during reads | V1 snapshot |
|---|---|---|---|---|---|---|---|
| 1k | 506 ns | 82 ns | 655 B | 0.00 ms | 0.01 ms | 0.8 ms | 0.65 ms |
| 10k | 198 ns | 66 ns | 313 B | 0.01 ms | 0.05 ms | 0.05 ms | 2.8 ms |
| 100k | 190 ns | 83 ns | 378 B | 0.09 ms | 0.48 ms | 0.3 ms | 30 ms |
| 1M | 194 ns | 86 ns | 401 B | 2.2 ms | 5.5 ms | 1.4 ms | 298 ms |

The value read is what a group builder with a cached schema takes every
tick; the label read is what it takes when membership changed. Against the
criteria: 2.2 ms (plus 5.5 ms on a membership change) is under 50 ms at 1M;
a create waits at most 1.4 ms, less than the 5.5 ms label copy, where a
registry entry waited 121 ms; 401 B per member is under 650 B.

metriken-exposition's V1 `Snapshotter` is slower on a family than on
registry entries (298 ms against 178 ms at 1M), because it builds a name
and clones the metadata of every member every snapshot. It is not the path
families are for; 5b's group builder, with its schema cache, is.

rezolus does not use 5c: its members are BPF map entries read in bulk, and a
registration per thread would add an allocation and a lock to every thread
start. 5c is for services.

## Decisions (2026-09-29)

1. **`.rez` is no longer a `--stream` target in rezolus 6.0.**
   `record --stream` writes `.dendro` only, which takes identity from
   `__uid__` in the rows' schemas. What existed only for `.rez --stream`
   goes:
   - the identity index (`IndexEntry`, `SourceIndex`, `Frame::Index`);
   - its broadcast and the `Demand` refcount;
   - the recorder's `index_state` check.

   5.x keeps `.rez --stream` on `release/5.x`. A 5.x `record --stream`
   against a 6.0 agent receives no index frames, which is accepted at a
   major version; `--stream` has been opt-in since it arrived in 5.21.
2. **All three parts land before rezolus 6.0.0**, 5c included, though
   rezolus does not use it: families are the reason the stack moved into
   metriken, and settling their registry before 6.0 keeps the producer API
   from changing again after it. The order is 5a, 5b, 5c, since a family's
   uid is 5a's and its snapshot as a group is 5b's.

## GO / NO-GO

- **5a:** rezolus's identity tests pass against the metriken type, and an
  agent's snapshot has the same group schemas and hashes before and after
  (same state, same labels, uids aside). Sampling cost no worse, from the
  agent's `sampling latency` debug line at fleet-representative scale
  (rezolus `docs/principles.md`, principle 16).
- **5b:** as 5a, plus the stream tests; per-refresh cost of the moved
  builder no worse than `create_v3`'s.
- **5c:** the family table, measured as above at 1k to 1M members. GO if
  a family of 1M members snapshots in under 50 ms on one core, a register
  or drop during a snapshot waits no longer than copying that family's
  values, and memory per member is below the registry's 650 B. Measured
  against the registry-per-member numbers above.

## Not in this phase

- The agent writing its own archive (rezolus #1224, part 3).
- Per-counter generation and width (rezolus #1224, ride-alongs).
- Changes-only rows: measured NO-GO (rezolus
  `docs/journal/2026-09-28-changes-only-stream.md`).

## 5b: built (2026-09-29)

### The group builder

`metriken-exposition::group_builder`, behind `msgpack` (the schema hash
needs it). The producer-specific half is a `Router`:

```rust
pub trait Router {
    type Guard: ReadGuard;
    fn route<'a>(&'a self, metric: &'a MetricEntry) -> Option<Route<'a>>;
    fn acquire(&self, group: GroupId<'_>) -> Acquisition<Self::Guard>; // default Windowless
    fn window(&self, group: GroupId<'_>) -> Option<Window>;             // default None
    fn annotate(&self, metric: &MetricEntry, metadata: &mut BTreeMap<String, String>); // default no-op
}
pub struct Route<'a> { pub group: GroupId<'a>, pub membership: Membership<'a> }
pub enum Membership<'a> { Present, All, Prefix(usize), Set(&'a [usize]), Slots }
pub enum Acquisition<G> { Windowless, Stamped(Option<Window>), Reader(G) }
pub trait ReadGuard { fn mark_end(&mut self) {} fn finish(self) -> Option<Window>; }
```

- `route` returning `None` leaves a metric out (rezolus's `log_` filter).
  `GroupId` is two borrowed parts, so routing allocates nothing; the wire
  name is `"{namespace}/{name}"`.
- `Acquisition::Stamped` is read at a group's first touch and again through
  `window` after its values, and the builder emits the union
  (`resolve_walk_window`). `Acquisition::Reader` is rezolus's reader-stamped
  bracket: the guard opens at first touch, `mark_end` runs after each group
  metric's values are read, `finish` runs at emit, and a group that produced
  nothing drops its guard unfinished. The builder knows nothing about how a
  guard stamps.
- `GroupBuilder::new(router)` owns the router and the skeleton cache.
  `build_groups(extra)` returns the groups; `snapshot(stamp, duration,
  extra, metadata)` wraps them in a `SnapshotV3` with `producer_epoch`,
  `clock_anchor_wall_ns`, `ts` and `wall_offset`, and `systemtime =
  ts + wall_offset`. `Stamp::now()` is `metriken::epoch::anchored_now()`.
- `ExtraGroup` is a caller's group of ready-made members, always built
  fresh. rezolus's `external/main` becomes one.
- Names come from `MemberNames`: `Positional` (`{metric_id}`,
  `{metric_id}x{idx}`, the default and what archives carry) or `ByName`
  (`{name}`, `{name}#{idx}`, stable across registration order, unique only
  when names are).
- Member metadata, the identity fold with `metadata_version` folded before
  members, the miss path storing the walk's own identity, the hit-path
  arity check that evicts and skips, and the empty-group skip are
  `create_v3`'s, with their comments rewritten without rezolus's types.
  `acq_group` (`GROUP_METADATA_KEY`) is stripped from every member.
- `is_family(metric)` tells a router a metric is a `CounterFamily` or
  `GaugeFamily`, whose membership is `Slots`. `DefaultRouter` routes by
  `acq_group` into one namespace, for a producer without a group registry.

### How rezolus implements it

```rust
struct RezolusRouter {
    sampler_mods: Vec<(&'static str, &'static str)>, // samplers::sampler_modules()
    groups: &'static HashMap<(&'static str, &'static str), &'static AcquisitionGroup>,
}
impl Router for RezolusRouter {
    type Guard = ReaderGuard; // AcquisitionGuard<'static> + &'static AcquisitionGroup
    fn route<'a>(&'a self, m: &'a MetricEntry) -> Option<Route<'a>> {
        if m.name().starts_with("log_") { return None; }
        let sampler = attribute_sampler(m.module(), &self.sampler_mods);
        match m.metadata().get("acq_group").map(|g| self.groups.get(&(sampler, g))) {
            Some(Some(ag)) => Some(Route {
                group: GroupId::new(ag.sampler, ag.name),
                membership: if ag.is_reader_stamped() { Membership::Slots }
                    else if let Some(s) = ag.member_set() { Membership::Set(s) }
                    else if let Some(n) = ag.member_bound() { Membership::Prefix(n) }
                    else { Membership::All },
            }),
            // Some(None) is the unregistered-acq_group debug_assert.
            _ => Some(Route { group: GroupId::new(sampler, "main"), membership: Membership::Present }),
        }
    }
    fn acquire(&self, g: GroupId<'_>) -> Acquisition<ReaderGuard> {
        match self.groups.get(&(g.namespace, g.name)) {
            Some(ag) if ag.is_reader_stamped() => Acquisition::Reader(ReaderGuard::new(ag)),
            Some(ag) => Acquisition::Stamped(ag.window()),
            None => Acquisition::Windowless,
        }
    }
    fn window(&self, g: GroupId<'_>) -> Option<Window> {
        self.groups.get(&(g.namespace, g.name)).and_then(|ag| ag.window())
    }
    fn annotate(&self, m: &MetricEntry, md: &mut BTreeMap<String, String>) {
        md.insert("sampler".into(), attribute_sampler(m.module(), &self.sampler_mods).into());
    }
}
// ReaderGuard::finish: guard.finish(); ag.window()
```

`SnapshotBuilder` holds a `GroupBuilder<RezolusRouter>` in place of its
`SkeletonCache`, passes `external/main` as an `ExtraGroup` (still sorted and
named by its labels hash in rezolus), and passes `source` and `version` as
snapshot metadata. `create_v3`, `fold_group_identities`, `GroupBuilder`,
`members`, `resolve_walk_window` and their tests are deleted.

### The frame producer

`metriken-archive::stream`, behind a new `stream` feature
(`dep:metriken-exposition`, `dep:metriken`) rather than `write`: a producer
serving a stream needs neither the archive writer nor dendro's writer.
`FrameProducer::new(labels, metadata)` takes the source uuid and anchor from
`metriken::epoch` and adds dendro's `PRODUCER_EPOCH` key;
`FrameProducer::for_source` names them explicitly. `opening()` is the
preamble plus handshake; `interval(rows, ts, wall_offset, seq)` is one
interval's `Frame::Rows`; `empty_interval(seq)` is the empty one.

`interval` takes any `StreamRow` (stream, schema hash, schema, payload), so
rezolus can pass its `AgentRow`s, which also serve `/metrics/rows`, without
converting. `EncodedGroup::encode(&GroupSnapshot)` is metriken's row type.
rezolus's `keep` closure became filtering the iterator before `interval`,
which keeps the rule that a filtered row never marks a schema as sent.

What stays in rezolus: the axum route, the content type, the per-connection
timer and interval index, the TTL-shared pass, and the per-subscriber
filters (a repeated reading, a group whose window did not advance).

### Differences from `create_v3`

For `Positional` names there is no difference in groups, schemas, hashes or
windows. What changed:

- Both passes read one `metriken::metrics()` guard, where `create_v3` took
  one per pass, so both see the same dynamic metrics. Registering a dynamic
  metric waits for the two walks instead of one at a time.
- `mark_end` is called after every counter-group and gauge-group metric of a
  guarded group, not only in the `Slots` arms. In rezolus a guarded group's
  metrics are all `Slots`, so this is the same calls.
- The two `debug!` lines on a hit-path eviction are gone
  (metriken-exposition has no logging dependency); the eviction is not.

### Tests

metriken-exposition: 28 integration tests (`tests/group_builder.rs`) and 8
unit tests. Ported from `create_v3`'s: declared and default groups, schema
hash stability, unique names, slot order and slot meaning, metadata changed
at a stable index, hit allocations constant from 8 to 512 members, slot
churn hits and misses, absent and unhandled-value groups, registered versus
value-derived membership, a zero-crossing default member, bounds and
clamping, reader-stamped windows, concurrent builders, a 4.2M-slot group
with 2 members, stamped and discarded sweeps, the anchored stamp, and the
`members`/`resolve_walk_window` unit tests. New: a router that declines a
metric, a router splitting metrics into two groups with per-group cache
counts, `ByName`, extra groups alone and joined to a routed group, and
counter and gauge families as groups whose members come and go with the
schema reused while membership is unchanged.

metriken-archive: 11 unit tests in `stream` (ported: schema inside the
payload, not resent, resent on change, the pass's stamp, `NO_INDEX_STATE`,
the empty interval, one timeline per process, the handshake, a tick into an
archive through dendro's subscriber; new: per-subscription schema state,
a round trip through dendro's frame encode and decode), and one integration
test (`tests/stream.rs`): a registry with a counter and a counter family
through `GroupBuilder`, `FrameProducer`, dendro's wire and `Subscriber`,
read back by `ArchiveReader` with each tenant's labels and `__uid__`.

Not ported: V2 (`create`) tests, the external-metrics store, TTL and body
caching, `/metrics/rows`, sampler attribution, `set_member_set`'s own test
(it tests `AcquisitionGroup`), and the recorder's `StreamSubscriber`.

## 5b: measured (2026-09-29): GO

rezolus's adoption (a `RezolusRouter` over this builder, and this frame
producer behind its `/metrics/stream`) was checked three ways.

**Equivalence.** Before deleting `create_v3`, one test built each tick with
both builders from the same registry and compared every group's name,
schema, `schema_hash`, values, windows and the snapshot metadata, and the
rebuild count, over nine ticks: cold start, cache hits, slot assign and
release, metadata replaced at a stable index, a restamped window, histograms
loading, external metrics growing, a default member leaving. No
difference; a deliberately changed label made it fail. The expectations
stay in rezolus as `v3_snapshot_contract`.

**Builder cost, isolated.** A 797-entry registry (a 4096-slot per-task
group with 2,500 live slots, a 512-slot cgroup group, three per-CPU groups
of 64, 200 scalars), 300 warm-up, 3,000 cache-hit and 2,000
membership-change ticks.

- First measured on an Apple-silicon laptop: membership-change ticks were
  3–4.5% slower than `create_v3`, from cloning a member's base metadata
  before, not inside, the slot-metadata callback and formatting the name
  first. Fixed (`member_metadata`); after it, 18 runs gave ratios of
  0.993–1.024 on membership-change ticks and 0.986–1.031 on cache-hit
  ticks, with one run where the new builder alone sat 14% slower on
  cache-hit ticks for the whole process. That run never reproduced under a
  profiler, and its cause is not known.
- On Linux (a systemslab VM, Debian 13, one pinned CPU), each builder run
  alone under `perf record`: 163.18 G cycles for `create_v3`, 163.57 G for
  this builder (+0.24%), with the same profile (the builder 16.8% against
  17.5%, `GroupSchema::hash` 5.7% both, `malloc`/`free` 16.5% against
  15.6%). 24 runs of both builders alternating spread 1.4% in total CPU,
  with no slow run.

**End to end.** The same VM, each build's own agent and recorder (rezolus
upstream `7dd615c8` against the adoption branch), the shipped agent config
with per-task series on, under process churn, 180 s per arm, pairs
alternating which build ran first. Sampler health was identical in every
arm (30 healthy). CPU seconds and peak RSS:

| arm | agent, old → new | recorder, old → new | agent peak | archive |
|---|---|---|---|---|
| 1 Hz scrape | 3.74 → 3.78 | 1.67 → 1.66 | 152 MB | 1.2 MB |
| 1 Hz `--stream` | 4.02 → 4.07 | 0.48 → 0.48 | 157 MB | 1.2 MB |
| 10 Hz scrape | 35.86 → 35.71 | 18.71 → 18.32 | 152 MB | 6.5 MB |
| 10 Hz `--stream` | 37.96 → 37.95 | 6.31 → 6.31 | 157 MB | 6.5 MB |

Every archive was finalized, with 181–182 rows of 180 at 1 Hz and 1,802 of
1,800 at 10 Hz at the measured cadence, a per-task table with 56–79
occupants, 24 occupant streams, and queries returning data. A real agent's
V3 build under churn took 0.70 ms at the median and 0.87 ms at p99.

Two harness mistakes on the way, recorded so the next run avoids them: a
filter that matched `RESULT` only at the start of a line lost the first
Linux run's per-tick numbers (libtest prints the test name first), and not
waiting for an agent to exit let the next agent's PMU budget probe find the
counters still held (1 per CPU free instead of 6), which disabled three
samplers in every other arm of the first end-to-end run.

## After 5b: why streaming cost the agent more (2026-09-30)

The end-to-end table above has the agent using more CPU when streamed than
when scraped: 4.07 s against 3.78 s at 1 Hz and 37.95 s against 35.71 s at
10 Hz, about 1.2–1.6 ms per pass. Both transports take one sampling pass per
interval, so the difference is in encoding.

The stream path converted every group's schema from metriken-exposition's
type to metriken-segment's on every pass (`EncodedGroup::encode`, and
rezolus's `wire::encode_group`), allocating each member's name and labels
and freeing them after the frame. A subscriber is sent a schema only when its
hash changes, so on a pass with no change every conversion was discarded.
When a hash did change, `FrameProducer` cloned the converted schema, decoded
the encoded row and encoded it again with the schema.

Measured on rezolus's `v3_build_cost` registry (795 entries, a 2,500-task
group; a scrape body of 7.7 MB, about 14 times a real host's), release build
on an Apple-silicon laptop, median per pass:

| step | before | after |
|---|---|---|
| scrape: encode the snapshot | 9.5 ms | 9.7 ms |
| stream, no schema changed: encode, frame, free | 20.4 ms | 0.29 ms |
| stream, task group changed: encode, frame, free | 24.0 ms | 4.2 ms |
| of which the frame, task group changed | 3.3 ms | 0.86 ms |

Before, converting schemas and freeing them was about 95% of the stream's
encode cost; encoding the values alone is 0.24 ms. The fix:

- `stream::SchemaCache` converts a group's schema once per hash and hands out
  an `Arc`; `EncodedGroup::schema` is an `Arc`.
- `metriken_segment::wal::encode_wal_group_row_with_schema` puts the schema
  into the encoded row in place of its `nil`, without decoding the values or
  cloning the schema, byte for byte what encoding the anchored row gives.

What is left on a pass where a group changed is converting that group's
schema once (3.4 ms for the 2,500-task group here). The two schema types
encode to the same bytes (pinned by metriken-exposition's `segment` tests),
so a producer could encode metriken-exposition's schema into the row
directly and never convert; not done.

Two smaller costs were measured and left: `group_approx_bytes`, which only
rezolus's `/metrics/rows` uses, is 27 µs per pass on this registry, and the
two copies of each payload (`payload().to_vec()` and `encode_frame`) are
inside the 19 µs the frame takes. Removing the copies would change dendro's
`WalRow`.

Measured end to end afterwards (rezolus #1383, 2026-09-30) on `delta`, a
32-core bare-metal host, with per-thread series on and process churn, two
180 s windows per arm: the streamed agent's CPU fell from 3.08–3.19 s to
2.11–2.19 s at 1 Hz and from 29.91–31.44 s to 18.84–19.09 s at 10 Hz, to
within 0–4% (1 Hz) and 8–16% (10 Hz) of the same build scraped.
