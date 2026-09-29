# The archive writer: dendro archives, long tables, occupant streams

**Status:** Gate run on four synthetic recordings and two real hosts (see
"Gate results", below): same answers, 1.5–9 times smaller, a tick tail
three to ten times shorter; the median tick at 100 ms is 12–28% worse
under an unpaced replay, accepted. rezolus's adoption is under way
(rezolus `docs/journal/2026-09-28-dendro-writer-adoption.md`). Phase 4 of
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

*Decided by the gate:* arrival order is the default (`sort_long: false`).
Sorting made the 100 ms archives 6–20% larger and did not improve the
tick path; sorting is left to compaction.

**Dedup, schema checks, seal policy.**
- Dedup and schema checks carry over from the `.rez` writer: dedup per
  stream by window end, `GroupSnapshot::validate`, a small schema ring per
  group, and an arity check.
- Seal policy is dendro's `SealPolicy`, with rezolus's defaults (8 MiB,
  900 rows, 300 s, staggered). Kept after measuring seven alternatives
  (see "Seal policy, measured").
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
- **V1/V2 ingest** was first left out, and then needed as soon as rezolus's
  recorder used the writer: `record` scrapes whatever the agent serves, and
  an agent older than acquisition groups serves V2, which the V3-only
  writer dropped without a word. It is now the `.rez` writer's rule: one
  `WalCell` table per `sampler` label, dedup by the sampler's newest
  window, metadata on a metric's first row in each segment.
- **Reader routing covers every segment (fixed 2026-09-29).** `ArchiveReader`
  learned a table's metric names from one segment, so a metric that first
  appeared in a later segment, or only in the live tail, could not be
  queried. Now:
  - the encoder stores a names fingerprint in each sealed segment's dendro
    `caller_index` (metric name and data type per column, `MNS1` + FNV-1a);
  - the reader probes the first segment and one more per fingerprint it
    has not seen, reading the fingerprints from the catalog without
    payloads;
  - it probes the live tail from only the rows that carry a schema
    (`metriken_segment::wal::schema_rows`, which reads a group or long row
    no further than its schema).

  A segment without a fingerprint (a `.rez`, a conversion, or one sealed
  before this) is assumed to hold the first segment's names, as before.
  Measured on the quiet host's recording: a finalized archive opens in 7
  ms against 5.4, and a live one (1,500 ticks, every table's tail in the
  WAL) in 27 ms against 7, the difference being the tails' rows read
  from SQLite.
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
- Events. rezolus keeps timeline events in a source's metadata under
  `events`. The recorder adds `run_start`/`run_end` while it records
  (rezolus#1323), which `SourceRecorder::update_metadata` carries on a
  dendro archive. Events added after recording, by `recording annotate
  --event` and the viewer's Save-as-Report, need a dendro arm through
  dendro's `ArchiveMut::patch_source_metadata`. The viewer reads them
  through `ArchiveReader`'s source metadata already, for either container.
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

## Gate results

Measured 2026-09-28 with a scratch harness (not committed). It reads a
`.rez` v3 recording's group tables, rebuilds each tick's V3 snapshot (a
row's non-null cells are its members, the schema is sent when it changes),
and replays the snapshots through the `.rez` v3 writer (`StreamRecorderV3`)
and through `ArchiveWriter`, sorted and in arrival order. Tick cost is the
caller's time in stage, commit and seal for one tick. The replay is not
paced: ticks are fed as fast as the writer takes them, about 27 times real
time at 100 ms. Answers are compared through rezolus's `RezReader`, which
opens both containers: for every metric, `sum(rate(m[5s]))`,
`count(rate(m[5s]))` and `sum by (id) (irate(m[5s]))` for counters,
`sum` and `count` for gauges, `histogram_quantile(0.9, m)` for histograms,
over the whole recording at its own step, relative tolerance 1e-9. Replay
ran on an Apple M4 Max (16 cores, 128 GiB) on macOS 26.6.

Inputs are the four synthetic thread and cgroup spike recordings from
rezolus's `docs/journal/2026-09-25-dendro-archive-layout.md` (thread CPU
0.5 ms "light" or 10 ms "heavy", each at 1 s and 100 ms). One segment of
the 1 s heavy input (`cpu_usage/cpu_usage_task` #1) cannot be read, the
wide layout's `TooManyTables` defect; both writers replay the same rows
without it.

| recording | ticks | answers same | size: `.rez` → sorted / arrival | tick p50: `.rez` / sorted / arrival | tick p99 | tick max |
|---|---|---|---|---|---|---|
| light, 1 s | 701 | 213 / 213 | 129.0 → 19.6 / 18.4 MB | 8.6 / 6.3 / 6.2 ms | 407 / 35 / 30 ms | 781 / 48 / 39 ms |
| heavy, 1 s | 700 | 213 / 213 | 188.9 → 21.4 / 20.2 MB | 7.3 / 5.3 / 5.3 ms | 397 / 17 / 16 ms | 1067 / 149 / 140 ms |
| light, 100 ms | 6,665 | 213 / 213 | 395.5 → 118.7 / 99.3 MB | 1.78 / 2.32 / 1.99 ms | 375 / 38 / 31 ms | 852 / 131 / 101 ms |
| heavy, 100 ms | 6,665 | 213 / 213 | 561.5 → 127.9 / 121.0 MB | 2.59 / 3.31 / 2.95 ms | 406 / 36 / 29 ms | 917 / 170 / 184 ms |

Against the four conditions:

1. **Same answers:** met on all four, 213 of 213 queries.
2. **No more tick cost:** met at 1 s. At 100 ms the tail is about ten
   times shorter but the median is 12–28% worse: stage time is higher
   (17.0 s against 15.5 s light, 22.8 s against 20.3 s heavy, sorted), and
   commit carries the writer thread's backpressure from the unpaced replay
   (5–6 s against 0.2 s at 1 s). Accepted without a paced replay
   (decided 2026-09-28): a 0.3–0.7 ms median against a 400 ms tail.
3. **Not larger:** met, 3.3–8.8 times smaller.
4. **Sort within a tick:** the sorted writer's worst tick is no worse than
   arrival order's, but sorting is larger and slower at the median, so
   arrival order is the default.

The `.rez` writer's tail is its commit: `wal_tick` waits for a synchronous
commit on its writer thread (52–58 s of commit at 100 ms), where dendro's
returns once the tick is queued.

Query time, from the same comparison: every query slower than 2 s was faster
on the dendro archive, most by a large margin; `sum by (id)
(irate(cgroup_syscall[5s]))` at 100 ms took 165 s on the `.rez` output and
2.4 s on the dendro one.

### Two real hosts

The same harness on two production recordings, both at 1 s, described as
in rezolus's layout entry: the **busy host** (agent 5.22.0, 2.3 h, 8,180
ticks, many short-lived threads) and the **quiet host** (agent 5.18–5.20,
9.6 h, 34,678 ticks, about 2,530 long-lived threads reporting every tick).
Every segment of both inputs decoded; neither had unsealed WAL rows.

| host | answers same | size: `.rez` → sorted / arrival | tick p50: `.rez` / sorted / arrival | tick p99 | tick max |
|---|---|---|---|---|---|
| busy | 216 / 219 | 545 → 147 / 157 MB | 1.35 / 1.36 / 1.35 ms | 79 / 40 / 36 ms | 2,405 / 256 / 259 ms |
| quiet | 210 / 210 | 1,126 → 763 / 915 MB | 0.10 / 0.33 / 0.35 ms | 106 / 36 / 33 ms | 728 / 104 / 123 ms |

The timings were taken while other builds ran on the same machine, so they
are noisier than the synthetic set.

- **The three busy-host differences are the `.rez` writer's.** Its output
  for the per-task table (`sum`, `count` and `sum by (id)` over
  `task_cpu_usage`) cannot be read: re-sealed at that writer's own
  boundaries, a segment crossed arrow-rs's `TooManyTables` limit, and the
  reader reports the table as evicted. The input recording reads, and its
  `sum(rate(task_cpu_usage[5s]))` matches the dendro archive's line for
  line. So on real data the `.rez` writer loses the per-task table and this
  writer does not.
- **The quiet host is where long saves least.** Its per-task table is 98%
  dense: every thread reports every tick. Long arrival is 507 MB and long
  sorted 345 MB against 663 MB for the `.rez`, the same as rezolus's layout
  gate measured on that recording (505.4 and 348.7 MB). The value column
  is 87% of a segment, at 5.9 bytes per value in arrival order and 4.0
  sorted (PLAIN, LZ4). Cgroup tables still shrink 3–4 times.
- **Sorting helps on both hosts:** 6% smaller on the busy host and 32% on
  the quiet one, against 6–20% larger on the synthetic spikes. Arrival
  order stays the default anyway, to keep the cost at seal low (decided
  2026-09-28); sorting is left to compaction.

What the per-task table's size is made of, measured on three quiet-host
segments of about 263,000 rows (value column only, per segment):

| encoding | size |
|---|---|
| PLAIN + LZ4 (the writer) | 1,050 kB |
| PLAIN + zstd | 553 kB |
| DELTA_BINARY_PACKED + LZ4 | 811 kB |
| DELTA_BINARY_PACKED + zstd | 798 kB |

A thread's CPU time grows by a median 0.88–0.94 ms per tick (p90
1.9–2.7 ms), about 20 bits of nanoseconds that delta encoding does not
remove; 36–44% of readings did not change. zstd halves the column; its
encode cost at seal is not measured.

**Segment length does not change it.** Re-encoding 1, 4, 16 and 64
consecutive quiet-host segments as one (104 to 6,656 ticks) moved the value
column from 4.35 to 4.21 bytes per row under LZ4 and from 2.34 to 2.21
under zstd. Sorted by occupant, a longer segment is *larger* overall under
LZ4 (4.69 to 8.63 bytes per row), because each occupant's run restarts the
timestamp and window columns and the repeat falls outside LZ4's 64 kB
window. Sorting at compaction into long segments therefore needs zstd, or
delta-encoded time columns, to pay off.

**zstd at seal, measured.** The writer re-run in arrival order with each
codec, on an otherwise idle machine; encode time is the writer thread's
time in the encoder, over every seal.

| recording | codec | size | encode total | mean / worst seal | tick p99 / max | query time, answers |
|---|---|---|---|---|---|---|
| busy host, 1 s | LZ4 | 157.4 MB | 5.56 s | 8.7 / 200 ms | 32 / 231 ms | 22.6 s |
| | zstd-1 | 98.3 MB | 5.94 s | 9.3 / 202 ms | 34 / 228 ms | 22.6 s, 71 / 71 same |
| | zstd-3 | 71.3 MB | 5.79 s | 9.0 / 206 ms | 31 / 232 ms | 22.6 s, 71 / 71 same |
| heavy spike, 100 ms | LZ4 | 121.0 MB | 6.61 s | 11.2 / 157 ms | 30 / 162 ms | 17.6 s |
| | zstd-1 | 77.1 MB | 7.10 s | 12.0 / 158 ms | 31 / 160 ms | 17.3 s, 69 / 69 same |
| | zstd-3 | 52.5 MB | 6.81 s | 11.5 / 161 ms | 27 / 166 ms | 17.5 s, 69 / 69 same |
| quiet host, 1 s | LZ4 | 915.4 MB | 22.97 s | 9.2 / 108 ms | 35 / 123 ms | 32.8 s |
| | zstd-1 | 521.8 MB | 24.35 s | 9.7 / 105 ms | 35 / 126 ms | 32.5 s, 72 / 72 same |
| | zstd-3 | 389.5 MB | 23.62 s | 9.4 / 102 ms | 32 / 154 ms | 32.5 s, 72 / 72 same |

zstd-3 is 55–57% smaller than LZ4 on all three, for 3–4% more encode
time; on the quiet host that makes the archive 2.9 times smaller than the
`.rez` (1,126 MB); a seal's
worst case is the WAL decode and table build, not the codec. Tick latency
and query time do not move. So the writer seals with zstd-3
(`WriterConfig::compression`, metriken#191). Readers already decode zstd
(metriken-query enables it). A reader rebuilds a table's unsealed tail
with zstd-3 too: `ArchiveReader` holds that segment in memory while it is
open, so the smaller encoding is the smaller resident footprint, at about
the same encode cost. (Leaving it uncompressed would save the encode and
cost the most memory.)

**Seal policy, measured.** Seven policies on the busy host, the quiet host
and the heavy 100 ms spike, zstd-3, arrival order. The replay is unpaced,
so dendro's wall-clock `max_age` never fires in it; a measurement-only
row-time bound stood in for it (a stream seals once its open segment spans
N of row time, the first segment shortened by the stream's stagger
bucket), which is what the 300 s age does at a real 1 s cadence. The byte
cap is the writer's size estimate, about ten times the encoded segment.

| policy | busy: size, segments, worst seal, tick p99 / max | quiet: size, segments, worst seal, tick p99 / max |
|---|---|---|
| 1. 8 MiB + 900 rows + 5 min (rezolus's today) | 77.2 MB, 1,868, 96 ms, 22 / 99 ms | 414.4 MB, 7,390, 41 ms, 31 / 718 ms |
| 2. 5 min span only | 76.5 MB, 1,810, 97 ms, 24 / 109 ms | 410.5 MB, 7,060, 92 ms, 36 / 375 ms |
| 3. 15 min span only | 70.8 MB, 710, 334 ms, 44 / 478 ms | 384.7 MB, 2,590, 261 ms, 48 / 385 ms |
| 4. 60 min span only | 68.2 MB, 240, 627 ms, 20 / 782 ms | 374.4 MB, 768, 257 ms, 18 / 443 ms |
| 5. 15 min aligned (dendro `align`) | 71.0 MB, 771, 271 ms, 4 / 1,769 ms | 386.4 MB, 2,912, 281 ms, 4 / 1,974 ms |
| 6. 8 MiB estimate only | 69.4 MB, 325, 202 ms, 34 / 243 ms | 381.2 MB, 1,233, 324 ms, 38 / 686 ms |
| 7. 900 rows only | 69.9 MB, 524, 298 ms, 47 / 359 ms | 381.9 MB, 1,901, 265 ms, 46 / 326 ms |

On the heavy spike at 100 ms, policy 1 was 52.9 MB with a tick p99 of 28 ms
and max 152 ms; the 5 min span alone was 50.0 MB, p99 58 ms and max
413 ms, because at 100 ms it makes segments three times the 900-row cap's;
15 and 60 min spans were 49.3 MB; aligned sealing reached a 4.2 s tick.
Every policy gave every query the same answer on all three recordings.

- Longer segments are 7–12% smaller and read up to 10% faster (72
  quiet-host queries: 31.8 s under policy 1, 28.5 s under a 60 min span).
- They cost seal time, which the tick pays: the worst seal grows from
  41–96 ms to 260–630 ms.
- Aligning every stream on one boundary puts every seal on one tick: 1.8
  and 2.0 s at 1 s, 4.2 s at 100 ms. The stagger exists for this.
- A byte or row cap alone never seals a slow stream: under policies 6 and
  7 some segment spans the whole recording (2.3 h and 9.6 h), so a reader
  of the live archive rebuilds it all and retention has nothing whole to
  evict. A time bound is required.

**Decided (2026-09-28):** keep the combination (policy 1). At 1 s its
5 min bound already makes it time-based (policies 1 and 2 nearly match);
at 100 ms the row cap keeps seals small. Open: taking the 5 min bound in
row time rather than wall time, so a paused producer or an offline
conversion seals as a live recording does; the measurement-only bound
above is the prototype.

**Sealing only at the end** (every cap off; the busy host):

| | never sealed until finalize | policy 1 |
|---|---|---|
| file while recording | 771 MB + 10 MB `-wal` | 77 MB at the end |
| open of the live archive | 5.3 s (every table rebuilt from WAL rows) | ~0.01 s |
| finalize | 21 s, every stream sealed at once | milliseconds |
| file after finalize | 671 MB (freed WAL pages stay in the file) | 77 MB |
| peak RSS of the run | 7.9 GB | |
| tick p99 / max | 3.4 / 23 ms | 22 / 99 ms |

WAL rows are msgpack, about ten times the sealed zstd segments; every open
of a live archive (viewer, `/status`, a hindsight dump) rebuilds the whole
recording in memory; the final seal is one long pause that a kill at the
end leaves to whichever reader opens the file next. Only the tick tail
improves. Rejected.

Not run: a replay paced at the recording's interval, which would separate
the unpaced replay's backpressure from the writer's own cost at 100 ms.

## Not in this change

- Compaction with a sort key. That's dendro's `CompactSpec`, still open.
- Replacing the `.rez` writer in rezolus. That's rezolus 6.0, which
  adopts this writer.
