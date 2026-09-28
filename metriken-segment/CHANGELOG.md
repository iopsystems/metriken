# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
