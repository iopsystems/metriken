use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;

use super::metadata::GroupMetadata;
use super::windows::GroupWindows;
use crate::{GaugeGroupMetric, Metric, Value};
use metriken_core::Window;

enum Backing {
    Owned(Vec<AtomicI64>),
    External(&'static [AtomicI64]),
}

impl Backing {
    fn as_slice(&self) -> &[AtomicI64] {
        match self {
            Backing::Owned(v) => v,
            Backing::External(s) => s,
        }
    }
}

/// A group of gauges backed by a dense array with sparse metadata.
///
/// The value array is allocated lazily on first access and is always dense.
/// Metadata is stored sparsely.
///
/// An external backing store can be attached via
/// [`attach_external`](GaugeGroup::attach_external) before any values are
/// written.
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
    values: OnceLock<Backing>,
    metadata: GroupMetadata,
    windows: GroupWindows,
    entries: usize,
}

impl GaugeGroup {
    /// Create a new gauge group with the given number of entries.
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

    /// Attach an external slice as the backing store for gauge values.
    ///
    /// This must be called before any values are written. If the internal
    /// backing has already been initialized, this is a no-op.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the slice remains valid and properly
    /// aligned for the lifetime of this `GaugeGroup`.
    pub unsafe fn attach_external(&self, slice: &'static [AtomicI64]) {
        let _ = self.values.set(Backing::External(slice));
    }

    fn get_or_init(&self) -> &[AtomicI64] {
        self.values
            .get_or_init(|| {
                let mut v = Vec::with_capacity(self.entries);
                for _ in 0..self.entries {
                    v.push(AtomicI64::new(i64::MIN));
                }
                Backing::Owned(v)
            })
            .as_slice()
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
    /// If the entry has not been initialized yet, it is treated as `0` before
    /// the addition.
    ///
    /// Returns `false` if `idx` is out of bounds.
    #[inline]
    pub fn add(&self, idx: usize, value: i64) -> bool {
        if idx >= self.entries {
            return false;
        }
        let atomic = &self.get_or_init()[idx];
        let mut current = atomic.load(Ordering::Relaxed);
        loop {
            let new = if current == i64::MIN {
                value
            } else {
                current.wrapping_add(value)
            };
            match atomic.compare_exchange_weak(current, new, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Subtract `value` from the gauge at `idx`.
    ///
    /// If the entry has not been initialized yet, it is treated as `0` before
    /// the subtraction.
    ///
    /// Returns `false` if `idx` is out of bounds.
    #[inline]
    pub fn sub(&self, idx: usize, value: i64) -> bool {
        if idx >= self.entries {
            return false;
        }
        let atomic = &self.get_or_init()[idx];
        let mut current = atomic.load(Ordering::Relaxed);
        loop {
            let new = if current == i64::MIN {
                value.wrapping_neg()
            } else {
                current.wrapping_sub(value)
            };
            match atomic.compare_exchange_weak(current, new, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Set the gauge at `idx` to `value`.
    ///
    /// Returns `false` if `idx` is out of bounds.
    pub fn set(&self, idx: usize, value: i64) -> bool {
        if idx >= self.entries {
            return false;
        }
        self.get_or_init()[idx].store(value, Ordering::Relaxed);
        true
    }

    /// Load the current value of the gauge at `idx`.
    ///
    /// Returns `None` if `idx` is out of bounds, values haven't been
    /// initialized, or the entry has not been written to yet.
    pub fn value(&self, idx: usize) -> Option<i64> {
        if idx >= self.entries {
            return None;
        }
        self.values.get().and_then(|b| {
            let v = b.as_slice()[idx].load(Ordering::Relaxed);
            (v != i64::MIN).then_some(v)
        })
    }

    /// Load all gauge values as a snapshot.
    ///
    /// Returns `None` if the group hasn't been initialized yet.
    pub fn load(&self) -> Option<Vec<i64>> {
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

    /// Set the gauge at `idx` to `value` and record its acquisition window as a
    /// torn-safe pair.
    ///
    /// The value store and the window insert happen under the group's window
    /// write guard, so a concurrent
    /// [`load_with_window`](GaugeGroup::load_with_window) never observes a
    /// value from one call paired with a window from another.
    ///
    /// # Torn-safety caveat (base type coexists with lock-free mutators)
    /// This base group also exposes the lock-free `set`/`add`/`sub`, which
    /// bypass the window lock. A concurrent lock-free write to the same entry
    /// can pair a fresh value with a stale window. The **enforced** torn-safe
    /// path is the [`WindowedGaugeGroup`](crate::WindowedGaugeGroup) wrapper,
    /// which exposes no lock-free mutator; use it (not the base group) for
    /// windowed metrics.
    ///
    /// Returns `false` if `idx` is out of bounds.
    pub fn set_with_window(&self, idx: usize, value: i64, window: Window) -> bool {
        if idx >= self.entries {
            return false;
        }
        let slice = self.get_or_init();
        self.windows.with_write(|map| {
            slice[idx].store(value, Ordering::Relaxed);
            map.insert(idx, window);
        });
        true
    }

    /// Load the gauge at `idx` and its acquisition window as a torn-safe pair.
    /// The value is `None` if `idx` is out of bounds or the slot has never been
    /// written (still the `i64::MIN` sentinel).
    pub fn load_with_window(&self, idx: usize) -> (Option<i64>, Option<Window>) {
        if idx >= self.entries {
            return (None, None);
        }
        self.windows.with_read(|map| {
            let value = self.values.get().and_then(|b| {
                let v = b.as_slice()[idx].load(Ordering::Relaxed);
                (v != i64::MIN).then_some(v)
            });
            let window = map.and_then(|m| m.get(&idx).copied());
            (value, window)
        })
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

    fn load_window(&self, idx: usize) -> Option<Window> {
        self.windows.load(idx)
    }

    fn window_snapshot(&self) -> Vec<(usize, Window)> {
        self.windows.snapshot()
    }

    fn load_with_window(&self, idx: usize) -> (Option<i64>, Option<Window>) {
        GaugeGroup::load_with_window(self, idx)
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

    #[test]
    fn windows() {
        use metriken_core::Window;
        static GROUP: GaugeGroup = GaugeGroup::new(4);

        assert!(GROUP.load_window(0).is_none());
        GROUP.set_window(0, 1_000, 3_000);
        assert_eq!(GROUP.load_window(0), Some(Window::new(1_000, 3_000)));

        GROUP.set_window(9, 1, 2); // out of bounds ignored
        assert!(GROUP.load_window(9).is_none());

        assert_eq!(
            GROUP.window_snapshot(),
            vec![(0, Window::new(1_000, 3_000))]
        );
    }

    #[test]
    fn attach_external_backing() {
        static EXTERNAL: [AtomicI64; 3] =
            [AtomicI64::new(100), AtomicI64::new(-50), AtomicI64::new(0)];
        static GROUP: GaugeGroup = GaugeGroup::new(3);

        unsafe {
            GROUP.attach_external(&EXTERNAL);
        }

        assert_eq!(GROUP.value(0), Some(100));
        assert_eq!(GROUP.value(1), Some(-50));

        GROUP.set(2, 42);
        assert_eq!(GROUP.value(2), Some(42));
        assert_eq!(EXTERNAL[2].load(Ordering::Relaxed), 42);
    }

    #[test]
    fn set_with_window_round_trip() {
        use metriken_core::Window;
        static GROUP: GaugeGroup = GaugeGroup::new(4);

        GROUP.set_with_window(1, -12, Window::new(10, 20));
        assert_eq!(
            GROUP.load_with_window(1),
            (Some(-12), Some(Window::new(10, 20)))
        );
        assert_eq!(GROUP.value(1), Some(-12));
        assert_eq!(GROUP.load_with_window(9), (None, None));

        use crate::GaugeGroupMetric;
        let m: &dyn GaugeGroupMetric = &GROUP;
        assert_eq!(
            m.load_with_window(1),
            (Some(-12), Some(Window::new(10, 20)))
        );
    }

    #[test]
    fn load_with_window_unset_does_not_allocate() {
        static GROUP: GaugeGroup = GaugeGroup::new(2);
        GROUP.set(0, 7);
        assert_eq!(GROUP.load_with_window(0), (Some(7), None));
        assert!(GROUP.load_window(0).is_none());
        assert!(
            GROUP.window_snapshot().is_empty(),
            "no window write must not allocate"
        );
    }
}
