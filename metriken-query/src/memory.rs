use std::collections::HashMap;

use crate::histogram_stream::{HistogramRow, HistogramStream, HistogramStreamMeta};
use crate::labels::Labels;
#[cfg(feature = "ingest")]
use crate::types::HistogramSnapshot;
use crate::types::{Counter, Counters, Gauge, Gauges, Histogram};
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

    pub(crate) fn set_interval_ms(&mut self, ms: u64) {
        self.interval_ms = ms;
    }

    #[cfg(test)]
    pub(crate) fn add_counter(&mut self, name: &str, counter: Counter) {
        self.counters
            .entry(name.to_string())
            .or_default()
            .push(counter);
    }

    #[cfg(test)]
    pub(crate) fn add_gauge(&mut self, name: &str, gauge: Gauge) {
        self.gauges.entry(name.to_string()).or_default().push(gauge);
    }

    #[cfg(test)]
    pub(crate) fn add_histogram(&mut self, name: &str, histogram: Histogram) {
        self.histograms
            .entry(name.to_string())
            .or_default()
            .push(histogram);
    }

    /// Append a counter sample. If a series with matching (name, labels)
    /// exists, the sample is appended to it; otherwise a new series is created.
    #[cfg(feature = "ingest")]
    pub(crate) fn upsert_counter_sample(
        &mut self,
        name: &str,
        labels: Labels,
        ts: u64,
        value: u64,
    ) {
        let series = self.counters.entry(name.to_string()).or_default();
        if let Some(c) = series.iter_mut().find(|c| c.labels == labels) {
            c.timestamps.push(ts);
            c.values.push(value);
        } else {
            series.push(Counter {
                labels,
                timestamps: vec![ts],
                values: vec![value],
                windows: None,
            });
        }
    }

    /// Append a gauge sample. If a series with matching (name, labels)
    /// exists, the sample is appended to it; otherwise a new series is created.
    #[cfg(feature = "ingest")]
    pub(crate) fn upsert_gauge_sample(&mut self, name: &str, labels: Labels, ts: u64, value: i64) {
        let series = self.gauges.entry(name.to_string()).or_default();
        if let Some(g) = series.iter_mut().find(|g| g.labels == labels) {
            g.timestamps.push(ts);
            g.values.push(value);
        } else {
            series.push(Gauge {
                labels,
                timestamps: vec![ts],
                values: vec![value],
                windows: None,
            });
        }
    }

    /// Append a histogram sample. If a series with matching (name, labels)
    /// exists, the sample is appended to it; otherwise a new series is created.
    #[cfg(feature = "ingest")]
    pub(crate) fn upsert_histogram_sample(
        &mut self,
        name: &str,
        labels: Labels,
        config: ::histogram::Config,
        ts: u64,
        snapshot: HistogramSnapshot,
    ) {
        let series = self.histograms.entry(name.to_string()).or_default();
        if let Some(h) = series.iter_mut().find(|h| h.labels == labels) {
            h.timestamps.push(ts);
            h.snapshots.push(snapshot);
        } else {
            series.push(Histogram {
                labels,
                config,
                timestamps: vec![ts],
                snapshots: vec![snapshot],
            });
        }
    }

    #[cfg(feature = "ingest")]
    pub(crate) fn interval_ms(&self) -> u64 {
        self.interval_ms
    }
}

/// Extract metric name and label set from snapshot metadata.
/// If `metadata` has a `"metric"` key, use it as the name; otherwise fall back
/// to `default_name` (the snapshot's `.name` field). Drop reserved keys
/// (`metric`, `metric_type`, `unit`) before constructing labels.
#[cfg(feature = "ingest")]
pub(crate) fn extract_name_labels(
    metadata: &std::collections::HashMap<String, String>,
    default_name: &str,
) -> (String, Labels) {
    let name = metadata
        .get("metric")
        .cloned()
        .unwrap_or_else(|| default_name.to_string());
    let mut inner = std::collections::BTreeMap::new();
    for (k, v) in metadata {
        match k.as_str() {
            "metric" | "metric_type" | "unit" => continue,
            _ => {
                inner.insert(k.clone(), v.clone());
            }
        }
    }
    (name, Labels { inner })
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
                    windows: None,
                }
            })
            .filter(|c| !c.timestamps.is_empty())
            .collect();
        if series.is_empty() {
            None
        } else {
            Some(Counters { series })
        }
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
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
                    windows: None,
                }
            })
            .filter(|g| !g.timestamps.is_empty())
            .collect();
        if series.is_empty() {
            None
        } else {
            Some(Gauges { series })
        }
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let stored = self.histograms.get(name)?;
        let mut series_labels: Vec<crate::labels::Labels> = Vec::new();
        let mut config: Option<::histogram::Config> = None;
        let mut rows: Vec<HistogramRow> = Vec::new();

        for hist in stored
            .iter()
            .filter(|h| filter.inner.is_empty() || h.labels.matches(filter))
        {
            debug_assert!(
                config.is_none_or(|c| c == hist.config),
                "histogram series within one metric must share the same config"
            );
            config = config.or(Some(hist.config));
            let r = slice_range(&hist.timestamps, start_ns, end_ns);
            if r.is_empty() {
                continue;
            }
            let series_idx = series_labels.len();
            series_labels.push(hist.labels.clone());
            for (ts, snap) in hist.timestamps[r.clone()]
                .iter()
                .zip(hist.snapshots[r].iter())
            {
                rows.push(HistogramRow {
                    series_idx,
                    timestamp: *ts,
                    snapshot: crate::types::HistogramSnapshot {
                        index: snap.index.clone(),
                        count: snap.count.clone(),
                    },
                });
            }
        }

        let config = config?;
        if series_labels.is_empty() {
            return None;
        }
        rows.sort_unstable_by_key(|r| (r.timestamp, r.series_idx));

        Some(HistogramStream {
            meta: HistogramStreamMeta {
                config,
                series: series_labels,
            },
            rows: Box::new(rows.into_iter()),
        })
    }

    fn interval(&self) -> f64 {
        self.interval_ms as f64 / 1000.0
    }

    fn counter_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.counters.keys().cloned().collect();
        names.sort();
        names
    }

    fn gauge_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.gauges.keys().cloned().collect();
        names.sort();
        names
    }

    fn histogram_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.histograms.keys().cloned().collect();
        names.sort();
        names
    }

    fn counter_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        self.counters
            .get(name)
            .map(|series| series.iter().map(|c| c.labels.inner.clone()).collect())
            .unwrap_or_default()
    }

    fn gauge_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        self.gauges
            .get(name)
            .map(|series| series.iter().map(|g| g.labels.inner.clone()).collect())
            .unwrap_or_default()
    }

    fn histogram_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
        self.histograms
            .get(name)
            .map(|series| series.iter().map(|h| h.labels.inner.clone()).collect())
            .unwrap_or_default()
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

    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        let mut out: HashMap<String, HashMap<Labels, String>> = HashMap::new();
        for (name, series) in &self.counters {
            for s in series {
                out.entry(name.clone())
                    .or_default()
                    .insert(s.labels.clone(), name.clone());
            }
        }
        for (name, series) in &self.gauges {
            for s in series {
                out.entry(name.clone())
                    .or_default()
                    .insert(s.labels.clone(), name.clone());
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
