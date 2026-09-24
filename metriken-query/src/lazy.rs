use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::sync::{Arc, OnceLock};

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::parquet::{ColDesc, CompositionSource};
use crate::types::{Counters, Gauges};
use crate::DataSource;

/// Metadata a lazy composition child can provide before loading.
#[derive(Clone, Debug)]
pub struct CompositionCatalog {
    counters: BTreeSet<String>,
    gauges: BTreeSet<String>,
    histograms: BTreeSet<String>,
    time_range_ns: Option<(u64, u64)>,
    interval_s: f64,
    metadata: HashMap<String, String>,
    series_count: Option<usize>,
}

impl CompositionCatalog {
    /// An empty catalog with the given sampling interval in seconds.
    pub fn new(interval_s: f64) -> Self {
        Self {
            counters: BTreeSet::new(),
            gauges: BTreeSet::new(),
            histograms: BTreeSet::new(),
            time_range_ns: None,
            interval_s,
            metadata: HashMap::new(),
            series_count: None,
        }
    }

    /// Counter metric names the source holds.
    pub fn counters<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.counters.extend(names.into_iter().map(Into::into));
        self
    }

    /// Gauge metric names the source holds.
    pub fn gauges<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.gauges.extend(names.into_iter().map(Into::into));
        self
    }

    /// Histogram metric names the source holds.
    pub fn histograms<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.histograms.extend(names.into_iter().map(Into::into));
        self
    }

    /// Set the source's time extent in nanoseconds.
    pub fn time_range_ns(mut self, start_ns: u64, end_ns: u64) -> Self {
        self.time_range_ns = Some((start_ns, end_ns));
        self
    }

    /// Set file-level metadata.
    pub fn metadata(mut self, metadata: HashMap<String, String>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Number of distinct series the source holds, across all metric types.
    /// Unset, a series count loads the source and counts its label sets.
    pub fn series_count(mut self, count: usize) -> Self {
        self.series_count = Some(count);
        self
    }
}

type Loader = dyn Fn() -> Result<Option<CompositionSource>, Box<dyn Error>> + Send + Sync;

pub(crate) struct LazySource {
    catalog: CompositionCatalog,
    loader: Box<Loader>,
    loaded: OnceLock<Option<Arc<dyn DataSource>>>,
}

impl LazySource {
    fn loaded(&self) -> Option<&Arc<dyn DataSource>> {
        self.loaded
            .get_or_init(|| match (self.loader)() {
                Ok(source) => source.map(|s| s.0),
                Err(e) => {
                    tracing::warn!("lazy composition source failed to load, skipping it: {e}");
                    None
                }
            })
            .as_ref()
    }
}

impl DataSource for LazySource {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Counters> {
        if !self.catalog.counters.contains(name) {
            return None;
        }
        self.loaded()?.counters(name, filter, start_ns, end_ns)
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        if !self.catalog.gauges.contains(name) {
            return None;
        }
        self.loaded()?.gauges(name, filter, start_ns, end_ns)
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        if !self.catalog.histograms.contains(name) {
            return None;
        }
        self.loaded()?
            .histogram_stream(name, filter, start_ns, end_ns)
    }

    fn interval(&self) -> f64 {
        self.catalog.interval_s
    }

    fn time_range(&self) -> Option<(u64, u64)> {
        self.catalog.time_range_ns
    }

    fn counter_names(&self) -> Vec<String> {
        self.catalog.counters.iter().cloned().collect()
    }

    fn gauge_names(&self) -> Vec<String> {
        self.catalog.gauges.iter().cloned().collect()
    }

    fn histogram_names(&self) -> Vec<String> {
        self.catalog.histograms.iter().cloned().collect()
    }

    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        if !self.catalog.counters.contains(name) {
            return Vec::new();
        }
        self.loaded()
            .map(|s| s.counter_labels(name))
            .unwrap_or_default()
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        if !self.catalog.gauges.contains(name) {
            return Vec::new();
        }
        self.loaded()
            .map(|s| s.gauge_labels(name))
            .unwrap_or_default()
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        if !self.catalog.histograms.contains(name) {
            return Vec::new();
        }
        self.loaded()
            .map(|s| s.histogram_labels(name))
            .unwrap_or_default()
    }

    fn file_metadata(&self) -> HashMap<String, String> {
        self.catalog.metadata.clone()
    }

    fn metadata_get(&self, key: &str) -> Option<String> {
        self.catalog.metadata.get(key).cloned()
    }

    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        self.loaded().map(|s| s.column_map()).unwrap_or_default()
    }

    fn sample_timestamps(&self) -> Vec<u64> {
        self.loaded()
            .map(|s| s.sample_timestamps())
            .unwrap_or_default()
    }

    fn columns_desc(&self) -> Vec<ColDesc> {
        self.loaded().map(|s| s.columns_desc()).unwrap_or_default()
    }

    fn series_count(&self) -> usize {
        self.catalog
            .series_count
            .unwrap_or_else(|| crate::label_walk_series_count(self))
    }
}

impl CompositionSource {
    /// A composition child that loads on its first matching metric or label
    /// lookup. The loader runs at most once; errors are logged and treated as
    /// an empty source.
    pub fn lazy<F>(catalog: CompositionCatalog, loader: F) -> Self
    where
        F: Fn() -> Result<Option<CompositionSource>, Box<dyn Error>> + Send + Sync + 'static,
    {
        CompositionSource(Arc::new(LazySource {
            catalog,
            loader: Box::new(loader),
            loaded: OnceLock::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{MemoryStore, MetricsSource, ParquetReader, QueryError};

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
                    matches!(result, Err(QueryError::MetricNotFound(_))),
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
}
