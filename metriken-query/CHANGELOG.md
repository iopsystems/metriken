# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.11.0] - 2026-06-03

### Changed

- Refactored from a materialized in-memory query engine (`Tsdb`) to a streaming Arrow-native parquet reader. See `README.md` for migration. (#113)

### Added

- PromQL: `histogram_sum(metric)` function. (#112)

[Unreleased]: https://github.com/iopsystems/metriken/compare/metriken-query-v0.11.0...HEAD
[0.11.0]: https://github.com/iopsystems/metriken/compare/metriken-query-v0.10.8...metriken-query-v0.11.0
