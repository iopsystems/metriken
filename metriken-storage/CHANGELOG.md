# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased: metriken-storage 0.2.0

### Changed

- dendro 0.4 is optional, behind a new default feature `archive`, which
  gates the catalog (`Catalog`, `DendroCatalog`, `SegmentIndex`), the reader
  (`ArchiveReader`, `LabeledRecordings`), the segment encoder (`Encoder`,
  `ENCODER_VERSION`, `READABLE_ENCODERS`), the column filter
  (`KeepMetrics`) and the shared archive items re-exported at the crate
  root. `rez` and `write` imply it. A user with `default-features = false`
  who reads archives adds `archive`. Without it the crate compiles no
  SQLite.
- `stream` is `metriken_exposition::stream` (metriken-exposition 0.22),
  re-exported at the same path for this release; import it from
  `metriken_exposition` before the next. `StreamRow` has a `window` method,
  which `AgentRow` implements.

### Added

- `convert` (feature `convert`): `MsgpackToParquet`, `ParquetOptions`,
  `ParquetCompression`, `ParquetHistogramType`, `ParquetSchema` and
  `ParquetWriter`, from metriken-exposition. Needs neither dendro nor the
  archive.
- `Pass` for `rez::wire::AgentRows` (features `rez` and `stream`), so an
  agent's rows pass is served by `StreamSubscription::frame_at`.

## [0.1.0] - 2026-10-10

### Changed

- Renamed from `metriken-segment`; its 0.1.x history is below. Paths are
  unchanged apart from the crate name (`metriken_segment::` becomes
  `metriken_storage::`).
- The slot-size constants in `builder` and the row cost functions in `wal`
  are `metriken-model`'s (`metriken_model::cost`), re-exported at the same
  paths.

### Added

- The column readers, from `metriken-query`: the data-access trait
  (`DataSource`) and its implementations for parquet files, segmented
  tables, unions, compositions and in-memory sources; the long-table
  relabel (`long::OccupantLabels`); the buffer pool; the counter scan
  (`DataSource::counter_scan`), which returns a counter's matched series and
  its segments' decoded columns a chunk at a time. Queries over them are
  `metriken-query`'s, through its `MetricsSource` wrappers. `SegmentedSource`
  opens with `open_with_pool`, `open_relabeled_with_pool` and `open_after`;
  `UnionChild::from_source` and `CompositionSource::{from_source,
  into_source}` build composition entries from any source. Methods on
  `BufferPool`, `Labels`, `UnionChild` and `CompositionSource` that were
  crate-private in `metriken-query` are public here, because the query
  engine now calls them from another crate.
- The archive, from `metriken-archive`: the catalog (`Catalog`,
  `DendroCatalog`), `ArchiveReader`, and with feature `write` the writer
  (`ArchiveWriter`, `SourceRecorder`, `StreamDecoder`); with feature
  `stream`, `FrameProducer`. `ArchiveReader` answers its names, span and
  interval from its probed catalog and its labels from the tables holding
  the name and finds the tables holding given
  metric names (`owners_of`); routing a query, and `MetricsSource` for it,
  are `metriken-query`'s. The writer depends on `metriken-model`, not on
  `metriken-exposition` or the `metriken` registry.
- The `.rez` format, from rezolus (feature `rez`): the container (v1/v2 tar
  reading, the v3 SQLite `RezDb`), its `Catalog` implementation, format
  detection, opening an archive by content (`rez::open`), the v3 rewrite
  tooling and parquet ingest; with `write`, the v3 writer and the
  `.rez`-to-dendro conversion; with `test-support`, the tar writer for
  fixtures. A `record --stream` `.rez`'s identity index (rezolus 5.21.0 to
  5.26.0) is not read, with a warning per indexed table; its slot columns
  carry the labels each segment was built with.

## [0.1.7] - 2026-10-09

### Changed

- The group schema, the write-ahead-log row types and their encodings, and
  the occupant stream's rows and naming are `metriken-model`'s, re-exported
  at the same paths. `window::Window` is `metriken_types::Window`, the same
  type as `metriken::Window` from `metriken` 0.11.3, which the `metriken`
  feature now requires; the `From` conversions between the two are gone and
  `.into()` between them is the identity conversion. A downstream crate that
  implemented one trait for both copies of a merged type gets conflicting
  implementations (E0119).

## [0.1.6] - 2026-09-30

### Added

- `wal::encode_wal_group_row_with_schema`, which puts a schema into an
  encoded `WalGroupRow` whose `schema` is `None` without decoding its values
  or cloning the schema. The output is byte for byte what
  `encode_wal_group_row` gives for the anchored row; a payload laid out any
  other way is decoded and re-encoded.

## [0.1.5] - 2026-09-29

### Added

- A segment format version. Every segment this crate writes (wide, long,
  occupant stream) carries `metriken.format` = `1` in its key-value
  metadata (`format::FORMAT_KEY`, `format::FORMAT_VERSION`), added to any
  metadata the caller's writer properties set (`format::stamped`).
  `format::check` refuses a segment with a newer or non-numeric format, or
  a `metriken.layout` other than `long`; a segment without the key is
  format 1, as is every segment written before it.
  `table::read_table_parquet` and `occupants::decode_segment` make the
  check.
- `wal::schema_rows`, the rows of a live tail that name every column it
  holds: a group or long row that anchors a schema (read no further than
  its schema), or a cell row whose names differ from the row before.
  Materialized alone they give a reader the tail's metric names without
  building the whole tail.
- `WalRowSource` is implemented for `&T`, so a subset of rows can be
  materialized without copying them.

## [0.1.4] - 2026-09-28

### Added

- `*_with` variants that take the caller's parquet writer properties, for a
  writer sealing with another codec than `segment_writer_props`'s:
  `table::write_table_parquet_with`, `LongTableBuilder::finish_with`,
  `wal::materialize_wal_tail_with` and `wal::materialize_long_wal_tail_with`.
  The existing functions are unchanged and keep the default.

## [0.1.3] - 2026-09-28

### Added

- The long layout's writer and WAL row, for phase 4 of
  `docs/journal/2026-09-28-high-cardinality-stack.md`:
  - `long_table::LongTableBuilder`: one row per tick and occupant, written
    with the `metriken.layout=long` marker and the `metriken.occupants`
    footer list, sorted by `(occupant, timestamp)` or kept in arrival order.
  - `wal::WalLongRow` and `wal::LongOccupant`: a long table's values for one
    tick, with each occupant's number, so a reader materializes a live tail
    without the writer's state. Their msgpack codec,
    `wal_long_row_approx_bytes`, and `materialize_long_wal_tail`.

## [0.1.2] - 2026-09-28

### Added

- `wal`: the write-ahead log's row format, phase 2 of
  `docs/journal/2026-09-28-high-cardinality-stack.md`, moved from rezolus's
  `crates/rez`. `WalGroupRow` (a group's values for one tick, one window,
  the schema when anchoring), `WalCell` and `WalValue` (a row of
  individually windowed metrics), their msgpack codec,
  `materialize_wal_tail` (a live tail as a segment, dispatching on
  `is_group_table_key`), and `wal_group_row_approx_bytes`. Materialization
  takes any row type implementing `WalRowSource`, so a container's rows are
  read where they are.

## [0.1.1] - 2026-09-28

### Added

- The wide layout, phase 1b of `docs/journal/2026-09-28-high-cardinality-stack.md`,
  moved from rezolus's `crates/rez` without changing behaviour:
  - `table`: `Table`, `Column`, `Values` (rezolus's `RezTable`,
    `RezColumn`, `RezValues`), `write_table_parquet`, `read_table_parquet`,
    `table_to_batch`, `segment_writer_props`, and the `:wall_offset`,
    `:window_begin` and `:window_width` column names.
  - `builder`: `TableBuilder` and `GroupTableBuilder`, their `Cell` and
    `CellValue` input, and the per-cell size constants a writer meters
    segments with. `TableBuilder` gains `last_key`/`set_last_key` and
    `columns` accessors, and `approx_bytes` and `col_len` are public.
  - `schema`: `GroupSchema` and `MetricDesc`, the wasm-safe mirror of
    metriken-exposition's, and `fnv1a_128`.
  - `window`: `Window`.
- A `metriken` feature: `From` conversions to and from `metriken::Window`.

## [0.1.0] - 2026-09-28

### Added

- The crate, phase 1 of `docs/journal/2026-09-28-high-cardinality-stack.md`.
  - `long`: the long layout's markers (`metriken.layout = "long"`, the
    `occupant` column, the footer's `metriken.occupants` ranges, the
    `__occupant__` label) and the occupant-range codec, moved from
    `metriken-query` 0.31.0.
  - `occupants`: the occupant stream, `<table>/occupants`, moved from
    rezolus. It covers the `Occupant` row, its WAL encoding (msgpack) and
    its segment encoding. A label column is `UInt64` when every value
    converts exactly, as canonical decimal or as 16-digit hex; field
    metadata records which, so the text comes back byte for byte. Anything
    else stays `Utf8`.
