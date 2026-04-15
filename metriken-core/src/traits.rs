use std::collections::HashMap;

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
}
