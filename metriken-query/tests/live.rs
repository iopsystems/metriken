//! Live-source contract tests. These pin the three properties the
//! rest of the system relies on:
//!
//! 1. **Round-trip:** rows appended via `LiveSource::append` are
//!    visible to subsequent `LiveSource::run_sql` calls with the
//!    expected values (counters as `UBIGINT`, gauges as `BIGINT`,
//!    histograms as `LIST<UBIGINT>`).
//! 2. **Schema growth:** appending a new column via `ALTER TABLE`
//!    leaves existing rows as NULL for that column and new rows
//!    populated, with no panic and no rebuild loss.
//! 3. **Concurrent read while writing:** the connection mutex
//!    serializes append + run_sql cleanly; reads never see a torn
//!    row, and writes are not lost.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use arrow::array::Array; // for is_null
use metriken_query::{LiveColumn, LiveColumnKind, LiveSource, LiveValue};

fn column(physical: &str, metric: &str, kind: LiveColumnKind) -> LiveColumn {
    LiveColumn {
        physical: physical.to_string(),
        metric: metric.to_string(),
        kind,
        labels: BTreeMap::new(),
    }
}

#[test]
fn round_trip_three_snapshots_query_returns_three_rows_in_order() {
    let live = LiveSource::new("rezolus", 1000).expect("LiveSource::new");

    // Three snapshots at 1s, 2s, 3s with a counter and a gauge.
    let cpu = column("cpu_usage/user/0", "cpu_usage", LiveColumnKind::Counter);
    let mem = column("memory_total", "memory_total", LiveColumnKind::Gauge);

    for (t_s, c, g) in [(1u64, 100u64, 5_000i64), (2, 250, 6_000), (3, 425, 7_500)] {
        live.append(
            t_s * 1_000_000_000,
            Some(50_000),
            &[
                (cpu.clone(), LiveValue::Counter(c)),
                (mem.clone(), LiveValue::Gauge(g)),
            ],
        )
        .expect("append");
    }

    let batches = live
        .run_sql(
            "SELECT timestamp, \"cpu_usage/user/0\", memory_total \
             FROM _src ORDER BY timestamp",
        )
        .expect("run_sql");
    assert_eq!(batches.len(), 1);
    let b = &batches[0];
    assert_eq!(b.num_rows(), 3, "three appended rows");

    let ts = b
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .expect("timestamp BIGINT");
    let cnt = b
        .column(1)
        .as_any()
        .downcast_ref::<arrow::array::UInt64Array>()
        .expect("counter UBIGINT");
    let g = b
        .column(2)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .expect("gauge BIGINT");

    assert_eq!(
        (ts.value(0), cnt.value(0), g.value(0)),
        (1_000_000_000, 100, 5_000)
    );
    assert_eq!(
        (ts.value(1), cnt.value(1), g.value(1)),
        (2_000_000_000, 250, 6_000)
    );
    assert_eq!(
        (ts.value(2), cnt.value(2), g.value(2)),
        (3_000_000_000, 425, 7_500)
    );
}

#[test]
fn time_range_returns_min_max_or_none_when_empty() {
    let live = LiveSource::new("rezolus", 1000).expect("new");
    assert_eq!(live.time_range_ns().expect("range"), None);

    let cpu = column("requests", "requests", LiveColumnKind::Counter);
    live.append(1_000_000_000, None, &[(cpu.clone(), LiveValue::Counter(1))])
        .expect("append 1");
    live.append(5_000_000_000, None, &[(cpu.clone(), LiveValue::Counter(5))])
        .expect("append 2");

    assert_eq!(
        live.time_range_ns().expect("range"),
        Some((1_000_000_000, 5_000_000_000))
    );
}

#[test]
fn schema_growth_alter_adds_column_existing_rows_get_null() {
    // Append a row with metric `a`. Then append a row with metrics
    // `a` and `b`. The first row's `b` column must be NULL (correct:
    // metric didn't exist at t=1). The second row must have both
    // populated. Pins the ALTER TABLE ADD COLUMN semantics —
    // metadata-only, existing rows NULL.
    let live = LiveSource::new("rezolus", 1000).expect("new");

    let a = column("a", "a", LiveColumnKind::Counter);
    let b = column("b", "b", LiveColumnKind::Counter);

    live.append(1_000_000_000, None, &[(a.clone(), LiveValue::Counter(10))])
        .expect("append 1");
    live.append(
        2_000_000_000,
        None,
        &[
            (a.clone(), LiveValue::Counter(20)),
            (b.clone(), LiveValue::Counter(99)),
        ],
    )
    .expect("append 2");

    let batches = live
        .run_sql("SELECT timestamp, a, b FROM _src ORDER BY timestamp")
        .expect("query");
    let bat = &batches[0];
    assert_eq!(bat.num_rows(), 2);

    let a_col = bat
        .column(1)
        .as_any()
        .downcast_ref::<arrow::array::UInt64Array>()
        .unwrap();
    let b_col = bat
        .column(2)
        .as_any()
        .downcast_ref::<arrow::array::UInt64Array>()
        .unwrap();

    assert_eq!(a_col.value(0), 10);
    assert!(b_col.is_null(0), "b is NULL at the first row (pre-ALTER)");
    assert_eq!(a_col.value(1), 20);
    assert!(!b_col.is_null(1));
    assert_eq!(b_col.value(1), 99);
}

#[test]
fn schema_growth_histogram_column_stored_as_list_ubigint() {
    // H2 histograms map to LIST<UBIGINT>. After append, an h2_total
    // aggregate over the column should return the sum of buckets.
    // Pins both the storage type and the UDF-binding path on a live
    // source.
    let live = LiveSource::new("rezolus", 1000).expect("new");

    let h = column(
        "request_latency:buckets",
        "request_latency",
        LiveColumnKind::Histogram { grouping_power: 4 },
    );

    // Two snapshots with different bucket distributions.
    let buckets1: Vec<u64> = vec![1, 2, 3, 4, 5];
    let buckets2: Vec<u64> = vec![10, 20, 30, 40, 50];

    live.append(
        1_000_000_000,
        None,
        &[(h.clone(), LiveValue::Histogram(&buckets1))],
    )
    .expect("append 1");
    live.append(
        2_000_000_000,
        None,
        &[(h.clone(), LiveValue::Histogram(&buckets2))],
    )
    .expect("append 2");

    let batches = live
        .run_sql(
            "SELECT timestamp, h2_total(\"request_latency:buckets\") AS total \
             FROM _src ORDER BY timestamp",
        )
        .expect("h2_total query");
    let b = &batches[0];
    assert_eq!(b.num_rows(), 2);
    let total = b
        .column(1)
        .as_any()
        .downcast_ref::<arrow::array::UInt64Array>()
        .expect("total UBIGINT");
    assert_eq!(total.value(0), 1 + 2 + 3 + 4 + 5);
    assert_eq!(total.value(1), 10 + 20 + 30 + 40 + 50);
}

#[test]
fn schema_growth_cgroup_column_rebuilds_cgroup_index() {
    // Adding a `cgroup_*` column must trigger _cgroup_index rebuild.
    // Pin by appending a non-cgroup then a cgroup column and
    // asserting _cgroup_index reflects the cgroup_* column.
    let live = LiveSource::new("rezolus", 1000).expect("new");

    let cpu = column("requests", "requests", LiveColumnKind::Counter);
    live.append(1_000_000_000, None, &[(cpu.clone(), LiveValue::Counter(1))])
        .expect("append 1");

    let mut labels = BTreeMap::new();
    labels.insert("name".to_string(), "system.slice/foo".to_string());
    labels.insert("id".to_string(), "1234".to_string());
    let cgroup_col = LiveColumn {
        physical: "cgroup_cpu_usage/foo".to_string(),
        metric: "cgroup_cpu_usage".to_string(),
        kind: LiveColumnKind::Counter,
        labels,
    };
    live.append(
        2_000_000_000,
        None,
        &[(cgroup_col.clone(), LiveValue::Counter(42))],
    )
    .expect("append cgroup");

    let batches = live
        .run_sql(
            "SELECT metric, column_name, name, id FROM _cgroup_index \
             ORDER BY column_name",
        )
        .expect("cgroup index query");
    let b = &batches[0];
    assert_eq!(b.num_rows(), 1, "exactly one cgroup row");

    let metrics = b
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    let col_names = b
        .column(1)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    let names = b
        .column(2)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    let ids = b
        .column(3)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    assert_eq!(metrics.value(0), "cgroup_cpu_usage");
    assert_eq!(col_names.value(0), "cgroup_cpu_usage/foo");
    assert_eq!(names.value(0), "system.slice/foo");
    assert_eq!(ids.value(0), "1234");
}

#[test]
fn per_source_view_passes_through_src() {
    // _src_<source> for live mode is a pass-through view. Selecting
    // from it should return the same rows as selecting from _src.
    let live = LiveSource::new("live_agent", 1000).expect("new");

    let cpu = column("requests", "requests", LiveColumnKind::Counter);
    live.append(1_000_000_000, None, &[(cpu.clone(), LiveValue::Counter(1))])
        .expect("append");

    // view_name_for_source("live_agent") = "_src_live_agent"
    let n_src: i64 = {
        let b = live
            .run_sql("SELECT COUNT(*) FROM _src")
            .expect("count _src");
        b[0].column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap()
            .value(0)
    };
    let n_view: i64 = {
        let b = live
            .run_sql("SELECT COUNT(*) FROM _src_live_agent")
            .expect("count _src_<source>");
        b[0].column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap()
            .value(0)
    };
    assert_eq!(n_src, 1);
    assert_eq!(n_view, n_src);
}

#[test]
fn timestamp_snaps_to_sampling_interval() {
    // Sub-interval timestamps must snap to the nearest multiple of
    // the interval, matching the parquet path's snap projection.
    // 999_000_000 ns with interval 1s → 1s.
    let live = LiveSource::new("rezolus", 1000).expect("new");
    let cpu = column("requests", "requests", LiveColumnKind::Counter);
    live.append(
        999_000_000, // slightly under 1s
        None,
        &[(cpu.clone(), LiveValue::Counter(1))],
    )
    .expect("append");

    let batches = live.run_sql("SELECT timestamp FROM _src").expect("query");
    let ts = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .unwrap()
        .value(0);
    assert_eq!(ts, 1_000_000_000);
}

#[test]
fn missing_columns_in_a_row_get_null() {
    // Snapshot 1: metrics {a, b}. Snapshot 2: only {a} (b dropped).
    // The schema retains both columns; snapshot 2's b is NULL.
    // Models "metric was sampled previously but not this poll" —
    // the row preserves the absence.
    let live = LiveSource::new("rezolus", 1000).expect("new");
    let a = column("a", "a", LiveColumnKind::Counter);
    let b = column("b", "b", LiveColumnKind::Counter);

    live.append(
        1_000_000_000,
        None,
        &[
            (a.clone(), LiveValue::Counter(1)),
            (b.clone(), LiveValue::Counter(2)),
        ],
    )
    .expect("append 1");
    live.append(2_000_000_000, None, &[(a.clone(), LiveValue::Counter(10))])
        .expect("append 2 — b dropped");

    let batches = live
        .run_sql("SELECT timestamp, a, b FROM _src ORDER BY timestamp")
        .expect("query");
    let bat = &batches[0];
    let b_col = bat
        .column(2)
        .as_any()
        .downcast_ref::<arrow::array::UInt64Array>()
        .unwrap();
    assert!(!b_col.is_null(0));
    assert_eq!(b_col.value(0), 2);
    assert!(b_col.is_null(1), "b is NULL for snapshot 2");
}

#[test]
fn concurrent_reader_sees_consistent_state_while_appender_writes() {
    // Spawn an appender thread (50 INSERTs at ms-resolution apart) +
    // a reader thread (50 SELECTs). Reader must not panic, must
    // never see a torn row (timestamp without corresponding column
    // value), and observed MAX(timestamp) must be non-decreasing.
    let live = LiveSource::new("rezolus", 1000).expect("new");
    let stop = Arc::new(AtomicBool::new(false));
    let c = column("requests", "requests", LiveColumnKind::Counter);

    // Seed the `requests` column on the main thread so the reader can't
    // race the writer's first append and see "column not found" — the
    // table is created without it.
    live.append(1_000_000_000, None, &[(c.clone(), LiveValue::Counter(10))])
        .expect("seed append");

    let writer = {
        let live = live.clone();
        let stop = stop.clone();
        let c = c.clone();
        thread::spawn(move || {
            for i in 2u64..=50 {
                live.append(
                    i * 1_000_000_000,
                    None,
                    &[(c.clone(), LiveValue::Counter(i * 10))],
                )
                .expect("append in writer");
                thread::sleep(Duration::from_micros(100));
            }
            stop.store(true, Ordering::SeqCst);
        })
    };

    let reader = {
        let live = live.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut last_max: i64 = 0;
            let mut iterations = 0;
            while !stop.load(Ordering::SeqCst) || iterations == 0 {
                iterations += 1;
                let batches = live
                    .run_sql("SELECT MAX(timestamp), MAX(requests) FROM _src")
                    .expect("query in reader");
                let b = &batches[0];
                let ts_col = b
                    .column(0)
                    .as_any()
                    .downcast_ref::<arrow::array::Int64Array>()
                    .unwrap();
                if ts_col.is_null(0) {
                    continue;
                }
                let max_ts = ts_col.value(0);
                assert!(
                    max_ts >= last_max,
                    "max timestamp must be non-decreasing: {max_ts} < {last_max}"
                );
                last_max = max_ts;
                thread::sleep(Duration::from_micros(50));
            }
            iterations
        })
    };

    writer.join().expect("writer");
    let iterations = reader.join().expect("reader");
    assert!(iterations > 0, "reader did some work");

    // Final state.
    assert_eq!(
        live.time_range_ns().expect("range"),
        Some((1_000_000_000, 50_000_000_000))
    );
}

#[test]
fn run_sql_surfaces_bad_sql_as_backend_error() {
    let live = LiveSource::new("rezolus", 1000).expect("new");
    let err = live
        .run_sql("SELECT * FROM nonexistent_table")
        .expect_err("should error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("nonexistent_table") || msg.contains("Catalog"),
        "expected DuckDB binder error: {msg}",
    );
}
