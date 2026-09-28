# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
