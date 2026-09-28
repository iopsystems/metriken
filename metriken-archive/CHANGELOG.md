# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- The writer seals segments with zstd level 3 by default, where it used
  the segment format's LZ4. `WriterConfig::compression` sets the codec
  (re-exported `Compression`, `ZstdLevel`). On replayed recordings zstd-3
  was 55–57% smaller than LZ4 for about 4% more encode time, with the same
  tick latency and query time. `Encoder::for_streams` seals with it too.
  Readers already decode zstd; a tail a reader rebuilds in memory keeps the
  default codec.

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
