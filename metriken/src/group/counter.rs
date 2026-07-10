use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use super::metadata::GroupMetadata;
use super::windows::GroupWindows;
use crate::{CounterGroupMetric, Metric, Value};
use metriken_core::Window;

enum Backing {
    Owned(Vec<AtomicU64>),
    External(&'static [AtomicU64]),
}

impl Backing {
    fn as_slice(&self) -> &[AtomicU64] {
        match self {
            Backing::Owned(v) => v,
            Backing::External(s) => s,
        }
    }
}

/// A group of counters backed by a dense array with sparse metadata.
///
/// The value array is allocated lazily on first access and is always dense
/// (every index from 0..entries has a slot). Metadata is stored sparsely —
/// only indices with explicitly attached metadata consume memory for it.
///
/// An external backing store (e.g., a BPF mmap region) can be attached via
/// [`attach_external`](CounterGroup::attach_external) before any values are
/// written. This enables zero-copy reads from memory-mapped regions.
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
    values: OnceLock<Backing>,
    metadata: GroupMetadata,
    windows: GroupWindows,
    entries: usize,
}

impl CounterGroup {
    /// Create a new counter group with the given number of entries.
    pub const fn new(entries: usize) -> Self {
        Self {
            values: OnceLock::new(),
            metadata: GroupMetadata::new(),
            windows: GroupWindows::new(),
            entries,
        }
    }

    /// Return the number of entries in this group.
    pub fn entries(&self) -> usize {
        self.entries
    }

    /// Attach an external slice as the backing store for counter values.
    ///
    /// This must be called before any values are written (via `increment`,
    /// `add`, or `set`). If the internal backing has already been initialized,
    /// this is a no-op.
    ///
    /// The slice must have at least `entries` elements. This is intended for
    /// memory-mapped regions (e.g., BPF maps) that live for the process
    /// lifetime.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the slice remains valid and properly
    /// aligned for the lifetime of this `CounterGroup` (typically `'static`
    /// for BPF map mmaps).
    pub unsafe fn attach_external(&self, slice: &'static [AtomicU64]) {
        let _ = self.values.set(Backing::External(slice));
    }

    fn get_or_init(&self) -> &[AtomicU64] {
        self.values
            .get_or_init(|| {
                let mut v = Vec::with_capacity(self.entries);
                for _ in 0..self.entries {
                    v.push(AtomicU64::new(0));
                }
                Backing::Owned(v)
            })
            .as_slice()
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
        self.get_or_init()[idx].fetch_add(value, Ordering::Relaxed);
        true
    }

    /// Set the counter at `idx` to `value`.
    ///
    /// Returns `false` if `idx` is out of bounds.
    pub fn set(&self, idx: usize, value: u64) -> bool {
        if idx >= self.entries {
            return false;
        }
        self.get_or_init()[idx].store(value, Ordering::Relaxed);
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
            .map(|b| b.as_slice()[idx].load(Ordering::Relaxed))
    }

    /// Load all counter values as a snapshot.
    ///
    /// Returns `None` if the group hasn't been initialized yet.
    pub fn load(&self) -> Option<Vec<u64>> {
        self.values.get().map(|b| {
            b.as_slice()
                .iter()
                .map(|a| a.load(Ordering::Relaxed))
                .collect()
        })
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

    /// Record the acquisition window for the entry at `idx`.
    pub fn set_window(&self, idx: usize, begin_ns: u64, end_ns: u64) {
        if idx < self.entries {
            self.windows.insert(idx, Window::new(begin_ns, end_ns));
        }
    }

    /// Load the acquisition window recorded for the entry at `idx`.
    pub fn load_window(&self, idx: usize) -> Option<Window> {
        self.windows.load(idx)
    }

    /// Snapshot all per-entry acquisition windows.
    pub fn window_snapshot(&self) -> Vec<(usize, Window)> {
        self.windows.snapshot()
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

    fn load_window(&self, idx: usize) -> Option<Window> {
        self.windows.load(idx)
    }

    fn window_snapshot(&self) -> Vec<(usize, Window)> {
        self.windows.snapshot()
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

    #[test]
    fn attach_external_backing() {
        static EXTERNAL: [AtomicU64; 4] = [
            AtomicU64::new(100),
            AtomicU64::new(200),
            AtomicU64::new(0),
            AtomicU64::new(400),
        ];
        static GROUP: CounterGroup = CounterGroup::new(4);

        unsafe {
            GROUP.attach_external(&EXTERNAL);
        }

        assert_eq!(GROUP.value(0), Some(100));
        assert_eq!(GROUP.value(1), Some(200));
        assert_eq!(GROUP.value(3), Some(400));

        // Writes go to the external backing
        GROUP.add(0, 1);
        assert_eq!(GROUP.value(0), Some(101));
        assert_eq!(EXTERNAL[0].load(Ordering::Relaxed), 101);
    }

    #[test]
    fn windows() {
        use metriken_core::Window;
        static GROUP: CounterGroup = CounterGroup::new(4);

        assert!(GROUP.load_window(0).is_none());
        GROUP.set_window(0, 1_000, 3_000);
        assert_eq!(GROUP.load_window(0), Some(Window::new(1_000, 3_000)));

        GROUP.set_window(9, 1, 2); // out of bounds ignored
        assert!(GROUP.load_window(9).is_none());

        assert_eq!(GROUP.window_snapshot(), vec![(0, Window::new(1_000, 3_000))]);
    }

    #[test]
    fn attach_external_after_init_is_noop() {
        static GROUP: CounterGroup = CounterGroup::new(2);
        static EXTERNAL: [AtomicU64; 2] = [AtomicU64::new(99), AtomicU64::new(99)];

        // Initialize internal backing first
        GROUP.increment(0);
        assert_eq!(GROUP.value(0), Some(1));

        // attach_external is a no-op since already initialized
        unsafe {
            GROUP.attach_external(&EXTERNAL);
        }

        // Still using internal backing
        assert_eq!(GROUP.value(0), Some(1));
    }
}
