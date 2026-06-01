# Benchmarks

Memory and query-latency benchmarks for `ParquetReader`. Both binaries:

- Accept a base parquet via `METRIKEN_TEST_PARQUET` env var
- Augment it to multiple sizes (1x, 10x, 25x, 100x) using `ParquetAugmentor`
- Print stdout output suitable for inclusion in PR descriptions

## Running

```bash
# With a real rezolus capture (recommended)
METRIKEN_TEST_PARQUET="$HOME/Downloads/metrics (10).parquet" \
  cargo bench --bench memory --features fixtures

METRIKEN_TEST_PARQUET="$HOME/Downloads/metrics (10).parquet" \
  cargo bench --bench query_latency --features fixtures

# Or use `cargo run --release` (cargo bench is also fine):
cargo run --release --bench memory --features fixtures
```

If `METRIKEN_TEST_PARQUET` is unset, both fall back to a synthetic ~1MB fixture.

## Interpreting

- **RSS delta on open**: Should be small (1-2 MB) regardless of file size. This is the streaming win — we hold parquet metadata, not the full data.
- **RSS delta per query**: Scales with the *working set* (number of series touched), not the file size. A query touching one metric should stay small even on a 1 GB file.
- **Latency**: Roughly linear in the data size (more row groups = more data to read), but with a smaller constant than full materialization.

For a true before/after comparison against the old `Tsdb` approach, see the `metriken-query-baseline-bench` crate (added in a follow-on commit).
