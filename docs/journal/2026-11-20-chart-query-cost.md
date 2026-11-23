# Chart query cost: opening a high-cardinality table, and rates over many series

**Status:** OPEN. Opened 2026-11-20. Baseline measured; criteria written
before any change. Change 1 measured: NO-GO, and its prediction refuted
(0.414 s against 0.25 s; refutation bar 0.4 s). With a second commit not
in the criteria (a bounds check), 0.294 s. Output unchanged; both kept
(see "Change 1: results"). Change 2 measured: NO-GO (1.43 s and 1,468 MB
against 1.0 s and 1,200 MB on fresh 100 ms; 4.07 s against 2.0 s on
9.6 h), prediction not refuted on fresh 100 ms, output unchanged, kept
(see "Change 2: results"). Change 4, first two steps measured: GO
(2.897 s against 3.4 s on 9.6 h), output unchanged (see "Change 4:
results").

## Goal

rezolus's viewer is too slow and too large on recordings with many
series: a chart takes seconds, and reading one `.dendro` file takes
gigabytes. This entry makes the first query on a high-cardinality table
and a `sum` of rates over many series fast, and bounds the memory a reader
holds after them.

Out of scope:

- the segment format. dendro's segment-encoding entry
  (`docs/journal/2026-11-19-segment-encoding.md` in iopsystems/dendro)
  closed a format change as NO-GO; one profile of the viewer on macOS put
  segment decode under 7% of the first query's index build. Reading the
  segments is a larger share of a warm rate query (see "Change 1:
  results");
- the viewer's HTTP and dashboard layers in rezolus;
- histogram queries' memory (the decoded `HistogramSnapshot` data, 2.2 GB
  on the 9.6 h recording in rezolus's viewer). It is a separate cost with a
  separate cause, recorded in "Not yet measured here" below.

## How it is measured

`metriken-query/benches/archive_query.rs` opens a `.dendro` archive the
way rezolus's viewer does (`ArchiveReader::open_read_groups`, then
`ArchiveReader::flatten`, with an 8192 MB `BufferPool`, the viewer's
default on a large host). It runs each query three times in order, in one
process, and prints the wall time and resident memory after each run, and
the peak.

- A `display` query is a range query at the recording's interval, reduced
  to 500 points, as the viewer's charts ask.
- A `grid` query is a plain range query at a step of range / 500,
  rounded up to a multiple of the interval.
- The first run of a query on a table pays for opening that table; the
  later runs are warm.

```sh
cargo bench -p metriken-query --bench archive_query -- RECORDING [QUERIES] [REPS] [POOL_MB]
```

Recordings (not in the tree):

- **fresh 100 ms:** rezolus 6.0.0-alpha.152 at a 100 ms interval with
  per-task series on, under thread and cgroup churn: 42 MB, 161,222 task
  series. Made for dendro's segment-encoding entry.
- **9.6 h:** a 9.6 hour rezolus recording at 1 s, 1.28 GB, wide task
  table.

Host: Apple M4 Max, macOS 26.6.2, release build.

## Baseline

metriken `9320a63` (metriken-storage 0.13.2, metriken-query 0.51.0).
Three fresh processes per recording; each figure is the median, with the
range. "Warm" pools runs 2 and 3 of all three processes.

Fresh 100 ms:

| query | first | warm | RSS after |
|---|---|---|---|
| `sum(irate(cpu_usage[5m])) / cpu_cores / 1e9` | 0.013 s | 0.004 s | 46 MB |
| `sum(irate(syscall[5m]))` | 0.039 s | 0.020 s | 118 MB |
| `sum by (op) (irate(cgroup_syscall[5m]))` | 1.710 s (1.629–1.989) | 0.412 s (0.408–0.488) | 672 MB |
| `sum by (name) (irate(cgroup_cpu_usage[5m])) / 1e9` | 0.174 s | 0.094 s | 846 MB |
| `sum(irate(task_cpu_usage{pid="1"}[5m]))` | 2.296 s (2.212–2.603) | 0.043 s | 2,463 MB |
| `sum(irate(task_cpu_usage[5m])) / 1e9` | 0.792 s | 0.778 s (0.753–0.832) | 2,550 MB |
| `sum by (id) (irate(cpu_usage[5m])) / 1e9`, grid | 0.003 s | 0.003 s | 2,566 MB |

Peak RSS 2,566 MB (2,550–2,653). After opening the archive: 18 MB.

9.6 h:

| query | first | warm | RSS after |
|---|---|---|---|
| `sum by (op) (irate(cgroup_syscall[5m]))` | 0.840 s | 0.174 s | 485 MB |
| `sum(irate(task_cpu_usage{pid="1"}[5m]))` | 4.086 s (4.068–4.144) | 0.005 s | 2,354 MB |
| `sum(irate(task_cpu_usage[5m])) / 1e9` | 0.610 s | 0.588 s (0.585–0.599) | 2,607 MB |

Peak RSS 2,629 MB (2,602–2,664). After opening the archive: 47 MB. The
other queries are under 0.15 s first and at most 0.06 s warm.

## Where the time goes

Two profiles of the bench on the fresh 100 ms recording (xctrace Time
Profiler, one capture each).

**The first query on the task table** (`task_cpu_usage{pid="1"}`, 2,793
samples):

- 68% is `SegmentedSource::open` (`metriken-storage/src/segmented.rs`),
  reached through the table's `OnceLock<TableReader>` on first use.
- Inside it:
  - `BTreeMap<String, String>` clones (`clone_subtree`) are 18% of all
    samples;
  - SipHash is 16%: `IdentityMemo`'s maps are std `HashMap`s keyed by
    `Labels`, and `register` and `presents` hash and clone a column's
    labels for each insert;
  - `Listing::fold` is 16%, `SegmentColumns::of` 13% (cloning column
    descriptors, 7%; histogram config checks, 3%).
- Freeing (`_xzm_free`, `_free`) is 28% of self time, over the whole
  capture.
- Dropping the reader at exit is 13% of samples, which is a measure of
  how many allocations the open made.
- Reading occupant streams is 7%; zstd decompression under 2%.

Read from the code: one task series' label set is copied into at least
these places at open:

- `IdentityMemo::sets` and `IdentityMemo::registered` (keys),
- `SeriesIdentity::order` and `SeriesIdentity::pos` (the same set twice),
- `OpenState::column_map` (the key, once per presented set),
- `CounterColumns`' `labels` and `labels_index` (twice).

`DataSource::column_map` also returns a clone of the whole map
(`SegmentedSource::column_map`), and `QueryEngine::columns` calls it for
each query it resolves; rezolus's viewer resolves every query through
`columns` first. The bench does not call `columns`, so the baseline above
does not include that cost.

**A `sum` of rates over the task table** (the first task query, which
opens the table, then 8 warm `sum(irate(task_cpu_usage[5m])) / 1e9`
runs;
21,962 samples):

- `SeriesRate::advance` is 36% of self time and `SeriesRate::edge` 25%,
  almost all under `SeriesRate::settle`.
- `SeriesRate::new` starts every series' cursor at the range's start.
  When a series' first samples arrive, `settle` ingests them and
  `advance` walks every grid point from the range's start up to the
  first sample. For each one it runs `range_start` and `edge`, and emits
  nothing: `edge` returns no value at a time before the series' first
  sample (`observed`).
- A task that starts late in a 700 s range at a 0.1 s step walks up to
  7,000 points for nothing, and there are 161,222 of them.

## Decision Criteria

Written before any change. Each change is measured with the bench above,
three fresh processes per recording, against the baseline, on the same
host.

**Change 1, rates start at a series' first sample.**

- GO: warm `sum(irate(task_cpu_usage[5m]))` on fresh 100 ms at most
  0.25 s (from 0.778 s), and no query in either table above slower by
  more than 10%.
- The output must be identical: every query in the bench, on both
  recordings, returns the same points before and after, compared value
  by value.
- Prediction: about 0.25 s. `advance` and `edge` are 61% of the samples
  and nearly all of that is the walk before the first sample.
- Refutation: warm `task_sum` above 0.4 s, which would mean the walk is
  not where the time is.

**Change 2, label sets stored once at open.**

- GO: on fresh 100 ms, the first `task_cpu_usage{pid="1"}` at most 1.0 s
  (from 2.296 s) and RSS after it at most 1,200 MB (from 2,463 MB); on
  9.6 h, the first task query at most 2.0 s (from 4.086 s); no other
  query slower by more than 10%.
- Prediction: about 0.9 s and 1,000 MB on fresh 100 ms. Clones, SipHash
  and frees are over half the open's samples.
- Refutation: under a 30% reduction in either time or RSS.

**Change 3, memory after the open.** Criteria to be written after changes
1 and 2, against the RSS they leave.

**Change 4, footer work on a wide table.** Written 2026-11-20, after
change 2's results and before measuring this change; the owner chose this
before change 3.

- The first step: `SegmentColumns::of` borrows each segment's column
  descriptors (`DataSource::columns_desc_ref`), where each of its ten
  accessor calls cloned all of them (`FileSource::columns_desc` is
  `self.0.columns().to_vec()`).
- GO: on 9.6 h, the first `task_cpu_usage{pid="1"}` at most 3.4 s, from
  4.07 s at change 2's head; no query slower by more than 10%; output
  unchanged.
- Prediction: about 3.3 s. In change 2's profile, cloning column
  descriptors is 15% of samples and the histogram config checks, which
  also clone them, 9%.
- Refutation: under a 10% reduction.
- Later steps, each measured on its own:
  - the query reopening segments the open already parsed
    (`segment_kept`, 22%);
  - building each footer's Arrow schema (27.5%, overlapping the above).

**Change 5, opening a table from its segments' indexes.** (Results:
"Change 5: results".) Written
2026-11-20, after change 4's results and before any code. The owner chose
this over keeping parsed footers in the pool (faster, but up to about
1.6 GB more resident on the 9.6 h task table by the pool's own estimate),
and chose an in-place re-index for existing archives over a copy.

- **What:** the names index gains a version (`MNI4`) with a third frame
  holding what `OpenState::fold` takes from a parsed footer:
  - the counter column refs (`ColumnPosition` per column, one per occupant
    per metric in a long segment);
  - the column map;
  - units per metric, duplicated series and file metadata;
  - the segment's time span and interval;
  - the histogram config check, run when the segment is sealed.

  When every segment of a table has that frame, the open folds the frames
  and fetches no segment. Otherwise it takes today's path. A query still
  fetches and parses the footer of each segment it reads.
- **Existing archives:** a re-index rewrites each segment's index in place
  through a new dendro call, `ArchiveMut::replace_segment_index`, without
  rewriting the segment. metriken and rezolus are on dendro 0.7.1, so that
  call is backported to a dendro 0.7.2.
- **GO, measured on the 9.6 h recording after a re-index:**
  - the first `task_cpu_usage{pid="1"}` at most 1.8 s, from 2.67 s at
    change 4's head;
  - no query slower by more than 10% on either recording;
  - output unchanged;
  - each table's indexes at most 25% of its segments' bytes;
  - the re-index of the 9.6 h archive under 60 s.
- **Prediction:** about 1.6 s. In change 4's profile:
  - fetching segment bytes from SQLite is 11.4%;
  - opening each segment's footer for the open is 17.5%;
  - `SegmentColumns::of` and `column_map` are 11.5%.

  An index row is smaller than a segment, so most of those samples go.
  The query's own parse (`segment_kept`, 29.8%) stays.
- **Refutation:** under a 20% reduction, which would mean the open's footer
  work is not where the time is.
- **Unknown before building:** how large the third frame is for a long
  table with 161,222 occupants, whose refs are one per occupant per
  metric.

## Plan

1. The bench and this entry.
2. Change 1 (`metriken-query/src/batch_rate.rs`): when a series settles,
   or takes its first sample, move its cursor to the first grid point at
   or after that sample. The points skipped are ones `edge` cannot
   evaluate. A test compares the output with and without the skip on
   series that start late, end early, and have a gap.
   (Done: `starting_at_the_first_sample_matches_walking_from_the_start`.)
3. Change 2 (`metriken-storage/src/segmented.rs`): share each presented
   label set (`Arc<Labels>`) across `IdentityMemo`, `SeriesIdentity`,
   `column_map` and `CounterColumns`, and key `IdentityMemo` with
   foldhash.
   - `Labels` itself does not change. Its `inner` map is a public field;
     `.inner` appears at 369 places in metriken-storage and metriken-query
     and 218 in rezolus (not all of them `Labels`), so changing its
     representation would be a breaking change across both repositories
     for the same saving.
   - `ColumnRelabel::identities` returns `Vec<Labels>`; its only
     production implementation is `OccupantLabels`
     (`metriken-storage/src/long.rs`).
4. `column_map` returned without a clone, or `columns` resolved without
   it.
5. Change 3: decided after changes 1 and 2, from what holds the memory
   they leave.

Each change is its own PR, with its own measurement here.

## Change 1: results

Two commits, measured apart:

- `perf(query): start a series' grid rate at its first sample`
  (`metriken-query/src/batch_rate.rs`, `SeriesRate::settle`): the cursor
  starts at the first grid point at or after the series' first sample.
  This is change 1 as the criteria name it.
- `perf(query): skip hashing a scan's labels when no row can follow the
  range` (`metriken-query/src/promql/streaming/dispatch.rs`,
  `next_for_scan`). Found by profiling the first commit, so it was not in
  the criteria.
  - The profile of the first commit put 16% of self time in SipHash, and
    983 of those 1,653 samples were under `next_for_scan`.
  - For a range that ends at or after the source's last row,
    `next_samples` reads nothing. `next_for_scan` still collected every
    series' labels into a `HashSet` and hashed each again to take it out
    of the empty result.
  - It now checks the bounds `next_samples` reads between first. The check
    is the old loop's condition (`after + 1 <= until`) moved before the
    set is built.

Measured as the baseline was: three fresh processes per arm, the three
arms interleaved, on the same host. Medians of the warm runs:

| query | baseline arm | rate cursor | + bounds check |
|---|---|---|---|
| fresh 100 ms, `sum(irate(task_cpu_usage[5m])) / 1e9` | 0.849 s | 0.414 s | 0.294 s |
| fresh 100 ms, `sum by (op) (irate(cgroup_syscall[5m]))` | 0.416 s | 0.326 s | 0.297 s |

- On fresh 100 ms the three arms' ranges do not overlap on these two
  queries.
- **The baseline arm was disturbed on 9.6 h.** Two of its three processes
  ran slower than the first baseline. For example, warm
  `sum by (op) (irate(cgroup_syscall[5m]))` was 0.17–0.18 s in one
  process and 0.23–0.33 s in the other two, against 0.174 s in the first
  baseline.
- **On 9.6 h, both commits together are within the noise.** Against the
  first baseline: `sum by (op)` warm is 0.176 s against 0.174 s, and
  `task_sum` warm is 0.565 s against 0.588 s.
- **The first runs on 9.6 h moved in every arm, including the
  baseline.** The first task query was 4.68 s with both commits, 4.70 s
  in the baseline arm, and 4.09 s in the first baseline. The host
  drifted between the two sets. Against the interleaved baseline arm, no
  query is slower, first run or warm.

Peak RSS on fresh 100 ms rose with the bounds check: 2,579 MB
(2,578–2,607) in the baseline arm, 2,591 MB (2,578–2,595) with the rate
cursor, 2,735 MB (2,687–2,739) with both. On 9.6 h it moved from 2,546 MB
to 2,624 MB, just above this set's baseline range (2,497–2,621) and inside
the first baseline's (2,602–2,664). The bounds check allocates nothing
that lives past the query, and I don't know why the fresh 100 ms peak
rose. It is left for change 3, which measures what holds the memory.

- **Output.** Every query on both recordings gives the same points as
  the baseline arm: same text, same count of numbers, each within
  8.3e-16 relative (6.5e-16 for both commits together).
  - The baseline is not exactly reproducible either. Two runs of the
    baseline binary on fresh 100 ms differed by up to 4.4e-16 relative on
    the two queries checked: on 40 of the 2,812 numbers in
    `sum(irate(cpu_usage[5m])) / cpu_cores / 1e9` and 807 of 2,812 in
    `sum(irate(syscall[5m]))`. Those counts include the timestamps.
  - `sum` merges partitions in the order the threads finish
    (`run_partitions`).
  - The comparison is `ARCHIVE_QUERY_DUMP` from the bench, then each
    number compared at 1e-12 relative.
- **Tests.**
  - `starting_at_the_first_sample_matches_walking_from_the_start`
    compares the new start with the old walk on 60,000 random series,
    most starting late. It checks the points, and after each push the
    pending point, last sample, spacing and earliest point. It covers
    `increase`, `end_early` and a sample after the range's end.
  - `series_rate_matches_counter_grid_rate` compares `SeriesRate` with
    `CounterGridRate` on 28,000 random series.
  - Starting one point later than the first sample makes both fail
    (checked).
  - `cargo nextest run --all-targets --all-features`: all passed (1,552
    before the new test).
- **Against the criterion: NO-GO, and the prediction is refuted.**
  - The criterion is for the rate cursor: warm `task_sum` on fresh 100 ms
    at most 0.25 s. It gives 0.414 s, which is also above the 0.4 s
    refutation bar. The walk before the first sample was about half of
    the time, not nearly all of it as predicted.
  - With the bounds check as well, 0.294 s, still above 0.25 s.
  - Both commits are kept: each is faster on the two fresh 100 ms queries
    above, no other query is slower by more than 10%, and neither changes
    the output.
- **What is left on 9.6 h,** from a profile of the two commits together
  (34,892 samples: the first task query, then 8 warm `task_sum` runs):
  - `SeriesRate::advance` is 31% of samples, including `edge`, which it
    calls. That is the rate at each of the 34,678 grid points (34,677 s at
    1 s), all computed before the display reduces them to 500 points.
  - Reading the segments is 30% (`SegmentedScan::read`). Of that,
    building the columns from the Parquet pages (`batch_columns`) is 27%
    of all samples, and lz4 decompression 7%.
  - Computing fewer points would change what the display's bands show,
    so it is not a change of this kind.

## Change 2: results

One commit, `perf(storage): store each label set once when opening a
segmented table` (`metriken-storage/src/segmented.rs`):

- `IdentityMemo` holds one entry per column of a metric, keyed with
  foldhash, in place of five SipHash maps each keyed by a copy of the
  column's labels. The entry holds:
  - the column's labels;
  - the sets it presents as (`Arc<[Arc<Labels>]>`);
  - its registrations;
  - its counter positions.
- `SeriesIdentity::order` and `SeriesIdentity::pos` share one
  `Arc<Labels>` per series. `CounterColumns`' `labels` and `labels_index`
  share the memo's copy of each column's labels.
- `counter_streams` copies a series' labels once per query, where it
  copied them twice.
- `column_map` is unchanged, although plan step 3 named it: it is
  returned from `DataSource::column_map` by value, keyed by `Labels`. It
  is left for plan step 4.

The memo's outer map, keyed by metric name, was moved to foldhash in a
second build. A profile of the first build showed SipHash in 273 of 4,991
samples (5.5%), 71 of them in the memo's outer map. The other 202 are
`column_map` and `SegmentColumns::duplicated`, which still use SipHash.
Both builds are below. The commit has the second.

Measured as before: three fresh processes per arm, interleaved, against
the head of change 1 (`93520c1`). Medians:

| | change 1 | labels shared | + foldhash names |
|---|---|---|---|
| fresh 100 ms, first `task_cpu_usage{pid="1"}` | 2.128 s | 1.485 s | 1.428 s |
| fresh 100 ms, RSS after it | 2,273 MB | 1,472 MB | 1,468 MB |
| fresh 100 ms, first `sum by (op) (irate(cgroup_syscall[5m]))` | 1.427 s | 1.194 s | 1.149 s |
| fresh 100 ms, warm `sum(irate(task_cpu_usage[5m])) / 1e9` | 0.294 s | 0.212 s | 0.208 s |
| fresh 100 ms, peak RSS | 2,564 MB | 2,004 MB | 1,927 MB |
| 9.6 h, first `task_cpu_usage{pid="1"}` | 4.358 s | 3.963 s | 4.069 s |
| 9.6 h, RSS after it | 2,354 MB | 2,318 MB | 2,333 MB |

- **The ranges do not overlap** between change 1 and either build on any
  fresh 100 ms row. For example, RSS after the first task query is
  2,267–2,288 MB against 1,457–1,485 MB.
- **The foldhash step adds little.** Its time ranges overlap the first
  build's on every row except warm `task_sum` (0.207–0.210 s against
  0.210–0.217 s). Its peak RSS is lower (1,886–1,987 MB against
  2,003–2,007 MB); I don't know why a hasher change moves the peak.
- **No query is slower than change 1** by more than 10%, first run or
  warm, on either recording.
- **Output:** every query on both recordings matches change 1: same text,
  numbers within the 1e-12 comparison.
- **Tests:**
  - `cargo nextest run --all-targets --all-features`: 1,553 passed.
    `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt
    --check` and the doctests are clean.
  - No test was added. The change moves no output, and the comparison
    above is the check that it did not.
- **Against the criterion: NO-GO.**
  - Fresh 100 ms: 1.43 s and 1,468 MB, against 1.0 s and 1,200 MB.
  - 9.6 h: 4.07 s against 2.0 s.
  - The prediction (about 0.9 s and 1,000 MB) is not refuted on fresh
    100 ms, where both fall by more than 30% (33% and 35%). On 9.6 h the
    first task query falls by 7% against change 1's head (4.358 s), or
    0.4% against the criteria's baseline (4.086 s), and RSS does not
    move.
- **Where the 9.6 h first query's time goes after the change,** from a
  profile of the first build (4,991 samples, the first task query alone).
  There is no profile of change 1 to compare, so this shows where the time
  is now, not why it did not move. The shares overlap.
  - Its task table is wide: a column per series in each segment, with
    each footer's Arrow schema to parse.
  - Building the Arrow schema from each footer
    (`parquet_to_arrow_schema_and_fields`, base64 and flatbuffers) is
    27.5% of samples: 11.9 points in the open and 15.6 inside
    `segment_kept`.
  - `SegmentColumns::of` is 30.5%, of which cloning column descriptors
    is 15% and the histogram config checks 9%.
  - The query's own reads open the same segments again
    (`segment_kept`, 22%), parsing their footers a second time.
  - These are the next candidates for the first query on a wide table.
    They are footer work, not label copies.
- **Not done, from the review:** each memo call hashes its key more than
  once. `column()` looks the name and labels up with `contains_key`
  before `get_mut`, and `positions` calls `column()` twice. A counter
  column is hashed about six times per segment, where the old path
  hashed it about twice. `IdentityMemo` is 10.3% of the profile above.
  The saving from hashing once is not measured.

## Change 4: results

Two commits, measured apart, each against the one before:

- `perf(storage): borrow a segment's column descriptors when listing
  them`. `MultiParquetSource`'s column accessors walk the descriptors
  through `DataSource::columns_desc_ref`, a new trait method that returns
  `None` by default; `FileSource`, `LabeledSource` and `LazySource` return
  them. Before, each accessor took `columns_desc`, a clone of every
  descriptor, and `SegmentColumns::of` calls ten of them per segment.
- `perf(storage): look each column up once in the open's identity memo`.
  `IdentityMemo::column` uses hashbrown's `entry_ref`, one hash per
  lookup, where it hashed twice; `positions` looks the column up once,
  where it did three times on a miss. hashbrown 0.17.1 becomes a direct
  dependency of metriken-storage; arrow already builds that version. The
  memo step was measured with hashbrown 0.16.1; the committed 0.17.1 build
  was not benchmarked.

Three fresh processes per arm, interleaved. The descriptor step against
change 2's head (`2f37139`), then the memo step against the descriptor
step in a second set. Medians of the first runs:

| | change 2 | descriptors borrowed | | descriptors borrowed | + memo lookup |
|---|---|---|---|---|---|
| 9.6 h, `task_cpu_usage{pid="1"}` | 4.377 s | 2.897 s | | 2.815 s | 2.673 s |
| 9.6 h, `sum by (op) (irate(cgroup_syscall[5m]))` | 0.949 s | 0.694 s | | 0.629 s | 0.603 s |
| fresh 100 ms, `task_cpu_usage{pid="1"}` | 1.675 s | 1.312 s | | 1.292 s | 1.210 s |
| fresh 100 ms, `sum by (op) (irate(cgroup_syscall[5m]))` | 1.350 s | 0.999 s | | 0.864 s | 0.780 s |

- **Against the criterion: GO.** The first task query on 9.6 h is
  2.897 s with the descriptor step alone, under the 3.4 s criterion. The
  saving, 1.48 s (34%), is about twice the 0.77 s (19%) the prediction
  implied. With both steps, 2.673 s.
- **The memo step is small, but its ranges do not overlap** on the 9.6 h
  task query (2,812–2,883 ms against 2,641–2,678 ms) or the fresh 100 ms
  one (1,264–1,357 against 1,200–1,250).
- **The host drifted between sets.** Change 2's head ran 1.675 s on fresh
  100 ms here, against 1.428 s in change 2's own measurement. On 9.6 h it
  ran 4.377 s here against 4.07 s; the criterion's 3.4 s was set against
  4.07 s. Each comparison is within one interleaved set.
- **No query is slower than its predecessor by more than 10%.** Warm
  queries do not move, as expected: the open runs once.
- **Output:** every query on both recordings matches, in both sets.
- **RSS says nothing here.** After the first fresh 100 ms task query it
  is about 1,280 MB or about 1,500 MB in runs of the same build. Change
  2's head gave 1,482, 1,282 and 1,276 MB; the descriptor step gave 1,306,
  1,511 and 1,500 MB.
- **Tests:**
  - `cargo nextest run --all-targets --all-features -p metriken-storage
    -p metriken-query`: 1,047 passed after each step.
  - `column_lists_are_the_same_from_borrowed_and_owned_descriptors`, added
    after review, covers two paths no test reached: a child's injected
    labels on its histogram columns, and the descriptors copied out of a
    child that does not lend them. Dropping those labels, or reversing that
    path, makes it fail.

What is left of the 9.6 h first task query, from a profile after the
descriptor step (4,136 samples; the shares overlap):

- Building each footer's Arrow schema (`parquet_to_arrow_schema_and_fields`)
  is 33.7%. About a quarter of it (8.3% of all samples) is flatbuffer
  verification of `ARROW:schema`. A column's labels come from its Arrow
  field metadata (`parse_schema`), so the open cannot skip that schema.
- The query reopening the segments the open parsed (`segment_kept`) is
  29.8%. The open parses each footer and drops it, and the query fetches
  and parses it again.
- `IdentityMemo::column` was 13.1%, which the memo step addresses.
- SQLite reads of the segment bytes are 11.4%.

## Change 5: results

Built on branch `perf/open-from-index` against a local backport of dendro's
`replace_segment_index` to 0.7.1 (dendro #95 on main). Not merged: it needs a
dendro 0.7.2 release.

As built, departures from the plan above:

- **The prefix stays `MNI3`; the open parts are a third zstd frame.**
  - A reader that does not know the third frame decompresses it with the
    second and parses the `SegmentColumns` at the front;
    `rmp_serde::from_slice` ignores the bytes after it.
  - A new prefix would have made those readers treat every new index as
    unreadable and fetch each segment to list a table.
- **The writer and the footer path build the parts with one function**,
  `SegmentOpen::of`, and `OpenState::fold` folds the parts
  (`fold_parts`). The two paths cannot drift apart.
- **The stored form groups a long segment's per-occupant entries.** A run
  of counter references that differ only in occupant is stored once with
  its occupant ids, and so are the column map entries. The read rebuilds
  them exactly, in order (`open_parts_read_back_exactly`).
- **`ARROW:schema` is left out of the parts' file metadata.** It was about
  80% of a wide segment's open parts (2.8 KB of 3.6 KB on drivehealth), and
  nothing reads it through `DataSource::file_metadata`. The footer path
  drops it too, so `SegmentedSource::file_metadata` no longer reports it.
- **The listing does not keep the third frame.** It keeps each segment's
  columns frame for the reader's life; holding the open frame there too
  was about 100 MB on the 9.6 h recording.
- **`reindex` refuses an archive before schema 5.**
  - On schema 4 a segment's index is a column after its bytes.
  - Re-indexing the 9.6 h recording (schema 4) in place took RSS after
    open from 46 MB to about 440 MB, and open from 0.03 s to 0.3 s: every
    open read through every segment's bytes to read the indexes.
  - A schema 4 archive is copied into a current one first (rezolus's
    `recording snapshot`).

What I first reported as a cost of the listing at open (about 400 MB) was
that schema 4 effect. On a schema 5 copy with two-frame indexes, RSS after
open is 41 MB, as without indexes.

**Measured** on schema 5 copies (the fresh 100 ms recording as recorded,
and a `recording snapshot` of the 9.6 h one), each re-indexed. Three fresh
processes per arm, interleaved, against change 4's head (`fe5967b`)
reading the same copies with two-frame indexes. Medians of the first runs:

| | change 4 | change 5 |
|---|---|---|
| 9.6 h, `task_cpu_usage{pid="1"}` | 3.086 s (3.073–3.100) | 1.717 s (1.698–1.768) |
| 9.6 h, `sum by (op) (irate(cgroup_syscall[5m]))` | 0.670 s | 0.448 s |
| fresh 100 ms, `task_cpu_usage{pid="1"}` | 1.503 s | 1.189 s |
| fresh 100 ms, `sum by (op) (irate(cgroup_syscall[5m]))` | 0.911 s | 0.768 s |
| 9.6 h, archive open | 0.33 s, 41 MB after | 0.42 s, 57 MB after |
| fresh 100 ms, archive open | 0.10 s, 18 MB after | 0.11 s, 20 MB after |

- **Output:** every query on both recordings matches.
- **Index size**, as a share of segment bytes:
  - 9.6 h: 1.4% overall, every table under 20%;
  - fresh 100 ms: 4.8% overall.
  - Two fresh 100 ms tables are over 25%:
    - `syscall_counts_cgroup` at 29.9%, against 24.8% with today's two
      frames;
    - `memory_slabinfo_sweep` at 30.4%, three segments of 5 KB each.
- **Re-index:** 1.4 s (fresh 100 ms, 704 segments) and 5.8 s (9.6 h,
  5,874 segments).
- **Against the criteria:**
  - time: met (1.717 s against 1.8 s; predicted 1.6 s);
  - output: met;
  - re-index time: met;
  - index size: not met on two tables;
  - no query slower by more than 10%: not cleanly met. The first
    `task_sum` on fresh 100 ms has a median 16% higher, though its ranges
    overlap (0.262–0.301 s against 0.253–0.344 s); in the set before, it
    was 8% higher.
  - Archive open is not a query in the criteria, and is 0.09 s and 16 MB
    worse on 9.6 h.
- **Tests:**
  - every archive the writer builds now has three-frame indexes, so the
    suite reads through the index path;
  - with the index path disabled, the new damaged-segment test fails;
  - with empty parts, 96 tests fail.
  - New tests:
    - `open_parts_read_back_exactly`;
    - `a_reindex_restores_the_open_parts_in_place`;
    - `a_damaged_segment_with_an_open_frame_still_opens_its_table`.
- **A behaviour change:** a table whose segment is damaged but whose index
  is intact now opens, and a query over it skips that segment with a
  logged warning. Before, the table failed to open.

## Not yet measured here

One profile of rezolus's viewer (rezolus c9e043ad, metriken-storage
0.13.0, metriken-query 0.50.0, M4 Max) found, on the 9.6 h recording
after four dashboard sections, a live heap of 3.22 GB: 2.20 GB of decoded
`HistogramSnapshot` data from the syscall-latency charts (compressed
source 114 MB), 361 MB of raw segment bytes in per-table segment caches,
and about 525 MB of their parsed footers. With `--cache-size-mb 512` the
live heap was still 1.97 GB.

Two of the causes it named were changed on metriken's main before this
entry, and are in metriken-query 0.50.1 and later:

- #407 (`365545e`) keeps a table's opened segments in the pool, within
  its budget; the per-table `SegmentCache` given the whole pool's budget
  is gone.
- #406 (`59ecca8`) charges a histogram block by its vectors' capacity.

Histogram memory under the current code is not measured here yet. The
bench's queries are counter rates.
