//! A series' samples in time order: what every evaluator walking a series
//! assumes (`rate`, `irate`, `deriv`, `*_over_time`, a histogram's deltas).
//!
//! A source returns a series' samples in increasing time. Where it does not,
//! the series is two or more stored series read as one: columns of one
//! table with one label set, which `DataSource::duplicated_series` reports
//! only when the footer's null counts prove they share rows. [`Ordered`]
//! wraps a query's sources and checks every series it returns: a sample at
//! or before the one before it in the same series is recorded in the
//! query's [`Disorder`], the series ends there, and the query returns the
//! error rather than its result.
//!
//! What is checked is each series as the source returns it: one `Counter`
//! or `Gauge`, one counter stream, one label set of a histogram stream
//! (whose rows are in time order, so two columns of one label set are found
//! only where they share a time). A batched counter scan is not checked:
//! the segmented reader gives none for a series with two columns in one
//! segment, so the engine reads that counter per series. A
//! composite source that returns two entries of one label set returns two
//! series, each checked alone.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use crate::histogram_stream::{HistogramRow, HistogramStream};
use crate::labels::Labels;
use crate::promql::QueryError;
use crate::types::{CounterSample, CounterStream, Counters, Gauges};
use crate::DataSource;

/// The first series a query read out of time order.
#[derive(Default)]
pub(crate) struct Disorder {
    first: Mutex<Option<QueryError>>,
}

impl Disorder {
    /// Series `name` with `labels` has a sample at `ts`, at or before the
    /// one before it.
    fn record(&self, name: &str, labels: &Labels, ts: u64) {
        let mut first = self.first.lock().expect("not poisoned");
        if first.is_none() {
            *first = Some(crate::promql::joined_series(
                name,
                labels.inner.iter(),
                &format!(
                    "has a sample at {ts} ns at or before the one read before it, so its \
                     samples are of two or more stored series read as one"
                ),
            ));
        }
    }

    /// The error, if a series was read out of order.
    pub(crate) fn take(&self) -> Option<QueryError> {
        self.first.lock().expect("not poisoned").take()
    }

    /// The first sample of `ts` at or before the one before it.
    fn check(&self, name: &str, labels: &Labels, ts: &[u64]) -> bool {
        match ts.windows(2).find(|w| w[1] <= w[0]) {
            Some(w) => {
                self.record(name, labels, w[1]);
                false
            }
            None => true,
        }
    }
}

/// `inner`, with every series it returns checked for time order into
/// `disorder` (see the module docs).
pub(crate) struct Ordered {
    inner: Arc<dyn DataSource>,
    disorder: Arc<Disorder>,
}

impl Ordered {
    pub(crate) fn new(inner: Arc<dyn DataSource>, disorder: Arc<Disorder>) -> Self {
        Self { inner, disorder }
    }

    fn histograms(&self, name: &str, stream: HistogramStream) -> HistogramStream {
        let HistogramStream { meta, rows } = stream;
        let labels = meta.series.clone();
        // Indices of one label set are one series to the readers, which
        // join them by labels: the first index of each set stands for it.
        let mut first: HashMap<&Labels, usize> = HashMap::new();
        let series: Vec<usize> = labels
            .iter()
            .enumerate()
            .map(|(i, l)| *first.entry(l).or_insert(i))
            .collect();
        HistogramStream {
            meta,
            rows: Box::new(OrderedRows {
                inner: rows,
                name: name.to_string(),
                labels,
                series,
                last: HashMap::new(),
                disorder: Arc::clone(&self.disorder),
                done: false,
            }),
        }
    }
}

/// A counter stream's samples, ended at the first out of order.
struct OrderedSamples<'a> {
    inner: Box<dyn Iterator<Item = CounterSample> + 'a>,
    name: String,
    labels: Labels,
    last: Option<u64>,
    disorder: Arc<Disorder>,
    done: bool,
}

impl Iterator for OrderedSamples<'_> {
    type Item = CounterSample;

    fn next(&mut self) -> Option<CounterSample> {
        if self.done {
            return None;
        }
        let sample = self.inner.next()?;
        if self.last.is_some_and(|last| sample.ts <= last) {
            self.disorder.record(&self.name, &self.labels, sample.ts);
            self.done = true;
            return None;
        }
        self.last = Some(sample.ts);
        Some(sample)
    }
}

/// A histogram stream's rows, ended at the first row of a series before
/// that series' previous row, or at its time from another index. A series
/// is a label set: a stream can list one label set under two indices (two
/// columns of one segment), which the readers join.
struct OrderedRows {
    inner: Box<dyn Iterator<Item = HistogramRow> + Send>,
    name: String,
    labels: Vec<Labels>,
    /// Per series index, the first index of its label set.
    series: Vec<usize>,
    /// Per label set (its first index), its last row's time and index.
    last: HashMap<usize, (u64, usize)>,
    disorder: Arc<Disorder>,
    done: bool,
}

impl Iterator for OrderedRows {
    type Item = HistogramRow;

    fn next(&mut self) -> Option<HistogramRow> {
        if self.done {
            return None;
        }
        let row = self.inner.next()?;
        let series = self
            .series
            .get(row.series_idx)
            .copied()
            .unwrap_or(row.series_idx);
        if let Some((ts, idx)) = self.last.insert(series, (row.timestamp, row.series_idx)) {
            // One index at one time twice is a composite's pooling onto
            // another recording's clock (`HistogramStream::merge_on_clock`),
            // or a merge of a process group's sources, which the readers
            // take; two indices of one label set at one time are two
            // columns. A segmented reader's splice keeps a segment's two
            // columns at one time under two indices
            // (`splice_histogram_streams`), so this sees them across
            // segments too.
            if row.timestamp < ts || (row.timestamp == ts && row.series_idx != idx) {
                let labels = self.labels.get(row.series_idx).cloned().unwrap_or_default();
                self.disorder.record(&self.name, &labels, row.timestamp);
                self.done = true;
                return None;
            }
        }
        Some(row)
    }
}

impl DataSource for Ordered {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Counters> {
        let mut counters = self.inner.counters(name, filter, start_ns, end_ns)?;
        counters
            .series
            .retain(|c| self.disorder.check(name, &c.labels, &c.timestamps));
        Some(counters)
    }
    fn counter_streams<'s>(
        &'s self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<CounterStream<'s>>> {
        let streams = self.inner.counter_streams(name, filter, start_ns, end_ns)?;
        Some(
            streams
                .into_iter()
                .map(|s| CounterStream {
                    samples: Box::new(OrderedSamples {
                        inner: s.samples,
                        name: name.to_string(),
                        labels: s.labels.clone(),
                        last: None,
                        disorder: Arc::clone(&self.disorder),
                        done: false,
                    }),
                    labels: s.labels,
                    windowed: s.windowed,
                    float: s.float,
                    interval: s.interval,
                })
                .collect(),
        )
    }
    fn counter_column_refs(&self) -> Vec<metriken_storage::CounterColumnRef> {
        self.inner.counter_column_refs()
    }
    fn counter_column(
        &self,
        at: &metriken_storage::ColumnPosition,
        start_ns: u64,
        end_ns: u64,
        selective: bool,
    ) -> Option<crate::types::ColumnChunk> {
        self.inner.counter_column(at, start_ns, end_ns, selective)
    }
    fn batch_columns(
        &self,
        cols: &[usize],
        start_ns: u64,
        end_ns: u64,
    ) -> Option<metriken_storage::parquet::BatchColumns> {
        self.inner.batch_columns(cols, start_ns, end_ns)
    }
    fn counter_scan(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<crate::scan::CounterScan<'_>> {
        // Not checked: the segmented reader returns no scan for a series
        // with two columns in one segment (`SegmentedSource::scan_counters`),
        // and the engine then reads it per series, through
        // `counter_streams`, which is checked.
        self.inner.counter_scan(name, filter, start_ns, end_ns)
    }
    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        let mut gauges = self.inner.gauges(name, filter, start_ns, end_ns)?;
        gauges
            .series
            .retain(|g| self.disorder.check(name, &g.labels, &g.timestamps));
        Some(gauges)
    }
    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let stream = self
            .inner
            .histogram_stream(name, filter, start_ns, end_ns)?;
        Some(self.histograms(name, stream))
    }
    fn gauge_histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let stream = self
            .inner
            .gauge_histogram_stream(name, filter, start_ns, end_ns)?;
        Some(self.histograms(name, stream))
    }
    fn gauge_histogram_interval(&self, name: &str) -> f64 {
        self.inner.gauge_histogram_interval(name)
    }
    fn gauge_histogram_parts(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<(HistogramStream, f64)>> {
        let parts = self
            .inner
            .gauge_histogram_parts(name, filter, start_ns, end_ns)?;
        Some(
            parts
                .into_iter()
                .map(|(stream, interval)| (self.histograms(name, stream), interval))
                .collect(),
        )
    }
    fn interval(&self) -> f64 {
        self.inner.interval()
    }
    fn histogram_interval(&self, name: &str) -> f64 {
        self.inner.histogram_interval(name)
    }
    fn counter_interval(&self, name: &str) -> f64 {
        self.inner.counter_interval(name)
    }
    fn gauge_interval(&self, name: &str) -> f64 {
        self.inner.gauge_interval(name)
    }
    fn time_range(&self) -> Option<(u64, u64)> {
        self.inner.time_range()
    }
    fn counter_names(&self) -> Vec<String> {
        self.inner.counter_names()
    }
    fn gauge_names(&self) -> Vec<String> {
        self.inner.gauge_names()
    }
    fn histogram_names(&self) -> Vec<String> {
        self.inner.histogram_names()
    }
    fn gauge_histogram_names(&self) -> Vec<String> {
        self.inner.gauge_histogram_names()
    }
    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.inner.counter_labels(name)
    }
    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.inner.gauge_labels(name)
    }
    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.inner.histogram_labels(name)
    }
    fn gauge_histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.inner.gauge_histogram_labels(name)
    }
    fn duplicated_series(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.inner.duplicated_series(name)
    }
    fn file_metadata(&self) -> HashMap<String, String> {
        self.inner.file_metadata()
    }
    fn metadata_get(&self, key: &str) -> Option<String> {
        self.inner.metadata_get(key)
    }
    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        self.inner.column_map()
    }
    fn sample_timestamps(&self) -> Vec<u64> {
        self.inner.sample_timestamps()
    }
    fn recording_label_keys(&self) -> Vec<String> {
        self.inner.recording_label_keys()
    }
    fn gauge_parts(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
    ) -> Option<Vec<(Gauges, f64)>> {
        let mut parts = self
            .inner
            .gauge_parts(name, filter, start_ns, end_ns, step_ns)?;
        for (gauges, _) in &mut parts {
            gauges
                .series
                .retain(|g| self.disorder.check(name, &g.labels, &g.timestamps));
        }
        Some(parts)
    }
    fn columns_desc(&self) -> Vec<metriken_storage::parquet::ColDesc> {
        self.inner.columns_desc()
    }
    fn column_count(&self) -> usize {
        self.inner.column_count()
    }
    fn resident_bytes(&self) -> usize {
        self.inner.resident_bytes()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
}
