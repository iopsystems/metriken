# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.5] - 2026-10-05

### Changed

- `ArchiveReader` forwards `query_range_display_opts` to the table or union
  a query routes to, with the cross-cadence evaluation timestamps
  `query_range_opts` applies, so a display query is reduced by the reader
  as it computes rather than after building the full matrix.

## [0.3.4] - 2026-10-04

### Changed

- The occupant stream of a long table is read keeping each sealed segment's
  first row per occupant. The relabel keeps an occupant's first labels, so
  the later rows changed no series' labels; `reuse_from` kept them in
  memory for the life of the following reader.

## [0.3.3] - 2026-10-02

### Added

- `ArchiveReader::keep_handover` and `reuse_from`. A table built after
  `keep_handover` saves its open state. `reuse_from` gives each table of a
  newer reader of the same file the state the older reader saved for it.
  That table then reads only the segments sealed since, the live tail, and
  the occupant-stream segments not yet decoded, and takes over the older
  reader's open segments when both use the same pool. Segments are matched by sequence number, row
  count and time span. Once retention evicts a segment the older reader
  read, only the decoded occupant-stream segments are reused. On a
  30-minute recording of a per-task group, the first query after a reopen
  took 2.7x a warm query, and takes 1.3x after a reopen that reuses the
  previous reader (one host, load average about 9).

### Changed

- Requires metriken-query 0.34.1.

## [0.3.2] - 2026-10-01

### Added

- Slot groups travel long on the replication stream. `FrameProducer` sends
  a long group as a `WalLongRow` keyed by per-group occupant keys, preceded
  by a `<group>/occupants` row for each key the subscription has not been
  sent. `StreamDecoder` reads each row's form from the row, and
  `SourceRecorder::stage_streamed` writes a long row with its keys mapped
  to occupant numbers by occupant identity.

## [0.3.1] - 2026-09-30

### Added

- `stream::SchemaCache::retain`, `len` and `is_empty`. A producer whose group
  names are unbounded drops the groups it no longer has; the cache otherwise
  keeps one entry per group name it has seen.

## [0.3.0] - 2026-09-30

### Added

- `stream::SchemaCache`, which converts each group's schema to the segment
  format once per schema hash and hands out an `Arc` on later passes.
  Converting every group's schema on every pass, then freeing it, was about
  95% of the cost of encoding a pass for the stream: on rezolus's
  `v3_build_cost` registry (a 2,500-task group), 20.4 ms per unchanged pass
  before, 0.29 ms after.

### Changed

- **Breaking:** `EncodedGroup::schema` is `Option<Arc<GroupSchema>>`, and
  `EncodedGroup::encode` and `encode_groups` take a `&mut SchemaCache`.
- `FrameProducer` puts a changed schema into an already-encoded row with
  metriken-segment's `encode_wal_group_row_with_schema` instead of cloning
  the schema and decoding and re-encoding the row: 3.3 ms to 0.86 ms per
  frame on the same registry when the task group's membership changed.

## [0.2.9] - 2026-09-29

### Added

- `stream` (feature `stream`): a producer's groups as dendro replication
  frames, moved from rezolus's agent (`frames.rs`). `FrameProducer` makes
  one subscription's frames: dendro's preamble and a `Frame::Handshake`
  whose uuid is the process's producer epoch and whose anchor is its clock
  anchor, then per interval a `Frame::Rows` of encoded `WalGroupRow`s, with
  a group's schema inside the payload on its first row and whenever its hash
  changes for that subscriber, and an empty `Frame::Rows` for an interval
  with nothing new. Every rows frame names dendro's `NO_INDEX_STATE`.
  `EncodedGroup` encodes a pass's `GroupSnapshot`s once for every
  subscriber; `StreamRow` lets a producer pass rows of its own type. The
  transport stays with the caller. A separate feature from `write`, since a
  producer serving a stream needs neither the archive writer nor dendro's.

## [0.2.8] - 2026-09-29

### Fixed

- A metric that first appears in a later segment of a table, or only in
  its live tail, can be queried. The reader learned a table's metric names
  from its first segment alone. The encoder now stores a names fingerprint
  in each sealed segment's dendro `caller_index`, and the reader probes one
  footer per distinct fingerprint, plus the tail's schema-carrying rows. A
  segment without a fingerprint is assumed to hold the first segment's
  names, as before.

### Added

- `READABLE_ENCODERS`, the encoder versions the reader decodes (today
  `ENCODER_VERSION` alone). `ArchiveReader::from_catalog` refuses a source
  whose `encoder` key, which dendro writes at `add_source`, names another,
  rather than misreading its WAL rows. A source without the key is not
  checked. `ENCODER_VERSION` is now also re-exported at the crate root.
- Sealed segments carry metriken-segment's format version (`metriken.format`).
- `Catalog::segment_indexes`, each sealed segment's caller index without
  its payload. It defaults to empty, for a container that keeps none.

## [0.2.7] - 2026-09-29

### Changed

- `Encoder` and `ENCODER_VERSION` move to a new `encoder` module outside
  the `write` feature (`metriken_archive::Encoder`), since they use only
  metriken-segment. A reader build, the browser viewer's included, can now
  copy an archive with dendro's `copy_sources_into`, which encodes each
  stream's live tail. `writer::Encoder` and `writer::ENCODER_VERSION` still
  name them.

## [0.2.6] - 2026-09-29

### Added

- `KeepMetrics`, a dendro `ColumnFilter` for a copy trimmed to some
  metrics (`CopySpec::keep_columns`). It keeps every column a segment
  needs to place its rows (timestamps, windows, a long table's `occupant`),
  a value column by its name, its base before `:`, or its `metric`
  metadata, and a per-metric window with its metric; occupant streams are
  copied whole (`projects`). Needs dendro 0.3.3, whose projection keeps a
  long segment's layout markers.
- `segment_props` is public: the writer properties a re-encoding caller
  passes so a projected segment matches a sealed one.

### Changed

- dendro 0.3.3.

## [0.2.5] - 2026-09-28

### Changed

- The writer seals segments with zstd level 3 by default, where it used
  the segment format's LZ4. `WriterConfig::compression` sets the codec
  (re-exported `Compression`, `ZstdLevel`). On replayed recordings zstd-3
  was 55–57% smaller than LZ4 for about 4% more encode time, with the same
  tick latency and query time. `Encoder::for_streams` seals with it too.
- `ArchiveReader` encodes the tail it rebuilds from unsealed rows with the
  same codec. The tail is held in memory while the reader is open, so zstd's
  smaller encoding is the smaller footprint; readers already decode zstd.
  `default_compression` and the re-exported `Compression`, `ZstdLevel` are
  at the crate root, outside the `write` feature.

## [0.2.4] - 2026-09-28

### Added

- `Encoder::for_streams`: the segment encoder for an archive this process
  is not writing (a copy, a ranged dump), built from the archive's stream
  list. A stream is long when its occupant stream is present, as a reader
  decides. Its version matches the writer's, so dendro's
  `copy_sources_into` accepts it for a live archive.

## [0.2.3] - 2026-09-28

### Added

- The writer ingests V1/V2 snapshots, from producers older than acquisition
  groups: one table per `sampler` label (`unattributed` without one), each
  metric with its own window, as `WalCell` rows. A sampler's row is skipped
  when its newest window has not advanced, and a metric's metadata rides on
  its first row in each segment, as rezolus's `.rez` writer does.

## [0.2.2] - 2026-09-28

### Changed

- `WriterConfig::sort_long` defaults to `false`: long segments are sealed
  in arrival order. On replayed recordings arrival order was 6–20% smaller
  at 100 ms and no slower on the tick path; sorting belongs at compaction.

## [0.2.1] - 2026-09-28

### Added

- `SourceRecorder::update_metadata`: merge a patch into a source's metadata
  while it records, ordered with the ticks around it. For facts learned
  during a recording, such as the events marking where a wrapped command
  started and ended (dendro's `SourceWriter::update_metadata`).

## [0.2.0] - 2026-09-28

### Added

- `writer` (feature `write`), phase 4 of
  `docs/journal/2026-09-28-high-cardinality-stack.md`: `ArchiveWriter`
  records V3 snapshots into a dendro archive. A group whose members carry a
  slot `id` is written long, with its occupants numbered at ingest and
  their labels in the table's occupant stream (restated every
  `restate_every_ns`, 300 s by default). Other groups are one row per tick.
  `SourceRecorder` holds one source's ingest state (dedup by window end,
  a schema ring of three, schema anchoring per segment, seal accounts) and
  evicts occupant streams one restatement period behind their data.
  `Encoder` is the `SegmentEncoder`, versioned `metriken-archive/1`.
  Whether a group is long is decided by its first schema with members and
  kept for the recording, so a stream never mixes long and wide WAL rows.
  A group with no members, and a long row with no occupant present, are
  not written.
  A long group's metric columns are kept for the recording and only grow,
  so a change of membership re-lays out the new schema over them without
  rebuilding the columns.

### Changed

- `ArchiveReader` materializes a long table's unsealed WAL tail as a long
  segment. A table is long when it has an occupant stream.

## [0.1.0] - 2026-09-28

### Added

- The crate, phase 3 of `docs/journal/2026-09-28-high-cardinality-stack.md`,
  moved from rezolus's `crates/rez` (`RezReader`) without changing
  behaviour:
  - `ArchiveReader`: one `MetricsSource` per source. It opens tables
    lazily, answers routing from a footer-probed name catalog, unions a
    sampler's tables within one source, evaluates across cadences on the
    slow table's rows, composes into labelled multi-sources
    (`composition_sources`), and reads long tables through their occupant
    stream. Constructors: `from_catalog` (with an optional `Reopen` for a
    file and an optional `IndexRelabel` for caller-row identity indexes),
    `from_in_memory`, `flatten`.
  - `Catalog`: what the reader needs from a container, with
    archive-owned row types, and `DendroCatalog`, dendro's implementation.
  - `source_name` and `table_sampler`.
  - `ArchiveReader::opened_tables`, which lists the tables built so far.
