//! A source whose series carry extra labels: a recording's labels on every
//! series it holds.
//!
//! An archive holds one recording per producer of a run (a rezolus agent and
//! the service it measured, say), and a recording's labels (`source`,
//! `host`, `record --label`) are in the catalog, not on its series. A
//! dashboard template selects a producer's series by those labels
//! (`{source="vllm"}`), so the reader adds them to every series it returns.
//!
//! A series that already carries one of the added keys keeps its own value
//! under `exported_<key>`, as Prometheus does for a scraped target's labels
//! that collide with the target labels it adds (`honor_labels: false`): also
//! when the two values are equal, so two series that differ only in whether
//! they carried the key stay apart, and with a further `exported_` prefix
//! when the series also carries `exported_<key>`. A filter naming `<key>`
//! matches the added value, and one naming `exported_<key>` matches the
//! series' own.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::source::{ColumnPosition, CounterColumnRef, DataSource};
use crate::types::{ColumnChunk, CounterStream, Counters, Gauges};
use crate::{parquet, scan};

/// The prefix a series' own value of an added key moves under.
pub const EXPORTED_PREFIX: &str = "exported_";

/// The label added, with the recording's index in its catalog, to the
/// series of recordings whose label sets are identical, so their series stay
/// apart. Internal under the `__` rule ([`crate::is_internal_label`]).
pub const RECORDING_LABEL: &str = "__recording__";

/// `labels` with `extra` added: a key the series already carries keeps its
/// own value under `exported_<key>` (see the module docs).
pub fn add_labels(labels: &mut Labels, extra: &Labels) {
    for (k, v) in &extra.inner {
        let Some(own) = labels.inner.insert(k.clone(), v.clone()) else {
            continue;
        };
        // Move `own` to `exported_<k>`, and whatever was there one prefix
        // further, as far as the chain goes.
        let mut moving = own;
        let mut key = format!("{EXPORTED_PREFIX}{k}");
        while let Some(displaced) = labels.inner.insert(key.clone(), moving) {
            moving = displaced;
            key = format!("{EXPORTED_PREFIX}{key}");
        }
    }
}

/// [`add_labels`] on a plain map.
pub fn add_labels_map(labels: &mut BTreeMap<String, String>, extra: &Labels) {
    let mut wrapped = Labels {
        inner: std::mem::take(labels),
    };
    add_labels(&mut wrapped, extra);
    *labels = wrapped.inner;
}

/// The filter to pass to the source under `extra`, or `None` when `extra`
/// fails it, so the source has no matching series.
///
/// A matcher on an added key is decided here against the added value and
/// removed. A matcher on `exported_<key>` (any number of prefixes) for an
/// added key is removed too: what a series carries there depends on its own
/// labels after the move, so it is decided on the relabelled series
/// ([`keeps`]), and the source returns a superset.
pub fn translate_filter(extra: &Labels, filter: &Labels) -> Option<Labels> {
    if extra.inner.is_empty() {
        return Some(filter.clone());
    }
    let mut out = Labels::default();
    for (k, v) in &filter.inner {
        if extra.inner.contains_key(k) {
            let one = Labels {
                inner: BTreeMap::from([(k.clone(), v.clone())]),
            };
            if !extra.matches(&one) {
                return None;
            }
        } else if !is_exported_of(k, extra) {
            out.inner.insert(k.clone(), v.clone());
        }
    }
    Some(out)
}

/// Whether `key` is `exported_<k>`, with any number of prefixes, for a key
/// `extra` adds: a key [`add_labels`] may have moved values into.
fn is_exported_of(key: &str, extra: &Labels) -> bool {
    let mut rest = key;
    while let Some(stripped) = rest.strip_prefix(EXPORTED_PREFIX) {
        if extra.inner.contains_key(stripped) {
            return true;
        }
        rest = stripped;
    }
    false
}

/// Whether a relabelled series satisfies `filter`'s matchers on
/// `exported_<key>`, which [`translate_filter`] left to this.
pub fn keeps(labels: &Labels, extra: &Labels, filter: &Labels) -> bool {
    let exported = Labels {
        inner: filter
            .inner
            .iter()
            .filter(|(k, _)| is_exported_of(k, extra))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };
    exported.inner.is_empty() || labels.matches(&exported)
}

/// A [`DataSource`] whose series carry `extra` (see the module docs).
///
/// Every read path is delegated, including the column scan, so a query
/// through this costs what it costs on the source plus the label edits.
pub struct LabeledSource {
    inner: Arc<dyn DataSource>,
    extra: Labels,
    interval: Option<f64>,
}

impl LabeledSource {
    pub fn new(inner: Arc<dyn DataSource>, extra: Labels) -> Self {
        Self {
            inner,
            extra,
            interval: None,
        }
    }

    /// Report `interval` (seconds) rather than the source's own, hold its
    /// gauges for it ([`gauge_parts`](DataSource::gauge_parts)), and set it
    /// on each counter series it returns. A segment that
    /// declares no interval reports 1 s; pass the spacing measured from the
    /// table's rows.
    pub fn with_interval(mut self, interval: f64) -> Self {
        self.interval = Some(interval).filter(|i| *i > 0.0);
        self
    }

    /// The labels this adds.
    pub fn extra(&self) -> &Labels {
        &self.extra
    }

    fn relabel(&self, labels: &mut Labels) {
        add_labels(labels, &self.extra);
    }

    fn relabel_listing(
        &self,
        sets: Vec<BTreeMap<String, String>>,
    ) -> Vec<BTreeMap<String, String>> {
        sets.into_iter()
            .map(|mut set| {
                add_labels_map(&mut set, &self.extra);
                set
            })
            .collect()
    }
}

impl DataSource for LabeledSource {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Counters> {
        let inner_filter = translate_filter(&self.extra, filter)?;
        let mut counters = self.inner.counters(name, &inner_filter, start_ns, end_ns)?;
        for c in &mut counters.series {
            self.relabel(&mut c.labels);
        }
        counters
            .series
            .retain(|c| keeps(&c.labels, &self.extra, filter));
        (!counters.series.is_empty()).then_some(counters)
    }

    fn counter_streams<'s>(
        &'s self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<CounterStream<'s>>> {
        let inner_filter = translate_filter(&self.extra, filter)?;
        let mut streams = self
            .inner
            .counter_streams(name, &inner_filter, start_ns, end_ns)?;
        for s in &mut streams {
            self.relabel(&mut s.labels);
            if self.interval.is_some() {
                s.interval = self.interval;
            }
        }
        streams.retain(|s| keeps(&s.labels, &self.extra, filter));
        (!streams.is_empty()).then_some(streams)
    }

    fn counter_column_refs(&self) -> Vec<CounterColumnRef> {
        let mut refs = self.inner.counter_column_refs();
        for r in &mut refs {
            self.relabel(&mut r.labels);
        }
        refs
    }

    fn counter_column(
        &self,
        at: &ColumnPosition,
        start_ns: u64,
        end_ns: u64,
        selective: bool,
    ) -> Option<ColumnChunk> {
        self.inner.counter_column(at, start_ns, end_ns, selective)
    }

    fn batch_columns(
        &self,
        cols: &[usize],
        start_ns: u64,
        end_ns: u64,
    ) -> Option<parquet::BatchColumns> {
        self.inner.batch_columns(cols, start_ns, end_ns)
    }

    fn counter_scan(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<scan::CounterScan<'_>> {
        let inner_filter = translate_filter(&self.extra, filter)?;
        let mut scan = self
            .inner
            .counter_scan(name, &inner_filter, start_ns, end_ns)?;
        for s in &mut scan.series {
            self.relabel(&mut s.labels);
            if self.interval.is_some() {
                s.interval = self.interval;
            }
        }
        // A scan's series are positions in its chunks, so one cannot be
        // dropped here; a filter that needs the exact `exported_` check
        // takes the per-series path instead.
        if scan
            .series
            .iter()
            .any(|s| !keeps(&s.labels, &self.extra, filter))
        {
            return None;
        }
        Some(scan)
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        let inner_filter = translate_filter(&self.extra, filter)?;
        let mut gauges = self.inner.gauges(name, &inner_filter, start_ns, end_ns)?;
        for g in &mut gauges.series {
            self.relabel(&mut g.labels);
        }
        gauges
            .series
            .retain(|g| keeps(&g.labels, &self.extra, filter));
        (!gauges.series.is_empty()).then_some(gauges)
    }

    /// With an interval set, the gauges as one part at that interval; else
    /// the source's parts, relabelled.
    fn gauge_parts(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
    ) -> Option<Vec<(Gauges, f64)>> {
        if let Some(interval) = self.interval {
            let lookback_ns = step_ns.max((interval * 1e9) as u64);
            let gauges = self.gauges(name, filter, start_ns.saturating_sub(lookback_ns), end_ns)?;
            return Some(vec![(gauges, interval)]);
        }
        let inner_filter = translate_filter(&self.extra, filter)?;
        let mut parts = self
            .inner
            .gauge_parts(name, &inner_filter, start_ns, end_ns, step_ns)?;
        for (gauges, _) in &mut parts {
            for g in &mut gauges.series {
                self.relabel(&mut g.labels);
            }
            gauges
                .series
                .retain(|g| keeps(&g.labels, &self.extra, filter));
        }
        parts.retain(|(g, _)| !g.series.is_empty());
        (!parts.is_empty()).then_some(parts)
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let inner_filter = translate_filter(&self.extra, filter)?;
        let mut stream = self
            .inner
            .histogram_stream(name, &inner_filter, start_ns, end_ns)?;
        for labels in &mut stream.meta.series {
            self.relabel(labels);
        }
        // Rows refer to series by index, so a series cannot be dropped from
        // the meta; drop its rows instead.
        let dropped: Vec<bool> = stream
            .meta
            .series
            .iter()
            .map(|l| !keeps(l, &self.extra, filter))
            .collect();
        if dropped.iter().all(|d| *d) {
            return None;
        }
        if dropped.iter().any(|d| *d) {
            let rows = stream.rows;
            stream.rows = Box::new(rows.filter(move |r| !dropped[r.series_idx]));
        }
        Some(stream)
    }

    fn interval(&self) -> f64 {
        self.interval.unwrap_or_else(|| self.inner.interval())
    }

    fn histogram_interval(&self, name: &str) -> f64 {
        self.interval
            .unwrap_or_else(|| self.inner.histogram_interval(name))
    }

    fn counter_interval(&self, name: &str) -> f64 {
        self.interval
            .unwrap_or_else(|| self.inner.counter_interval(name))
    }

    fn gauge_interval(&self, name: &str) -> f64 {
        self.interval
            .unwrap_or_else(|| self.inner.gauge_interval(name))
    }

    /// The keys this adds, and the source's own.
    fn recording_label_keys(&self) -> Vec<String> {
        let mut keys = self.inner.recording_label_keys();
        keys.extend(self.extra.inner.keys().cloned());
        keys.sort();
        keys.dedup();
        keys
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

    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.relabel_listing(self.inner.counter_labels(name))
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.relabel_listing(self.inner.gauge_labels(name))
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.relabel_listing(self.inner.histogram_labels(name))
    }

    fn file_metadata(&self) -> HashMap<String, String> {
        self.inner.file_metadata()
    }

    fn metadata_get(&self, key: &str) -> Option<String> {
        self.inner.metadata_get(key)
    }

    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        self.inner
            .column_map()
            .into_iter()
            .map(|(name, by_labels)| {
                let by_labels = by_labels
                    .into_iter()
                    .map(|(mut labels, column)| {
                        self.relabel(&mut labels);
                        (labels, column)
                    })
                    .collect();
                (name, by_labels)
            })
            .collect()
    }

    fn sample_timestamps(&self) -> Vec<u64> {
        self.inner.sample_timestamps()
    }

    fn columns_desc(&self) -> Vec<parquet::ColDesc> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::histogram_stream::{HistogramRow, HistogramStreamMeta};
    use crate::scan::{ChunkReader, CounterScan, ScanChunk, ScanError, ScanSeries};
    use crate::types::{Counter, HistogramSnapshot};

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        Labels {
            inner: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn a_clashing_key_keeps_the_series_value_under_exported() {
        let extra = labels(&[("host", "lb"), ("source", "svc")]);
        let mut own = labels(&[("host", "backend-1"), ("id", "0")]);
        add_labels(&mut own, &extra);
        assert_eq!(
            own,
            labels(&[
                ("exported_host", "backend-1"),
                ("host", "lb"),
                ("id", "0"),
                ("source", "svc")
            ])
        );
        // Equal values move too, so this series stays apart from one that
        // carried no host.
        let mut same = labels(&[("host", "lb")]);
        add_labels(&mut same, &extra);
        assert_eq!(
            same,
            labels(&[("exported_host", "lb"), ("host", "lb"), ("source", "svc")])
        );
        // A series' own `exported_host` moves one prefix further.
        let mut chained = labels(&[("host", "a"), ("exported_host", "x")]);
        add_labels(&mut chained, &extra);
        assert_eq!(
            chained,
            labels(&[
                ("exported_exported_host", "x"),
                ("exported_host", "a"),
                ("host", "lb"),
                ("source", "svc")
            ])
        );
    }

    #[test]
    fn a_filter_on_an_added_key_is_decided_here() {
        let extra = labels(&[("source", "svc")]);
        assert_eq!(
            translate_filter(&extra, &labels(&[("source", "svc"), ("id", "0")])),
            Some(labels(&[("id", "0")]))
        );
        assert_eq!(translate_filter(&extra, &labels(&[("source", "db")])), None);
        assert_eq!(
            translate_filter(&extra, &labels(&[("source", "!db")])),
            Some(Labels::default())
        );
        assert_eq!(
            translate_filter(&extra, &labels(&[("source", "!svc")])),
            None
        );
        assert_eq!(
            translate_filter(&extra, &labels(&[("source", "~db|svc")])),
            Some(Labels::default())
        );
        // `exported_source` is left to `keeps`, on the relabelled series.
        assert_eq!(
            translate_filter(&extra, &labels(&[("exported_source", "x")])),
            Some(Labels::default())
        );
        // A key nobody adds passes through, `exported_` or not.
        assert_eq!(
            translate_filter(&extra, &labels(&[("exported_id", "1")])),
            Some(labels(&[("exported_id", "1")]))
        );
    }

    /// A source with two counter series and a two-series histogram, whose
    /// own `host` differs on one of them.
    struct Fake;

    impl Fake {
        fn all() -> [Labels; 2] {
            [labels(&[("host", "backend-1")]), labels(&[("host", "lb")])]
        }
    }

    struct NoChunks;
    impl ChunkReader for NoChunks {
        fn next_chunk(&mut self, _: usize) -> Result<Option<ScanChunk>, ScanError> {
            Ok(None)
        }
    }

    impl DataSource for Fake {
        fn counters(&self, _: &str, filter: &Labels, _: u64, _: u64) -> Option<Counters> {
            let series: Vec<Counter> = Self::all()
                .into_iter()
                .filter(|l| l.matches(filter))
                .map(|labels| Counter {
                    labels,
                    timestamps: vec![1],
                    values: vec![1],
                    windows: None,
                })
                .collect();
            (!series.is_empty()).then_some(Counters { series })
        }
        fn counter_scan(
            &self,
            _: &str,
            filter: &Labels,
            _: u64,
            _: u64,
        ) -> Option<CounterScan<'_>> {
            let series = Self::all()
                .into_iter()
                .filter(|l| l.matches(filter))
                .map(|labels| ScanSeries {
                    labels,
                    windowed: false,
                    interval: None,
                })
                .collect();
            Some(CounterScan::new(series, Box::new(NoChunks)))
        }
        fn gauges(&self, _: &str, _: &Labels, _: u64, _: u64) -> Option<Gauges> {
            None
        }
        fn histogram_stream(
            &self,
            _: &str,
            filter: &Labels,
            _: u64,
            _: u64,
        ) -> Option<HistogramStream> {
            let series: Vec<Labels> = Self::all()
                .into_iter()
                .filter(|l| l.matches(filter))
                .collect();
            let n = series.len();
            let rows = (0..n).map(|i| HistogramRow {
                series_idx: i,
                timestamp: 1,
                snapshot: HistogramSnapshot {
                    index: vec![],
                    count: vec![],
                },
            });
            Some(HistogramStream {
                meta: HistogramStreamMeta {
                    config: ::histogram::Config::new(2, 8).unwrap(),
                    series,
                },
                rows: Box::new(rows.collect::<Vec<_>>().into_iter()),
            })
        }
        fn interval(&self) -> f64 {
            1.0
        }
        fn time_range(&self) -> Option<(u64, u64)> {
            None
        }
        fn counter_names(&self) -> Vec<String> {
            vec!["c".into()]
        }
        fn gauge_names(&self) -> Vec<String> {
            vec![]
        }
        fn histogram_names(&self) -> Vec<String> {
            vec!["h".into()]
        }
        fn counter_labels(&self, _: &str) -> Vec<BTreeMap<String, String>> {
            Self::all().into_iter().map(|l| l.inner).collect()
        }
        fn gauge_labels(&self, _: &str) -> Vec<BTreeMap<String, String>> {
            vec![]
        }
        fn histogram_labels(&self, _: &str) -> Vec<BTreeMap<String, String>> {
            vec![]
        }
        fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
            HashMap::new()
        }
    }

    fn labeled() -> LabeledSource {
        LabeledSource::new(Arc::new(Fake), labels(&[("host", "lb"), ("source", "svc")]))
    }

    fn hosts(sets: impl IntoIterator<Item = Labels>) -> Vec<Option<String>> {
        sets.into_iter()
            .map(|l| l.inner.get("exported_host").cloned())
            .collect()
    }

    #[test]
    fn series_carry_the_labels_on_every_read_path() {
        let s = labeled();
        let c = s.counters("c", &Labels::default(), 0, 9).unwrap();
        assert_eq!(
            hosts(c.series.into_iter().map(|c| c.labels)),
            [Some("backend-1".to_string()), Some("lb".to_string())]
        );
        let scan = s.counter_scan("c", &Labels::default(), 0, 9).unwrap();
        assert!(scan
            .series
            .iter()
            .all(|x| x.labels.inner["source"] == "svc"));
        let h = s.histogram_stream("h", &Labels::default(), 0, 9).unwrap();
        assert!(h.meta.series.iter().all(|l| l.inner["host"] == "lb"));
        assert_eq!(h.rows.count(), 2);
        assert_eq!(
            s.counter_labels("c"),
            [
                labels(&[
                    ("exported_host", "backend-1"),
                    ("host", "lb"),
                    ("source", "svc")
                ])
                .inner,
                labels(&[("exported_host", "lb"), ("host", "lb"), ("source", "svc")]).inner,
            ]
        );
    }

    /// With an interval, every counter series returned carries it, on the
    /// stream and the scan path; without, none does.
    #[test]
    fn counter_series_carry_the_interval() {
        let timed = labeled().with_interval(10.0);
        let streams = timed
            .counter_streams("c", &Labels::default(), 0, 9)
            .unwrap();
        assert!(streams.iter().all(|s| s.interval == Some(10.0)));
        let scan = timed.counter_scan("c", &Labels::default(), 0, 9).unwrap();
        assert!(scan.series.iter().all(|s| s.interval == Some(10.0)));
        let plain = labeled();
        let streams = plain
            .counter_streams("c", &Labels::default(), 0, 9)
            .unwrap();
        assert!(streams.iter().all(|s| s.interval.is_none()));
    }

    /// `exported_` matchers are decided on the relabelled series.
    #[test]
    fn exported_matchers_are_decided_on_the_relabelled_series() {
        let s = labeled();
        let not_backend = labels(&[("exported_host", "!backend-1")]);
        let c = s.counters("c", &not_backend, 0, 9).unwrap();
        assert_eq!(
            hosts(c.series.into_iter().map(|c| c.labels)),
            [Some("lb".to_string())]
        );
        // A deeper prefix is decided on the relabelled series too.
        assert!(s
            .counters("c", &labels(&[("exported_exported_host", "x")]), 0, 9)
            .is_none());

        let backend = labels(&[("exported_host", "backend-1")]);
        let c = s.counters("c", &backend, 0, 9).unwrap();
        assert_eq!(
            hosts(c.series.into_iter().map(|c| c.labels)),
            [Some("backend-1".to_string())]
        );

        // A scan cannot drop a series, so it declines and the per-series
        // path answers.
        assert!(s.counter_scan("c", &backend, 0, 9).is_none());
        assert!(s.counter_scan("c", &Labels::default(), 0, 9).is_some());

        // A histogram keeps its series list and drops the rows.
        let h = s.histogram_stream("h", &backend, 0, 9).unwrap();
        assert_eq!(h.meta.series.len(), 2);
        let rows: Vec<usize> = h.rows.map(|r| r.series_idx).collect();
        assert_eq!(rows, [0]);

        assert!(s
            .counters("c", &labels(&[("source", "db")]), 0, 9)
            .is_none());
    }
}
