# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

## 0.3.0
### Added
- `Window` acquisition-window type (`begin_ns`/`end_ns`, opt-in serde).
- Default `Metric::load_window` / `value_with_window` accessors and
  `window_snapshot` / `load_window` on the group traits (default empty).
- `MetricEntry::module()` — the metric definition's `module_path!()`.

### Changed
- **BREAKING:** `MetricEntry` gained a `module` field; its constructor takes a
  `module` argument (emitted by `#[metric]` via metriken-derive 0.6.0).

## 0.1.3
### Fixed
- Fixed missing metric metadata on dynamic metrics.

## 0.1.2
### Added
- Add `Metric::provide` method and `request_[ref|value]` APIs.

## 0.1.1
metriken-core versions older than 0.1.1 did not have changelogs.
