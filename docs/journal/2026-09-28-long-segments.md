# Long segments: one row per observation, keyed by occupant

**Status:** built on `feat/long-segments`, for metriken-query 0.31.0.
Page-index pruning for a single-series read is the open follow-up (below).

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

## Tests

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
a 10 s step; "one series" is `rate()` of the occupant with the most
observations. Times on an Apple M4 Max (16 cores), single runs.

| table | series | open: wide / long | `sum(rate)`: wide / long | one series: wide / long |
|---|---|---|---|---|
| busy host, 2.3 h, 32 segments | 396,117 | 2.04 / 1.31 s | 55.3 / 2.8 s | 0.63 / 0.27 s |
| thread spike, 100 ms, 23 segments | 184,332 | 0.96 / 0.57 s | 12.8 / 1.4 s | 0.31 / 0.25 s |
| quiet host, 9.6 h, 159 segments | 6,644 | 1.20 / 0.59 s | 8.4 / 7.8 s | 0.49 / 1.70 s |

The one regression is a single series on the quiet host, where each thread
lives for the whole recording: the read decodes every row of the table (76
million) to keep one thread's 34,678. Page-index pruning on `occupant` is
the fix, below.

## Not in this change

- Page-index pruning on `occupant` for a single-series read, so a sorted
  segment decodes only the pages holding the occupant. Needed for the quiet
  host's single-series case above (1.70 s against 0.49 s wide). The read is
  correct without it.
- Any writer. rezolus and dendro write long segments; this crate reads them.
