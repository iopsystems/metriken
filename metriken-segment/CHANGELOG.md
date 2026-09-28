# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
