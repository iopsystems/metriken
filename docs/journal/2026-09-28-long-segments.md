# Long segments: one row per observation, keyed by occupant

**Status:** built on `feat/long-segments`, for metriken-query 0.31.0.

## Goal

Read a parquet segment in which one metric column holds the values of many
series, one row per (timestamp, occupant). Each `(metric column, occupant)`
pair is presented as a series, and everything above the single-file reader
(`SegmentedParquetReader`, unions, PromQL) sees ordinary series.

## Why (context from rezolus)

rezolus records groups whose members come and go: a CPU counter per thread,
per cgroup, per CPU. Today each member is a parquet column, `{metric}x{slot}`,
with the member's labels in the column's field metadata. rezolus measured
what that costs (its `docs/journal/2026-09-25-dendro-archive-layout.md`):

- Each column adds about 520 bytes of footer to every segment of about 300
  rows, however dense it is. The per-thread table of a busy host was 273 MB
  in that layout and 26 MB as one row per observation.
- A segment of a 1 s recording during a thread spike reached 90,227
  columns. arrow-rs could not open one of 52,109: the `ARROW:schema`
  flatbuffer failed verification with `TooManyTables`.

rezolus is moving its archives to one row per observation, keyed by an
occupant number, with each occupant's labels held outside the segment. The
reader for that layout has to live here: `DataSource` is `pub(crate)`, and
the public ways into composition take this crate's readers only.

## Design

**Recognising a long segment.** A segment is long when its file key-value
metadata has `metriken.layout = "long"`. It then has a `UInt64` column named
`occupant`, the time columns (`timestamp`, `:wall_offset`, and the window
pair), and one column per metric, carrying the metric's field metadata as
a wide column does.

**The occupants a segment holds are in its footer.** The key
`metriken.occupants` lists them as ascending ranges, `0-1520,1523,1530-1600`.
Open stays footer-only, which `open_performs_no_row_group_decode` requires:
the reader learns every series a segment holds without decoding a row.
Occupant numbers are dense and assigned in order of first sight, so a
segment's set is a few ranges. A list that names more occupants than the
segment has rows is malformed, and the segment presents no series.

**One `ColDesc` per metric column and occupant.** Its labels are the
column's labels plus `__occupant__ = "<n>"`, an internal label by the `__`
rule (`labels::is_internal_label`). The occupant's real labels are the
caller's to supply, through the existing `ColumnRelabel` hook: rezolus maps
`__occupant__` to the occupant's label set. An occupant never changes labels,
so `split` returns one run. Without a relabel the series are still distinct,
named by occupant number.

**Reading.** A read of metric `m` decodes `m`'s column and the `occupant`
column once per row group and routes each row to its series through a map
from occupant to series index. The cost is linear in rows, not rows times
occupants. `read_counter_column` (one series, from a `ColumnPosition` that
now carries the occupant) filters the rows the same way.

**Sample timestamps** of a long segment are its distinct `timestamp` values,
since a tick is many rows.

**Writers** encode the occupant list with `encode_occupant_ranges`, exported
beside the key constants, so the format is defined in one place.

**A column lists its own occupants when it is sparse** (metriken-storage
0.2.1; see "Per-column occupant lists" below).

**Single-series reads use a row index.** A stream reads one series per
segment (`read_counter_column`), and a query streaming every series of a
long segment would otherwise scan every row once per series. Each source
builds, per row group and on first use, the rows each occupant has, with
one pass over the `occupant` column, and keeps it as long as the segment is
cached. The first measurement, before the index, took 152 s for the busy
table's `sum(rate(...))` below.

**A read of few series decodes only their pages.** When a read of a long
segment wants at most 64 occupants, the reader loads that segment's page
index (only a long segment's, whose few columns make it small) and keeps
the pages whose `occupant` bounds can hold one of them. It decodes only
those pages, provided they are at most a quarter of the row group. A
pruned read bypasses the pool's per-column cache, which suits a query
reading a few series and not one streaming every series, since each
series would build its own reader. So the caller says which it is:
`counter_streams` passes `selective` to `counter_column` when it streams
at most 64 series, and a filtered `counters`/`gauges` read prunes when its
filter leaves at most 64 occupants. Histogram streams prune the same way.
Pruning is correct in any row order and selective when the segment is
sorted by occupant.

A first version decided by counting pruned reads per row group and falling
back to a shared decode after eight. It was wrong: the count outlived the
query, so a single-series query after an aggregate on the same reader never
pruned.

## Tests

A second pair of tests writes a long segment sorted by occupant with eight
rows to a page. A single-series read, a filtered gauge read, a filtered
aggregate and a histogram read match the wide answers and leave the pool
untouched, which shows they pruned. An all-series query over a hundred occupants matches too, and
goes through the pool.

`long::reader_tests` builds the same observations as a wide table (a column
per metric and occupant, labelled `__occupant__`) and as long segments, and
requires every query, label listing, `counter_streams` result and sample
timestamp list to be equal, across a segment boundary and in either row
order. Histograms are compared for the occupant present at every tick only:
a wide file decodes a null histogram cell as an empty snapshot and emits it
as a row, while a long segment has no row for an absent observation, so an
occupant absent from some ticks reads differently. The long reading is the
one that matches what was observed: a further test requires an occupant
present at only some ticks to read as a file holding just those ticks.

## Measured

rezolus recordings' per-thread CPU table, re-encoded by a scratch harness
into both layouts with the same writer settings (LZ4_RAW, dictionary off),
long sorted by `(occupant, timestamp)`, and opened with
`SegmentedParquetReader` (512 MiB pool). Queries over the whole recording at
a 10 s step. "One series" is `rate()` of the occupant with the most
observations, on a freshly opened reader. Long segments use arrow-rs's
default page size (at most 20,000 rows to a page). Times on an Apple M4 Max
(16 cores), single runs.

| table | series | open: wide / long | `sum(rate)`: wide / long | one series: wide / long |
|---|---|---|---|---|
| busy host, 2.3 h, 32 segments | 396,117 | 1.99 / 1.30 s | 55.2 / 2.8 s | 640 / 41 ms |
| thread spike, 100 ms, 23 segments | 184,332 | 0.95 / 0.56 s | 12.7 / 1.4 s | 305 / 23 ms |
| quiet host, 9.6 h, 159 segments | 6,644 | 1.20 / 0.59 s | 8.4 / 7.6 s | 483 / 71 ms |

Before pruning, the quiet host's single series took 1.70 s long: it decoded
all 76 million rows of the table to keep one thread's 34,678.

## Per-column occupant lists (metriken-storage 0.2.1)

The reader above crossed every metric column with every occupant in
`metriken.occupants`, so a column listed occupants it has no value for. An
llm-perf archive (one `llm-perf/main` long group, scalar counters beside
labelled slots, 11 occupants) listed `turns` as 11 series, each with a
slot's `id`, where a query returns 1. Queries were correct: a series with
no samples drops out.

`LongTableBuilder` now writes, for each metric column whose non-null rows
cover fewer occupants than the segment holds, the file key-value entry
`metriken.column_occupants.<column>` (`long::column_occupants_key`; the
column's name in the segment, so `<metric>:buckets` for a histogram), in the
same range encoding. A column with values for every occupant writes nothing.
`expand_long` reads the column's list when present and the segment's list
otherwise; a malformed column list falls back to the segment's with a
warning. Open stays footer-only.

**The list is file metadata.** Every reader through
0.2.0 turns each field metadata key that is not in `STORAGE_KEYS` into a
label. The list differs from segment to segment, so in field metadata it
would give an old reader a new label on every long series and split each
series at every segment boundary. Unknown file keys are ignored by those
readers, so no `FORMAT_VERSION` change is needed.

**Archives written before 0.2.1 keep the inflated listing.** Their segments
carry no column lists and nothing recomputes them on read; rewriting the
archive through the writer is the only way to get the exact listing.

**Rewrites.** `KeepMetrics` (dendro's `project_segment_columns`) carries the
file key-value metadata and keeps every row, so each surviving column's list
stays correct; a dropped column's entry stays behind and is never read.
dendro 0.4.0's compaction (`concat_parquet`) wrote the merged segment with the
caller's writer properties and none of the inputs' key-value metadata. That
dropped `metriken.layout` and `metriken.occupants` as well, so a compacted
long segment read as a wide table (measured on the
`tests/writer.rs` fixture on 0.2.0: `rate(cpu_time[3s])` returns 5 series
after `compact_stream` where it returned 4 before; the extra series is the
long table read as wide, with no slot labels). It predates this change.
`tests/writer.rs::a_compacted_segment_is_not_taken_for_the_one_it_replaced`
compacts long streams but compares two readers of the compacted archive, so
it does not detect it; `a_compacted_long_stream_answers_as_before` compares
answers before and after compaction, and failed against dendro 0.4.0.
rezolus does not compact (hindsight evicts whole segments and its dumps copy
them). A fix in dendro must carry the key-value metadata when
every input in a run agrees, and stop the run where it changes; the column
lists then have to be unioned or dropped along with the segment list.
dendro 0.4.1 does the first: its default merge carries the metadata of a run
whose inputs agree and stops the run where they differ, and
`compact_stream_with` takes a caller's merge. metriken-storage 0.2.2 requires
it, runs `a_compacted_long_stream_answers_as_before` unignored, and adds
`long::OccupantUnion`, which unions the segment and column lists so a run
continues across occupant-list changes.

**Measured footer cost.** Each archive's long segments re-encoded with the
same writer properties, with and without the column lists:

| archive | segments | column×segment pairs, sparse | footer before / after | added, of segment bytes |
|---|---|---|---|---|
| llm-perf run, `llm-perf/main` | 11 | 386, 386 | 254,596 / 272,843 B | +18,247 B, 4.6% |
| rezolus per-thread CPU (`cpu_usage/cpu_usage_task`) | 291 | 291, 0 | 4,968,084 / 4,968,084 B | 0 |

The rezolus table has one metric column with a value for every occupant, so
it writes no list. The llm-perf segments are small (36 KB each, 64% of it
footer already) and every column is sparse, about 1.7 KB per segment. On
the llm-perf archive with lists added, each counter lists what an `irate`
query returns, except `requests{status="canceled"}`, which has one sample,
in the last segment; `rate` and `irate` need two.

**Bound.** A column list costs about one more byte than the occupant id's
digits for each run boundary, so a contiguous list is short and a fragmented
one is not. A synthetic segment with 4096 occupants and 64 counters, each
column holding every other occupant, grew its footer from 19,541 to 641,740
bytes (file 102 KB to 708 KB); open went from 0.70 to 0.87 ms. The common
shape, plain metrics on one occupant and each family on its own contiguous
range, stays short. Writing the complement when it is shorter is not done.

## Not in this change

- Any writer. rezolus and dendro write long segments; this crate reads them.
