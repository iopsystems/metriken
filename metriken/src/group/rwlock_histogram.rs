use std::collections::HashMap;
use std::sync::OnceLock;

use histogram::{Config, Error, Histogram};
use parking_lot::RwLock;

use super::metadata::GroupMetadata;
use crate::{HistogramGroupMetric, Metric, Value};

/// A group of counting histograms whose entries are written as whole bucket
/// arrays, the group form of [`RwLockCounterHistogram`].
///
/// Each entry is stored under one `RwLock` with the others, so
/// [`update_from`] replaces an entry's buckets and a concurrent [`load`]
/// sees either the old buckets or the new ones. It suits histograms
/// maintained elsewhere and copied in, such as buckets summed from a BPF
/// map. An entry that was never written, or was [`clear`]ed, loads as
/// `None`, so an exposition lists only the written entries.
///
/// All entries share one configuration (grouping power and max value
/// power), and storage is allocated on the first `update_from` or
/// `increment`.
///
/// # Example
/// ```
/// use metriken::{metric, RwLockCounterHistogramGroup};
///
/// #[metric(name = "device_latency")]
/// static LATENCY: RwLockCounterHistogramGroup = RwLockCounterHistogramGroup::new(32, 3, 64);
///
/// let buckets = vec![0u64; LATENCY.config().total_buckets()];
/// LATENCY.set_metadata(0, [("device".to_string(), "nvme0n1".to_string())].into());
/// assert!(LATENCY.update_from(0, &buckets).unwrap());
/// assert!(LATENCY.load(0).is_some());
/// assert!(LATENCY.load(1).is_none());
/// ```
///
/// [`RwLockCounterHistogram`]: crate::RwLockCounterHistogram
/// [`update_from`]: RwLockCounterHistogramGroup::update_from
/// [`load`]: RwLockCounterHistogramGroup::load
/// [`clear`]: RwLockCounterHistogramGroup::clear
pub struct RwLockCounterHistogramGroup {
    inner: OnceLock<RwLock<Vec<Option<Histogram>>>>,
    metadata: GroupMetadata,
    config: Config,
    entries: usize,
}

impl RwLockCounterHistogramGroup {
    /// Create a new histogram group of `entries` entries.
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

    fn get_or_init(&self) -> &RwLock<Vec<Option<Histogram>>> {
        self.inner
            .get_or_init(|| RwLock::new(vec![None; self.entries]))
    }

    /// Replace the buckets of the entry at `idx` with `data`.
    ///
    /// Returns `Ok(false)` if `idx` is out of bounds, and `Ok(true)` once the
    /// entry holds `data`.
    ///
    /// # Errors
    /// Returns [`Error::IncompatibleParameters`] if `data.len()` does not
    /// equal [`Config::total_buckets`]; the entry is then unchanged.
    pub fn update_from(&self, idx: usize, data: &[u64]) -> Result<bool, Error> {
        if idx >= self.entries {
            return Ok(false);
        }
        if data.len() != self.config.total_buckets() {
            return Err(Error::IncompatibleParameters);
        }
        let mut inner = self.get_or_init().write();
        let entry = inner[idx].get_or_insert_with(|| Histogram::with_config(&self.config));
        entry.as_mut_slice().copy_from_slice(data);
        Ok(true)
    }

    /// Record `value` in the entry at `idx`, starting it empty if it was
    /// never written.
    ///
    /// Returns `Ok(false)` if `idx` is out of bounds.
    ///
    /// # Errors
    /// Returns an error if `value` is outside the configured range; an entry
    /// that was unwritten then stays unwritten.
    pub fn increment(&self, idx: usize, value: u64) -> Result<bool, Error> {
        if idx >= self.entries {
            return Ok(false);
        }
        let mut inner = self.get_or_init().write();
        match &mut inner[idx] {
            Some(h) => h.increment(value)?,
            slot @ None => {
                let mut h = Histogram::with_config(&self.config);
                h.increment(value)?;
                *slot = Some(h);
            }
        }
        Ok(true)
    }

    /// Remove the buckets of the entry at `idx`, so it loads as `None` until
    /// it is written again. Metadata is unchanged; see [`clear_metadata`].
    ///
    /// When a slot changes occupant, call `clear` before releasing its
    /// metadata ([`clear_metadata`] or `SlotIdentity::release`), and write
    /// the new occupant's buckets with [`update_from`] before setting its
    /// metadata, so no exposition pairs the new labels with the previous
    /// entry's buckets. An entry cleared and written again under unchanged
    /// metadata is the same occupant, so buckets that restart from zero read
    /// as a counter reset.
    ///
    /// [`clear_metadata`]: RwLockCounterHistogramGroup::clear_metadata
    /// [`update_from`]: RwLockCounterHistogramGroup::update_from
    pub fn clear(&self, idx: usize) {
        if idx >= self.entries {
            return;
        }
        if let Some(inner) = self.inner.get() {
            inner.write()[idx] = None;
        }
    }

    /// Load a copy of the histogram at `idx`.
    ///
    /// Returns `None` if `idx` is out of bounds or the entry has not been
    /// written since it was created or cleared.
    pub fn load(&self, idx: usize) -> Option<Histogram> {
        if idx >= self.entries {
            return None;
        }
        self.inner.get().and_then(|v| v.read()[idx].clone())
    }

    /// Load a copy of every entry, an entry that has not been written as an
    /// empty histogram.
    ///
    /// Returns `None` until an `update_from` succeeds or `increment` is called
    /// on an index in bounds.
    pub fn load_all(&self) -> Option<Vec<Histogram>> {
        self.inner.get().map(|v| {
            v.read()
                .iter()
                .map(|h| {
                    h.clone()
                        .unwrap_or_else(|| Histogram::with_config(&self.config))
                })
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

    /// A value that changes whenever any entry's metadata is set, added to
    /// or removed. See `CounterGroupMetric::metadata_version` for the
    /// contract and the read ordering it needs.
    pub fn metadata_version(&self) -> u64 {
        self.metadata.version()
    }

    /// Run `f` with a borrowed view of the metadata for the entry at `idx`,
    /// without cloning the underlying map.
    ///
    /// The group's metadata read lock is held for the duration of `f`:
    /// callers must not block, await, or call this group's methods (e.g.
    /// `set_metadata`, `load_metadata`) inside the closure.
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

impl HistogramGroupMetric for RwLockCounterHistogramGroup {
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

    fn metadata_version(&self) -> u64 {
        RwLockCounterHistogramGroup::metadata_version(self)
    }

    fn with_metadata(&self, idx: usize, f: &mut dyn FnMut(Option<&HashMap<String, String>>)) {
        self.metadata.with(idx, f);
    }

    fn for_each_metadata(&self, f: &mut dyn FnMut(usize, &HashMap<String, String>)) {
        self.metadata.for_each(f);
    }
}

impl Metric for RwLockCounterHistogramGroup {
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

    fn buckets(group: &RwLockCounterHistogramGroup, at: usize, count: u64) -> Vec<u64> {
        let mut data = vec![0u64; group.config().total_buckets()];
        data[at] = count;
        data
    }

    #[test]
    fn update_from_replaces_one_entry() {
        static GROUP: RwLockCounterHistogramGroup = RwLockCounterHistogramGroup::new(4, 3, 64);

        assert!(GROUP.load(0).is_none());
        assert!(GROUP.update_from(0, &buckets(&GROUP, 5, 7)).unwrap());
        assert_eq!(GROUP.load(0).unwrap().as_slice()[5], 7);
        // Replaced, not added to.
        assert!(GROUP.update_from(0, &buckets(&GROUP, 6, 2)).unwrap());
        let h = GROUP.load(0).unwrap();
        assert_eq!(h.as_slice()[5], 0);
        assert_eq!(h.as_slice()[6], 2);
        // The other entries stay unwritten.
        assert!(GROUP.load(1).is_none());
    }

    #[test]
    fn update_from_checks_bounds_and_length() {
        static GROUP: RwLockCounterHistogramGroup = RwLockCounterHistogramGroup::new(2, 3, 64);

        assert!(!GROUP.update_from(2, &buckets(&GROUP, 0, 1)).unwrap());
        assert!(GROUP.update_from(0, &[1, 2, 3]).is_err());
        assert!(GROUP.load(0).is_none(), "a rejected write leaves the entry");
        assert!(GROUP.load(2).is_none());
    }

    #[test]
    fn clear_unwrites_an_entry_and_keeps_metadata() {
        static GROUP: RwLockCounterHistogramGroup = RwLockCounterHistogramGroup::new(2, 3, 64);

        GROUP.insert_metadata(0, "device".into(), "sda".into());
        GROUP.update_from(0, &buckets(&GROUP, 1, 1)).unwrap();
        GROUP.clear(0);
        assert!(GROUP.load(0).is_none());
        assert_eq!(GROUP.load_metadata(0).unwrap()["device"], "sda");
        GROUP.clear(5); // out of bounds: no effect, no panic
    }

    #[test]
    fn increment_starts_an_unwritten_entry() {
        static GROUP: RwLockCounterHistogramGroup = RwLockCounterHistogramGroup::new(2, 3, 64);

        assert!(GROUP.increment(1, 100).unwrap());
        assert_eq!(GROUP.load(1).unwrap().as_slice().iter().sum::<u64>(), 1);
        assert!(!GROUP.increment(2, 100).unwrap());
    }

    #[test]
    fn a_failed_increment_leaves_an_unwritten_entry_unwritten() {
        static GROUP: RwLockCounterHistogramGroup = RwLockCounterHistogramGroup::new(2, 2, 10);

        assert!(GROUP.increment(0, 1 << 20).is_err());
        assert!(GROUP.load(0).is_none());
        assert!(GROUP.load_all().is_some(), "storage is allocated");
        assert_eq!(
            GROUP.load_all().unwrap()[0].as_slice().iter().sum::<u64>(),
            0
        );
    }

    #[test]
    fn load_all_fills_unwritten_entries_empty() {
        static GROUP: RwLockCounterHistogramGroup = RwLockCounterHistogramGroup::new(3, 3, 64);

        assert!(GROUP.load_all().is_none());
        GROUP.update_from(1, &buckets(&GROUP, 2, 4)).unwrap();
        let all = GROUP.load_all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].as_slice().iter().sum::<u64>(), 0);
        assert_eq!(all[1].as_slice()[2], 4);
    }

    #[test]
    fn the_trait_reads_what_was_written() {
        static GROUP: RwLockCounterHistogramGroup = RwLockCounterHistogramGroup::new(2, 3, 64);

        GROUP.set_metadata(0, [("device".to_string(), "nvme0n1".to_string())].into());
        GROUP.update_from(0, &buckets(&GROUP, 3, 9)).unwrap();
        let dyn_group: &dyn HistogramGroupMetric = &GROUP;
        assert_eq!(dyn_group.entries(), 2);
        assert_eq!(dyn_group.load_histogram(0).unwrap().as_slice()[3], 9);
        assert!(dyn_group.load_histogram(1).is_none());
        assert_eq!(dyn_group.load_metadata(0).unwrap()["device"], "nvme0n1");
        let mut seen = Vec::new();
        dyn_group.for_each_metadata(&mut |idx, _| seen.push(idx));
        assert_eq!(seen, vec![0]);
        assert!(matches!(GROUP.value(), Some(Value::HistogramGroup(_))));
    }
}
