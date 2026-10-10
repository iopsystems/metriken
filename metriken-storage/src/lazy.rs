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

pub struct LazySource {
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

    /// Through the loaded source, so a lazy child over a segmented reader
    /// keeps its per-series streams rather than falling to the default,
    /// which would materialize every series through `counters`.
    fn counter_streams<'s>(
        &'s self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<crate::CounterStream<'s>>> {
        if !self.catalog.counters.contains(name) {
            return None;
        }
        self.loaded()?
            .counter_streams(name, filter, start_ns, end_ns)
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
