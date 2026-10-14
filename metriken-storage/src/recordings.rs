//! [`RecordingsSource`]: the series of several recordings as one source.
//!
//! Each child is one recording's source, its series carrying that
//! recording's labels ([`LabeledSource`](crate::labeled::LabeledSource)), so
//! the same metric in two recordings is two sets of series told apart by
//! their labels. A read of a name concatenates every child's series of that
//! name; nothing is joined or spliced. What differs between recordings is
//! kept per child where the query engine needs it:
//!
//! - A gauge is held for its own recording's interval
//!   ([`gauge_parts`](DataSource::gauge_parts)).
//! - A histogram's rows are pooled on the slowest recording's row times
//!   ([`HistogramStream::merge_on_clock`]), since two recordings' rows fall
//!   at different instants.
//! - The recordings' label keys are reported
//!   ([`recording_label_keys`](DataSource::recording_label_keys)), so a
//!   binary operator's broadcast pairs series of one recording only.
//!
//! [`UnionSource`](crate::union::UnionSource) is the other composition: it
//! joins tables holding different names of one recording, and refuses a
//! name two of them hold.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::source::DataSource;
use crate::types::{CounterStream, Counters, Gauges};

/// Several recordings' sources as one; see the module docs.
pub struct RecordingsSource {
    children: Vec<Arc<dyn DataSource>>,
}

impl RecordingsSource {
    pub fn new(children: Vec<Arc<dyn DataSource>>) -> Self {
        Self { children }
    }

    fn names(&self, of: impl Fn(&dyn DataSource) -> Vec<String>) -> Vec<String> {
        let names: BTreeSet<String> = self.children.iter().flat_map(|c| of(c.as_ref())).collect();
        names.into_iter().collect()
    }

    fn listing(
        &self,
        of: impl Fn(&dyn DataSource) -> Vec<BTreeMap<String, String>>,
    ) -> Vec<BTreeMap<String, String>> {
        self.children.iter().flat_map(|c| of(c.as_ref())).collect()
    }
}

/// `Some` of every child's answer concatenated, or `None` when no child
/// answered.
fn concat<T>(answers: impl Iterator<Item = Option<Vec<T>>>) -> Option<Vec<T>> {
    answers.flatten().reduce(|mut all, more| {
        all.extend(more);
        all
    })
}

impl DataSource for RecordingsSource {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Counters> {
        let series = concat(
            self.children
                .iter()
                .map(|c| c.counters(name, filter, start_ns, end_ns).map(|c| c.series)),
        )?;
        Some(Counters { series })
    }

    fn counter_streams<'s>(
        &'s self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<CounterStream<'s>>> {
        concat(
            self.children
                .iter()
                .map(|c| c.counter_streams(name, filter, start_ns, end_ns)),
        )
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        let series = concat(
            self.children
                .iter()
                .map(|c| c.gauges(name, filter, start_ns, end_ns).map(|g| g.series)),
        )?;
        Some(Gauges { series })
    }

    /// Each recording's parts, at its own interval.
    fn gauge_parts(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
    ) -> Option<Vec<(Gauges, f64)>> {
        concat(
            self.children
                .iter()
                .map(|c| c.gauge_parts(name, filter, start_ns, end_ns, step_ns)),
        )
    }

    /// Pooled on the row times of the recording with the longest interval.
    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let mut streams = Vec::new();
        let mut intervals = Vec::new();
        for c in &self.children {
            if let Some(s) = c.histogram_stream(name, filter, start_ns, end_ns) {
                streams.push(s);
                intervals.push(c.interval());
            }
        }
        let clock = intervals
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)?;
        HistogramStream::merge_on_clock(streams, clock)
    }

    /// The shortest of the recordings' intervals.
    fn interval(&self) -> f64 {
        self.children
            .iter()
            .map(|c| c.interval())
            .filter(|i| *i > 0.0)
            .min_by(f64::total_cmp)
            .unwrap_or(1.0)
    }

    fn time_range(&self) -> Option<(u64, u64)> {
        self.children
            .iter()
            .filter_map(|c| c.time_range())
            .reduce(|(a0, a1), (b0, b1)| (a0.min(b0), a1.max(b1)))
    }

    fn counter_names(&self) -> Vec<String> {
        self.names(|c| c.counter_names())
    }

    fn gauge_names(&self) -> Vec<String> {
        self.names(|c| c.gauge_names())
    }

    fn histogram_names(&self) -> Vec<String> {
        self.names(|c| c.histogram_names())
    }

    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.listing(|c| c.counter_labels(name))
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.listing(|c| c.gauge_labels(name))
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.listing(|c| c.histogram_labels(name))
    }

    fn recording_label_keys(&self) -> Vec<String> {
        self.names(|c| c.recording_label_keys())
    }

    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        let mut out: HashMap<String, HashMap<Labels, String>> = HashMap::new();
        for c in &self.children {
            for (name, by_labels) in c.column_map() {
                out.entry(name).or_default().extend(by_labels);
            }
        }
        out
    }

    fn column_count(&self) -> usize {
        self.children.iter().map(|c| c.column_count()).sum()
    }

    fn resident_bytes(&self) -> usize {
        self.children.iter().map(|c| c.resident_bytes()).sum()
    }

    fn series_count(&self) -> usize {
        self.children.iter().map(|c| c.series_count()).sum()
    }
}
