# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `labeled::LabeledSource`, a `DataSource` whose series carry extra labels,
  delegating every read path (the column scan included). A series' own value
  of an added key moves to `exported_<key>` (also when equal, and one prefix
  further when `exported_<key>` is taken), as Prometheus does for a scraped
  target with `honor_labels: false`; a filter on `<key>` matches the added
  value and one on `exported_<key>` the series' own. `add_labels`,
  `add_labels_map`, `translate_filter`, `keeps`, `EXPORTED_PREFIX` and
  `RECORDING_LABEL`
  (`__recording__`) are public with it.
- `SamplerReader::recording_labels`, `same_recording`, `holds` and
  `holds_histogram`, and `ArchiveReader::series_labels` (a
  `reader::SeriesLabels`): the labels each recording's series carry,
  with `__recording__` (the recording's catalog index) added for recordings
  whose label sets are identical, found among every recording of the reader
  whether it has tables or not. Worked out once per reader and returned by
  reference.

### Fixed

- `HistogramStream::merge` decoded every stream under the first stream's
  bucket configuration, checked only by a `debug_assert!`, so a release build
  read a differently configured stream's buckets as other buckets. It now
  leaves out streams of another configuration with a warning (a label
  matcher reaches them), as a table split into `__run__`s does, and finds
  each series' merged index by hash rather than a linear search.

### Changed

- **Breaking:** `ArchiveReader::counter_labels`, `gauge_labels` and
  `histogram_labels` return each label set with its recording's labels
  added, matching the series metriken-query now returns.

## [0.2.3] - 2026-10-13

### Fixed

- `SourceRecorder::maybe_seal` could write a segment with more rows than
  the seal policy's `max_rows`. The account counts rows as they are staged
  and the seal check runs once per commit, so a commit holding several
  passes of a source (frames that queued while a slow seal ran) left more
  than `max_rows` live rows, and dendro's seal put them all in one
  segment. `maybe_seal` and `finalize` now seal with dendro 0.4.2's
  `SourceWriter::seal_capped` and the policy's `max_rows`, which splits
  such a backlog into consecutive segments of at most `max_rows` rows.
  The byte cap and the age bound are unchanged: they decide when a stream
  seals, not where a split falls. If a part cannot be encoded on its own,
  dendro logs a warning and seals that stream's rows as one segment.
  A split costs one encode per `max_rows` rows. At production caps (900
  rows) a backlog past the cap is rare, but with a small `max_rows` under
  sustained CPU overload the extra encodes put the recording behind wall
  clock: 3.4 to 4.5 s behind in rezolus's 2-row hindsight test fixture
  with one busy loop per core, against 0.1 to 0.4 s with uncapped seals.
- `Encoder` implements dendro's `SegmentEncoder::encode_after` for the
  parts of a split seal after the first. The writer puts a group's schema,
  a long table's columns and a sampler cell's metadata only on the first
  row since the last seal that needs them, so a part that starts after
  that row is anchored from the rows before it: the schema is put on its
  first row when the hashes match, and a metric's first cell in the part
  gets the metadata it last carried. Without it the part's group and long
  rows were skipped as unanchored and its cells read back without labels.
- Rows were lost when a group's schema returned to an earlier one within a
  segment (A, B, A), which predates the seal cap. The writer stated each
  schema hash once per segment, and 0.2.2's WAL decoder resolved a row
  against the last schema stated, so the rows of the second run of A were
  skipped as unanchored, in a sealed segment and in a live tail. A
  Prometheus-style group whose series come and go between scrapes does this
  routinely. The writer now states a schema whenever a row's hash differs
  from the previous row's, for wide and long groups. The WAL row format is
  unchanged, and the first row of each run in an A, B, A sequence states its
  schema, so 0.2.2's decoder also reads what this writer produces. In a
  split seal, `encode_after` looks the part's schema up by hash, which also
  anchors WAL rows a 0.2.2 writer left.
- `SourceRecorder::evict_before` lost every row of an open segment once
  the cutoff passed the segment's first row, which predates the seal cap.
  Retention deletes unsealed WAL rows by timestamp, and the open segment's
  first row is the one that carries a group's schema or a sampler cell's
  metadata, so the rows after it read back as nothing, live and once
  sealed (a group of 20 ticks evicted at tick 5 after tick 10 kept none of
  its 15 rows from tick 5). This happens whenever the retention window is
  shorter than the seal policy's `max_age` (300 s by default): a short
  hindsight lookback or a recorder `retain`. `evict_before` now first
  seals each stream whose open segment started before the cutoff, so its
  rows stay in a segment, which retention drops once its last row is past
  the cutoff. Such a window therefore also bounds segment age: up to
  `window + min(window, max_age)` is kept (about twice the window when it
  is shorter than `max_age`). Before, the WAL rows were cut at the window
  and segments straddling it were kept whole but held nothing readable. A
  window of `max_age` or longer (hindsight's 15 m default) is unaffected.
  When a window is just under `max_age`, every stream of a source is sealed
  in the same eviction, so their seals align from then on. A seal that
  dendro defers after its retries is retried by dendro 0.4.2 at the start
  of the next eviction, and a stream it still cannot seal is left out of
  that eviction, so a deferred seal does not let retention cut an anchor
  either, unless the retried seal then fails with an error that is not
  retryable (an encoder error, for one): dendro logs it and evicts that
  stream's live rows. `evict_before` makes two dendro eviction passes
  (data streams, then occupant streams) after its seal, so while a seal
  stays blocked by a lock it can wait for up to three retry schedules.
- The WAL decoders (`materialize_wal_tail`, `materialize_long_wal_tail`)
  resolve a schema-less row against the schema its hash names, wherever in
  the tail it was stated, rather than only the last schema stated. A tail
  a 0.2.2 writer left for a group whose schema went A, B, A now reads in
  full, live, sealed or copied.
- Restating the schema on every change of hash has a cost for a group
  whose schema changes every tick, such as a scraped target with a series
  that is present on alternate scrapes: each row then carries its whole
  schema. The review measured the WAL of such a Prometheus-shaped group
  at about 28 to 32 times its size with a steady schema over 60 ticks
  (50 series: 21.5 KB to 600.8 KB; 500 series: 0.19 MB to 6.06 MB). The
  seal's byte estimate (`metriken_model::cost::group_approx_bytes`) counts
  values and not schema bytes, so `max_bytes` does not react to this;
  only the row and age caps bound how much accumulates before a seal.
- `Staged` documents that every staged pass must be committed: staging
  advances per-stream state (the schema a segment carries, dedup keys,
  occupant numbering, seal accounts) before the rows land.
- dendro 0.4.2 is required.

## [0.2.2] - 2026-10-12

### Added

- `long::SegmentMetadataMerge`, `long::merge_segment_metadata` and
  `long::SegmentMetadata`: the file key-value metadata of the segment that
  concatenating several segments produces. `SegmentMetadataMerge` builds it
  one segment at a time (`start`, `extend`, which refuses a segment that
  cannot be merged and leaves the run as it was, and `finish`), decoding
  each segment's lists once, so a run costs time linear in its metadata;
  `merge_segment_metadata` applies it to a slice and returns `None` when
  the segments cannot be merged. Long segments merge by union of their
  segment and column occupant lists; any other key must agree. Segments
  that are not all long merge only when they have the same entries, in any
  order, and the merged segment has the first's order. `SegmentMetadata`
  is `#[non_exhaustive]`; build one with `SegmentMetadata::new`. For a
  compaction's metadata merge, so that a long stream merges across
  occupant-list changes.
- `long::OccupantUnion` (feature `archive`): `SegmentMetadataMerge` as
  dendro's `rewrite::MetadataMerge`, for `rewrite::compact_stream_with`.

### Changed

- dendro 0.4.1 is required. Its compaction carries the segments' key-value
  metadata, so a long stream compacted with `compact_stream` stays long;
  with 0.4.0 the merged segment lost `metriken.layout` and
  `metriken.occupants` and read as a wide table.

## [0.2.1] - 2026-10-12

### Fixed

- A long segment's label listing (`counter_labels`, `gauge_labels`,
  `histogram_labels`, and the series a footer-only open registers) named
  every metric column once per occupant in the segment, including
  occupants the column has no value for. A group mixing unslotted metrics
  with slotted ones listed each unslotted metric once per slot, with the
  slot's labels. Queries were not affected.

### Added

- `LongTableBuilder` writes, for each metric column that has values for
  only some of the segment's occupants, the occupants it has, under the
  file key-value key `long::column_occupants_key(column)`
  (`metriken.column_occupants.<column>`), in the segment list's range
  encoding. The reader lists those occupants for that column, and the
  segment's list for a column without the key. Segments written before
  this release have no keys and list as before. `long::COLUMN_OCCUPANTS_PREFIX`
  and `long::column_occupants_key` are new.

## [0.2.0] - 2026-10-11

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
  `metriken_exposition` before the next. `StreamRow` has a required `window`
  method, which `AgentRow` implements; this breaks every other implementor.

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
