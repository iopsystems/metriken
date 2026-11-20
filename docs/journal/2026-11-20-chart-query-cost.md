# Chart query cost: opening a high-cardinality table, and rates over many series

**Status:** OPEN. Opened 2026-11-20. Baseline measured; criteria written
before any change. Change 1 measured: NO-GO, and its prediction refuted
(0.414 s against 0.25 s; refutation bar 0.4 s). With a second commit not
in the criteria (a bounds check), 0.294 s. Output unchanged; both kept
(see "Change 1: results").

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
- Freeing is 27% of self time, over the whole capture.
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
opens the table, then 8 warm `sum(irate(task_cpu_usage[5m]))` runs;
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
  - Both commits are kept: each is faster on fresh 100 ms and changes no
    output.
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
