use crate::*;

pub struct Snapshotter {
    counters: bool,
    gauges: bool,
    atomic_histograms: bool,
    rwlock_histograms: bool,
    filter: fn(&str) -> bool,
}

pub struct SnapshotterBuilder {
    snapshotter: Snapshotter,
}

impl SnapshotterBuilder {
    pub fn build(self) -> Snapshotter {
        self.snapshotter
    }

    pub fn counters(mut self, included: bool) -> Self {
        self.snapshotter.counters = included;
        self
    }

    pub fn gauges(mut self, included: bool) -> Self {
        self.snapshotter.gauges = included;
        self
    }

    pub fn atomic_histograms(mut self, included: bool) -> Self {
        self.snapshotter.atomic_histograms = included;
        self
    }

    pub fn rwlock_histograms(mut self, included: bool) -> Self {
        self.snapshotter.rwlock_histograms = included;
        self
    }

    pub fn filter(mut self, filter: fn(&str) -> bool) -> Self {
        self.snapshotter.filter = filter;
        self
    }
}

impl Default for Snapshotter {
    fn default() -> Self {
        Self {
            counters: true,
            gauges: true,
            atomic_histograms: true,
            rwlock_histograms: true,
            filter: |_| true,
        }
    }
}

impl Snapshotter {
    pub fn snapshot(&self) -> Snapshot {
        let mut snapshot = Snapshot::new();

        // iterate through the metrics and build-up the snapshot
        for metric in &metriken::metrics() {
            if !(self.filter)(metric.name()) {
                continue;
            }

            match metric.value() {
                Some(Value::Counter(value)) => {
                    if !self.counters {
                        continue;
                    }

                    snapshot
                        .counters
                        .push((metric.formatted(metriken::Format::Simple), value));
                }
                Some(Value::Gauge(value)) => {
                    if !self.gauges {
                        continue;
                    }

                    snapshot
                        .gauges
                        .push((metric.formatted(metriken::Format::Simple), value));
                }
                Some(Value::Other(other)) => {
                    if let Some(histogram) = other.downcast_ref::<AtomicHistogram>() {
                        if !self.atomic_histograms {
                            continue;
                        }

                        if let Some(histogram) = histogram.snapshot() {
                            snapshot
                                .histograms
                                .push((metric.formatted(metriken::Format::Simple), histogram));
                        }
                    }
                    if let Some(histogram) = other.downcast_ref::<RwLockHistogram>() {
                        if !self.rwlock_histograms {
                            continue;
                        }

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
