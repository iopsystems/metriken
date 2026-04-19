# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### metriken-query 0.9.4

- Support PromQL `on(...)` and `ignoring(...)` label-matching modifiers on
  binary operators, allowing expressions whose operands carry mismatched label
  sets (e.g. `tx_bytes / ignoring(direction) link_bandwidth`) to combine
  correctly.

### 0.5.1
Metriken versions older than 0.5.1 did not have changelogs.
