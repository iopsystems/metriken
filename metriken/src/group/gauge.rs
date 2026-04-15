use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;

use parking_lot::RwLock;

use super::metadata::GroupMetadata;
use crate::{GaugeGroupMetric, Metric, Value};

/// A group of gauges backed by a dense array with sparse metadata.
///
/// The value array is allocated lazily on first access and is always dense.
/// Metadata is stored sparsely.
///
/// # Example
/// ```
/// use metriken::{metric, GaugeGroup};
///
/// const NUM_CPUS: usize = 8;
///
/// #[metric(name = "cpu_frequency")]
/// static CPU_FREQ: GaugeGroup = GaugeGroup::new(NUM_CPUS);
///
/// CPU_FREQ.set(0, 3600);
/// CPU_FREQ.set(1, 2400);
///
/// assert_eq!(CPU_FREQ.value(0), Some(3600));
/// ```
pub struct GaugeGroup {
    values: OnceLock<RwLock<Vec<AtomicI64>>>,
    metadata: GroupMetadata,
    entries: usize,
}

impl GaugeGroup {
    /// Create a new gauge group with the given number of entries.
    pub const fn new(entries: usize) -> Self {
        Self {
            values: OnceLock::new(),
            metadata: GroupMetadata::new(),
            entries,
        }
    }

    /// Return the number of entries in this group.
    pub fn entries(&self) -> usize {
        self.entries
    }

    fn get_or_init(&self) -> &RwLock<Vec<AtomicI64>> {
        self.values.get_or_init(|| {
            let mut v = Vec::with_capacity(self.entries);
            for _ in 0..self.entries {
                v.push(AtomicI64::new(0));
            }
            RwLock::new(v)
        })
    }

    /// Increment the gauge at `idx` by 1.
    ///
    /// Returns `false` if `idx` is out of bounds.
    #[inline]
    pub fn increment(&self, idx: usize) -> bool {
        self.add(idx, 1)
    }

    /// Decrement the gauge at `idx` by 1.
    ///
    /// Returns `false` if `idx` is out of bounds.
    #[inline]
    pub fn decrement(&self, idx: usize) -> bool {
        self.sub(idx, 1)
    }

    /// Add `value` to the gauge at `idx`.
    ///
    /// Returns `false` if `idx` is out of bounds.
    #[inline]
    pub fn add(&self, idx: usize, value: i64) -> bool {
        if idx >= self.entries {
            return false;
        }
        let inner = self.get_or_init().read();
        inner[idx].fetch_add(value, Ordering::Relaxed);
        true
    }

    /// Subtract `value` from the gauge at `idx`.
    ///
    /// Returns `false` if `idx` is out of bounds.
    #[inline]
    pub fn sub(&self, idx: usize, value: i64) -> bool {
        if idx >= self.entries {
            return false;
        }
        let inner = self.get_or_init().read();
        inner[idx].fetch_sub(value, Ordering::Relaxed);
        true
    }

    /// Set the gauge at `idx` to `value`.
    ///
    /// Returns `false` if `idx` is out of bounds.
    pub fn set(&self, idx: usize, value: i64) -> bool {
        if idx >= self.entries {
            return false;
        }
        let inner = self.get_or_init().read();
        inner[idx].store(value, Ordering::Relaxed);
        true
    }

    /// Load the current value of the gauge at `idx`.
    ///
    /// Returns `None` if `idx` is out of bounds or values haven't been
    /// initialized.
    pub fn value(&self, idx: usize) -> Option<i64> {
        if idx >= self.entries {
            return None;
        }
        self.values
            .get()
            .map(|v| v.read()[idx].load(Ordering::Relaxed))
    }

    /// Load all gauge values as a snapshot.
    ///
    /// Returns `None` if the group hasn't been initialized yet.
    pub fn load(&self) -> Option<Vec<i64>> {
        self.values
            .get()
            .map(|v| v.read().iter().map(|a| a.load(Ordering::Relaxed)).collect())
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

impl GaugeGroupMetric for GaugeGroup {
    fn entries(&self) -> usize {
        self.entries
    }

    fn gauge_value(&self, idx: usize) -> Option<i64> {
        self.value(idx)
    }

    fn load_gauges(&self) -> Option<Vec<i64>> {
        self.load()
    }

    fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
        self.metadata.load(idx)
    }

    fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
        self.metadata.snapshot()
    }
}

impl Metric for GaugeGroup {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::GaugeGroup(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_operations() {
        static GROUP: GaugeGroup = GaugeGroup::new(4);

        assert_eq!(GROUP.value(0), None);
        GROUP.set(0, 100);
        assert_eq!(GROUP.value(0), Some(100));
        GROUP.increment(0);
        assert_eq!(GROUP.value(0), Some(101));
        GROUP.decrement(0);
        assert_eq!(GROUP.value(0), Some(100));
        GROUP.add(1, 50);
        GROUP.sub(1, 10);
        assert_eq!(GROUP.value(1), Some(40));

        // out of bounds
        assert!(!GROUP.set(4, 0));
        assert_eq!(GROUP.value(4), None);
    }

    #[test]
    fn metadata() {
        static GROUP: GaugeGroup = GaugeGroup::new(4);

        GROUP.insert_metadata(0, "cpu".into(), "0".into());
        let meta = GROUP.load_metadata(0).unwrap();
        assert_eq!(meta.get("cpu").unwrap(), "0");

        assert!(GROUP.load_metadata(1).is_none());
    }

    #[test]
    fn load_snapshot() {
        static GROUP: GaugeGroup = GaugeGroup::new(3);

        GROUP.set(0, 10);
        GROUP.set(1, -20);
        GROUP.set(2, 30);

        let snap = GROUP.load().unwrap();
        assert_eq!(snap, vec![10, -20, 30]);
    }
}
