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

    /// Run `f` with a borrowed view of the metadata for the entry at `idx`,
    /// without cloning the underlying map.
    ///
    /// The group's metadata read lock is held for the duration of `f` —
    /// callers must not block, await, or re-enter this group's methods
    /// (e.g. `set_metadata`, `load_metadata`) inside the closure.
    pub fn with_metadata<R>(
        &self,
        idx: usize,
        f: impl FnOnce(Option<&HashMap<String, String>>) -> R,
    ) -> R {
        self.metadata.with(idx, f)
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

    fn with_metadata(&self, idx: usize, f: &mut dyn FnMut(Option<&HashMap<String, String>>)) {
        self.metadata.with(idx, f);
    }

    fn for_each_metadata(&self, f: &mut dyn FnMut(usize, &HashMap<String, String>)) {
        self.metadata.for_each(f);
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
    fn with_metadata_borrows_without_cloning() {
        static GROUP: HistogramGroup = HistogramGroup::new(4, 7, 64);

        GROUP.insert_metadata(0, "op".into(), "read".into());

        // Content matches `load_metadata` for a populated index.
        let owned = GROUP.load_metadata(0).unwrap();
        GROUP.with_metadata(0, |m| {
            assert_eq!(m.unwrap(), &owned);
        });

        // `None` for an unpopulated index.
        GROUP.with_metadata(1, |m| {
            assert!(m.is_none());
        });

        // A value returned out of the closure works (proves the `R` generic).
        let op = GROUP.with_metadata(0, |m| m.and_then(|m| m.get("op").cloned()));
        assert_eq!(op.as_deref(), Some("read"));
    }

    #[test]
    fn trait_with_metadata_matches_load_metadata() {
        // The object-safe trait method (reachable through `&dyn
        // HistogramGroupMetric`, distinct from the inherent generic
        // `with_metadata<R>` above since inherent methods shadow same-named
        // trait methods on the concrete type).
        static GROUP: HistogramGroup = HistogramGroup::new(4, 7, 64);
        GROUP.insert_metadata(0, "op".into(), "read".into());

        let dyn_group: &dyn HistogramGroupMetric = &GROUP;

        let expected = dyn_group.load_metadata(0);
        let mut seen: Option<HashMap<String, String>> = None;
        dyn_group.with_metadata(0, &mut |m| seen = m.cloned());
        assert_eq!(seen, expected);

        let mut called = false;
        let mut seen_absent: Option<HashMap<String, String>> = None;
        dyn_group.with_metadata(1, &mut |m| {
            called = true;
            seen_absent = m.cloned();
        });
        assert!(called);
        assert!(seen_absent.is_none());
    }

    #[test]
    fn trait_for_each_metadata_matches_metadata_snapshot() {
        static GROUP: HistogramGroup = HistogramGroup::new(4, 7, 64);
        GROUP.insert_metadata(0, "op".into(), "read".into());
        GROUP.insert_metadata(2, "op".into(), "write".into());

        let dyn_group: &dyn HistogramGroupMetric = &GROUP;

        let mut seen: Vec<(usize, HashMap<String, String>)> = Vec::new();
        dyn_group.for_each_metadata(&mut |idx, m| seen.push((idx, m.clone())));
        seen.sort_by_key(|(idx, _)| *idx);

        let mut expected = dyn_group.metadata_snapshot();
        expected.sort_by_key(|(idx, _)| *idx);

        assert_eq!(seen.len(), expected.len());
        assert_eq!(seen, expected);
    }

    #[test]
    fn config() {
        static GROUP: HistogramGroup = HistogramGroup::new(2, 7, 64);
        let config = GROUP.config();
        assert_eq!(config, Config::new(7, 64).unwrap());
    }
}
