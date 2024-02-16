use std::any::Any;
use crate::*;

/// Produces a snapshot of metric readings.
pub struct Snapshotter {
    kind_filter: fn(Option<&dyn Any>) -> bool,
    name_filter: fn(&str) -> bool,
}

/// Used to build a new `Snapshotter`.
#[derive(Default)]
pub struct SnapshotterBuilder {
    snapshotter: Snapshotter,
}

impl SnapshotterBuilder {
    /// Construct a new builder. By default, all metric types are enabled and no
    /// filtering is applied.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume the builder and return a `Snapshotter`.
    pub fn build(self) -> Snapshotter {
        self.snapshotter
    }

    /// Allow a user-supplied filtering function to be applied based on the
    /// metric name. The function is given the metric name and must return true
    /// for any metric that should be included in the snapshot.
    pub fn name_filter(mut self, filter: fn(&str) -> bool) -> Self {
        self.snapshotter.name_filter = filter;
        self
    }

    /// Allow a user-supplied filtering function to be applied. The function is
    /// given the metric name and must return true for any metric that should
    /// be included in the snapshot.
    pub fn kind_filter(mut self, filter: fn(Option<&dyn std::any::Any>) -> bool) -> Self {
        self.snapshotter.kind_filter = filter;
        self
    }
}

impl Default for Snapshotter {
    fn default() -> Self {
        Self {
            kind_filter: |_| true,
            name_filter: |_| true,
        }
    }
}

impl Snapshotter {
    /// Produce a new snapshot.
    pub fn snapshot(&self) -> Snapshot {
        let mut snapshot = Snapshot::new();

        // iterate through the metrics and build-up the snapshot
        for metric in &metriken::metrics() {
            if !(self.name_filter)(metric.name()) {
                continue;
            }

            if ! (self.kind_filter)(metric.as_any()) {
                continue;
            }

            match metric.value() {
                Some(Value::Counter(value)) => {
                    snapshot
                        .counters
                        .push((metric.formatted(metriken::Format::Simple), value));
                }
                Some(Value::Gauge(value)) => {
                    snapshot
                        .gauges
                        .push((metric.formatted(metriken::Format::Simple), value));
                }
                Some(Value::Other(other)) => {
                    if let Some(histogram) = other.downcast_ref::<AtomicHistogram>() {
                        if let Some(histogram) = histogram.snapshot() {
                            snapshot
                                .histograms
                                .push((metric.formatted(metriken::Format::Simple), histogram));
                        }
                    } else if let Some(histogram) = other.downcast_ref::<RwLockHistogram>() {
                        if let Some(histogram) = histogram.snapshot() {
                            snapshot
                                .histograms
                                .push((metric.formatted(metriken::Format::Simple), histogram));
                        }
                    }
                }
                _ => continue,
            }
        }

        snapshot
    }
}
