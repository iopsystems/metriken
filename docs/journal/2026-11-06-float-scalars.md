# Float scalars: Prometheus counters and gauges without truncation

**Status:** OPEN. Design 2026-11-06, owner decisions the same day (below).
Steps 1–4 are built and reviewed on branch `feat/float-scalars`, not yet
released.
Issue #378. rezolus 6.0.0 waits for it (rezolus
`docs/journal/2026-10-08-6-0-release-readiness.md`, decision 13).

## Goal

A Prometheus sample is an f64. Before this change metriken-recorder stored
a Prometheus counter as `u64` and a gauge as `i64`
(`PrometheusConverter::convert` in `metriken-recorder/src/prometheus.rs`,
summary quantile values included), so every fractional value was
truncated: a CPU limit of 0.25 cores read 0, a counter in seconds moved in
whole seconds, a histogram's `_sum` in seconds lost its fraction, a ratio
read 0. After this change, every scalar a Prometheus endpoint serves is
stored as an f64 and read back unchanged. Native metriken producers keep
their integer types.

It lands before rezolus 6.0.0 because a reader without the float kinds
cannot read a recording that has them: if 6.0.0 shipped first, a 6.0.0
reader could not read a later 6.x recording of a Prometheus endpoint.

## Where a Prometheus scalar went before this change

`PrometheusConverter::convert` emits `Snapshot::V3` groups. The writer stores
a group as a `WalGroupRow` (`metriken_model::convert::wal_group_row`, called
from `SourceRecorder::stage_group` in `metriken-storage/src/writer.rs`), or,
when its members carry an `id` (`slotted` in `writer.rs`), in the long
layout as `WalLongRow` and `LongOccupant`; groups are sealed into parquet
segments. `WalValue` and `CellValue` carry only V1/V2 sampler cells, which
the Prometheus path does not use.

## Decisions (owner, 2026-11-06)

1. **Queries see counters and gauges.** A float counter is a counter and a
   float gauge is a gauge to every query, catalog and viewer. An endpoint
   recorded before this change (integer columns) and after it (float
   columns) unions in one query; `refuse_kinds_across_recordings`
   (`metriken-query/src/archive.rs`) would otherwise refuse it. The
   storage read types carry either integer or f64 values per series. The
   alternative, separate kinds everywhere, was rejected.
2. **Floats in the long layout.** A group whose members carry an `id`, such
   as cAdvisor's, keeps the long layout: `LongOccupant` and long tables gain
   float columns. Forcing such a group wide, as gauge histograms are, was
   rejected: a cAdvisor scrape would switch layouts compared with before
   this change.
3. **Any decrease of a counter is a reset**, as Prometheus and the integer
   path do. No tolerance for float noise.
4. **Non-finite samples are still dropped** by the recorder
   (`PrometheusConverter::convert`). Storage also drops a NaN or infinite
   float value at ingest (`GroupTableBuilder::push_group_row`,
   `LongTableBuilder::push_tick`, `Memory::upsert_float_counter_sample` and
   `upsert_float_gauge_sample`), so a value from another producer, staged
   through `ArchiveWriter` or a `StreamDecoder` or ingested by
   `MemoryStore::ingest_snapshot`, never enters a counter's accumulation
   either. The WAL keeps the value as sent; a segment built from it, sealed
   or materialized from the live tail, does not.

## Design

### Wire (metriken-model)

`GroupSchema`, `GroupSnapshot`, `WalGroupRow` and `LongOccupant` gain two
last fields, `float_counters` and `float_gauges` (descriptors in the schema,
values in the snapshot and rows), after `gauge_histograms`.

rmp-serde encodes these structs as positional arrays, and `gauge_histograms`
is left out of the encoding when it is empty. Appending fields the same way
is unsafe: tested against the types of metriken-model 0.2.1, whose wire
layout 0.2.2 (the latest release) keeps unchanged, a schema with
float descriptors and no gauge histograms decodes with the float
descriptors read as `gauge_histograms`, and a row whose float slots are all
`None` decodes with them as gauge histogram slots. So the encoding writes
every field up to the last non-empty one, the empty ones before it
included, by a hand-written `Serialize`. A schema or row with no floats
encodes exactly as before, byte for byte, so its hash and every existing
archive are unchanged. With floats, a 0.2.2 decoder refuses with a length
error: "array had incorrect length, expected 4" for a schema, a group or a
row carrying its schema, "expected 7" for a bare `WalGroupRow`, and
"expected 5" for a long row's occupant. A snapshot is refused as matching
no variant of the untagged `Snapshot`. None of these names the kind.
`schema_nil_offset` (`metriken-model/src/wal.rs`) accepts the new
lengths. `GroupSnapshot::validate`,
`counters()`/`gauges()`-style accessors for floats, `convert::wal_group_row`
and the cost meters (`group_approx_bytes`, `wal_group_row_approx_bytes`,
`wal_long_row_approx_bytes` in `cost.rs`) cover the new fields.

The `SnapshotV3` doc says a future field means a V4. A V4 would let an old
decoder name the refusal; it would also change every consumer's untagged
`Snapshot` match. The trailing fields keep V3, as `gauge_histograms` did.

### Storage (metriken-storage)

- A float scalar is a `Float64` column whose `metric_type` is `counter` or
  `gauge`. The builders stamp `metric_type` on a float column whatever the
  producer sent, as they do for histograms, so an exporter label named
  `metric_type` cannot change it.
- A segment holding a `Float64` column is written as segment format 3
  (`format.rs`), detected from the Arrow type. metriken-storage 0.7.0
  refuses it ("segment format 3 is newer than this reader, which reads up to
  2"). Without the bump, 0.7.0 drops the column without an error and
  `read_table_parquet` panics (tested), so the bump is required. Segments
  without one stay format 1 or 2.
- `parse_schema` classifies `Float64` with `metric_type` `counter` or
  `gauge` as a float counter or gauge column (`ColKind`); a `Float64` column
  with any other `metric_type` is not read.
- The read types (`Counter`, `Gauge`, `CounterSample`, the batch columns)
  carry integer or f64 values per series. `DataSource` keeps its counter and
  gauge methods, and their listings include float columns. The names index
  and the segment column listings gain float sets, which older readers
  ignore (they refuse the segment by format first).
- The wide and long group builders, the WAL replay and the stream decoder
  carry the float fields. A float member makes a group no less slotted.

### Query (metriken-query)

Integer counters keep exact `u64` differencing; float counters difference in
f64 with the same reset rule (decision 3). Gauges evaluate in f64 already
(`gauge.rs`), so an f64 gauge is read directly. The batched rate scan gains
an f64 column path: before this change it read a values column only as
`UInt64` (`grid_rates` in `batch_rate.rs`), so a float column made `u64s()`
return `None` and the series was skipped without an error. Every reader of
a scan's chunks reads either column type (`batch_rate::CounterColumn`): the
rates, the per-source union's row coverage (`RecordingChunks`) and conflict
detection (`RecordedChunks`). The in-memory store (`memory_store.rs`, the
live path) takes f64 values.

Limit: integer counter values are exact as `u64`, but a series that turns
float (stored as integers in some segments and floats in others) reads as
one `Float` series, so its integer readings above 2^53 lose exactness in a
range that spans the change (`CounterValues::extend` and `to_f64` in
metriken-storage).

### Recorder (metriken-recorder)

One converter is one endpoint, so "per source" is per converter: every
scalar it emits is a float counter or gauge, whether or not a given
sample has a fraction. That covers typed counters and gauges, untyped
samples (the `_total`/`_sum`/`_count` name rule picks counter or gauge),
summary quantile values, and a histogram's `_sum` and `_count`.

### rezolus

The readers and the dashboard forward the float listings, the catalog and
MCP report float columns as `counter` and `gauge`, and MCP wraps a float
counter in `irate()` as it does an integer one. About 17 sites, after the
metriken releases.

## Not changed

`WalValue`, `CellValue` and V1/V2 snapshots (no Prometheus data passes
through them); metriken-core's `Value` and native producers;
`prometheus_text` output.

## Order of work

1. metriken-model: the wire fields and their encoding, with tests that copy
   the 0.2.1 types and check both directions.
2. metriken-storage: format 3, float columns wide and long, the read types,
   listings and indexes.
3. metriken-query: f64 counters and gauges through every evaluator and the
   batched scan.
4. metriken-recorder: float conversion per endpoint; metriken-exposition
   literals.
5. Releases (model, exposition, storage, query, recorder), then rezolus.

Each step is a commit of one PR, gated by the workspace's checks.
