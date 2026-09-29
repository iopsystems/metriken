# Members that come and go: slot identity, the group builder and families in metriken

**Status:** OPEN — design, nothing built. Phase 5 of
[the high-cardinality stack](2026-09-28-high-cardinality-stack.md). Two
decisions below are the maintainer's ("Decisions needed").

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
- **No change broadcast** unless the identity index survives (decision 1).
  The occupant stream is built from `__uid__` in the schema; nothing else
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

rezolus does not use 5c: its members are BPF map entries read in bulk, and a
registration per thread would add an allocation and a lock to every thread
start. 5c is for services.

## Decisions needed

1. **Does `.rez` stay a `--stream` target in 6.0?** If not, the identity
   index, its broadcast, `Demand`, and the recorder's `index_state` check go
   (a row's `schema_hash` and in-band schema are what a `.dendro` recording
   uses). 5.x keeps `.rez --stream` on `release/5.x`. Recommendation: drop it
   in 6.0; `record --stream` then writes `.dendro` only.
2. **Does 5c gate rezolus 6.0.0?** The high-cardinality plan put every phase
   before 6.0. 5c changes no storage format and rezolus does not use it; 5a
   and 5b are what rezolus 6.0 builds on. Recommendation: 5a and 5b before
   6.0.0, 5c after.

## GO / NO-GO

- **5a:** rezolus's identity tests pass against the metriken type, and an
  agent's snapshot has the same group schemas and hashes before and after
  (same state, same labels, uids aside). Sampling cost no worse, from the
  agent's `sampling latency` debug line at fleet-representative scale
  (rezolus `docs/principles.md`, principle 16).
- **5b:** as 5a, plus the stream tests; per-refresh cost of the moved
  builder no worse than `create_v3`'s.
- **5c:** the measurements above exist. GO if a snapshot of 100k members
  builds within one 1 s interval's budget on one core with room to spare,
  and register or drop is not serialized behind a snapshot for longer than a
  snapshot takes. The thresholds are proposals, to be settled against the
  first numbers.

## Not in this phase

- The agent writing its own archive (rezolus #1224, part 3).
- Per-counter generation and width (rezolus #1224, ride-alongs).
- Changes-only rows: measured NO-GO (rezolus
  `docs/journal/2026-09-28-changes-only-stream.md`).
