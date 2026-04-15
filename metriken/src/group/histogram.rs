use std::collections::HashMap;
use std::sync::OnceLock;

use histogram::{Config, Error, Histogram};
use parking_lot::RwLock;

use super::metadata::GroupMetadata;
use crate::{HistogramGroupMetric, Metric, Value};

/// A group of histograms backed by a dense array with sparse metadata.
///
/// All histograms in the group share the same configuration (grouping power
/// and max value power). The array is allocated lazily on first access.
///
/// # Example
/// ```
/// use metriken::{metric, HistogramGroup};
///
/// #[metric(name = "latency")]
/// static LATENCY: HistogramGroup = HistogramGroup::new(4, 7, 64);
///
/// // Index 0 = reads, 1 = writes, etc.
/// let _ = LATENCY.increment(0, 1200);
/// let _ = LATENCY.increment(1, 500);
/// ```
pub struct HistogramGroup {
    inner: OnceLock<RwLock<Vec<histogram::AtomicHistogram>>>,
    metadata: GroupMetadata,
    config: Config,
    entries: usize,
}

impl HistogramGroup {
    /// Create a new histogram group.
    ///
    /// All histograms share the same `grouping_power` and `max_value_power`
    /// configuration.
    ///
    /// # Panics
    /// Panics if the histogram configuration is invalid. See
    /// [`histogram::Config::new`] for constraints.
    pub const fn new(entries: usize, grouping_power: u8, max_value_power: u8) -> Self {
        let config = match Config::new(grouping_power, max_value_power) {
            Ok(c) => c,
            Err(_) => panic!("invalid histogram config"),
        };

        Self {
            inner: OnceLock::new(),
            metadata: GroupMetadata::new(),
            config,
            entries,
        }
    }

    /// Return the number of entries in this group.
    pub fn entries(&self) -> usize {
        self.entries
    }

    /// Return the histogram configuration shared by all entries.
    pub fn config(&self) -> Config {
        self.config
    }

    fn get_or_init(&self) -> &RwLock<Vec<histogram::AtomicHistogram>> {
        self.inner.get_or_init(|| {
            let mut v = Vec::with_capacity(self.entries);
            for _ in 0..self.entries {
                v.push(histogram::AtomicHistogram::with_config(&self.config));
            }
            RwLock::new(v)
        })
    }

    /// Record a value in the histogram at `idx`.
    ///
    /// Returns `Err` if the value is outside the histogram's range.
    /// Returns `Ok(false)` if `idx` is out of bounds.
    pub fn increment(&self, idx: usize, value: u64) -> Result<bool, Error> {
        if idx >= self.entries {
            return Ok(false);
        }
        let inner = self.get_or_init().read();
        inner[idx].increment(value)?;
        Ok(true)
    }

    /// Load a snapshot of the histogram at `idx`.
    ///
    /// Returns `None` if `idx` is out of bounds or the group hasn't been
    /// initialized.
    pub fn load(&self, idx: usize) -> Option<Histogram> {
        if idx >= self.entries {
            return None;
        }
        self.inner.get().map(|v| v.read()[idx].load())
    }

    /// Load snapshots of all histograms.
    ///
    /// Returns `None` if the group hasn't been initialized.
    pub fn load_all(&self) -> Option<Vec<Histogram>> {
        self.inner
            .get()
            .map(|v| v.read().iter().map(|h| h.load()).collect())
    }

    /// Set metadata for the entry at `idx`.
    pub fn set_metadata(&self, idx: usize, metadata: HashMap<String, String>) {
        if idx < self.entries {
            self.metadata.insert(idx, metadata);
        }
    }

    /// Set a single metadata key-value pair for the entry at `idx`.
    pub fn insert_metadata(&self, idx: usize, key: String, value: String) {
        if idx < self.entries {
            self.metadata.insert_kv(idx, key, value);
        }
    }

    /// Load metadata for the entry at `idx`.
    pub fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
        self.metadata.load(idx)
    }

    /// Remove metadata for the entry at `idx`.
    pub fn clear_metadata(&self, idx: usize) {
        self.metadata.remove(idx);
    }

    /// Snapshot all metadata.
    pub fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
        self.metadata.snapshot()
    }
}

impl HistogramGroupMetric for HistogramGroup {
    fn entries(&self) -> usize {
        self.entries
    }

    fn config(&self) -> Config {
        self.config
    }

    fn load_histogram(&self, idx: usize) -> Option<Histogram> {
        self.load(idx)
    }

    fn load_all_histograms(&self) -> Option<Vec<Histogram>> {
        self.load_all()
    }

    fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
        self.metadata.load(idx)
    }

    fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
        self.metadata.snapshot()
    }
}

impl Metric for HistogramGroup {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::HistogramGroup(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_operations() {
        static GROUP: HistogramGroup = HistogramGroup::new(4, 7, 64);

        assert!(GROUP.load(0).is_none()); // not initialized yet
        assert!(GROUP.increment(0, 100).unwrap());
        assert!(GROUP.load(0).is_some());

        // out of bounds
        assert!(!GROUP.increment(4, 100).unwrap());
        assert!(GROUP.load(4).is_none());
    }

    #[test]
    fn load_all() {
        static GROUP: HistogramGroup = HistogramGroup::new(2, 7, 64);

        let _ = GROUP.increment(0, 100);
        let _ = GROUP.increment(1, 200);

        let all = GROUP.load_all().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn metadata() {
        static GROUP: HistogramGroup = HistogramGroup::new(4, 7, 64);

        GROUP.insert_metadata(0, "op".into(), "read".into());
        let meta = GROUP.load_metadata(0).unwrap();
        assert_eq!(meta.get("op").unwrap(), "read");

        assert!(GROUP.load_metadata(1).is_none());
    }

    #[test]
    fn config() {
        static GROUP: HistogramGroup = HistogramGroup::new(2, 7, 64);
        let config = GROUP.config();
        assert_eq!(config, Config::new(7, 64).unwrap());
    }
}
