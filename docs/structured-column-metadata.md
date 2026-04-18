# Structured Column Metadata

## Status: Future Design Change

## Problem

Arrow field metadata on parquet metric columns currently uses a flat
`HashMap<String, String>` that mixes metric properties with user-facing
labels:

```
{
  "metric": "cpu_usage",
  "metric_type": "counter",
  "unit": "nanoseconds",
  "state": "user",
  "id": "0"
}
```

The TSDB reader must maintain a skip list of internal keys (`metric`,
`metric_type`, `unit`, `grouping_power`, `max_value_power`) to avoid
leaking them as PromQL labels. This is fragile — every new metadata
field requires updating the skip list in every reader.

## Proposed Solution

Separate metric properties from labels with a structured schema:

```
{
  "metric": "cpu_usage",
  "metric_type": "counter",
  "unit": "nanoseconds",
  "labels": "{\"state\":\"user\",\"id\":\"0\"}"
}
```

The `labels` value is a JSON-encoded map since Arrow field metadata
values are strings. The reader parses `labels` to build the TSDB label
set and ignores all other keys — no skip list needed.

## Migration

Backward compatibility via version detection:

- If a `labels` key is present in field metadata, use the structured
  path (parse JSON, ignore everything else).
- If absent, fall back to the current flat-bag behavior with the skip
  list.

This allows old parquet files to load without modification while new
recordings use the clean format.

## Affected Crates

- **metriken-exposition** (`src/parquet.rs`): writes field metadata when
  producing parquet files.
- **metriken-query** (`src/tsdb/mod.rs`): reads field metadata when
  loading parquet into the TSDB.
- **rezolus** (`src/parquet_tools/combine.rs`): copies and merges field
  metadata when combining multi-source files.
