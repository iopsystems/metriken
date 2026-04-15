use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use parking_lot::RwLock;

use super::metadata::GroupMetadata;
use crate::{CounterGroupMetric, Metric, Value};

/// A group of counters backed by a dense array with sparse metadata.
///
/// The value array is allocated lazily on first access and is always dense
/// (every index from 0..entries has a slot). Metadata is stored sparsely —
/// only indices with explicitly attached metadata consume memory for it.
///
/// This is the right choice for per-CPU, per-cgroup, or per-operation
/// counters where the index is meaningful (e.g., CPU ID, cgroup index,
/// enum variant).
///
/// # Example
/// ```
/// use metriken::{metric, CounterGroup};
///
/// const NUM_OPS: usize = 4;
///
/// #[metric(name = "requests")]
/// static REQUESTS: CounterGroup = CounterGroup::new(NUM_OPS);
///
/// // Index 0 = reads, 1 = writes, etc.
/// REQUESTS.increment(0);
/// REQUESTS.add(1, 5);
///
/// assert_eq!(REQUESTS.value(0), Some(1));
/// assert_eq!(REQUESTS.value(1), Some(5));
/// ```
pub struct CounterGroup {
    values: OnceLock<RwLock<Vec<AtomicU64>>>,
    metadata: GroupMetadata,
    entries: usize,
}

impl CounterGroup {
    /// Create a new counter group with the given number of entries.
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

    fn get_or_init(&self) -> &RwLock<Vec<AtomicU64>> {
        self.values.get_or_init(|| {
            let mut v = Vec::with_capacity(self.entries);
            for _ in 0..self.entries {
                v.push(AtomicU64::new(0));
            }
            RwLock::new(v)
        })
    }

    /// Increment the counter at `idx` by 1.
    ///
    /// Returns `false` if `idx` is out of bounds.
    #[inline]
    pub fn increment(&self, idx: usize) -> bool {
        self.add(idx, 1)
    }

    /// Add `value` to the counter at `idx`.
    ///
    /// Returns `false` if `idx` is out of bounds.
    #[inline]
    pub fn add(&self, idx: usize, value: u64) -> bool {
        if idx >= self.entries {
            return false;
        }
        let inner = self.get_or_init().read();
        inner[idx].fetch_add(value, Ordering::Relaxed);
        true
    }

    /// Set the counter at `idx` to `value`.
    ///
    /// Returns `false` if `idx` is out of bounds.
    pub fn set(&self, idx: usize, value: u64) -> bool {
        if idx >= self.entries {
            return false;
        }
        let inner = self.get_or_init().read();
        inner[idx].store(value, Ordering::Relaxed);
        true
    }

    /// Load the current value of the counter at `idx`.
    ///
    /// Returns `None` if `idx` is out of bounds or values haven't been
    /// initialized.
    pub fn value(&self, idx: usize) -> Option<u64> {
        if idx >= self.entries {
            return None;
        }
        self.values
            .get()
            .map(|v| v.read()[idx].load(Ordering::Relaxed))
    }

    /// Load all counter values as a snapshot.
    ///
    /// Returns `None` if the group hasn't been initialized yet.
    pub fn load(&self) -> Option<Vec<u64>> {
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

impl CounterGroupMetric for CounterGroup {
    fn entries(&self) -> usize {
        self.entries
    }

    fn counter_value(&self, idx: usize) -> Option<u64> {
        self.value(idx)
    }

    fn load_counters(&self) -> Option<Vec<u64>> {
        self.load()
    }

    fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
        self.metadata.load(idx)
    }

    fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
        self.metadata.snapshot()
    }
}

impl Metric for CounterGroup {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::CounterGroup(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_operations() {
        static GROUP: CounterGroup = CounterGroup::new(4);

        assert_eq!(GROUP.value(0), None); // not yet initialized
        GROUP.increment(0);
        assert_eq!(GROUP.value(0), Some(1));
        GROUP.add(1, 10);
        assert_eq!(GROUP.value(1), Some(10));

        // out of bounds
        assert!(!GROUP.increment(4));
        assert_eq!(GROUP.value(4), None);
    }

    #[test]
    fn metadata() {
        static GROUP: CounterGroup = CounterGroup::new(4);

        GROUP.insert_metadata(0, "cpu".into(), "0".into());
        GROUP.insert_metadata(0, "node".into(), "numa0".into());

        let meta = GROUP.load_metadata(0).unwrap();
        assert_eq!(meta.get("cpu").unwrap(), "0");
        assert_eq!(meta.get("node").unwrap(), "numa0");

        // index without metadata
        assert!(GROUP.load_metadata(1).is_none());

        GROUP.clear_metadata(0);
        assert!(GROUP.load_metadata(0).is_none());
    }

    #[test]
    fn load_snapshot() {
        static GROUP: CounterGroup = CounterGroup::new(3);

        GROUP.set(0, 10);
        GROUP.set(1, 20);
        GROUP.set(2, 30);

        let snap = GROUP.load().unwrap();
        assert_eq!(snap, vec![10, 20, 30]);
    }

    #[test]
    fn metriken_trait() {
        use crate::Metric;

        static GROUP: CounterGroup = CounterGroup::new(2);
        GROUP.increment(0);

        let value = Metric::value(&GROUP);
        assert!(matches!(value, Some(Value::CounterGroup(_))));
    }
}
