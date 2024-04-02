use std::collections::HashMap;

use metriken::{AtomicHistogram, MetricEntry, RwLockHistogram, Value};

use crate::snapshot::{Counter, Gauge, Histogram};
use crate::Snapshot;

/// Produces a snapshot of metric readings.
pub struct Snapshotter {
    filter: fn(&MetricEntry) -> bool,
    metadata: HashMap<String, String>,
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
    /// metric entry. The function must return true for any metric that should
    /// be included in the snapshot.
    pub fn filter(mut self, filter: fn(&MetricEntry) -> bool) -> Self {
        self.snapshotter.filter = filter;
        self
    }

    /// Add a key-value pair to the metadata.
    pub fn metadata(mut self, key: String, value: String) -> Self {
        self.snapshotter.metadata.insert(key, value);
        self
    }
}

impl Default for Snapshotter {
    fn default() -> Self {
        Self {
            filter: |_| true,
            metadata: HashMap::new(),
        }
    }
}

impl Snapshotter {
    /// Produce a new snapshot.
    pub fn snapshot(&self) -> Snapshot {
        let mut snapshot = Snapshot::new();
        snapshot.metadata = self.metadata.clone();

        // iterate through the metrics and build-up the snapshot
        for metric in &metriken::metrics() {
            if !(self.filter)(metric) {
                continue;
            }

            match metric.value() {
                Some(Value::Counter(value)) => {
                    let counter = Counter {
                        name: metric.formatted(metriken::Format::Simple),
                        value,
                        metadata: HashMap::from_iter(
                            metric
                                .metadata()
                                .into_iter()
                                .map(|(k, v)| (k.to_string(), v.to_string())),
                        ),
                    };

                    snapshot.counters.push(counter);
                }
                Some(Value::Gauge(value)) => {
                    let gauge = Gauge {
                        name: metric.formatted(metriken::Format::Simple),
                        value,
                        metadata: HashMap::from_iter(
                            metric
                                .metadata()
                                .into_iter()
                                .map(|(k, v)| (k.to_string(), v.to_string())),
                        ),
                    };

                    snapshot.gauges.push(gauge);
                }
                Some(Value::Other(other)) => {
                    let histogram = if let Some(histogram) = other.downcast_ref::<AtomicHistogram>()
                    {
                        histogram.load()
                    } else if let Some(histogram) = other.downcast_ref::<RwLockHistogram>() {
                        histogram.load()
                    } else {
                        None
                    };

                    if let Some(histogram) = histogram {
                        let mut metadata = HashMap::from_iter(
                            metric
                                .metadata()
                                .into_iter()
                                .map(|(k, v)| (k.to_string(), v.to_string())),
                        );

                        // Store configuration parameters as metadata
                        metadata.insert(
                            "grouping_power".to_string(),
                            histogram.config().grouping_power().to_string(),
                        );
                        metadata.insert(
                            "max_value_power".to_string(),
                            histogram.config().max_value_power().to_string(),
                        );

                        let histogram = Histogram {
                            name: metric.formatted(metriken::Format::Simple),
                            value: histogram,
                            metadata,
                        };

                        snapshot.histograms.push(histogram);
                    }
                }
                _ => continue,
            }
        }

        snapshot
    }
}
