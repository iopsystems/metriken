use std::collections::HashMap;
use std::time::SystemTime;

use metriken::{MetricEntry, Value};

use crate::snapshot::{Counter, Gauge, Histogram, Snapshot, SnapshotV1};

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
        let ts = SystemTime::now();
        let mut counters: Vec<Counter> = Vec::new();
        let mut gauges: Vec<Gauge> = Vec::new();
        let mut histograms: Vec<Histogram> = Vec::new();

        // Iterate through metrics using numeric IDs as column names to avoid
        // collisions between same-name metrics with different labels. The base
        // metric name is stored in the "metric" metadata key for Tsdb/PromQL
        // indexing.
        for (metric_id, metric) in metriken::metrics().iter().enumerate() {
            if !(self.filter)(metric) {
                continue;
            }
            let column_name = format!("{metric_id}");

            // Build metadata from user-defined labels + metric base name
            let build_metadata = |metric: &MetricEntry| -> HashMap<String, String> {
                let mut metadata = HashMap::from_iter(
                    metric
                        .metadata()
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string())),
                );
                metadata.insert("metric".to_string(), metric.name().replace('/', "_"));
                if let Some(description) = metric.description().map(|v| v.to_string()) {
                    metadata.insert("description".to_string(), description);
                }
                metadata
            };

            match metric.value() {
                Some(Value::Counter(value)) => {
                    counters.push(Counter {
                        name: column_name.clone(),
                        value,
                        metadata: build_metadata(metric),
                    });
                }
                Some(Value::Gauge(value)) => {
                    gauges.push(Gauge {
                        name: column_name.clone(),
                        value,
                        metadata: build_metadata(metric),
                    });
                }
                Some(Value::Histogram(h)) => {
                    if let Some(histogram) = h.load() {
                        let mut metadata = build_metadata(metric);
                        metadata.insert(
                            "grouping_power".to_string(),
                            histogram.config().grouping_power().to_string(),
                        );
                        metadata.insert(
                            "max_value_power".to_string(),
                            histogram.config().max_value_power().to_string(),
                        );

                        histograms.push(Histogram {
                            name: column_name.clone(),
                            value: histogram,
                            metadata,
                        });
                    }
                }
                Some(Value::CounterGroup(g)) => {
                    let base_metadata = build_metadata(metric);
                    for (idx, entry_meta) in g.metadata_snapshot() {
                        if let Some(value) = g.counter_value(idx) {
                            let mut metadata = base_metadata.clone();
                            metadata.extend(entry_meta);
                            counters.push(Counter {
                                name: format!("{column_name}x{idx}"),
                                value,
                                metadata,
                            });
                        }
                    }
                }
                Some(Value::GaugeGroup(g)) => {
                    let base_metadata = build_metadata(metric);
                    for (idx, entry_meta) in g.metadata_snapshot() {
                        if let Some(value) = g.gauge_value(idx) {
                            let mut metadata = base_metadata.clone();
                            metadata.extend(entry_meta);
                            gauges.push(Gauge {
                                name: format!("{column_name}x{idx}"),
                                value,
                                metadata,
                            });
                        }
                    }
                }
                Some(Value::HistogramGroup(g)) => {
                    let base_metadata = build_metadata(metric);
                    for (idx, entry_meta) in g.metadata_snapshot() {
                        if let Some(histogram) = g.load_histogram(idx) {
                            let mut metadata = base_metadata.clone();
                            metadata.extend(entry_meta);
                            metadata.insert(
                                "grouping_power".to_string(),
                                histogram.config().grouping_power().to_string(),
                            );
                            metadata.insert(
                                "max_value_power".to_string(),
                                histogram.config().max_value_power().to_string(),
                            );
                            histograms.push(Histogram {
                                name: format!("{column_name}x{idx}"),
                                value: histogram,
                                metadata,
                            });
                        }
                    }
                }
                _ => continue,
            }
        }

        Snapshot::V1(SnapshotV1 {
            systemtime: ts,
            metadata: self.metadata.clone(),
            counters,
            gauges,
            histograms,
        })
    }
}
