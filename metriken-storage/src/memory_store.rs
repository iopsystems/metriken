use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::memory::Memory;

use crate::types::{Counters, Gauges};
use crate::DataSource;

pub struct MemoryStoreInner {
    pub memory: RwLock<Memory>,
    pub metadata: RwLock<HashMap<String, String>>,
    pub filename: RwLock<Option<String>>,
}

impl DataSource for MemoryStoreInner {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Counters> {
        // Nothing rounds a sample's timestamp, so `raw` is a no-op here.
        self.memory
            .read()
            .unwrap()
            .counters(name, filter, start_ns, end_ns)
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        self.memory
            .read()
            .unwrap()
            .gauges(name, filter, start_ns, end_ns)
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        self.memory
            .read()
            .unwrap()
            .histogram_stream(name, filter, start_ns, end_ns)
    }

    fn interval(&self) -> f64 {
        self.memory.read().unwrap().interval()
    }

    fn time_range(&self) -> Option<(u64, u64)> {
        self.memory.read().unwrap().time_range()
    }

    fn counter_names(&self) -> Vec<String> {
        self.memory.read().unwrap().counter_names()
    }

    fn gauge_names(&self) -> Vec<String> {
        self.memory.read().unwrap().gauge_names()
    }

    fn histogram_names(&self) -> Vec<String> {
        self.memory.read().unwrap().histogram_names()
    }

    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.memory.read().unwrap().counter_labels(name)
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.memory.read().unwrap().gauge_labels(name)
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.memory.read().unwrap().histogram_labels(name)
    }

    fn file_metadata(&self) -> HashMap<String, String> {
        self.metadata.read().unwrap().clone()
    }

    fn metadata_get(&self, key: &str) -> Option<String> {
        self.metadata.read().unwrap().get(key).cloned()
    }

    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        self.memory.read().unwrap().column_map()
    }

    fn sample_timestamps(&self) -> Vec<u64> {
        self.memory.read().unwrap().sample_timestamps()
    }
}

// ─── MetricsSource on MemoryStore ─────────────────────────────────────────────
