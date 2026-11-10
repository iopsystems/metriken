use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::histogram_stream::{HistogramRow, HistogramStream, HistogramStreamMeta, Overflow};
use crate::labels::Labels;
#[cfg(feature = "ingest")]
use crate::types::HistogramSnapshot;
use crate::types::{Counter, Counters, Gauge, Gauges, Histogram};
use crate::DataSource;

#[cfg(test)]
std::thread_local! {
    /// Counter series [`Memory::counter_series`] has yielded on this thread.
    static COUNTER_SERIES_VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One histogram series' overflow links by row timestamp: the overflow
/// counter's name, shared by the series' rows, and the highest finite
/// bound.
type RowLinks = BTreeMap<u64, (Arc<str>, f64)>;

/// An in-memory data source: the live store behind metriken-query's
/// `MemoryStore`, which ingests snapshots into it, and a source tests build
/// from whole series. Callers
/// either upsert samples or hand in `Counter`/`Gauge`/`Histogram` values
/// directly; the source handles time-range filtering on query.
pub struct Memory {
    counters: HashMap<String, Vec<Counter>>,
    gauges: HashMap<String, Vec<Gauge>>,
    histograms: HashMap<String, Vec<Histogram>>,
    /// Gauge histogram series: stored as histograms are, listed and read
    /// apart (`DataSource::gauge_histogram_stream`).
    gauge_histograms: HashMap<String, Vec<Histogram>>,
    /// Per histogram name, series labels and row timestamp: the row's
    /// overflow counter's name and highest finite bound
    /// ([`set_histogram_overflow`](Self::set_histogram_overflow)). A row
    /// with no entry has no overflow.
    histogram_overflows: HashMap<String, HashMap<Labels, RowLinks>>,
    interval_ms: u64,
    /// The row timestamps a caller declared, if it did. See
    /// [`DataSource::sample_timestamps`]; a source assembled from a table
    /// knows its rows, and a series-by-series union would not recover a row
    /// that every series skipped.
    sample_timestamps: Option<Vec<u64>>,
}

impl Memory {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            counters: HashMap::new(),
            gauges: HashMap::new(),
            histograms: HashMap::new(),
            gauge_histograms: HashMap::new(),
            histogram_overflows: HashMap::new(),
            interval_ms,
            sample_timestamps: None,
        }
    }

    /// Add a whole counter series, windows included. A second series with
    /// the same name and labels is kept as a second series — this is not
    /// the sample-level upsert `ingest` does.
    pub fn push_counter_series(&mut self, name: &str, counter: Counter) {
        self.counters
            .entry(name.to_string())
            .or_default()
            .push(counter);
    }

    pub fn push_gauge_series(&mut self, name: &str, gauge: Gauge) {
        self.gauges.entry(name.to_string()).or_default().push(gauge);
    }

    pub fn push_histogram_series(&mut self, name: &str, histogram: Histogram) {
        self.histograms
            .entry(name.to_string())
            .or_default()
            .push(histogram);
    }

    /// Add a whole gauge histogram series: each snapshot is the state at its
    /// timestamp (stored in the cumulative sparse form a histogram's is).
    pub fn push_gauge_histogram_series(&mut self, name: &str, histogram: Histogram) {
        self.gauge_histograms
            .entry(name.to_string())
            .or_default()
            .push(histogram);
    }

    /// Name the overflow series of histogram `name`'s series `labels` for
    /// its row at `ts`: the counter `overflow` with the same labels, whose
    /// value at `ts` is that row's overflow, above `bound`. What a histogram
    /// column's [`OVERFLOW_KEY`](crate::OVERFLOW_KEY) and
    /// [`OVERFLOW_BOUND_KEY`](crate::OVERFLOW_BOUND_KEY) say in a recording,
    /// for one row: a row named by no call has no overflow, as a recorded
    /// row of a column without the keys has none. A link is keyed by the
    /// row's timestamp, so two rows of the series at one timestamp share
    /// one link, the last set.
    pub fn set_histogram_overflow(
        &mut self,
        name: &str,
        labels: Labels,
        ts: u64,
        overflow: &str,
        bound: f64,
    ) {
        let links = self
            .histogram_overflows
            .entry(name.to_string())
            .or_default()
            .entry(labels)
            .or_default();
        // The series' rows share its counter's name.
        let counter = match links.values().next_back() {
            Some((last, _)) if **last == *overflow => Arc::clone(last),
            _ => Arc::from(overflow),
        };
        links.insert(ts, (counter, bound));
    }

    /// The series of counter `name`, each one counted in
    /// `COUNTER_SERIES_VISITED` (tests) as it is yielded, so a test can
    /// bound how many a read visits.
    fn counter_series<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a Counter> + 'a {
        self.counters.get(name).into_iter().flatten().inspect(|_| {
            #[cfg(test)]
            COUNTER_SERIES_VISITED.with(|v| v.set(v.get() + 1));
        })
    }

    /// A reader of histogram `name`'s rows' overflow, `(labels, ts)` to
    /// the row's [`Overflow`]: the row's link, and its counter's sample at
    /// `ts`. Each overflow counter's series are indexed by labels on first
    /// use, once per reader, so a read visits each counter series once
    /// rather than once per histogram row.
    fn overflow_reader<'a>(
        &'a self,
        name: &str,
    ) -> impl FnMut(&Labels, u64) -> Option<Overflow> + 'a {
        let links = self.histogram_overflows.get(name);
        let indexes: RefCell<HashMap<Arc<str>, HashMap<&'a Labels, &'a Counter>>> =
            RefCell::new(HashMap::new());
        move |labels, ts| {
            let (counter, bound) = links?.get(labels)?.get(&ts)?;
            let mut indexes = indexes.borrow_mut();
            let index = indexes.entry(Arc::clone(counter)).or_insert_with(|| {
                // The first series of a label set, as a search in order
                // would find.
                let mut index = HashMap::new();
                for c in self.counter_series(counter) {
                    index.entry(&c.labels).or_insert(c);
                }
                index
            });
            let series = index.get(labels)?;
            let i = series.timestamps.binary_search(&ts).ok()?;
            Some(Overflow {
                count: series.values.get(i)?.as_f64(),
                bound: *bound,
            })
        }
    }

    pub fn set_sample_timestamps(&mut self, ts: Vec<u64>) {
        self.sample_timestamps = Some(ts);
    }

    pub fn set_interval_ms(&mut self, ms: u64) {
        self.interval_ms = ms;
    }

    /// Adds a whole series, for tests that build a source in memory.
    #[doc(hidden)]
    pub fn add_counter(&mut self, name: &str, counter: Counter) {
        self.counters
            .entry(name.to_string())
            .or_default()
            .push(counter);
    }

    /// Adds a whole series, for tests that build a source in memory.
    #[doc(hidden)]
    pub fn add_gauge(&mut self, name: &str, gauge: Gauge) {
        self.gauges.entry(name.to_string()).or_default().push(gauge);
    }

    /// Adds a whole series, for tests that build a source in memory.
    #[doc(hidden)]
    pub fn add_histogram(&mut self, name: &str, histogram: Histogram) {
        self.histograms
            .entry(name.to_string())
            .or_default()
            .push(histogram);
    }

    /// Append a counter sample. If a series with matching (name, labels)
    /// exists, the sample is appended to it; otherwise a new series is created.
    #[cfg(feature = "ingest")]
    pub fn upsert_counter_sample(&mut self, name: &str, labels: Labels, ts: u64, value: u64) {
        let series = self.counters.entry(name.to_string()).or_default();
        if let Some(c) = series.iter_mut().find(|c| c.labels == labels) {
            c.timestamps.push(ts);
            c.values.push_int(value);
        } else {
            series.push(Counter {
                labels,
                timestamps: vec![ts],
                values: vec![value].into(),
                windows: None,
            });
        }
    }

    /// Append a gauge sample. If a series with matching (name, labels)
    /// exists, the sample is appended to it; otherwise a new series is created.
    #[cfg(feature = "ingest")]
    pub fn upsert_gauge_sample(&mut self, name: &str, labels: Labels, ts: u64, value: i64) {
        let series = self.gauges.entry(name.to_string()).or_default();
        if let Some(g) = series.iter_mut().find(|g| g.labels == labels) {
            g.timestamps.push(ts);
            g.values.push_int(value);
        } else {
            series.push(Gauge {
                labels,
                timestamps: vec![ts],
                values: vec![value].into(),
                windows: None,
            });
        }
    }

    /// Append a float counter sample, as
    /// [`upsert_counter_sample`](Self::upsert_counter_sample) appends an
    /// integer one. An integer series it is appended to becomes a float
    /// series (`CounterValues::push_float`). A NaN or infinite value is
    /// dropped.
    #[cfg(feature = "ingest")]
    pub fn upsert_float_counter_sample(&mut self, name: &str, labels: Labels, ts: u64, value: f64) {
        if !value.is_finite() {
            return;
        }
        let series = self.counters.entry(name.to_string()).or_default();
        if let Some(c) = series.iter_mut().find(|c| c.labels == labels) {
            c.timestamps.push(ts);
            c.values.push_float(value);
        } else {
            series.push(Counter {
                labels,
                timestamps: vec![ts],
                values: vec![value].into(),
                windows: None,
            });
        }
    }

    /// Append a float gauge sample, as
    /// [`upsert_gauge_sample`](Self::upsert_gauge_sample) appends an integer
    /// one. An integer series it is appended to becomes a float series. A
    /// NaN or infinite value is dropped.
    #[cfg(feature = "ingest")]
    pub fn upsert_float_gauge_sample(&mut self, name: &str, labels: Labels, ts: u64, value: f64) {
        if !value.is_finite() {
            return;
        }
        let series = self.gauges.entry(name.to_string()).or_default();
        if let Some(g) = series.iter_mut().find(|g| g.labels == labels) {
            g.timestamps.push(ts);
            g.values.push_float(value);
        } else {
            series.push(Gauge {
                labels,
                timestamps: vec![ts],
                values: vec![value].into(),
                windows: None,
            });
        }
    }

    /// Append a histogram sample. If a series with matching (name, labels)
    /// exists, the sample is appended to it; otherwise a new series is created.
    #[cfg(feature = "ingest")]
    pub fn upsert_histogram_sample(
        &mut self,
        name: &str,
        labels: Labels,
        config: ::histogram::Config,
        ts: u64,
        snapshot: HistogramSnapshot,
    ) {
        upsert_histogram(
            self.histograms.entry(name.to_string()).or_default(),
            labels,
            config,
            ts,
            snapshot,
        );
    }

    /// [`upsert_histogram_sample`](Self::upsert_histogram_sample) for a
    /// gauge histogram: `snapshot` is the state at `ts`.
    #[cfg(feature = "ingest")]
    pub fn upsert_gauge_histogram_sample(
        &mut self,
        name: &str,
        labels: Labels,
        config: ::histogram::Config,
        ts: u64,
        snapshot: HistogramSnapshot,
    ) {
        upsert_histogram(
            self.gauge_histograms.entry(name.to_string()).or_default(),
            labels,
            config,
            ts,
            snapshot,
        );
    }
}

#[cfg(feature = "ingest")]
fn upsert_histogram(
    series: &mut Vec<Histogram>,
    labels: Labels,
    config: ::histogram::Config,
    ts: u64,
    snapshot: HistogramSnapshot,
) {
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

/// The rows of `stored` (one metric's series) in `[start_ns, end_ns]` whose
/// labels match `filter`.
fn histogram_rows(
    stored: &[Histogram],
    filter: &Labels,
    start_ns: u64,
    end_ns: u64,
    mut overflow: impl FnMut(&Labels, u64) -> Option<Overflow>,
) -> Option<HistogramStream> {
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
                resumes: false,
                snapshot: crate::types::HistogramSnapshot {
                    index: snap.index.clone(),
                    count: snap.count.clone(),
                },
                overflow: overflow(&hist.labels, *ts),
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

/// Extract metric name and label set from snapshot metadata.
/// If `metadata` has a `"metric"` key, use it as the name; otherwise fall back
/// to `default_name` (the snapshot's `.name` field). The labels are everything
/// that is not a storage key — the same rule, from the same function, as the
/// parquet loader (`Labels::from_metadata`).
#[cfg(feature = "ingest")]
pub fn extract_name_labels(
    metadata: &std::collections::HashMap<String, String>,
    default_name: &str,
) -> (String, Labels) {
    let name = metadata
        .get("metric")
        .cloned()
        .unwrap_or_else(|| default_name.to_string());
    (name, Labels::from_metadata(metadata.iter()))
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
                    values: c.values.slice(r.clone()),
                    windows: c.windows.as_ref().map(|w| w[r].to_vec()),
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
                    values: g.values.slice(r.clone()),
                    windows: g.windows.as_ref().map(|w| w[r].to_vec()),
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
        histogram_rows(
            self.histograms.get(name)?,
            filter,
            start_ns,
            end_ns,
            self.overflow_reader(name),
        )
    }

    fn gauge_histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        histogram_rows(
            self.gauge_histograms.get(name)?,
            filter,
            start_ns,
            end_ns,
            |_, _| None,
        )
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

    fn gauge_histogram_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.gauge_histograms.keys().cloned().collect();
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

    fn gauge_histogram_labels(
        &self,
        name: &str,
    ) -> Vec<std::collections::BTreeMap<String, String>> {
        self.gauge_histograms
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
        for series in self
            .histograms
            .values()
            .chain(self.gauge_histograms.values())
            .flatten()
        {
            update(&mut min_ns, &mut max_ns, &series.timestamps);
        }
        min_ns.zip(max_ns)
    }

    fn sample_timestamps(&self) -> Vec<u64> {
        if let Some(ts) = &self.sample_timestamps {
            return ts.clone();
        }
        // Not declared: the union of every series' timestamps. A live store
        // fed sample by sample has no rows of its own, and this is what its
        // rows amount to.
        let mut out: Vec<u64> = Vec::new();
        for series in self.counters.values().flatten() {
            out.extend_from_slice(&series.timestamps);
        }
        for series in self.gauges.values().flatten() {
            out.extend_from_slice(&series.timestamps);
        }
        for series in self
            .histograms
            .values()
            .chain(self.gauge_histograms.values())
            .flatten()
        {
            out.extend_from_slice(&series.timestamps);
        }
        out.sort_unstable();
        out.dedup();
        out
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
        for (name, series) in self.histograms.iter().chain(&self.gauge_histograms) {
            for s in series {
                out.entry(name.clone())
                    .or_default()
                    .insert(s.labels.clone(), format!("{name}:buckets"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::HistogramSnapshot;

    /// `k` histogram series of `rows` rows each, every row linked to its
    /// series' overflow counter `h_overflow`, whose value is the row index.
    fn linked(k: u64, rows: u64) -> Memory {
        let mut m = Memory::new(1000);
        let config = ::histogram::Config::new(7, 16).unwrap();
        for s in 0..k {
            let labels = Labels::from(BTreeMap::from([("s".to_string(), s.to_string())]));
            let ts: Vec<u64> = (1..=rows).collect();
            m.push_histogram_series(
                "h",
                Histogram {
                    labels: labels.clone(),
                    config,
                    timestamps: ts.clone(),
                    snapshots: ts
                        .iter()
                        .map(|&t| HistogramSnapshot {
                            index: vec![1],
                            count: vec![t],
                        })
                        .collect(),
                },
            );
            m.push_counter_series(
                "h_overflow",
                Counter {
                    labels: labels.clone(),
                    timestamps: ts.clone(),
                    values: ts.iter().map(|&t| t as f64).collect::<Vec<_>>().into(),
                    windows: None,
                },
            );
            for &t in &ts {
                m.set_histogram_overflow("h", labels.clone(), t, "h_overflow", 10.0);
            }
        }
        m
    }

    /// A read of `k` linked series visits each overflow counter series once,
    /// however many rows each has: the counter's series are indexed once
    /// per read, not searched once per row (which visited k²·rows/2 here).
    #[test]
    fn a_read_visits_each_overflow_counter_series_once() {
        let (k, rows) = (50, 20);
        let m = linked(k, rows);
        COUNTER_SERIES_VISITED.with(|v| v.set(0));
        let stream = m
            .histogram_stream("h", &Labels::default(), 0, u64::MAX)
            .unwrap();
        let overflow: Vec<Option<Overflow>> = stream.rows.map(|r| r.overflow).collect();
        assert_eq!(overflow.len(), (k * rows) as usize);
        assert!(overflow.iter().all(|o| o.is_some()));
        assert_eq!(COUNTER_SERIES_VISITED.with(|v| v.get()), k as usize);
    }

    /// One histogram row at `t` with labels `labels`, linked to counter
    /// `counter`.
    fn one_row(m: &mut Memory, labels: &Labels, counter: &str, t: u64) {
        m.push_histogram_series(
            "h",
            Histogram {
                labels: labels.clone(),
                config: ::histogram::Config::new(7, 16).unwrap(),
                timestamps: vec![t],
                snapshots: vec![HistogramSnapshot {
                    index: vec![1],
                    count: vec![1],
                }],
            },
        );
        m.set_histogram_overflow("h", labels.clone(), t, counter, 10.0);
    }

    fn counter(m: &mut Memory, name: &str, labels: &Labels, t: u64, value: f64) {
        m.push_counter_series(
            name,
            Counter {
                labels: labels.clone(),
                timestamps: vec![t],
                values: vec![value].into(),
                windows: None,
            },
        );
    }

    fn overflows(m: &Memory) -> Vec<Option<f64>> {
        m.histogram_stream("h", &Labels::default(), 0, u64::MAX)
            .unwrap()
            .rows
            .map(|r| r.overflow.map(|o| o.count))
            .collect()
    }

    /// Of two overflow counter series with the same labels, the first is
    /// read, as a search of them in order would.
    #[test]
    fn of_two_counter_series_with_one_label_set_the_first_is_read() {
        let mut m = Memory::new(1000);
        let none = Labels::default();
        one_row(&mut m, &none, "h_overflow", 1);
        counter(&mut m, "h_overflow", &none, 1, 5.0);
        counter(&mut m, "h_overflow", &none, 1, 9.0);
        assert_eq!(overflows(&m), vec![Some(5.0)]);
    }

    /// Two series of one histogram linked to different counters each read
    /// their own.
    #[test]
    fn each_series_reads_its_own_counter() {
        let mut m = Memory::new(1000);
        let label = |v: &str| Labels::from(BTreeMap::from([("s".to_string(), v.to_string())]));
        let (a, b) = (label("a"), label("b"));
        one_row(&mut m, &a, "a_overflow", 1);
        one_row(&mut m, &b, "b_overflow", 1);
        counter(&mut m, "a_overflow", &a, 1, 3.0);
        counter(&mut m, "b_overflow", &b, 1, 7.0);
        assert_eq!(overflows(&m), vec![Some(3.0), Some(7.0)]);
    }

    /// The rows of one series share one allocation of the counter's name.
    #[test]
    fn a_series_rows_share_the_counter_name() {
        let m = linked(1, 5);
        let links: Vec<&Arc<str>> = m.histogram_overflows["h"]
            .values()
            .flat_map(|l| l.values().map(|(n, _)| n))
            .collect();
        assert_eq!(links.len(), 5);
        assert!(links.iter().all(|n| Arc::ptr_eq(n, links[0])));
    }
}
