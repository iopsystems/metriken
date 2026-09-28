# The archive writer: dendro archives, long tables, occupant streams

**Status:** OPEN. The writer is built (see "Built", below); the gate
against the `.rez` writer has not been run. Phase 4 of
[the high-cardinality stack](2026-09-28-high-cardinality-stack.md), and step 3
of rezolus 6.0 (iopsystems/rezolus#1224).

## Goal

A writer in `metriken-archive` that records metriken snapshots into a dendro
archive in the layout rezolus decided
(rezolus `docs/journal/2026-09-25-dendro-archive-layout.md`):

- groups whose members come and go ("slotted" groups) are written long, one
  row per tick and occupant, with an occupant stream beside each;
- groups of fixed members stay one row per tick;
- `ArchiveReader` reads the result, including the unsealed WAL tail.

rezolus's recorder and hindsight then use it with rezolus's defaults, in
place of the `.rez` v3 writer (`crates/rez/src/rez_v3_writer.rs`).

## What dendro already does

dendro's `writer::Writer` covers what the `.rez` v3 writer built for
itself:

- **Writing:** one writer thread behind a bounded channel, and one
  transaction per tick across every source (`wal_tick`).
- **Sealing:** the WAL is sealed into segments on the writer thread, off
  the tick path, through a caller-supplied `SegmentEncoder`. Sealing uses
  `SealPolicy`, `SegmentAccount` and the per-source stagger, which rezolus's
  `seal_policy.rs` near-duplicates.
- **Upkeep:** a checkpoint every 10 s, and eviction with page reclaim.
- **Lifecycle:** clock offsets recorded at seal, complete/incomplete
  marking, and `resume_source`.

dendro never decides when to seal; the caller does. The new writer supplies
the ingest logic, the encoder and the seal driving. It does not reimplement
the container.

## Design

**Types.**
- `ArchiveWriter` wraps a dendro `Writer`.
- `SourceRecorder` holds one source's ingest state:
  - dedup keys;
  - schema caches and the per-segment anchoring of schemas;
  - occupant maps;
  - seal accounts.
- `Encoder` is the `SegmentEncoder` that turns a stream's WAL rows into a
  segment.

**Ingest.** The writer takes metriken-exposition snapshots.
- A V3 snapshot's groups are handled one by one.
- A V1/V2 snapshot is grouped per sampler, as the `.rez` writer does
  (`WalCell` rows, a wide table per sampler). Agents older than
  acquisition groups still produce these.
- The recorder's stream path gives the writer the same group snapshots. It
  reconstructs them from `record --stream` rows and identity frames, so the
  writer has one ingest path.

**Which groups are long.** A group is slotted when its members carry a slot
`id` in their metadata. That's how rezolus's per-CPU, cgroup, per-thread and
device groups declare themselves (`id=<slot>` beside the slot's labels). A
group without `id` members is fixed and stays one row per tick. The rule is
the writer's, so every producer is treated alike.

**Occupants.**
- **Numbering.** Each long stream has a map from identity to occupant
  number. Identity is `__uid__` when the member carries one, and otherwise
  the member's full label set (slot included). Numbers are dense `u64`s
  from 0, in order of first sight. The map lives for the whole recording,
  across seals and eviction.
- **Labels.** On first sight the writer appends the occupant's labels to
  `<stream>/occupants`, in the same tick transaction as the data rows. It
  uses metriken-segment's occupant WAL row.
- **Restatement.** Every 300 s of row time the writer restates every
  occupant seen since the last restatement. That keeps a live occupant's
  labels inside the retention window.
- **Identity input.** The writer takes identity from each group's schema
  (`MetricDesc` metadata). A scrape carries it there. The stream path has to
  build it there from its index frames before calling the writer.

**A long table's WAL row carries occupant numbers.** A new
metriken-segment type, `WalLongRow`, holds:
- the group's window;
- its metric set, anchored on the first row of a segment the way
  `WalGroupRow` anchors its schema;
- for each occupant present that tick, its number and its values per
  metric.

Occupants are assigned at ingest, not at seal, for one reason: a reader
materializing an unsealed tail has none of the writer's state. The row
has to say which occupant each value belongs to, as `WalGroupRow` already
carries what its reader needs.

**Telling a long table's WAL rows apart.** A reader knows a table is long
because it has an occupant stream. That is how `ArchiveReader` already
recognizes one. `materialize_wal_tail` gains a long arm chosen that way,
not by guessing from the bytes.

**Encoding.** The encoder handles three table shapes:
- **long:** `WalLongRow`s become a long segment, with the layout markers
  and the footer occupant list;
- **fixed group:** `WalGroupRow`s become a wide group table, through
  `GroupTableBuilder` as today;
- **V1/V2:** `WalCell`s become a per-sampler table.

It sets dendro's `ENCODER` key so a reader can refuse an encoding it does
not know.

**Row order within a long segment.** The encoder sorts by
`(occupant, timestamp)` at seal. The case for it:
- Page pruning makes a single-series read of a sorted long segment
  7–15 times cheaper than wide (metriken#165).
- Sorting costs at most 32 ms for the largest measured segment.
- dendro seals on the writer thread, not the tick thread.

The case against:
- A long seal delays the next tick's commit, which waits on that thread.
- Sorting made tables of short-lived occupants up to 50% larger.

The gate below measures both, and arrival order is the fallback.

**Dedup, schema checks, seal policy.**
- Dedup and schema checks carry over from the `.rez` writer: dedup per
  stream by window end, `GroupSnapshot::validate`, a small schema ring per
  group, and an arity check.
- Seal policy is dendro's `SealPolicy`, with rezolus's defaults (8 MiB,
  900 rows, 300 s, staggered).
- A long row's size charge is its values plus one occupant slot per
  member.

**Retention.**
- Eviction is dendro's.
- A long table's occupant stream is evicted one restatement period behind
  its data, so a row just inside the cutoff can still find its occupant's
  labels.
- Nothing is written to `caller_rows` for a long table. dendro's one gap,
  that caller rows cannot join a tick's transaction, therefore does not
  arise for new archives.

## Built

`metriken-archive/src/writer.rs`, behind the `write` feature, with the
long layout's builder and WAL row in metriken-segment
(`long_table.rs`, `wal.rs`). Decisions made while building it:

- **Which labels are the occupant's.** Per slot (members sharing an `id`),
  a label whose value differs between the slot's metrics belongs to the
  metric column (`op=read`, `op=write`); a label constant across them
  belongs to the occupant (`comm`, `pid`, `__uid__`, `id`). The storage
  keys (`metric`, `metric_type`, `unit`, `grouping_power`,
  `max_value_power`), `sampler` and `description` always stay on the
  column. This is the converter's rule from the layout entry, applied per
  schema rather than per stream, and it needs no key list. A column is one
  fixed metadata set, named for its `metric`, with a `#N` suffix when a
  group holds the metric more than once; readers match on metadata.
- **A group's layout is decided once.** The first schema with members
  decides whether a group is long, and the decision holds for the
  recording. Deciding per schema let a group that once sent no `id`
  members (or no members) write a wide row into a long stream, which the
  long materializer cannot decode. A member without an `id` in a long
  group is the occupant of slot `""`. A group with no members writes
  nothing, and neither does a long row with no occupant present: such a
  row could land in a long stream before its occupant stream exists, and
  a reader tells a long table by that stream.
- **A long group's columns are append-only.** Each group keeps its metric
  columns for the recording, keyed by their fixed metadata, and a producer
  schema is laid out over them. The columns (and their anchored schema)
  change only when a metric is new, not when membership does; each slot's
  occupant identity is worked out once per schema. The first version
  rebuilt all of it for every new schema, and a per-thread group sends one
  nearly every tick: on a replayed 1 s recording its stage time was 8.6 s
  against 5.2 s for the `.rez` writer, and 3.9 s after this change.
- **Ingest is V3 only.** V1/V2 snapshots are ignored. Agents older than
  acquisition groups are a follow-up, not needed by rezolus 6.0's own
  agent.
- **Eviction lag** is two dendro passes: data streams at the cutoff, and
  occupant streams at the cutoff minus the restatement period. dendro's
  filter selects the streams to evict.
- **A segment's row count is the WAL rows it consumes.** dendro checks
  `Segment::rows` against the WAL rows in its span, and a long segment has
  one parquet row per occupant, so the encoder reports `rows.len()`.
- **The encoder learns which streams are long from the recorders**, through
  a set they share. A writer that resumes an existing archive would need
  that set rebuilt before its first seal; resume is not built.
- **The occupant stream's size charge** against its seal account is a flat
  64 bytes per occupant row, not measured.
- **The reader's tail** of a long table is materialized in arrival order,
  not sorted.

**Found by the tests:** metriken-query decoded a null histogram cell as a
histogram of zeros, so in a wide table a member's first reading counted in
full when an earlier row of its segment was null, and not at all when the
segment began there (fixed in metriken#180). The long table has no nulls,
which is how the two disagreed.

**Tests** (`metriken-archive/tests/writer.rs`) record the same snapshots
twice, long and with `long_groups` off (every group one row per tick), and
require the same answers through `ArchiveReader` for rates, sums by an
occupant label and by a column label, selection by `__uid__`, a fixed
group's gauge, a member without a slot in a slotted group, and a histogram
quantile. The fixture includes a tick where the slotted group has no
members. `__occupant__` must appear on the
long series and never on the wide. They run finalized, from the live WAL
tail, and after eviction at a cutoff past the last restatement; that last
test fails with the eviction lag removed.

## What stays in rezolus

- The recorder's and hindsight's defaults: which endpoints, the intervals,
  the seal and retention settings.
- The stream path's translation from identity frames into group schemas.
- Hindsight's `summarize`, `dump` and `copy_range`, which read the `.rez`
  catalog directly today and need dendro equivalents in 6.0.

## Gate

The writer replaces the `.rez` writer only if all of these hold:

1. **It answers the same.** The same snapshots, recorded by both writers,
   give the same answers to the same queries through `ArchiveReader`.
   The only allowed difference is `__occupant__`. This is checked on
   fixtures and on a replay of a real busy-host recording.
2. **It costs no more on the tick path.** Measured on the replay at 1 s and
   100 ms: the per-tick commit latency, and the longest tick with a seal
   in progress, both no worse than the `.rez` writer.
3. **It is not larger.** Archive size is no larger than the `.rez` for the
   same data, and smaller for the per-thread and cgroup tables, as the
   layout gate predicted.
4. **Sorting at seal stays inside a tick.** If it doesn't at 100 ms, long
   segments are sealed in arrival order and sorted at compaction.

## Not in this change

- Compaction with a sort key. That's dendro's `CompactSpec`, still open.
- Replacing the `.rez` writer in rezolus. That's rezolus 6.0, which
  adopts this writer.
