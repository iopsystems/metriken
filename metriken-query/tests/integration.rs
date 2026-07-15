//! Integration tests using synthetic parquet fixtures.
//!
//! These tests exercise behaviors that are hard to cover with hand-rolled
//! in-memory data: row-group boundaries, parquet schema metadata edge cases,
//! k-way merge across files, counter reset semantics, etc.
//!
//! Skipped from default `cargo test` runs because they require the `fixtures`
//! feature. Run on demand with:
//!
//! ```bash
//! cargo test -p metriken-query --features fixtures --test integration -- --ignored
//! ```

#![cfg(feature = "fixtures")]

use std::sync::Arc;

use metriken_query::fixtures::FixtureBuilder;
use metriken_query::{BufferPool, MetricsSource, ParquetReader, QueryResult};

// ─── Counter behavior ────────────────────────────────────────────────────────

#[test]
#[ignore]
fn counter_irate_clamps_on_reset() {
    let fixture = FixtureBuilder::new()
        .samples(20)
        .row_group_size(20)
        .resetting_counter("requests", &[], 10, 10) // resets at t=10
        .build()
        .unwrap();
    let reader = ParquetReader::open(fixture.path()).unwrap();
    let (start, end) = reader.time_range().unwrap();
    let result = reader
        .query_range("irate(requests[5s])", start, end + 1.0, 1.0)
        .unwrap();

    // We expect the rate around t=10 to be 0 (clamped), not negative.
    // Verify no value is negative (counter resets should produce 0, not -100).
    if let QueryResult::Matrix { result } = result {
        for series in &result {
            for (_t, v) in &series.values {
                assert!(*v >= 0.0, "counter reset produced negative rate: {v}");
            }
        }
    }
}

#[test]
#[ignore]
fn counter_with_label_filter_selects_one_series() {
    let fixture = FixtureBuilder::new()
        .samples(50)
        .row_group_size(50)
        .monotonic_counter("requests", &[("zone", "us-east")], 10)
        .monotonic_counter("requests", &[("zone", "us-west")], 20)
        .build()
        .unwrap();
    let reader = ParquetReader::open(fixture.path()).unwrap();
    let (start, end) = reader.time_range().unwrap();
    let result = reader
        .query_range(
            r#"rate(requests{zone="us-east"}[1m])"#,
            start,
            end + 1.0,
            1.0,
        )
        .unwrap();

    if let QueryResult::Matrix { result } = result {
        assert_eq!(
            result.len(),
            1,
            "label filter should select 1 series, got {}",
            result.len()
        );
    } else {
        panic!("expected Matrix result, got {result:?}");
    }
}

// ─── Histogram behavior ──────────────────────────────────────────────────────

#[test]
#[ignore]
fn histogram_quantile_across_row_groups() {
    let fixture = FixtureBuilder::new()
        .samples(1000)
        .row_group_size(50) // 20 row groups
        .point_histogram("latency", &[], 4, 16, 5, 100)
        .build()
        .unwrap();
    let reader = ParquetReader::open(fixture.path()).unwrap();
    let (start, end) = reader.time_range().unwrap();

    // Query a window spanning multiple row groups; should produce a non-empty result
    let result = reader
        .query_range("histogram_quantile(0.99, latency)", start, end + 1.0, 1.0)
        .unwrap();
    if let QueryResult::Matrix { result } = result {
        assert!(!result.is_empty(), "expected at least one quantile series");
    } else {
        panic!("expected Matrix, got {result:?}");
    }
}

#[test]
#[ignore]
fn histogram_count_matches_observation_total() {
    // A point_histogram with rate=100 at bucket 5 means each tick adds 100 observations
    // at bucket 5. So at tick t, total cumulative count is 100 * t.
    // histogram_count returns the delta count per tick = 100.
    let fixture = FixtureBuilder::new()
        .samples(10)
        .row_group_size(10)
        .point_histogram("hits", &[], 4, 16, 3, 100)
        .build()
        .unwrap();
    let reader = ParquetReader::open(fixture.path()).unwrap();
    let (start, end) = reader.time_range().unwrap();
    let result = reader
        .query_range("histogram_count(hits)", start, end + 1.0, 1.0)
        .unwrap();
    if let QueryResult::Matrix { result } = result {
        if let Some(series) = result.first() {
            for (_t, v) in &series.values {
                // Allow for the first/last tick edge cases; just verify ~100 deltas
                assert!(
                    (*v - 100.0).abs() < 50.0 || *v == 0.0,
                    "expected delta ~100 per tick, got {v}"
                );
            }
        }
    }
}

// ─── Multi-row-group queries ─────────────────────────────────────────────────

#[test]
#[ignore]
fn query_spans_multiple_row_groups() {
    let fixture = FixtureBuilder::new()
        .samples(100)
        .row_group_size(10) // 10 row groups
        .monotonic_counter("x", &[], 1)
        .build()
        .unwrap();
    let reader = ParquetReader::open(fixture.path()).unwrap();
    let (start, end) = reader.time_range().unwrap();
    let result = reader
        .query_range("rate(x[5s])", start, end + 1.0, 1.0)
        .unwrap();
    // Should produce a non-empty result spanning many row groups
    if let QueryResult::Matrix { result } = result {
        assert!(!result.is_empty(), "expected results across row groups");
        if let Some(series) = result.first() {
            assert!(
                series.values.len() > 10,
                "expected many points, got {}",
                series.values.len()
            );
        }
    }
}

// ─── Multi-file k-way merge ──────────────────────────────────────────────────

#[test]
#[ignore]
fn two_files_same_metric_different_labels_merge() {
    let a = FixtureBuilder::new()
        .samples(50)
        .row_group_size(50)
        .monotonic_counter("requests", &[("zone", "us-east")], 10)
        .build()
        .unwrap();
    let b = FixtureBuilder::new()
        .samples(50)
        .row_group_size(50)
        .monotonic_counter("requests", &[("zone", "us-west")], 20)
        .build()
        .unwrap();

    let reader = ParquetReader::builder()
        .file(a.path())
        .file(b.path())
        .build()
        .unwrap();

    let zones = reader.label_values("requests", "zone");
    assert_eq!(zones.len(), 2, "expected 2 zones, got {zones:?}");
}

#[test]
#[ignore]
fn reader_composition_works() {
    // Build two separate readers, compose into a third
    let a_fixture = FixtureBuilder::new()
        .samples(20)
        .monotonic_counter("metric_a", &[], 1)
        .build()
        .unwrap();
    let b_fixture = FixtureBuilder::new()
        .samples(20)
        .monotonic_counter("metric_b", &[], 1)
        .build()
        .unwrap();

    let reader_a = Arc::new(ParquetReader::open(a_fixture.path()).unwrap());
    let reader_b = Arc::new(ParquetReader::open(b_fixture.path()).unwrap());

    let combined = ParquetReader::builder()
        .reader(Arc::clone(&reader_a))
        .reader_labeled(Arc::clone(&reader_b), [("group", "b")])
        .build()
        .unwrap();

    assert!(combined.has_counter("metric_a"));
    assert!(combined.has_counter("metric_b"));

    // The b-side counter should have the injected label
    let b_labels = combined.counter_labels("metric_b");
    assert!(
        b_labels
            .iter()
            .any(|l| l.get("group").map(String::as_str) == Some("b")),
        "expected injected `group=b` label on metric_b"
    );
}

// ─── Bytes / file equivalence ────────────────────────────────────────────────

#[test]
#[ignore]
fn open_bytes_matches_open_path() {
    let fixture = FixtureBuilder::new()
        .samples(50)
        .row_group_size(25)
        .monotonic_counter("x", &[], 1)
        .build()
        .unwrap();

    let bytes = std::fs::read(fixture.path()).unwrap();
    let reader_a = ParquetReader::open(fixture.path()).unwrap();
    let reader_b = ParquetReader::open_bytes(bytes).unwrap();

    assert_eq!(reader_a.counter_names(), reader_b.counter_names());
    assert_eq!(reader_a.time_range_ns(), reader_b.time_range_ns());
    let (start, end) = reader_a.time_range().unwrap();
    let r_a = reader_a
        .query_range("rate(x[1s])", start, end + 1.0, 1.0)
        .unwrap();
    let r_b = reader_b
        .query_range("rate(x[1s])", start, end + 1.0, 1.0)
        .unwrap();

    // Compare matrix-side counts as a smoke check
    if let (QueryResult::Matrix { result: a }, QueryResult::Matrix { result: b }) = (r_a, r_b) {
        assert_eq!(a.len(), b.len(), "matrix series count differs");
    }
}

#[test]
#[ignore]
fn open_file_matches_open_path() {
    let fixture = FixtureBuilder::new()
        .samples(10)
        .monotonic_counter("x", &[], 1)
        .build()
        .unwrap();

    let path_clone = fixture.path().to_path_buf();
    let reader_a = ParquetReader::open(&path_clone).unwrap();

    let file = fixture.into_file(); // NamedTempFile -> File (unlinks path)
    let reader_b = ParquetReader::open_file(file).unwrap();

    assert_eq!(reader_a.counter_names(), reader_b.counter_names());
}

// ─── Window sidecar columns are not metrics ──────────────────────────────────

// A `.rez` per-sampler table carries per-metric acquisition-window sidecar
// columns `<m>:window_begin` (Int64) and `<m>:window_width` (UInt64) that have
// no `metric` metadata. `parse_schema` classifies purely by Arrow type, so
// without special handling the Int64 begin column becomes a phantom gauge and
// the UInt64 width column a phantom counter. They must be recognized as sidecars
// of `<m>` and excluded from the metric listings.
#[test]
fn window_sidecar_columns_are_not_metrics() {
    use arrow::array::{Int64Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use std::collections::HashMap;
    use std::sync::Arc;

    let ts = Field::new("timestamp", DataType::UInt64, false);
    let counter = Field::new("cpu_cycles", DataType::UInt64, true).with_metadata(HashMap::from([
        ("metric".to_string(), "cpu_cycles".to_string()),
        ("metric_type".to_string(), "counter".to_string()),
    ]));
    // Sidecars carry no metadata at all — exactly as the rezolus writer emits them.
    let wbegin = Field::new("cpu_cycles:window_begin", DataType::Int64, true);
    let wwidth = Field::new("cpu_cycles:window_width", DataType::UInt64, true);
    let schema = Arc::new(Schema::new_with_metadata(
        vec![ts, counter, wbegin, wwidth],
        HashMap::from([
            ("source".to_string(), "rezolus".to_string()),
            ("sampling_interval_ms".to_string(), "1000".to_string()),
        ]),
    ));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(vec![1_000u64, 2_000u64])),
            Arc::new(UInt64Array::from(vec![Some(10u64), Some(20u64)])),
            Arc::new(Int64Array::from(vec![Some(-100i64), Some(-50i64)])),
            Arc::new(UInt64Array::from(vec![Some(80u64), Some(90u64)])),
        ],
    )
    .unwrap();

    let mut bytes: Vec<u8> = Vec::new();
    {
        let mut w = ArrowWriter::try_new(&mut bytes, schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    let reader = ParquetReader::open_bytes(bytes).unwrap();
    assert_eq!(
        reader.counter_names(),
        vec!["cpu_cycles".to_string()],
        "the width sidecar must not appear as a phantom counter"
    );
    assert!(
        reader.gauge_names().is_empty(),
        "the begin sidecar must not appear as a phantom gauge: {:?}",
        reader.gauge_names()
    );
    assert!(
        !reader.all_names().iter().any(|n| n.contains(":window")),
        "no sidecar should surface in any metric listing: {:?}",
        reader.all_names()
    );
}

// ─── Time range edges ────────────────────────────────────────────────────────

#[test]
#[ignore]
fn query_window_after_data_returns_empty_or_not_found() {
    let fixture = FixtureBuilder::new()
        .samples(10)
        .monotonic_counter("x", &[], 1)
        .build()
        .unwrap();
    let reader = ParquetReader::open(fixture.path()).unwrap();
    let (_, end) = reader.time_range().unwrap();

    // Query a window entirely after the data
    let result = reader.query_range("rate(x[1s])", end + 100.0, end + 200.0, 1.0);
    // Either an empty Matrix or a MetricNotFound is acceptable
    match result {
        Ok(QueryResult::Matrix { result }) => {
            assert!(
                result.is_empty(),
                "expected empty matrix for after-data window"
            );
        }
        Err(_) => {} // also acceptable
        other => panic!("unexpected result: {other:?}"),
    }
}

// ─── Schema introspection ────────────────────────────────────────────────────

#[test]
#[ignore]
fn filename_defaults_to_basename_for_path_open() {
    let fixture = FixtureBuilder::new()
        .samples(5)
        .monotonic_counter("x", &[], 1)
        .build()
        .unwrap();
    let reader = ParquetReader::open(fixture.path()).unwrap();
    let name = reader
        .filename()
        .expect("filename should default from path");
    assert!(
        name.ends_with(".parquet"),
        "expected .parquet basename, got {name}"
    );
}

#[test]
#[ignore]
fn label_values_returns_distinct_values() {
    let fixture = FixtureBuilder::new()
        .samples(20)
        .monotonic_counter("rps", &[("cpu", "0"), ("zone", "us-east")], 1)
        .monotonic_counter("rps", &[("cpu", "1"), ("zone", "us-east")], 1)
        .monotonic_counter("rps", &[("cpu", "2"), ("zone", "us-west")], 1)
        .build()
        .unwrap();
    let reader = ParquetReader::open(fixture.path()).unwrap();
    let cpus = reader.label_values("rps", "cpu");
    let zones = reader.label_values("rps", "zone");
    assert_eq!(cpus.len(), 3);
    assert_eq!(zones.len(), 2);
}

// ─── BufferPool integration ──────────────────────────────────────────────────

#[test]
#[ignore]
fn buffer_pool_caches_decoded_data() {
    let fixture = FixtureBuilder::new()
        .samples(100)
        .row_group_size(20)
        .monotonic_counter("x", &[], 1)
        .build()
        .unwrap();

    let pool = BufferPool::new(10 * 1024 * 1024);
    let reader = ParquetReader::open_with_pool(fixture.path(), Arc::clone(&pool)).unwrap();
    let (start, end) = reader.time_range().unwrap();

    // First query: misses everywhere
    let _ = reader
        .query_range("rate(x[1s])", start, end + 1.0, 1.0)
        .unwrap();
    let s1 = pool.stats();

    // Second query: should hit
    let _ = reader
        .query_range("rate(x[1s])", start, end + 1.0, 1.0)
        .unwrap();
    let s2 = pool.stats();

    assert!(
        s2.hits > s1.hits,
        "expected cache hits on second query: {s1:?} -> {s2:?}"
    );
}

#[test]
#[ignore]
fn buffer_pool_evicts_when_full() {
    // Tight budget that can only hold 1-2 row groups worth of decoded data
    let pool = BufferPool::new(1024);

    let fixture = FixtureBuilder::new()
        .samples(1000)
        .row_group_size(50) // 20 row groups
        .monotonic_counter("x", &[], 1)
        .build()
        .unwrap();
    let reader = ParquetReader::open_with_pool(fixture.path(), Arc::clone(&pool)).unwrap();
    let (start, end) = reader.time_range().unwrap();

    // Run a query that touches all row groups
    let _ = reader
        .query_range("rate(x[1s])", start, end + 1.0, 1.0)
        .unwrap();

    let stats = pool.stats();
    assert!(
        stats.bytes_used <= 1024,
        "bytes_used should respect budget: {stats:?}"
    );
}
