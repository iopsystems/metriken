use std::collections::HashMap;

use crate::Window;

/// Trait for histogram metrics that can produce snapshots.
///
/// Implemented by both `AtomicHistogram` (for recording individual events)
/// and `RwLockHistogram` (for bulk updates from pre-aggregated data).
/// Exposition code can use this trait without knowing which variant it has.
pub trait HistogramMetric: Send + Sync + 'static {
    /// Return the histogram configuration.
    fn config(&self) -> histogram::Config;

    /// Load a snapshot of the histogram.
    ///
    /// Returns `None` if the histogram has never been written to.
    fn load(&self) -> Option<histogram::Histogram>;
}

/// Trait for a group of counter metrics with per-entry metadata.
///
/// Counter groups store a dense array of `u64` values indexed by `usize`,
/// with sparse metadata attached to individual entries.
pub trait CounterGroupMetric: Send + Sync + 'static {
    /// Return the number of entries in this group.
    fn entries(&self) -> usize;

    /// Load the value of the counter at `idx`.
    fn counter_value(&self, idx: usize) -> Option<u64>;

    /// Load all counter values as a snapshot.
    fn load_counters(&self) -> Option<Vec<u64>>;

    /// Load metadata for the entry at `idx`.
    fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>>;

    /// Snapshot all metadata.
    fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)>;

    /// Visit the metadata for the entry at `idx` without cloning it.
    ///
    /// The implementation may hold an internal read lock for the duration of
    /// the callback — do not block, await, or re-enter the group inside it.
    /// Default: falls back to the allocating `load_metadata`.
    #[allow(clippy::type_complexity)]
    fn with_metadata(&self, idx: usize, f: &mut dyn FnMut(Option<&HashMap<String, String>>)) {
        f(self.load_metadata(idx).as_ref());
    }

    /// Visit every populated entry's metadata without cloning. Order is
    /// unspecified; callers needing determinism must sort. Same locking
    /// caveat as `with_metadata`. Default: falls back to the allocating
    /// `metadata_snapshot`.
    fn for_each_metadata(&self, f: &mut dyn FnMut(usize, &HashMap<String, String>)) {
        for (idx, map) in self.metadata_snapshot() {
            f(idx, &map);
        }
    }

    /// Load the acquisition window recorded for the entry at `idx`, if any.
    /// Default: none — general groups do not record windows.
    fn load_window(&self, _idx: usize) -> Option<Window> {
        None
    }

    /// Snapshot all per-entry acquisition windows. Default: empty.
    fn window_snapshot(&self) -> Vec<(usize, Window)> {
        Vec::new()
    }

    /// Load the counter at `idx` and its acquisition window as a pair.
    ///
    /// Default: separate `counter_value` and `load_window` reads — **not**
    /// atomic. Windowed groups override this to read the pair under one lock so
    /// exposition never sees a torn `(value, window)` pair.
    fn load_with_window(&self, idx: usize) -> (Option<u64>, Option<Window>) {
        (self.counter_value(idx), self.load_window(idx))
    }
}

/// Trait for a group of gauge metrics with per-entry metadata.
///
/// Gauge groups store a dense array of `i64` values indexed by `usize`,
/// with sparse metadata attached to individual entries.
pub trait GaugeGroupMetric: Send + Sync + 'static {
    /// Return the number of entries in this group.
    fn entries(&self) -> usize;

    /// Load the value of the gauge at `idx`.
    fn gauge_value(&self, idx: usize) -> Option<i64>;

    /// Load all gauge values as a snapshot.
    fn load_gauges(&self) -> Option<Vec<i64>>;

    /// Load metadata for the entry at `idx`.
    fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>>;

    /// Snapshot all metadata.
    fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)>;

    /// Visit the metadata for the entry at `idx` without cloning it.
    ///
    /// The implementation may hold an internal read lock for the duration of
    /// the callback — do not block, await, or re-enter the group inside it.
    /// Default: falls back to the allocating `load_metadata`.
    #[allow(clippy::type_complexity)]
    fn with_metadata(&self, idx: usize, f: &mut dyn FnMut(Option<&HashMap<String, String>>)) {
        f(self.load_metadata(idx).as_ref());
    }

    /// Visit every populated entry's metadata without cloning. Order is
    /// unspecified; callers needing determinism must sort. Same locking
    /// caveat as `with_metadata`. Default: falls back to the allocating
    /// `metadata_snapshot`.
    fn for_each_metadata(&self, f: &mut dyn FnMut(usize, &HashMap<String, String>)) {
        for (idx, map) in self.metadata_snapshot() {
            f(idx, &map);
        }
    }

    /// Load the acquisition window recorded for the entry at `idx`, if any.
    /// Default: none — general groups do not record windows.
    fn load_window(&self, _idx: usize) -> Option<Window> {
        None
    }

    /// Snapshot all per-entry acquisition windows. Default: empty.
    fn window_snapshot(&self) -> Vec<(usize, Window)> {
        Vec::new()
    }

    /// Load the gauge at `idx` and its acquisition window as a pair.
    ///
    /// Default: separate `gauge_value` and `load_window` reads — **not**
    /// atomic. Windowed groups override this to read the pair under one lock.
    fn load_with_window(&self, idx: usize) -> (Option<i64>, Option<Window>) {
        (self.gauge_value(idx), self.load_window(idx))
    }
}

/// Trait for a group of histogram metrics with per-entry metadata.
///
/// Histogram groups store a dense array of histograms (all sharing the same
/// configuration) indexed by `usize`, with sparse metadata attached to
/// individual entries.
pub trait HistogramGroupMetric: Send + Sync + 'static {
    /// Return the number of entries in this group.
    fn entries(&self) -> usize;

    /// Return the histogram configuration shared by all entries.
    fn config(&self) -> histogram::Config;

    /// Load a snapshot of the histogram at `idx`.
    fn load_histogram(&self, idx: usize) -> Option<histogram::Histogram>;

    /// Load snapshots of all histograms.
    fn load_all_histograms(&self) -> Option<Vec<histogram::Histogram>>;

    /// Load metadata for the entry at `idx`.
    fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>>;

    /// Snapshot all metadata.
    fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)>;

    /// Visit the metadata for the entry at `idx` without cloning it.
    ///
    /// The implementation may hold an internal read lock for the duration of
    /// the callback — do not block, await, or re-enter the group inside it.
    /// Default: falls back to the allocating `load_metadata`.
    #[allow(clippy::type_complexity)]
    fn with_metadata(&self, idx: usize, f: &mut dyn FnMut(Option<&HashMap<String, String>>)) {
        f(self.load_metadata(idx).as_ref());
    }

    /// Visit every populated entry's metadata without cloning. Order is
    /// unspecified; callers needing determinism must sort. Same locking
    /// caveat as `with_metadata`. Default: falls back to the allocating
    /// `metadata_snapshot`.
    fn for_each_metadata(&self, f: &mut dyn FnMut(usize, &HashMap<String, String>)) {
        for (idx, map) in self.metadata_snapshot() {
            f(idx, &map);
        }
    }

    /// Load the acquisition window recorded for the entry at `idx`, if any.
    /// Default: none — general groups do not record windows.
    fn load_window(&self, _idx: usize) -> Option<Window> {
        None
    }

    /// Snapshot all per-entry acquisition windows. Default: empty.
    fn window_snapshot(&self) -> Vec<(usize, Window)> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal implementors that only supply the REQUIRED trait methods,
    // leaving `with_metadata`/`for_each_metadata` on their defaults. Exercised
    // only through `&dyn Trait` — the object-safety point of this API — to
    // prove the default bodies compile and behave for an external
    // implementor that has never heard of `GroupMetadata`.

    struct DefaultOnlyCounterGroup {
        metadata: HashMap<usize, HashMap<String, String>>,
    }

    impl CounterGroupMetric for DefaultOnlyCounterGroup {
        fn entries(&self) -> usize {
            2
        }
        fn counter_value(&self, _idx: usize) -> Option<u64> {
            None
        }
        fn load_counters(&self) -> Option<Vec<u64>> {
            None
        }
        fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
            self.metadata.get(&idx).cloned()
        }
        fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
            self.metadata.iter().map(|(k, v)| (*k, v.clone())).collect()
        }
    }

    struct DefaultOnlyGaugeGroup {
        metadata: HashMap<usize, HashMap<String, String>>,
    }

    impl GaugeGroupMetric for DefaultOnlyGaugeGroup {
        fn entries(&self) -> usize {
            2
        }
        fn gauge_value(&self, _idx: usize) -> Option<i64> {
            None
        }
        fn load_gauges(&self) -> Option<Vec<i64>> {
            None
        }
        fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
            self.metadata.get(&idx).cloned()
        }
        fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
            self.metadata.iter().map(|(k, v)| (*k, v.clone())).collect()
        }
    }

    struct DefaultOnlyHistogramGroup {
        metadata: HashMap<usize, HashMap<String, String>>,
    }

    impl HistogramGroupMetric for DefaultOnlyHistogramGroup {
        fn entries(&self) -> usize {
            2
        }
        fn config(&self) -> histogram::Config {
            histogram::Config::new(7, 64).unwrap()
        }
        fn load_histogram(&self, _idx: usize) -> Option<histogram::Histogram> {
            None
        }
        fn load_all_histograms(&self) -> Option<Vec<histogram::Histogram>> {
            None
        }
        fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
            self.metadata.get(&idx).cloned()
        }
        fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
            self.metadata.iter().map(|(k, v)| (*k, v.clone())).collect()
        }
    }

    fn sample_metadata() -> HashMap<usize, HashMap<String, String>> {
        let mut m = HashMap::new();
        m.insert(0, [("cpu".to_string(), "0".to_string())].into());
        m.insert(1, [("cpu".to_string(), "1".to_string())].into());
        m
    }

    fn sorted(
        mut v: Vec<(usize, HashMap<String, String>)>,
    ) -> Vec<(usize, HashMap<String, String>)> {
        v.sort_by_key(|(idx, _)| *idx);
        v
    }

    macro_rules! default_impl_tests {
        ($mod_name:ident, $group_ty:ident, $trait_ty:ident) => {
            mod $mod_name {
                use super::*;

                #[test]
                fn default_with_metadata_matches_load_metadata() {
                    let group = $group_ty {
                        metadata: sample_metadata(),
                    };
                    let dyn_group: &dyn $trait_ty = &group;

                    // Populated index.
                    let expected = dyn_group.load_metadata(0);
                    let mut seen: Option<HashMap<String, String>> = None;
                    dyn_group.with_metadata(0, &mut |m| seen = m.cloned());
                    assert_eq!(seen, expected);

                    // Unpopulated index.
                    let mut seen_absent: Option<HashMap<String, String>> = None;
                    let mut called = false;
                    dyn_group.with_metadata(5, &mut |m| {
                        called = true;
                        seen_absent = m.cloned();
                    });
                    assert!(called, "the callback must still run with None");
                    assert!(seen_absent.is_none());
                }

                #[test]
                fn default_for_each_metadata_matches_metadata_snapshot() {
                    let group = $group_ty {
                        metadata: sample_metadata(),
                    };
                    let dyn_group: &dyn $trait_ty = &group;

                    let mut seen: Vec<(usize, HashMap<String, String>)> = Vec::new();
                    dyn_group.for_each_metadata(&mut |idx, m| seen.push((idx, m.clone())));

                    let expected = dyn_group.metadata_snapshot();
                    assert_eq!(sorted(seen.clone()), sorted(expected.clone()));
                    assert_eq!(seen.len(), expected.len());
                }
            }
        };
    }

    default_impl_tests!(
        counter_defaults,
        DefaultOnlyCounterGroup,
        CounterGroupMetric
    );
    default_impl_tests!(gauge_defaults, DefaultOnlyGaugeGroup, GaugeGroupMetric);
    default_impl_tests!(
        histogram_defaults,
        DefaultOnlyHistogramGroup,
        HistogramGroupMetric
    );
}
