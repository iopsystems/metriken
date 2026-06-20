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

/// A type that maps to a fixed set of labeled metric slots.
///
/// Implement this on enums (or tuples of enums) to drive
/// `DimensionedCounter`, `DimensionedGauge`, and `DimensionedHistogram`.
/// Use `#[derive(MetricDimension)]` from `metriken-derive` rather than
/// implementing this by hand.
pub trait MetricDimension: Send + Sync + 'static {
    /// Total number of distinct values (slots in the dense backing array).
    const COUNT: usize;

    /// Maps this value to an index in `[0, COUNT)`.
    ///
    /// # Invariants
    ///
    /// The returned index must be in the range `[0, COUNT)`. Implementations
    /// that return out-of-bounds indices will cause a panic at the call site.
    fn index(&self) -> usize;

    /// Returns the Prometheus labels for this specific value.
    fn labels(&self) -> HashMap<String, String>;

    /// Returns labels for every index in order, used to pre-populate
    /// group metadata so zero-value entries are still exported.
    ///
    /// # Invariants
    ///
    /// The returned vector must be in index-ascending order: `all_labels()[i]`
    /// must contain the labels for the value that would return `i` from `index()`.
    fn all_labels() -> Vec<HashMap<String, String>>;
}

/// Composite dimension for tuples of two dimensions.
///
/// Merges labels from both `A` and `B` dimensions. If both produce labels
/// with the same key, `B`'s value takes precedence (last-writer-wins).
/// Users are responsible for ensuring their dimensions use disjoint label key sets.
impl<A, B> MetricDimension for (A, B)
where
    A: MetricDimension,
    B: MetricDimension,
{
    const COUNT: usize = A::COUNT * B::COUNT;

    fn index(&self) -> usize {
        self.0.index() * B::COUNT + self.1.index()
    }

    fn labels(&self) -> HashMap<String, String> {
        let mut map = self.0.labels();
        map.extend(self.1.labels());
        map
    }

    fn all_labels() -> Vec<HashMap<String, String>> {
        let mut result = Vec::with_capacity(Self::COUNT);
        for a_labels in A::all_labels() {
            for b_labels in B::all_labels() {
                let mut map = a_labels.clone();
                map.extend(b_labels);
                result.push(map);
            }
        }
        result
    }
}
