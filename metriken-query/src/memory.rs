use std::collections::HashMap;

use crate::labels::Labels;
use crate::types::{
    Counter, Counters, Gauge, Gauges, Histogram, Histograms,
};
use crate::DataSource;

/// In-memory data source for tests. Callers build `Counter`/`Gauge`/`Histogram`
/// values directly and hand them in; the source handles time-range filtering on
/// query.
pub(crate) struct Memory {
    counters: HashMap<String, Vec<Counter>>,
    gauges: HashMap<String, Vec<Gauge>>,
    histograms: HashMap<String, Vec<Histogram>>,
    interval_ms: u64,
}

impl Memory {
    pub(crate) fn new(interval_ms: u64) -> Self {
        Self {
            counters: HashMap::new(),
            gauges: HashMap::new(),
            histograms: HashMap::new(),
            interval_ms,
        }
    }

    pub(crate) fn add_counter(&mut self, name: &str, counter: Counter) {
        self.counters.entry(name.to_string()).or_default().push(counter);
    }

    pub(crate) fn add_gauge(&mut self, name: &str, gauge: Gauge) {
        self.gauges.entry(name.to_string()).or_default().push(gauge);
    }

    pub(crate) fn add_histogram(&mut self, name: &str, histogram: Histogram) {
        self.histograms.entry(name.to_string()).or_default().push(histogram);
    }
}

fn slice_range(timestamps: &[u64], start_ns: u64, end_ns: u64) -> std::ops::Range<usize> {
    let lo = timestamps.partition_point(|&ts| ts < start_ns);
    let hi = timestamps.partition_point(|&ts| ts <= end_ns);
    lo..hi
}

impl DataSource for Memory {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Counters> {
        let stored = self.counters.get(name)?;
        let series: Vec<Counter> = stored
            .iter()
            .filter(|c| filter.inner.is_empty() || c.labels.matches(filter))
            .map(|c| {
                let r = slice_range(&c.timestamps, start_ns, end_ns);
                Counter {
                    labels: c.labels.clone(),
                    timestamps: c.timestamps[r.clone()].to_vec(),
                    values: c.values[r].to_vec(),
                }
            })
            .filter(|c| !c.timestamps.is_empty())
            .collect();
        if series.is_empty() { None } else { Some(Counters { series }) }
    }

    fn gauges(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Gauges> {
        let stored = self.gauges.get(name)?;
        let series: Vec<Gauge> = stored
            .iter()
            .filter(|g| filter.inner.is_empty() || g.labels.matches(filter))
            .map(|g| {
                let r = slice_range(&g.timestamps, start_ns, end_ns);
                Gauge {
                    labels: g.labels.clone(),
                    timestamps: g.timestamps[r.clone()].to_vec(),
                    values: g.values[r].to_vec(),
                }
            })
            .filter(|g| !g.timestamps.is_empty())
            .collect();
        if series.is_empty() { None } else { Some(Gauges { series }) }
    }

    fn histograms(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Histograms> {
        let stored = self.histograms.get(name)?;
        let series: Vec<Histogram> = stored
            .iter()
            .filter(|h| filter.inner.is_empty() || h.labels.matches(filter))
            .map(|h| {
                let r = slice_range(&h.timestamps, start_ns, end_ns);
                Histogram {
                    labels: h.labels.clone(),
                    config: h.config,
                    timestamps: h.timestamps[r.clone()].to_vec(),
                    snapshots: h.snapshots[r].iter().map(|s| {
                        crate::types::HistogramSnapshot {
                            index: s.index.clone(),
                            count: s.count.clone(),
                        }
                    }).collect(),
                }
            })
            .filter(|h| !h.timestamps.is_empty())
            .collect();
        if series.is_empty() { None } else { Some(Histograms { series }) }
    }

    fn interval(&self) -> f64 {
        self.interval_ms as f64 / 1000.0
    }

    fn time_range(&self) -> Option<(u64, u64)> {
        let mut min_ns: Option<u64> = None;
        let mut max_ns: Option<u64> = None;

        let update = |min_ns: &mut Option<u64>, max_ns: &mut Option<u64>, ts: &[u64]| {
            if let Some(&first) = ts.first() {
                *min_ns = Some(min_ns.map_or(first, |m| m.min(first)));
            }
            if let Some(&last) = ts.last() {
                *max_ns = Some(max_ns.map_or(last, |m| m.max(last)));
            }
        };

        for series in self.counters.values().flatten() {
            update(&mut min_ns, &mut max_ns, &series.timestamps);
        }
        for series in self.gauges.values().flatten() {
            update(&mut min_ns, &mut max_ns, &series.timestamps);
        }
        for series in self.histograms.values().flatten() {
            update(&mut min_ns, &mut max_ns, &series.timestamps);
        }
        min_ns.zip(max_ns)
    }

    #[cfg(test)]
    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        let mut out: HashMap<String, HashMap<Labels, String>> = HashMap::new();
        for (name, series) in &self.counters {
            for s in series {
                out.entry(name.clone()).or_default().insert(s.labels.clone(), name.clone());
            }
        }
        for (name, series) in &self.gauges {
            for s in series {
                out.entry(name.clone()).or_default().insert(s.labels.clone(), name.clone());
            }
        }
        for (name, series) in &self.histograms {
            for s in series {
                out.entry(name.clone())
                    .or_default()
                    .insert(s.labels.clone(), format!("{name}:buckets"));
            }
        }
        out
    }
}
