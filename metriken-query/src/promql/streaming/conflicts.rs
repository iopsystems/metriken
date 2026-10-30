//! Conflicts within a process group (S4): two of its sources holding a row
//! of one series at one time, with one acquisition window, and different
//! values.
//!
//! Three readers find them, each adding to the query's [`Conflicts`]:
//!
//! - The batched scan of a group's counter (`rate`, `irate`) compares its
//!   members' rows as it reads them (`batch_rate::UnionStage`), holding a
//!   row only until every other source's member has read past its time.
//! - Every other read of a group's counters and gauges goes through a
//!   [`Recorded`] wrapper, which keeps the rows read (a per-series read, a
//!   counter stream, a scan, a gauge read) within the scope's span and from
//!   the first row of any other source of the group on. After the query,
//!   [`Conflicts::notices`] compares them per series across sources. The
//!   rows S3 reads only to decide a member's coverage are read without it.
//! - The group's histogram combine compares snapshots as it merges them
//!   ([`Conflicts::add`]), at times up to the read's end.
//!
//! Gauge histograms are not compared. The rows compared include those read
//! before the query's range: a range function's window, a selector's
//! staleness window, a histogram's previous rows.
//!
//! Only an archive's queries have process groups, so without the `archive`
//! feature `Recorded` is never built.
#![cfg_attr(not(feature = "archive"), allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::types::{CounterStream, Counters, Gauges};
use crate::DataSource;

/// A source of a process group, as a notice names it.
#[derive(Clone, Debug)]
pub(crate) struct SourceName {
    /// Its position among the reader's sources: two members at one
    /// position (one source's epochs) are never compared.
    pub position: usize,
    /// Its uuid, else `source <position>`.
    pub name: String,
}

/// What a series is to a conflict: its kind (rows of a counter and of a
/// gauge are not compared), metric name and labels.
type SeriesKey = (Kind, String, BTreeMap<String, String>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Kind {
    Counter,
    Gauge,
}

/// A row as kept: its time, window and value.
pub(crate) type Row = (u64, Option<(u64, u64)>, u64);

/// One source's rows of one series as read. Rows read twice appear twice.
type Rows = Vec<Row>;

/// Conflicting times, by series (`__name__` included) and pair of source
/// names.
type Found = BTreeMap<(BTreeMap<String, String>, String, String), BTreeSet<u64>>;

/// The rows a query read from its process groups' sources, and the
/// conflicts found in histogram rows; shared by the query's readers.
#[derive(Default)]
pub(crate) struct Conflicts {
    rows: Mutex<HashMap<SeriesKey, Vec<(SourceName, Rows)>>>,
    found: Mutex<Found>,
}

impl Conflicts {
    fn record(&self, key: SeriesKey, source: &SourceName, rows: impl Iterator<Item = Row>) {
        let mut all = self.rows.lock().expect("not poisoned");
        let held = all.entry(key).or_default();
        let at = match held.iter().position(|(s, _)| s.position == source.position) {
            Some(at) => at,
            None => {
                held.push((source.clone(), Rows::new()));
                held.len() - 1
            }
        };
        held[at].1.extend(rows);
    }

    /// A conflict at time `t` of `series` (its labels with `__name__`)
    /// between sources `a` and `b`.
    pub(crate) fn add(
        &self,
        series: BTreeMap<String, String>,
        a: &SourceName,
        b: &SourceName,
        t: u64,
    ) {
        if a.position == b.position {
            return;
        }
        let (a, b) = if a.name <= b.name { (a, b) } else { (b, a) };
        self.found
            .lock()
            .expect("not poisoned")
            .entry((series, a.name.clone(), b.name.clone()))
            .or_default()
            .insert(t);
    }

    /// The conflicts so far, one per series and pair of sources, with the
    /// first conflicting time and the count; the rows kept are dropped.
    pub(crate) fn notices(&self) -> Vec<crate::Notice> {
        let rows = std::mem::take(&mut *self.rows.lock().expect("not poisoned"));
        for ((_, metric, labels), mut held) in rows {
            let mut series = labels;
            series.insert("__name__".to_string(), metric);
            for (_, rows) in &mut held {
                rows.sort_unstable();
                rows.dedup();
            }
            for (i, (a, rows_a)) in held.iter().enumerate() {
                for (b, rows_b) in &held[i + 1..] {
                    for t in conflicting(rows_a, rows_b) {
                        self.add(series.clone(), a, b, t);
                    }
                }
            }
        }
        let found = std::mem::take(&mut *self.found.lock().expect("not poisoned"));
        found
            .into_iter()
            .map(|((series, a, b), times)| {
                let first = *times.first().expect("a conflict has a time");
                crate::Notice::conflict(series, vec![a, b], first as f64 / 1e9, times.len() as u64)
            })
            .collect()
    }
}

/// Whether `t` is within `keep`, both ends inclusive.
fn within(keep: (u64, u64), t: u64) -> bool {
    keep.0 <= t && t <= keep.1
}

/// The times at which sorted rows `a` and `b` hold a row each with one
/// window and different values.
fn conflicting(a: &[Row], b: &[Row]) -> Vec<u64> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let t = a[i].0;
        match t.cmp(&b[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                let ea = i + a[i..].iter().take_while(|r| r.0 == t).count();
                let eb = j + b[j..].iter().take_while(|r| r.0 == t).count();
                if a[i..ea]
                    .iter()
                    .any(|x| b[j..eb].iter().any(|y| x.1 == y.1 && x.2 != y.2))
                {
                    out.push(t);
                }
                (i, j) = (ea, eb);
            }
        }
    }
    out
}

/// A source of a process group that keeps every counter and gauge row read
/// from it within `keep` (both ends inclusive) in a [`Conflicts`].
pub(crate) struct Recorded {
    inner: Arc<dyn DataSource>,
    source: SourceName,
    conflicts: Arc<Conflicts>,
    keep: (u64, u64),
}

impl Recorded {
    pub(crate) fn new(
        inner: Arc<dyn DataSource>,
        source: SourceName,
        conflicts: Arc<Conflicts>,
    ) -> Self {
        Self {
            inner,
            source,
            conflicts,
            keep: (0, u64::MAX),
        }
    }

    /// Keep only rows within `[from_ns, to_ns]`, both inclusive.
    pub(crate) fn keeping(mut self, from_ns: u64, to_ns: u64) -> Self {
        self.keep = (from_ns, to_ns);
        self
    }

    fn keep_counters(&self, name: &str, counters: &Counters) {
        for c in &counters.series {
            let windows = c.windows.as_deref();
            self.conflicts.record(
                (Kind::Counter, name.to_string(), c.labels.inner.clone()),
                &self.source,
                c.timestamps
                    .iter()
                    .zip(&c.values)
                    .enumerate()
                    .filter(|(_, (t, _))| within(self.keep, **t))
                    .map(|(i, (t, v))| (*t, windows.and_then(|w| w.get(i).copied()), *v)),
            );
        }
    }

    fn keep_gauges(&self, name: &str, gauges: &Gauges) {
        for g in &gauges.series {
            let windows = g.windows.as_deref();
            self.conflicts.record(
                (Kind::Gauge, name.to_string(), g.labels.inner.clone()),
                &self.source,
                g.timestamps
                    .iter()
                    .zip(&g.values)
                    .enumerate()
                    .filter(|(_, (t, _))| within(self.keep, **t))
                    .map(|(i, (t, v))| (*t, windows.and_then(|w| w.get(i).copied()), *v as u64)),
            );
        }
    }
}

/// A scan's chunks, each row within `keep` kept as it is read and
/// recorded when the reader is dropped.
struct RecordedChunks<'a> {
    inner: Box<dyn crate::scan::ChunkReader + 'a>,
    /// Each scan series' key.
    keys: Vec<SeriesKey>,
    /// Each scan series' rows read so far.
    kept: Vec<Rows>,
    source: SourceName,
    conflicts: Arc<Conflicts>,
    keep: (u64, u64),
}

impl Drop for RecordedChunks<'_> {
    fn drop(&mut self) {
        for (key, rows) in self.keys.iter().zip(std::mem::take(&mut self.kept)) {
            if !rows.is_empty() {
                self.conflicts
                    .record(key.clone(), &self.source, rows.into_iter());
            }
        }
    }
}

/// A counter stream's samples, each within `keep` kept as it is
/// read and recorded when the stream is dropped.
struct RecordedSamples<'a> {
    inner: Box<dyn Iterator<Item = crate::types::CounterSample> + 'a>,
    key: SeriesKey,
    kept: Rows,
    source: SourceName,
    conflicts: Arc<Conflicts>,
    keep: (u64, u64),
}

impl Iterator for RecordedSamples<'_> {
    type Item = crate::types::CounterSample;

    fn next(&mut self) -> Option<Self::Item> {
        let x = self.inner.next()?;
        if within(self.keep, x.ts) {
            self.kept.push((x.ts, x.window, x.value));
        }
        Some(x)
    }
}

impl Drop for RecordedSamples<'_> {
    fn drop(&mut self) {
        let rows = std::mem::take(&mut self.kept);
        if !rows.is_empty() {
            self.conflicts
                .record(self.key.clone(), &self.source, rows.into_iter());
        }
    }
}

impl crate::scan::ChunkReader for RecordedChunks<'_> {
    fn next_chunk(
        &mut self,
        n: usize,
    ) -> Result<Option<crate::scan::ScanChunk>, crate::scan::ScanError> {
        let chunk = self.inner.next_chunk(n)?;
        if let Some(chunk) = &chunk {
            let rows = &mut self.kept;
            let get = |a: Option<&arrow::array::UInt64Array>, r: usize| {
                a.and_then(|a| (!arrow::array::Array::is_null(a, r)).then(|| a.value(r)))
            };
            let keep = self.keep;
            for segment in &chunk.segments {
                let columns = &segment.columns;
                for (batch, series) in columns.batches.iter().zip(&segment.series) {
                    let Some(ts) = columns.u64s(batch, columns.ts) else {
                        continue;
                    };
                    if !ts.values().iter().any(|t| within(keep, *t)) {
                        continue;
                    }
                    let duration = columns.duration.and_then(|c| columns.u64s(batch, c));
                    for (col, series) in segment.cols.iter().zip(series) {
                        let Some(values) = columns.u64s(batch, col.values) else {
                            continue;
                        };
                        let begin = col.begin.and_then(|c| columns.i64s(batch, c));
                        let width = col.width.and_then(|c| columns.u64s(batch, c));
                        let windowed = (begin.is_some() && width.is_some()) || duration.is_some();
                        let mut push = |r: usize, s: usize| {
                            let (Some(base), Some(value)) =
                                (get(Some(ts), r), get(Some(values), r))
                            else {
                                return;
                            };
                            if !within(keep, base) {
                                return;
                            }
                            let window = windowed.then(|| {
                                let bo = begin.and_then(|b| {
                                    (!arrow::array::Array::is_null(b, r)).then(|| b.value(r))
                                });
                                metriken_storage::parquet::resolve_window(
                                    base,
                                    bo,
                                    get(width, r),
                                    get(duration, r),
                                )
                            });
                            if let Some(rows) = rows.get_mut(s) {
                                rows.push((base, window, value));
                            }
                        };
                        match series {
                            crate::scan::RowSeries::One(s) => {
                                for r in 0..batch.num_rows() {
                                    push(r, *s as usize);
                                }
                            }
                            crate::scan::RowSeries::PerRow(v) => {
                                for (r, s) in v.iter().enumerate() {
                                    if *s != u32::MAX {
                                        push(r, *s as usize);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(chunk)
    }

    fn later_rows(&self, series: u32) -> crate::scan::LaterRows {
        self.inner.later_rows(series)
    }
}

impl DataSource for Recorded {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Counters> {
        let counters = self.inner.counters(name, filter, start_ns, end_ns)?;
        self.keep_counters(name, &counters);
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
                .map(|s| {
                    let key = (Kind::Counter, name.to_string(), s.labels.inner.clone());
                    CounterStream {
                        labels: s.labels,
                        windowed: s.windowed,
                        interval: s.interval,
                        samples: Box::new(RecordedSamples {
                            inner: s.samples,
                            key,
                            kept: Rows::new(),
                            source: self.source.clone(),
                            conflicts: Arc::clone(&self.conflicts),
                            keep: self.keep,
                        }),
                    }
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
        let scan = self.inner.counter_scan(name, filter, start_ns, end_ns)?;
        let (series, reader) = scan.into_parts();
        let keys: Vec<SeriesKey> = series
            .iter()
            .map(|s| (Kind::Counter, name.to_string(), s.labels.inner.clone()))
            .collect();
        Some(crate::scan::CounterScan::new(
            series,
            Box::new(RecordedChunks {
                inner: reader,
                kept: vec![Rows::new(); keys.len()],
                keys,
                source: self.source.clone(),
                conflicts: Arc::clone(&self.conflicts),
                keep: self.keep,
            }),
        ))
    }
    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        let gauges = self.inner.gauges(name, filter, start_ns, end_ns)?;
        self.keep_gauges(name, &gauges);
        Some(gauges)
    }
    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        self.inner.histogram_stream(name, filter, start_ns, end_ns)
    }
    fn gauge_histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        self.inner
            .gauge_histogram_stream(name, filter, start_ns, end_ns)
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
        self.inner
            .gauge_histogram_parts(name, filter, start_ns, end_ns)
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
        let parts = self
            .inner
            .gauge_parts(name, filter, start_ns, end_ns, step_ns)?;
        for (gauges, _) in &parts {
            self.keep_gauges(name, gauges);
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

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    /// A test row: its time in seconds, value and window.
    type TestRow = (u64, u64, Option<(u64, u64)>);

    /// A source of counter `c`, series `cpu=0`, with rows `rows`.
    fn counters(rows: &[TestRow]) -> Arc<dyn DataSource> {
        let mut memory = crate::memory::Memory::new(1000);
        let windowed = rows.iter().all(|r| r.2.is_some());
        memory.push_counter_series(
            "c",
            crate::types::Counter {
                labels: Labels::from(&[("cpu", "0")][..]),
                timestamps: rows.iter().map(|r| r.0 * S).collect(),
                values: rows.iter().map(|r| r.1).collect(),
                windows: windowed.then(|| rows.iter().map(|r| r.2.unwrap()).collect()),
            },
        );
        Arc::new(memory)
    }

    fn named(position: usize) -> SourceName {
        SourceName {
            position,
            name: format!("u{position}"),
        }
    }

    /// Read counter `c` over `[from, to]` seconds from each of `sources`,
    /// recorded at its position, and return the notices.
    fn conflicts_of(
        sources: &[(usize, Arc<dyn DataSource>)],
        reads: &[(u64, u64)],
    ) -> Vec<crate::Notice> {
        let conflicts = Arc::new(Conflicts::default());
        for (position, source) in sources {
            let recorded =
                Recorded::new(Arc::clone(source), named(*position), Arc::clone(&conflicts));
            for &(from, to) in reads {
                recorded.counters("c", &Labels::default(), from * S, to * S);
            }
        }
        conflicts.notices()
    }

    /// Rows of two sources at one time with different values conflict only
    /// when their windows are equal; two reads of overlapping ranges count
    /// each time once; a source's two epochs (one position) are not
    /// compared.
    #[test]
    fn rows_conflict_at_one_time_and_window() {
        let w = |t: u64| Some((t * S - 1000, t * S));
        let a = counters(&[(1, 10, w(1)), (2, 20, w(2)), (3, 30, w(3))]);
        let b = counters(&[(1, 10, w(1)), (2, 21, w(2)), (3, 31, w(3))]);
        let notices = conflicts_of(
            &[(0, Arc::clone(&a)), (1, Arc::clone(&b))],
            &[(0, 2), (1, 3)],
        );
        let [crate::Notice::Conflict {
            series,
            sources,
            first,
            count,
            ..
        }] = notices.as_slice()
        else {
            panic!("{notices:?}");
        };
        assert_eq!(series.get("__name__").map(String::as_str), Some("c"));
        assert_eq!(series.get("cpu").map(String::as_str), Some("0"));
        assert_eq!(sources, &vec!["u0".to_string(), "u1".to_string()]);
        assert_eq!((*first, *count), (2.0, 2));

        // Another window at the differing rows: no conflict.
        let shifted = counters(&[
            (1, 10, w(1)),
            (2, 21, Some((2 * S - 5, 2 * S))),
            (3, 31, Some((3 * S - 5, 3 * S))),
        ]);
        assert!(conflicts_of(&[(0, Arc::clone(&a)), (1, shifted)], &[(0, 3)]).is_empty());

        // One source's two epochs.
        assert!(conflicts_of(&[(0, a), (0, b)], &[(0, 3)]).is_empty());
    }

    /// A source keeping rows from 2 s on reports a conflict at 3 s and not
    /// the one at 1 s, whichever path reads the rows.
    #[test]
    fn rows_before_the_bound_are_not_kept() {
        let w = |t: u64| Some((t * S - 1000, t * S));
        for streams in [false, true] {
            let conflicts = Arc::new(Conflicts::default());
            for (position, edit) in [(0, 0), (1, 1)] {
                let source = counters(&[(1, 10 + edit, w(1)), (2, 20, w(2)), (3, 30 + edit, w(3))]);
                let recorded = Recorded::new(source, named(position), Arc::clone(&conflicts))
                    .keeping(2 * S, u64::MAX);
                match streams {
                    false => {
                        recorded.counters("c", &Labels::default(), 0, 4 * S);
                    }
                    true => {
                        for stream in recorded
                            .counter_streams("c", &Labels::default(), 0, 4 * S)
                            .expect("streams")
                        {
                            stream.samples.for_each(drop);
                        }
                    }
                }
            }
            let notices = conflicts.notices();
            assert!(
                matches!(notices.as_slice(), [crate::Notice::Conflict { first, count: 1, .. }] if *first == 3.0),
                "{notices:?}"
            );
        }
    }

    /// Rows read through counter streams are kept as the streams are read.
    #[test]
    fn rows_read_as_streams_are_kept() {
        let w = |t: u64| Some((t * S - 1000, t * S));
        let conflicts = Arc::new(Conflicts::default());
        for (position, edit) in [(0, 0), (1, 1)] {
            let source = counters(&[(1, 10, w(1)), (2, 20 + edit, w(2))]);
            let recorded = Recorded::new(source, named(position), Arc::clone(&conflicts));
            for stream in recorded
                .counter_streams("c", &Labels::default(), 0, 3 * S)
                .expect("streams")
            {
                stream.samples.for_each(drop);
            }
        }
        let notices = conflicts.notices();
        assert!(
            matches!(notices.as_slice(), [crate::Notice::Conflict { first, count: 1, .. }] if *first == 2.0),
            "{notices:?}"
        );
    }

    /// A metric one source holds as a counter and another as a gauge is
    /// compared within each kind only.
    #[test]
    fn rows_of_another_kind_are_not_compared() {
        let conflicts = Arc::new(Conflicts::default());
        let counter = Recorded::new(counters(&[(1, 10, None)]), named(0), Arc::clone(&conflicts));
        counter.counters("c", &Labels::default(), 0, 2 * S);
        let mut memory = crate::memory::Memory::new(1000);
        memory.push_gauge_series(
            "c",
            crate::types::Gauge {
                labels: Labels::from(&[("cpu", "0")][..]),
                timestamps: vec![S],
                values: vec![11],
                windows: None,
            },
        );
        let gauge = Recorded::new(Arc::new(memory), named(1), Arc::clone(&conflicts));
        gauge.gauges("c", &Labels::default(), 0, 2 * S);
        assert!(conflicts.notices().is_empty());
    }
}
