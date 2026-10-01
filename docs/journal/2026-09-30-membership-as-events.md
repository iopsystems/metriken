# Membership as events: slot groups on the stream in the archive's long form

**Status:** BUILT, GO (2026-10-01) — awaiting release. Phase 5d of
[the high-cardinality stack](2026-09-28-high-cardinality-stack.md), after
[members that come and go](2026-09-29-members-that-come-and-go.md).

## Goal

When an occupant joins or leaves a group with slots, the replication stream
should carry that change, not the group's whole schema. The storage end
already works this way: the archive writes such a group long, keyed by
occupant, with each occupant's labels in a `<table>/occupants` stream. The
producer end already works this way too: `SlotIdentity::assign` and
`release` are the events. Every stage in between discards the event and
rebuilds the full member list.

## Why

### Measured: the schema is most of the stream under churn

A throwaway probe subscribed to a rezolus agent's `/metrics/stream` on
`delta` (32 cores, Debian 13, kernel 6.12) and tallied each group's rows.
The agent had per-thread series on (`task_attribution = true`) and ran under
process churn (16 short `awk` loops at a time, repeatedly). The agent was
rezolus #1383, with metriken-archive 0.3.0. 120 s per interval:

| interval | stream bytes | schema bytes | task group: schema resent | task group: schema share |
|---|---|---|---|---|
| 1 s | 29.5 MB (245 KB/s) | 23.9 MB (81.2%) | 120 of 121 rows | 97.8% |
| 100 ms | 309.5 MB (2.58 MB/s) | 250.9 MB (81.1%) | 1,191 of 1,201 rows | 97.7% |

At 1 s each task-group resend carried about 845 members, of which about 16
had been added and 12 removed since the previous one (1,941 added and 1,479
removed over 120 resends), at about 212 bytes of schema per member. So the
stream sent about 180 KB of schema per tick to say that roughly 28 members
changed. The cgroup groups (`syscall_counts_cgroup`, `cpu_tlb_flush_cgroup`,
`cpu_usage_cgroup_*`, `scheduler_runqueue_cgroup_*`) were resent 5–15 times
in 120 s at 1 s, and schema was 47–91% of their bytes.

### How it got this way

`record --stream` briefly had an event form: the identity index
(`Frame::Index` entries against `caller_rows`). rezolus's stream journal
(`docs/journal/2026-09-22-recorder-stream-ingest.md`) found identity held
twice, once in the schema and once in the index. Phase 5's decision 1
(2026-09-29) removed the index and kept identity in the schema, since the
long writer could build its occupant stream from `__uid__` in the schema and
nothing else read a change feed. That decision did not measure what
restating the whole schema on every change costs. The numbers above are that
cost.

### Measured: what each stage pays per change

A group's values are a positional vector, and its schema lists every member
with its full labels under one hash. So a single assign or release changes
the hash, and every stage treats the result as a new schema:

| stage | work on a changed group | measured |
|---|---|---|
| builder (`GroupBuilder`) | rebuilds every member's descriptor, hashes the schema | membership-change tick 7.0–8.4 ms vs 1.4–1.6 ms cache hit (rezolus `v3_build_cost`, 2,500-task group) |
| stream encode | converts the changed schema once (`SchemaCache`) | 3.4 ms on the same registry |
| frame | splices the schema into the row | 0.86 ms |
| wire | the full schema, per subscriber | 81% of bytes (above) |
| recorder (rezolus `StreamSchemas::snapshot`) | decodes the schema, converts it back to metriken-exposition's type, builds a `GroupSnapshot` | not isolated |
| writer (`LongLayout::of`, `metriken-archive/src/writer.rs`) | walks every member to separate occupant labels from column metadata, keys each slot by `__uid__` | not isolated |

End to end on `delta` under the same churn, with rezolus #1383, over 180 s
windows, the recorder used 2.79 s streaming at 1 Hz and 33.53 s at 10 Hz;
the agent it was streaming from used 2.19 s and 18.84 s. So a streaming
recorder at 10 Hz used 1.8 times the CPU of the agent it recorded. How much of that goes to schemas has not been isolated.

The archive does not need any of this. `LongLayout` exists to recover, from
a full member list, the occupants the producer already knew about as
events.

## Decisions (2026-09-30)

- **Every rezolus consumer of a live agent is a stream consumer.** The
  recorder already is. Hindsight and the live viewer move from
  `/metrics/binary` to `/metrics/stream?layout=long`. They share one
  consumer: a subscription feeding an `ArchiveWriter`.
  - **Recorder:** the writer with no retention.
  - **Hindsight:** the same writer with retention (it already evicts
    through `ArchiveWriter::evict_before`).
  - **Live viewer:** writes into a temporary archive and queries it through
    `ArchiveReader` while it is still being written, as it already reads a
    live hindsight buffer. Today the viewer polls `/metrics/binary`
    (rezolus `src/viewer/actions.rs` `ingest_loop`), ingests each snapshot
    into a `MemoryStore`, and keeps every raw body in an unbounded
    `VecDeque` for save-as-parquet and reports (`state.snapshots`). With an
    archive, saving copies the archive, as a dendro source's Save-as-Report
    already does (`report_save::build_dendro_report`).
- **Who still reads `/metrics/binary`:** the exporter, and `record` to
  `.rez`, parquet or raw. Those need self-contained snapshots, so that
  endpoint keeps full schemas.

## Step 1: where the streaming recorder's CPU goes (2026-09-30)

The recorder under `perf record --call-graph dwarf` on `delta` (the rezolus
#1383 build) was streaming at 100 ms from an agent with per-thread series
on, under the churn above. 40 s were sampled at 199 Hz. The DWARF unwind
left many frames unresolved, so these are shares of all samples, with
overlaps, and should be read as approximate:

| where | share of samples | what it is |
|---|---|---|
| schema handling | ~24% | `GroupSchema::hash` 3.7%, `LongLayout::of` 3.6%, dropping schemas (`Arc<GroupSchema>::drop_slow`) 5.0%, cloning label maps 2.5%, converting `MetricDesc` lists 2.2%, decoding strings and label maps ~7% |
| decoding values | ~12% | `decode_wal_group_row` 12.2%, 9.7% of it histograms |
| the writer thread | ~12% | sealing (`Encoder::encode` 6.8%, parquet 2.3%), SQLite commit and WAL writes (~5%) |

Two costs are fixable without the long form:

- **Each row's payload is decoded twice.** `AgentRow::from_payload` decodes
  it to compute arity and `approx_bytes`, and to lift the schema out.
  `StreamSchemas::snapshot` then decodes it again to build a
  `GroupSnapshot` (both in rezolus: `crates/rez/src/wire.rs` and
  `src/recorder/stream.rs`). Decoding once would save up to half of the
  ~12%.
- **The recorder turns the rows back into a snapshot,** and the writer
  encodes them into WAL rows again. The long form removes the schema half of
  that. The value half is covered by the writer's staging entry for rows
  that are already encoded (plan step 5).

The ~24% spent on schemas is roughly what criterion 3 asks for, and it
removes only what the long form removes. So criterion 3 stays at a third,
and the double decode becomes a separate, earlier fix in rezolus.

## Progress (2026-09-30)

- **Step 1b** (rezolus #1384): the recorder decodes each streamed row once.
  Over four 180 s windows at 100 ms under the churn above, the recorder used
  24–27% less CPU: 25.9–37.4 s before, 19.6–27.5 s after.
- **Step 7** (rezolus #1385): hindsight subscribes to the stream. Checked
  against a real agent on `delta`: it buffered from the stream, a dump opened
  in `recording metadata`, and the dump and the live buffer both answered
  `irate(cpu_usage)`.
- **Step 8** (rezolus #1386): the live viewer records the stream into a
  temporary archive. An open `ArchiveReader`'s view turned out to be fixed:
  its tables, spans and WAL tail are read once, and its time range does not
  grow (metriken-archive `reader.rs`: "a live archive is re-opened").
  rezolus's `rez::live::LiveReader` therefore reopens after each interval.
  The same fact means `rezolus view` on a running hindsight buffer shows it
  as it was when opened; that is not fixed yet.
- **For metriken-archive:** the reader warns that a source which is not
  finalized "was recovered up to its last checkpoint". That wording is right
  for a crashed recording and wrong for one that is still being written. A
  reader that reopens every second repeated it every second, so rezolus
  reopens with logging off. A reader option saying the archive is live, or
  wording that covers both cases, would let a caller keep the warning.

## Built (2026-10-01)

Steps 2-6 are built in metriken #223 (exposition), #224 (archive) and the
rezolus change that serves and asks for `/metrics/stream?layout=long`. What
differs from the design below:

- **Producer keys are dense per group, not the assignment's generation.**
  `GroupBuilder` numbers occupants from 0 per group as they appear; a slot
  gets the next number when its occupant changes. The first build used the
  `__uid__` read as a number, or a label hash for slots without one: nine
  bytes of msgpack per occupant per row, about 7.6 KB per tick for the
  per-task group on `delta`. Step 3 (expose a generation from
  `SlotIdentity`) was therefore not needed; metriken and metriken-core are
  unchanged.
- **Keys live for one builder, so the writer numbers occupants by
  identity.** An occupants row maps its key to an archive number through
  the occupant's identity (`__uid__`, else its labels), the same identity
  the wide path numbers by (`LongLayout::of`). A reconnect, a restarted
  producer whose keys start again from 0, and a group that changes form
  keep each occupant's series. A review found the first version gave a
  restarted producer's new occupants the old ones' labels; that is the case
  this rule closes.
- **A row's form is read from the row.** A `WalGroupRow` and a `WalLongRow`
  encode as msgpack arrays of 6 and 4 fields, so `StreamDecoder` needs no
  per-stream state to tell the forms apart, and a group can change form (a
  scalar registered into a group of counter groups) between rows. A change
  of form re-sends the row's schema and, for a long row, every occupant.
- **The writer gives the occupant the keys the wide path would.** A key
  that is not a storage key and has one value across all of a long row's
  columns (`op` on a one-metric group) is moved from the columns to each
  occupant's labels before the occupant is numbered, which is how
  `LongLayout::of` splits a wide schema. Without it a group that changed
  form kept the same values under two numbers. A second review found this,
  and that columns sent on a row the writer skips as a repeat were lost
  (both paths now take a row's schema before the dedup), and that an
  occupant skipped once for a width mismatch lost its key;
  `metriken-archive/tests/stream_edges.rs` covers all three.
- **Every slotted group travels long**, per the owner's decision
  (2026-10-01): any group whose metrics are all counter groups or gauge
  groups, per-CPU and per-device included. Measured cost: those groups are
  10-20% larger long than wide (softirq time 9.3 KB/s wide, 10.7 KB/s long,
  at 100 ms), small next to the per-task group's saving.
- **The recorder accepts an agent that serves only the wide layout**
  instead of refusing it. The decoder reads both, and the recorder logs that
  the agent serves only the wide layout.
- **A value-derived slot keeps its occupant across a build without a
  value.** Only groups of `Membership::Slots` metrics forget an absent slot.
- **The agent builds the wide snapshot only when something reads it.** A
  pass's wide snapshot and its long stream rows are each built on first
  request, so an agent that is only streamed long never builds the wide
  schema.

### GO / NO-GO, measured on `delta`

32 cores, Debian 13, kernel 6.12, per-thread series on, 16 short `awk` loops
at a time throughout. Base is rezolus main (wide stream, step 1b included);
"long" is this phase. One agent per recorder, interleaved windows of 180 s.

1. **Same answers: GO.** `metriken-archive/tests/stream_long.rs` records one
   registry both ways over 30 ticks: a `SlotIdentity` group with a stamped
   window, a fixed per-CPU group, a value-derived group with a slot reading
   zero for a tick, a mixed group, and a producer restarted at tick 15 with
   keys starting from 0. Ten queries answer identically, finalized and from
   the live tail. The writer gate's 213-query set was not rerun.
2. **Wire: GO.** Stream bytes per second, both layouts subscribed to one
   agent at once under churn:

   | interval | wide | long | ratio | per-task group, wide → long |
   |---|---|---|---|---|
   | 1 s | 263 KB/s | 61.3 KB/s | 4.3x | 166 KB/s → 11.2 KB/s |
   | 100 ms | 1.90 MB/s | 555 KB/s | 3.4x | 1.40 MB/s → 86 KB/s |

   What remains of the long stream is mostly histogram groups
   (`syscall_latency` 106 KB/s at 100 ms), which are the same in both
   layouts. An earlier probe at 100 ms read 1.15x; its script waited on the
   churn loop before starting the 100 ms pass, so that pass ran without
   churn, and it predates dense keys.
3. **Recorder CPU: GO.** At 10 Hz, 15.3 s and 15.8 s (base) against 9.4 s
   and 8.1 s (long), 39-49% less; at 1 Hz 1.8 s against 0.9-1.0 s. The
   first run of windows (before dense keys) gave the same split: 15.5 and
   18.0 s against 8.2 and 8.6 s.
4. **Agent CPU: GO.** At 10 Hz 10.9 s and 11.6 s (base) against 7.1 s and
   6.9 s (long), 35-40% less; at 1 Hz 1.53 s against 1.00 s. A
   membership-change tick of `build_stream` is 1.5-1.9x an unchanged one
   (2,500 occupants, 16 in and 16 out; `metriken-exposition/tests/stream_cost.rs`),
   against 25-80x for the wide build.

One window of the second run is unexplained: the long recorder exited 1 in
its first 1 Hz window and its log was overwritten by the next window. Five
repetitions of that window's conditions (a fresh agent, recording 10 s after
start, under churn) all exited 0. Reopen if a recorder exits 1 against a
6.0 agent without a refusal in its log.

## Design

### The stream carries the long form

For a group the writer would write long (any member carries a slot `id`:
`slotted()` in `metriken-archive/src/writer.rs`), the subscriber asks for
the long form and each interval carries two rows:

- **`<group>`: a long row.** A `WalLongRow` whose schema is the group's
  metric columns only: each metric's fixed descriptor, with no occupant
  labels. Its hash changes only when the set of metrics changes, never when
  membership does. Each present occupant has one `LongOccupant` with its
  values, keyed by the producer's occupant key.
- **`<group>/occupants`: occupant rows.** An `Occupant` (key plus labels)
  for each occupant this subscriber has not been told about yet. That means
  new assignments, plus every live occupant on the connection's first
  interval.

A departure is not sent. The archive represents a departed occupant by its
absence from later rows, and so does this form. `SlotIdentity::release`
means the slot has no values from then on.

Groups without slots (host-wide, per-sampler scalars, groups whose
membership follows values) keep the current `WalGroupRow` form, where a
schema change is rare.

### The producer's occupant key, and the writer's numbers

The archive numbers occupants densely per stream, and the writer assigns
those numbers (`GroupState::occupants`). That stays with the writer. The
writer serves several sources and outlives a producer's connection, so a
number minted by the producer could collide after a reconnect.

The producer sends a `u64` key that identifies an occupant for the life of
the process:

- for a `SlotIdentity` space, the assignment's generation, the same one
  `mint_uid` hashes into `__uid__` (`metriken/src/group/identity.rs`);
- for a bounded group with fixed slots (per-CPU, per-device), the slot
  index, since each slot's occupant does not change.

The writer maps key to archive number per stream: one hash lookup per
present occupant per tick, instead of a layout rebuild per change. A
producer restart is a new producer epoch, and therefore a new dendro source
(the handshake uuid is the epoch), so keys never need to survive a restart.

### The builder

For a `Membership::Slots` group, `GroupBuilder` builds the metric columns
once per set of metrics. Each tick it emits the live slots' values keyed by
occupant key, and it takes new occupants' labels from `SlotIdentity` rather
than rebuilding every member's descriptor.

The wide `GroupSnapshot` is still needed for `/metrics/binary`. That body is
self-contained by contract: `record --format raw`, the exporter and the
viewer decode each one alone. So the wide schema is built only when a
scraper asks for it. An agent that is only streamed never builds it.

### Subscribing

The long form is opt-in per subscription: `/metrics/stream?layout=long`.
Without the parameter the stream is today's.
- A 5.x `record --stream` keeps working against a 6.0 agent. It records
  rows without an identity index, as decision 1 already accepted.
- The 6.0 recorder asks for `long` and refuses an agent that ignores it.
  The response names the layout it serves, the way it names the frame
  interval (`x-rezolus-frame-interval` today).

On a reconnect the subscriber receives every live occupant again on the
first interval. The writer dedups them by key, like the restatements it
already writes every `restate_every_ns`.

### The writer

`ArchiveWriter` gains a staging entry for a group that arrives long: a
`WalLongRow` with producer keys, plus the new occupants. It maps the keys to
archive numbers, writes the long row, and writes first-seen occupants to the
occupant stream. The restatement is unchanged. `LongLayout::of` remains for
a snapshot that arrives wide: a scrape, or a stream without `layout=long`.

## GO / NO-GO

Measured on `delta` under the churn above, per-thread series on, 1 Hz and
10 Hz, against rezolus #1383 as the baseline. Proposed thresholds, to be
settled before building:

1. **Same answers.** An archive recorded over the long stream and one
   recorded over the wide stream of the same agent, over the same ticks,
   give the same answer to every query in the writer gate's set (metriken
   `2026-09-28-archive-writer.md`, 213 queries). Occupant labels match
   series for series.
2. **Wire.** Stream bytes per second fall by at least 3x at 1 s and at
   100 ms. Schema is 81% of the bytes today; this criterion checks that the
   occupant rows do not add back what the schema removed.
3. **Recorder CPU** falls, at 10 Hz by at least a third, from 33.53 s per
   180 s.
4. **Agent CPU** streaming is no higher than the baseline's (2.19 s at 1 Hz,
   18.84 s at 10 Hz), and a membership-change tick on `v3_build_cost`
   costs no more than twice a cache-hit tick when only the long form is
   requested (today 5–6 times).

NO-GO if (1) fails in a way that needs the wide schema to repair, or if (2)
and (3) together are under 2x and a third.

## Plan

In order, each its own PR and release:

1. **Measure the recorder's split.** Done, above: ~24% schemas, ~12%
   value decode (done twice per row), ~12% the writer thread.
1b. **rezolus: decode each streamed row once.** Independent of the long
   form. Open as rezolus #1384.
2. **metriken-segment:** nothing new if `WalLongRow`, `LongOccupant` and
   `Occupant` carry the producer key as `occupant`; the writer rewrites it.
   Confirm that the WAL format needs no version bump when the writer (not
   the wire) assigns the stored number.
3. **metriken:** expose an occupant's generation from `SlotIdentity` next
   to its uid.
4. **metriken-exposition:** `GroupBuilder` emits a slot group's long form
   (metric columns, occupant keys, new occupants since the last call per
   consumer), and builds the wide schema only on request.
5. **metriken-archive:** `FrameProducer` serves the long form per
   subscription; `ArchiveWriter` stages a long group with producer keys.
6. **rezolus:** `/metrics/stream?layout=long`, and the recorder asking for
   it, with the gate above.
7. **rezolus hindsight:** a stream consumer. It subscribes as the recorder
   does, keeps its retention, `/status`, `/dump` and SIGHUP capture, and its
   config names an agent instead of a `/metrics/binary` URL. Gate: its
   `hindsight_dump` tests pass, and a dump answers the same as one from
   today's scraping buffer over the same window.
8. **rezolus live viewer:** a stream consumer writing a temporary archive,
   read live through `ArchiveReader`. `state.snapshots` and the
   `MemoryStore` ingest path go away, and save and report copy the archive.
   Gate: `viewer_smoke` in live mode, the same dashboards and queries as
   today, and flat memory over a long session.

Steps 7 and 8 need only the stream, not the long form. Either can land
before steps 2–6, and then gain the long form when step 6 lands.

## Open questions

- **Hindsight in-agent** (rezolus #1224 part 3) would skip the transport
  as well. That remains a separate step. Moving hindsight to the stream
  first leaves a smaller change for later: swapping the subscription for an
  in-process channel.
- **The live viewer's temporary archive:** where it lives (a temp file, or
  memory if dendro can hold a SQLite archive in memory), and what bounds it
  for a viewer left open for days. Hindsight's retention is the likely
  answer. Not decided.
- **Families (5c)** have registration ids, which would serve as occupant
  keys. A family could produce the long form directly and never have a wide
  schema at all. Not checked against the 5c code.
- **Whether per-CPU and per-device groups need the long form on the wire.**
  Their membership does not churn, so the wide form costs them nothing
  after the first row. Sending every slotted group the same way keeps the
  writer to one path; sending only `Membership::Slots` groups long keeps the
  change smaller.
- **Histogram groups with slots** (rezolus backlog, after 6.0) would travel
  in this form with no further wire change.

## Not in this phase

- `/metrics/binary` and V3 snapshots keep full schemas.
- Changes-only values (measured NO-GO, rezolus
  `docs/journal/2026-09-28-changes-only-stream.md`). This design changes
  how membership travels, not values.
