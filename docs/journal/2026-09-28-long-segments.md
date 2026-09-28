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
filter leaves at most 64 occupants. Pruning is correct in any row order and
selective when the segment is sorted by occupant. Histogram reads decode
whole row groups; no rezolus group with slots holds histograms.

A first version decided by counting pruned reads per row group and falling
back to a shared decode after eight. It was wrong: the count outlived the
query, so a single-series query after an aggregate on the same reader never
pruned.

## Tests

A second pair of tests writes a long segment sorted by occupant with eight
rows to a page. A single-series read, a filtered gauge read and a filtered
aggregate match the wide answers and leave the pool untouched, which shows
they pruned. An all-series query over a hundred occupants matches too, and
goes through the pool.

`long::reader_tests` builds the same observations as a wide table (a column
per metric and occupant, labelled `__occupant__`) and as long segments, and
requires every query, label listing, `counter_streams` result and sample
timestamp list to be equal, across a segment boundary and in either row
order. Histograms are compared for the occupant present at every tick only:
a wide file decodes a null histogram cell as an empty snapshot and emits it
as a row, while a long segment has no row for an absent observation, so an
occupant absent from some ticks reads differently. The long reading is the
one that matches what was observed.

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

## Not in this change

- Any writer. rezolus and dendro write long segments; this crate reads them.
