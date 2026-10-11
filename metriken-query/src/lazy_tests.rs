//! Tests of `metriken_storage::lazy`'s composition children, through the
//! readers.

use std::sync::atomic::{AtomicUsize, Ordering};

use std::sync::Arc;

use crate::*;
use crate::{MemoryStore, MetricsSource, ParquetReader, QueryError};
use metriken_storage::lazy::*;

const SEC: u64 = 1_000_000_000;

fn store() -> MemoryStore {
    let store = MemoryStore::builder().build();
    store
        .insert_counter_series(
            "cpu_usage",
            [("cpu", "0")],
            vec![SEC, 2 * SEC, 3 * SEC, 4 * SEC],
            vec![10, 20, 30, 40],
            None,
        )
        .unwrap();
    store
}

fn catalog() -> CompositionCatalog {
    CompositionCatalog::new(1.0)
        .counters(["cpu_usage"])
        .time_range_ns(SEC, 4 * SEC)
}

fn counted(catalog: CompositionCatalog) -> (CompositionSource, Arc<AtomicUsize>) {
    let loads = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&loads);
    let store = store();
    let source = CompositionSource::lazy(catalog, move || {
        seen.fetch_add(1, Ordering::SeqCst);
        Ok(Some(CompositionSource::from(&store)))
    });
    (source, loads)
}

fn compose(source: CompositionSource) -> ParquetReader {
    ParquetReader::builder()
        .source_labeled(source, [("job", "a")])
        .build()
        .unwrap()
}

#[test]
fn catalog_calls_and_foreign_metrics_do_not_load() {
    let (source, loads) = counted(catalog());
    let reader = compose(source);

    assert_eq!(reader.counter_names(), vec!["cpu_usage".to_string()]);
    assert_eq!(MetricsSource::time_range(&reader), Some((1.0, 4.0)));
    assert!(reader.counter_labels("memory_used").is_empty());
    let result = reader.query_range("rate(memory_used[2s])", 1.0, 4.0, 1.0);
    assert!(
        matches!(result, Err(QueryError::MetricNotFound(_))),
        "{result:?}"
    );

    assert_eq!(loads.load(Ordering::SeqCst), 0);
}

#[test]
fn a_query_loads_once_and_reuses_the_result() {
    let (source, loads) = counted(catalog());
    let reader = compose(source);

    for _ in 0..3 {
        let result = reader
            .query_range("sum(rate(cpu_usage[2s]))", 3.0, 4.0, 1.0)
            .unwrap();
        let crate::QueryResult::Matrix { result } = result else {
            panic!("expected a matrix, got {result:?}");
        };
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].values.last().unwrap().1, 10.0);
    }
    let labels = reader.counter_labels("cpu_usage");
    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0].get("job").map(String::as_str), Some("a"));

    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

#[test]
fn evicted_or_failed_loads_answer_empty() {
    for evicted in [true, false] {
        let loads = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&loads);
        let source = CompositionSource::lazy(catalog(), move || {
            seen.fetch_add(1, Ordering::SeqCst);
            if evicted {
                Ok(None)
            } else {
                Err("segment table missing".into())
            }
        });
        let reader = compose(source);

        for _ in 0..2 {
            let result = reader.query_range("rate(cpu_usage[2s])", 1.0, 4.0, 1.0);
            assert!(
                matches!(&result, Ok(crate::QueryResult::Matrix { result }) if result.is_empty()),
                "evicted={evicted}: {result:?}"
            );
        }
        assert!(reader.counter_labels("cpu_usage").is_empty());
        assert_eq!(loads.load(Ordering::SeqCst), 1, "evicted={evicted}");
    }
}

#[test]
fn catalog_series_count_does_not_load() {
    let (a, a_loads) = counted(catalog().series_count(7));
    let (b, b_loads) = counted(catalog().series_count(5));
    let reader = ParquetReader::builder()
        .source_labeled(a, [("job", "a")])
        .source_labeled(b, [("job", "b")])
        .build()
        .unwrap();

    assert_eq!(reader.total_series_count(), 12);
    assert_eq!(a_loads.load(Ordering::SeqCst), 0);
    assert_eq!(b_loads.load(Ordering::SeqCst), 0);
}

#[test]
fn series_count_without_catalog_count_loads() {
    let (source, loads) = counted(catalog());
    let reader = compose(source);

    assert_eq!(reader.total_series_count(), 1);
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

// Children with the same injected labels may hold the same series, so the
// count falls back to the label walk, which counts a shared series once.
#[test]
fn children_sharing_labels_are_not_double_counted() {
    let (a, _) = counted(catalog().series_count(1));
    let (b, _) = counted(catalog().series_count(1));
    let reader = ParquetReader::builder()
        .source_labeled(a, [("job", "same")])
        .source_labeled(b, [("job", "same")])
        .build()
        .unwrap();

    assert_eq!(reader.total_series_count(), 1);
}

// One recording's tables share injected labels but not metric names, so
// they are summed without loading.
#[test]
fn same_labels_with_disjoint_names_do_not_load() {
    let (cpu, cpu_loads) = counted(catalog().series_count(3));
    let (mem, mem_loads) = counted(
        CompositionCatalog::new(1.0)
            .gauges(["memory_used"])
            .series_count(4),
    );
    let reader = ParquetReader::builder()
        .source_labeled(cpu, [("job", "a")])
        .source_labeled(mem, [("job", "a")])
        .build()
        .unwrap();

    assert_eq!(reader.total_series_count(), 7);
    assert_eq!(cpu_loads.load(Ordering::SeqCst), 0);
    assert_eq!(mem_loads.load(Ordering::SeqCst), 0);
}

// A name two tables share loads only those two; a third stays unloaded.
#[test]
fn a_shared_name_loads_only_its_holders() {
    let (a, a_loads) = counted(catalog().series_count(1));
    let (b, b_loads) = counted(catalog().series_count(1));
    let (mem, mem_loads) = counted(
        CompositionCatalog::new(1.0)
            .gauges(["memory_used"])
            .series_count(4),
    );
    let reader = ParquetReader::builder()
        .source_labeled(a, [("job", "a")])
        .source_labeled(b, [("job", "a")])
        .source_labeled(mem, [("job", "a")])
        .build()
        .unwrap();

    assert_eq!(reader.total_series_count(), 5);
    assert_eq!(a_loads.load(Ordering::SeqCst), 1);
    assert_eq!(b_loads.load(Ordering::SeqCst), 1);
    assert_eq!(mem_loads.load(Ordering::SeqCst), 0);
}

// Artifacts with different injected values cannot share a series, even
// under the same metric name.
#[test]
fn conflicting_labels_do_not_load_a_shared_name() {
    let (a, a_loads) = counted(catalog().series_count(1));
    let (b, b_loads) = counted(catalog().series_count(1));
    let reader = ParquetReader::builder()
        .source_labeled(a, [("artifact_id", "1")])
        .source_labeled(b, [("artifact_id", "2")])
        .build()
        .unwrap();

    assert_eq!(reader.total_series_count(), 2);
    assert_eq!(a_loads.load(Ordering::SeqCst), 0);
    assert_eq!(b_loads.load(Ordering::SeqCst), 0);
}

// Plain files with no injected labels holding the same series count it
// once, as the label walk always did.
#[test]
fn identical_plain_files_count_a_series_once() {
    let store = store();
    let reader = ParquetReader::builder()
        .source_labeled(&store, Labels::default())
        .source_labeled(&store, Labels::default())
        .build()
        .unwrap();

    assert_eq!(reader.total_series_count(), 1);
}

/// A lazy child hands out its source's streams, injected labels and all:
/// a rate over a composed lazy reader keeps one interval's worth per
/// series instead of materializing the source through `counters`.
#[test]
fn streams_pass_through_a_lazy_child() {
    let (source, loads) = counted(catalog());
    let reader = compose(source);
    let data = reader.data_source();
    let streams = data
        .counter_streams("cpu_usage", &Labels::default(), 0, u64::MAX)
        .unwrap();
    assert_eq!(streams.len(), 1);
    assert_eq!(
        streams[0].labels.inner.get("job").map(String::as_str),
        Some("a")
    );
    assert_eq!(
        streams[0].labels.inner.get("cpu").map(String::as_str),
        Some("0")
    );
    let samples: Vec<_> = streams.into_iter().next().unwrap().samples.collect();
    assert_eq!(
        samples.iter().map(|s| s.value).collect::<Vec<_>>(),
        vec![10, 20, 30, 40]
    );
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert!(data
        .counter_streams("memory_used", &Labels::default(), 0, u64::MAX)
        .is_none());
}

#[test]
fn only_the_named_child_loads() {
    let (cpu, cpu_loads) = counted(catalog());
    let (mem, mem_loads) = counted(
        CompositionCatalog::new(1.0)
            .gauges(["memory_used"])
            .time_range_ns(SEC, 4 * SEC),
    );
    let reader = ParquetReader::builder()
        .source_labeled(cpu, [("table", "cpu")])
        .source_labeled(mem, [("table", "mem")])
        .build()
        .unwrap();

    reader
        .query_range("rate(cpu_usage[2s])", 1.0, 4.0, 1.0)
        .unwrap();

    assert_eq!(cpu_loads.load(Ordering::SeqCst), 1);
    assert_eq!(mem_loads.load(Ordering::SeqCst), 0);
}
