//! The S9 per-source union: a query over a process group reads each of the
//! group's sources alone and combines the results per series.
//!
//! A process group is the set of sources of one producer process (copies, a
//! recording and a hindsight dump of it, two recorders of one agent at once;
//! metriken-storage's `process` module). Each source is evaluated up to the
//! per-series function (`rate`, `irate`, a gauge read, a windowed gauge
//! function) as one recording is evaluated, with its own rows, interval and
//! acquisition windows, and the results are combined per series: at each
//! point the finest source whose rows cover the function's whole window
//! answers, ties going to the lowest uuid and then to catalog order; where
//! none covers the window, the finest source holding rows there; where no
//! source holds rows (S3's hole rule), nothing (S5). No value is a
//! difference between rows of two sources, so a counter reset is always
//! within one source's rows. A source holding several producer epochs is
//! one member per epoch, so each epoch's first row starts a new run of rows
//! and no pair of rows spans a restart.
//!
//! The rules are S1 to S9 of
//! `docs/journal/2026-10-19-recording-identity-and-views.md`. Aggregation
//! and binary operators run on the combined series, so `sum(rate(x[1m]))`
//! counts each process once.
//!
//! Histogram functions do not combine yet: over a group they read one source
//! alone, the one covering the most of the read ([`GroupSource`]), which
//! counts the process once and leaves the other sources' spans out.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use super::Point;
use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::types::{CounterStream, Counters, Gauges};
use crate::DataSource;

/// One member of a process group: one source, or one producer epoch of one
/// source, with what orders it among the others.
pub(crate) struct Member {
    /// The source's series, carrying the group's labels.
    pub source: Arc<dyn DataSource>,
    /// The rows this member holds: `[lo, hi]`, both inclusive. The whole
    /// source, or one epoch of it, from its `from_ts` to the next epoch's.
    pub lo: u64,
    pub hi: u64,
    /// S3's tie rule: lowest uuid (a source with none after every source
    /// with one), then catalog order, then the position among the reader's
    /// sources (readers joined by `flatten` repeat catalog indices).
    pub uuid: Option<String>,
    pub catalog_index: usize,
    pub position: usize,
}

impl Member {
    /// S3's tie rule between two members of equal spacing: the lowest uuid
    /// (a source without one after those with one), then catalog order, then
    /// the position among the reader's sources.
    pub(crate) fn cmp_ties(&self, other: &Member) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (&self.uuid, &other.uuid) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
        .then(self.catalog_index.cmp(&other.catalog_index))
        .then(self.position.cmp(&other.position))
    }
}

/// The sources of a query, as the engine reads them when some of them form
/// process groups of more than one member.
pub(crate) struct SourceGroups {
    /// The recordings the query reaches, in catalog order: each a source of
    /// one member, or a process group of several.
    pub parts: Vec<Part>,
}

/// One recording of [`SourceGroups`].
#[cfg_attr(not(feature = "archive"), allow(dead_code))]
pub(crate) enum Part {
    /// A source that is a group of one member, read as before.
    Single(Arc<dyn DataSource>),
    /// The members of a process group.
    Group(Vec<Member>),
}

/// What a leaf's point at `t` reads, for the coverage rule.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Window {
    /// `[t - range, t]`: `rate`, `avg_over_time`, `idelta`, `deriv`.
    Range(u64),
    /// `[t - step, t]`: `irate` on the grid, or the gap from the previous
    /// explicit evaluation timestamp.
    Step,
    /// `[t - s, t]`, `s` the step or the member's interval when that is
    /// longer: a gauge read.
    Stale,
    /// The point itself: `irate` and gauge reads in raw mode, whose value at
    /// a row is that member's own pair or row.
    Point,
}

/// The rows a member's series hold, as runs without a hole (S3), with the
/// member's interval.
#[derive(Clone)]
struct Coverage {
    runs: Vec<(u64, u64)>,
    interval_ns: u64,
}

impl Coverage {
    /// The run holding `t`: from its first row to one interval past its
    /// last.
    fn run_at(&self, t: u64) -> Option<(u64, u64)> {
        let i = self.runs.partition_point(|r| r.0 <= t);
        let run = *self.runs.get(i.checked_sub(1)?)?;
        (t <= run.1.saturating_add(self.interval_ns)).then_some(run)
    }

    /// Whether a run holds `t`.
    fn holds(&self, t: u64) -> bool {
        self.run_at(t).is_some()
    }

    /// Whether one run holds the whole window `[t - window, t]`.
    fn covers(&self, t: u64, window: u64) -> bool {
        self.run_at(t)
            .is_some_and(|(first, _)| t.checked_sub(window).is_some_and(|w| w >= first))
    }
}

/// [`Coverage`] built as rows arrive, for a member read by the batched scan:
/// the same runs [`runs`] gives for the same rows, decided as soon as the
/// eight gaps after each gap are known (or the rows end), so a point can be
/// answered with no more than nine rows read past it, and keeping only the
/// rows and runs a later answer can still need.
///
/// Rows are offered in increasing time (a scan's rows of one series); the
/// rows read on either side of the scan (`Tap::context_rows`) are merged in
/// order as rows past them arrive, so the rows are those `runs` would be
/// given.
pub(crate) struct RowTrack {
    interval_ns: u64,
    before: VecDeque<u64>,
    after: VecDeque<u64>,
    /// The latest rows accepted; `rows[0]` is row `base`.
    rows: VecDeque<u64>,
    base: usize,
    count: usize,
    /// Gaps decided: gap `k` lies between rows `k` and `k + 1`.
    decided: usize,
    /// The runs of the rows up to the last decided gap's later row; the last
    /// is the run still open.
    runs: VecDeque<(u64, u64)>,
    finished: bool,
    /// The smallest gap seen.
    smallest: u64,
}

impl RowTrack {
    /// A track for rows of a member of `interval_ns`, with the rows read
    /// before (`before`) and after (`after`) its scan, each in increasing
    /// time.
    pub(crate) fn new(interval_ns: u64, before: Vec<u64>, after: Vec<u64>) -> Self {
        Self {
            interval_ns,
            before: before.into(),
            after: after.into(),
            rows: VecDeque::new(),
            base: 0,
            count: 0,
            decided: 0,
            runs: VecDeque::new(),
            finished: false,
            smallest: u64::MAX,
        }
    }

    /// A row of the scan.
    pub(crate) fn push(&mut self, ts: u64) {
        while self.before.front().is_some_and(|&b| b < ts) {
            let b = self.before.pop_front().expect("front");
            self.accept(b);
        }
        while self.after.front().is_some_and(|&a| a < ts) {
            let a = self.after.pop_front().expect("front");
            self.accept(a);
        }
        self.accept(ts);
    }

    /// No more rows: the rest of the rows on either side, and every gap
    /// decided with the gaps there are.
    pub(crate) fn finish(&mut self) {
        while let Some(b) = self.before.pop_front() {
            self.accept(b);
        }
        while let Some(a) = self.after.pop_front() {
            self.accept(a);
        }
        self.finished = true;
        self.decide();
    }

    fn accept(&mut self, ts: u64) {
        if let Some(&last) = self.rows.back() {
            if ts <= last {
                return;
            }
            self.smallest = self.smallest.min(ts - last);
        }
        self.rows.push_back(ts);
        self.count += 1;
        if self.count == 1 {
            self.runs.push_back((ts, ts));
        }
        self.decide();
    }

    fn row(&self, k: usize) -> u64 {
        self.rows[k - self.base]
    }

    fn gap(&self, k: usize) -> u64 {
        self.row(k + 1) - self.row(k)
    }

    fn decide(&mut self) {
        const SIDE: usize = 8;
        let median = |gaps: &mut [u64]| -> Option<u64> {
            if gaps.is_empty() {
                return None;
            }
            gaps.sort_unstable();
            Some(gaps[gaps.len() / 2])
        };
        while self.decided + 1 < self.count {
            let k = self.decided;
            let after_available = self.count - 1 - (k + 1);
            if !self.finished && after_available < SIDE {
                break;
            }
            let gap = self.gap(k);
            // The median of the gaps before is at least the smallest gap
            // seen, so a gap within twice that is no hole.
            if k > 0 && gap <= self.smallest.saturating_mul(2) {
                self.extend_run(k);
                continue;
            }
            let mut before = [0u64; SIDE];
            let nb = k.min(SIDE);
            for (i, g) in before.iter_mut().take(nb).enumerate() {
                *g = self.gap(k - nb + i);
            }
            let mut after = [0u64; SIDE];
            let na = after_available.min(SIDE);
            for (i, g) in after.iter_mut().take(na).enumerate() {
                *g = self.gap(k + 1 + i);
            }
            // Each median is at least its side's smallest gap, so a gap
            // within twice the larger of those is no hole, and most gaps are
            // decided without sorting.
            let low = |g: &[u64]| g.iter().copied().min();
            let bound = low(&before[..nb]).max(low(&after[..na]));
            let hole = match bound {
                Some(b) if gap <= b.saturating_mul(2) => false,
                _ => {
                    let typical = match (median(&mut before[..nb]), median(&mut after[..na])) {
                        (Some(b), Some(a)) => Some(b.max(a)),
                        (Some(x), None) | (None, Some(x)) => Some(x),
                        (None, None) => None,
                    };
                    typical.is_some_and(|t| gap > t.saturating_mul(2))
                }
            };
            if hole {
                let next = self.row(k + 1);
                self.runs.push_back((next, next));
                self.advance();
            } else {
                self.extend_run(k);
            }
        }
    }

    /// Gap `k` is no hole: its later row joins the open run.
    fn extend_run(&mut self, k: usize) {
        let next = self.row(k + 1);
        if let Some(run) = self.runs.back_mut() {
            run.1 = next;
        }
        self.advance();
    }

    fn advance(&mut self) {
        const SIDE: usize = 8;
        self.decided += 1;
        // The rows the next decision's gaps before it need.
        while self.base + SIDE < self.decided {
            self.rows.pop_front();
            self.base += 1;
        }
    }

    /// Whether every row up to the first one after `t` is known, with the
    /// gap before it decided: whether `t` can be answered.
    pub(crate) fn ready(&self, t: u64) -> bool {
        self.finished || (self.count > 0 && t < self.row(self.decided))
    }

    /// Drop the runs that end more than an interval before `t`, which no
    /// answer at or after `t` reads.
    pub(crate) fn forget_before(&mut self, t: u64) {
        while self.runs.len() > 1
            && self.runs[0].1.saturating_add(self.interval_ns) < t
            && self.runs[1].0 <= t
        {
            self.runs.pop_front();
        }
    }

    fn run_at(&self, t: u64) -> Option<(u64, u64)> {
        let i = self.runs.partition_point(|r| r.0 <= t);
        let run = *self.runs.get(i.checked_sub(1)?)?;
        (t <= run.1.saturating_add(self.interval_ns)).then_some(run)
    }

    /// [`Coverage::holds`], for a `t` that is [`ready`](Self::ready).
    pub(crate) fn holds(&self, t: u64) -> bool {
        self.run_at(t).is_some()
    }

    /// [`Coverage::covers`], for a `t` that is [`ready`](Self::ready).
    pub(crate) fn covers(&self, t: u64, window: u64) -> bool {
        self.run_at(t)
            .is_some_and(|(first, _)| t.checked_sub(window).is_some_and(|w| w >= first))
    }
}

/// The runs of `timestamps` (sorted): a hole is a gap longer than twice the
/// larger of the median of the up to eight gaps before it and the median of
/// the up to eight gaps after it, with only one side's median where the
/// other side has no gap (S3).
pub(crate) fn runs(timestamps: &[u64]) -> Vec<(u64, u64)> {
    const SIDE: usize = 8;
    let Some(&first) = timestamps.first() else {
        return Vec::new();
    };
    let gaps: Vec<u64> = timestamps.windows(2).map(|w| w[1] - w[0]).collect();
    let median = |side: &[u64]| -> Option<u64> {
        if side.is_empty() {
            return None;
        }
        let mut s = [0u64; SIDE];
        let s = &mut s[..side.len()];
        s.copy_from_slice(side);
        s.sort_unstable();
        Some(s[s.len() / 2])
    };
    // Every median is at least the smallest gap, so a gap within twice it
    // is no hole, and most gaps are decided without a median.
    let floor = gaps.iter().copied().min().unwrap_or(0).saturating_mul(2);
    let mut out = Vec::new();
    let mut start = first;
    for (i, &gap) in gaps.iter().enumerate() {
        if gap <= floor {
            continue;
        }
        let before = median(&gaps[i.saturating_sub(SIDE)..i]);
        let after = median(&gaps[(i + 1).min(gaps.len())..(i + 1 + SIDE).min(gaps.len())]);
        let typical = match (before, after) {
            (Some(b), Some(a)) => b.max(a),
            (Some(x), None) | (None, Some(x)) => x,
            (None, None) => continue,
        };
        if gap > typical.saturating_mul(2) {
            out.push((start, timestamps[i]));
            start = timestamps[i + 1];
        }
    }
    out.push((start, *timestamps.last().expect("not empty")));
    out
}

/// One member's evaluated series, with the runs of rows each read.
pub(crate) struct MemberSeries {
    pub series: Vec<(Labels, Vec<Point>)>,
    coverage: HashMap<Labels, Coverage>,
    interval_ns: u64,
}

impl MemberSeries {
    /// `series` with the rows each was read from (`Tap::take_rows`), kept as
    /// runs (S3), and the member's interval.
    pub(crate) fn new(
        series: Vec<(Labels, Vec<Point>)>,
        rows: HashMap<Labels, Vec<u64>>,
        interval_ns: u64,
    ) -> Self {
        let coverage = rows
            .into_iter()
            .map(|(labels, mut ts)| {
                // Rows arrive in time order from one table; a series read
                // from several (a union of group tables) is sorted here.
                if !ts.is_sorted() {
                    ts.sort_unstable();
                }
                ts.dedup();
                let runs = runs(&ts);
                (labels, Coverage { runs, interval_ns })
            })
            .collect();
        Self {
            series,
            coverage,
            interval_ns,
        }
    }
}

/// Combine the members' results per series (S3, S9). `members` are in rank
/// order, finest first. `window(t, interval_ns)` is the length of the window
/// a point at `t` reads; `raw` selects raw placement, where each member's
/// points fall at its own rows and a point is kept where its member is the
/// one chosen at its time.
pub(crate) fn combine(
    members: Vec<MemberSeries>,
    window: &dyn Fn(u64, u64) -> u64,
    raw: bool,
) -> Vec<(Labels, Vec<Point>)> {
    // Each series' points and coverage per member, in rank order, series in
    // the order they first appear.
    let mut order: Vec<Labels> = Vec::new();
    let mut by_series: HashMap<Labels, Vec<(Vec<Point>, Coverage)>> = HashMap::new();
    for m in members {
        let MemberSeries {
            series,
            mut coverage,
            interval_ns,
        } = m;
        for (labels, points) in series {
            let coverage = coverage.remove(&labels).unwrap_or(Coverage {
                runs: Vec::new(),
                interval_ns,
            });
            let entry = by_series.entry(labels.clone()).or_insert_with(|| {
                order.push(labels.clone());
                Vec::new()
            });
            entry.push((points, coverage));
        }
    }
    order
        .into_iter()
        .map(|labels| {
            let members = by_series.remove(&labels).expect("listed");
            (labels, combine_series(members, window, raw))
        })
        .collect()
}

fn combine_series(
    mut members: Vec<(Vec<Point>, Coverage)>,
    window: &dyn Fn(u64, u64) -> u64,
    raw: bool,
) -> Vec<Point> {
    // Which member answers each point, by a merge over the members' points
    // in time order; then the answer is built, or, when one member answers
    // every point it holds and no other answers anything, that member's
    // points are the answer as they are.
    let n = members.len();
    let mut cursor = vec![0usize; n];
    let mut at: Vec<Option<usize>> = vec![None; n];
    let mut chosen: Vec<(usize, usize)> = Vec::new();
    while let Some(t) = (0..n)
        .filter_map(|i| members[i].0.get(cursor[i]).map(|p| p.t))
        .min()
    {
        for i in 0..n {
            let points = &members[i].0;
            at[i] = (points.get(cursor[i]).map(|p| p.t) == Some(t)).then_some(cursor[i]);
        }
        let holds = |i: usize| members[i].1.holds(t);
        let covers = |i: usize| {
            let c = &members[i].1;
            c.covers(t, window(t, c.interval_ns))
        };
        let pick = if raw {
            // The member answering at `t`; its point, if it has one there.
            (0..n)
                .find(|&i| covers(i))
                .or_else(|| (0..n).find(|&i| holds(i)))
                .filter(|&i| at[i].is_some())
        } else {
            (0..n)
                .find(|&i| at[i].is_some() && covers(i))
                .or_else(|| (0..n).find(|&i| at[i].is_some() && holds(i)))
        };
        if let Some(i) = pick {
            chosen.push((i, at[i].expect("has a point")));
        }
        for i in 0..n {
            if at[i].is_some() {
                cursor[i] += 1;
            }
        }
    }
    if let Some(&(only, _)) = chosen.first() {
        if chosen.len() == members[only].0.len() && chosen.iter().all(|&(i, _)| i == only) {
            return std::mem::take(&mut members[only].0);
        }
    }
    chosen.into_iter().map(|(i, k)| members[i].0[k]).collect()
}

/// The rows on each side of a read that a member's coverage needs (S3: the
/// medians of up to eight gaps on either side of a gap).
const CONTEXT_ROWS: usize = 9;

/// A member's source as a leaf reads it: its rows clipped to `[lo, hi]`, the
/// timestamps of every row it hands out recorded per series, and no batched
/// counter scan (whose rows it could not record), so the per-series path
/// answers.
pub(crate) struct Tap {
    inner: Arc<dyn DataSource>,
    lo: u64,
    hi: u64,
    rows: Mutex<HashMap<Labels, Arc<Mutex<Vec<u64>>>>>,
}

impl Tap {
    pub(crate) fn new(inner: Arc<dyn DataSource>, lo: u64, hi: u64) -> Self {
        Self {
            inner,
            lo,
            hi,
            rows: Mutex::new(HashMap::new()),
        }
    }

    fn sink(&self, labels: &Labels) -> Arc<Mutex<Vec<u64>>> {
        let mut rows = self.rows.lock().expect("not poisoned");
        Arc::clone(rows.entry(labels.clone()).or_default())
    }

    /// For each of `series` (and any other series the read finds), the
    /// timestamps of the nine rows nearest `at` on one side of it:
    /// before it when `before`, else after it. A read starting or ending at
    /// `at` does not return them, and S3's hole test needs the medians of
    /// up to eight gaps on each side of a gap, so without them a series
    /// looks as if its rows start at the read's start or end at its end.
    /// The window read is ten of the metric's own intervals, and `longest`
    /// when a series has fewer than nine rows in that (or the member's rows
    /// end first). The read decodes values as well as timestamps (the
    /// sources have no timestamp-only read per series); only the timestamps
    /// are kept.
    fn context(
        &self,
        counter: bool,
        name: &str,
        filter: &Labels,
        (at, before): (u64, bool),
        series: &[Labels],
        longest: u64,
    ) -> HashMap<Labels, Vec<u64>> {
        let mut out = HashMap::new();
        let Some((first, last)) = self.inner.time_range() else {
            return out;
        };
        let (first, last) = (first.max(self.lo), last.min(self.hi));
        let interval = if counter {
            self.inner.counter_interval(name)
        } else {
            self.inner.gauge_interval(name)
        };
        let mut span = ((interval * 1e9) as u64).max(1).saturating_mul(10);
        loop {
            let (from, to) = if before {
                (at.saturating_sub(span).max(first), at.saturating_sub(1))
            } else {
                (at.saturating_add(1), at.saturating_add(span).min(last))
            };
            if from > to {
                return out;
            }
            let mut rows: HashMap<Labels, Vec<u64>> = HashMap::new();
            let read: Vec<(Labels, Vec<u64>)> = if counter {
                self.inner
                    .counters(name, filter, from, to)
                    .map(|c| {
                        c.series
                            .into_iter()
                            .map(|c| (c.labels, c.timestamps))
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                self.inner
                    .gauges(name, filter, from, to)
                    .map(|g| {
                        g.series
                            .into_iter()
                            .map(|g| (g.labels, g.timestamps))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            for (labels, ts) in read {
                rows.entry(labels).or_default().extend(
                    ts.into_iter()
                        .filter(|&t| self.keeps(t) && t >= from && t <= to),
                );
            }
            let reached = if before { from <= first } else { to >= last } || span >= longest;
            let enough = |l: &Labels| rows.get(l).is_some_and(|r| r.len() >= CONTEXT_ROWS);
            if reached || series.iter().all(enough) {
                for (labels, mut ts) in rows {
                    ts.sort_unstable();
                    ts.dedup();
                    let keep = ts.len().min(CONTEXT_ROWS);
                    let kept = if before {
                        ts[ts.len() - keep..].to_vec()
                    } else {
                        ts[..keep].to_vec()
                    };
                    out.insert(labels, kept);
                }
                return out;
            }
            span = longest;
        }
    }

    /// [`context`](Self::context) on both sides of a read of `[start_ns,
    /// end_ns]`, recorded with the rows the read hands out.
    fn contexts(
        &self,
        counter: bool,
        name: &str,
        filter: &Labels,
        span: (u64, u64),
        series: &[Labels],
    ) {
        let (before, after) = self.context_rows(counter, name, filter, span, series);
        for (labels, ts) in before.into_iter().chain(after) {
            self.record(&labels, &ts);
        }
    }

    /// [`context`](Self::context) on both sides of a read of `[start_ns,
    /// end_ns]`: the rows before it and the rows after it, per series.
    pub(crate) fn context_rows(
        &self,
        counter: bool,
        name: &str,
        filter: &Labels,
        (start_ns, end_ns): (u64, u64),
        series: &[Labels],
    ) -> (HashMap<Labels, Vec<u64>>, HashMap<Labels, Vec<u64>>) {
        // A series with fewer than nine rows within 64 of the metric's
        // intervals of the read (it ended, or a long hole follows) is taken
        // as having none further.
        let interval = if counter {
            self.inner.counter_interval(name)
        } else {
            self.inner.gauge_interval(name)
        };
        let longest = ((interval * 1e9) as u64).max(1).saturating_mul(64);
        (
            self.context(counter, name, filter, (start_ns, true), series, longest),
            self.context(counter, name, filter, (end_ns, false), series, longest),
        )
    }

    fn record(&self, labels: &Labels, timestamps: &[u64]) {
        self.sink(labels)
            .lock()
            .expect("not poisoned")
            .extend_from_slice(timestamps);
    }

    fn keeps(&self, ts: u64) -> bool {
        self.lo <= ts && ts <= self.hi
    }

    /// Every row handed out, per series.
    pub(crate) fn take_rows(&self) -> HashMap<Labels, Vec<u64>> {
        let rows = std::mem::take(&mut *self.rows.lock().expect("not poisoned"));
        rows.into_iter()
            .map(|(labels, ts)| {
                let ts = std::mem::take(&mut *ts.lock().expect("not poisoned"));
                (labels, ts)
            })
            .collect()
    }

    fn clip_gauges(&self, mut gauges: Gauges) -> Gauges {
        for g in &mut gauges.series {
            if !g.timestamps.iter().all(|&t| self.keeps(t)) {
                let keep: Vec<bool> = g.timestamps.iter().map(|&t| self.keeps(t)).collect();
                retain_by(&keep, &mut g.timestamps);
                retain_by(&keep, &mut g.values);
                if let Some(w) = &mut g.windows {
                    retain_by(&keep, w);
                }
            }
            self.record(&g.labels, &g.timestamps);
        }
        gauges
    }
}

/// Keep the elements of `v` whose `keep` is set.
fn retain_by<T>(keep: &[bool], v: &mut Vec<T>) {
    let mut k = keep.iter();
    v.retain(|_| *k.next().unwrap_or(&false));
}

/// A scan's chunk reader that records, per matched series, the stamp of
/// every row holding a value, as the batched rate path will read them.
struct RecordingChunks<'a> {
    inner: Box<dyn crate::scan::ChunkReader + 'a>,
    sinks: Vec<Arc<Mutex<Vec<u64>>>>,
}

impl crate::scan::ChunkReader for RecordingChunks<'_> {
    fn next_chunk(
        &mut self,
        n: usize,
    ) -> Result<Option<crate::scan::ScanChunk>, crate::scan::ScanError> {
        use arrow::array::Array;
        let Some(chunk) = self.inner.next_chunk(n)? else {
            return Ok(None);
        };
        let mut rows: Vec<Vec<u64>> = vec![Vec::new(); self.sinks.len()];
        for seg in &chunk.segments {
            let columns = &seg.columns;
            for (batch, series) in columns.batches.iter().zip(&seg.series) {
                let Some(ts) = columns.u64s(batch, columns.ts) else {
                    continue;
                };
                for (col, series) in seg.cols.iter().zip(series) {
                    let Some(values) = columns.u64s(batch, col.values) else {
                        continue;
                    };
                    let mut push = |r: usize, s: usize| {
                        if !ts.is_null(r) && !values.is_null(r) {
                            if let Some(v) = rows.get_mut(s) {
                                v.push(ts.value(r));
                            }
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
        for (sink, mut r) in self.sinks.iter().zip(rows) {
            if !r.is_empty() {
                sink.lock().expect("not poisoned").append(&mut r);
            }
        }
        Ok(Some(chunk))
    }
}

impl DataSource for Tap {
    /// The source's scan, its rows recorded as they are read; `None` for a
    /// member clipped to an epoch (a scan cannot be clipped), which then
    /// takes the per-series path.
    fn counter_scan(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<crate::scan::CounterScan<'_>> {
        if self.lo != 0 || self.hi != u64::MAX {
            return None;
        }
        let scan = self.inner.counter_scan(name, filter, start_ns, end_ns)?;
        let labels: Vec<Labels> = scan.series.iter().map(|s| s.labels.clone()).collect();
        self.contexts(true, name, filter, (start_ns, end_ns), &labels);
        let sinks: Vec<_> = scan.series.iter().map(|s| self.sink(&s.labels)).collect();
        let (series, reader) = scan.into_parts();
        Some(crate::scan::CounterScan::new(
            series,
            Box::new(RecordingChunks {
                inner: reader,
                sinks,
            }),
        ))
    }

    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Counters> {
        let mut counters = self.inner.counters(name, filter, start_ns, end_ns)?;
        let labels: Vec<Labels> = counters.series.iter().map(|s| s.labels.clone()).collect();
        self.contexts(true, name, filter, (start_ns, end_ns), &labels);
        for c in &mut counters.series {
            if !c.timestamps.iter().all(|&t| self.keeps(t)) {
                let keep: Vec<bool> = c.timestamps.iter().map(|&t| self.keeps(t)).collect();
                retain_by(&keep, &mut c.timestamps);
                retain_by(&keep, &mut c.values);
                if let Some(w) = &mut c.windows {
                    retain_by(&keep, w);
                }
            }
            self.record(&c.labels, &c.timestamps);
        }
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
        let labels: Vec<Labels> = streams.iter().map(|s| s.labels.clone()).collect();
        self.contexts(true, name, filter, (start_ns, end_ns), &labels);
        let (lo, hi) = (self.lo, self.hi);
        Some(
            streams
                .into_iter()
                .map(|s| {
                    let sink = self.sink(&s.labels);
                    CounterStream {
                        labels: s.labels,
                        windowed: s.windowed,
                        interval: s.interval,
                        samples: Box::new(
                            s.samples
                                .filter(move |x| lo <= x.ts && x.ts <= hi)
                                .inspect(move |x| sink.lock().expect("not poisoned").push(x.ts)),
                        ),
                    }
                })
                .collect(),
        )
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        let gauges = self.inner.gauges(name, filter, start_ns, end_ns)?;
        let labels: Vec<Labels> = gauges.series.iter().map(|s| s.labels.clone()).collect();
        self.contexts(false, name, filter, (start_ns, end_ns), &labels);
        Some(self.clip_gauges(gauges))
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
        let lookback = step_ns.max((self.inner.gauge_interval(name) * 1e9) as u64);
        let labels: Vec<Labels> = parts
            .iter()
            .flat_map(|(g, _)| g.series.iter().map(|s| s.labels.clone()))
            .collect();
        self.contexts(
            false,
            name,
            filter,
            (start_ns.saturating_sub(lookback), end_ns),
            &labels,
        );
        Some(
            parts
                .into_iter()
                .map(|(g, i)| (self.clip_gauges(g), i))
                .collect(),
        )
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        self.inner
            .histogram_stream(name, filter, start_ns.max(self.lo), end_ns.min(self.hi))
    }

    fn gauge_histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        self.inner
            .gauge_histogram_stream(name, filter, start_ns.max(self.lo), end_ns.min(self.hi))
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
            .gauge_histogram_parts(name, filter, start_ns.max(self.lo), end_ns.min(self.hi))
    }
    fn gauge_histogram_names(&self) -> Vec<String> {
        self.inner.gauge_histogram_names()
    }
    fn gauge_histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.inner.gauge_histogram_labels(name)
    }

    fn interval(&self) -> f64 {
        self.inner.interval()
    }
    fn counter_interval(&self, name: &str) -> f64 {
        self.inner.counter_interval(name)
    }
    fn gauge_interval(&self, name: &str) -> f64 {
        self.inner.gauge_interval(name)
    }
    fn histogram_interval(&self, name: &str) -> f64 {
        self.inner.histogram_interval(name)
    }
    fn time_range(&self) -> Option<(u64, u64)> {
        let (a, b) = self.inner.time_range()?;
        let (a, b) = (a.max(self.lo), b.min(self.hi));
        (a <= b).then_some((a, b))
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
        self.inner.counter_labels(name)
    }
    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.inner.gauge_labels(name)
    }
    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.inner.histogram_labels(name)
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
        let mut ts = self.inner.sample_timestamps();
        ts.retain(|&t| self.keeps(t));
        ts
    }
    fn recording_label_keys(&self) -> Vec<String> {
        self.inner.recording_label_keys()
    }
}

/// A process group as one source, for what a query asks of the whole: its
/// names, label sets, intervals and span; and, until histograms combine per
/// source, a histogram read, which the source covering the most of the read
/// answers alone.
#[cfg_attr(not(feature = "archive"), allow(dead_code))]
pub(crate) struct GroupSource {
    /// One per source of the group, unclipped, in S3's rank order.
    sources: Vec<Arc<dyn DataSource>>,
}

#[cfg_attr(not(feature = "archive"), allow(dead_code))]
impl GroupSource {
    pub(crate) fn new(sources: Vec<Arc<dyn DataSource>>) -> Self {
        Self { sources }
    }

    fn names(&self, of: impl Fn(&dyn DataSource) -> Vec<String>) -> Vec<String> {
        let names: std::collections::BTreeSet<String> =
            self.sources.iter().flat_map(|s| of(s.as_ref())).collect();
        names.into_iter().collect()
    }

    fn listing(
        &self,
        of: impl Fn(&dyn DataSource) -> Vec<BTreeMap<String, String>>,
    ) -> Vec<BTreeMap<String, String>> {
        let mut out: Vec<BTreeMap<String, String>> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for s in &self.sources {
            for set in of(s.as_ref()) {
                if seen.insert(set.clone()) {
                    out.push(set);
                }
            }
        }
        out
    }

    /// The finest source holding histogram `name`, else the first.
    fn finest_holding(&self, held: impl Fn(&dyn DataSource) -> bool) -> &dyn DataSource {
        self.sources
            .iter()
            .find(|s| held(s.as_ref()))
            .unwrap_or(&self.sources[0])
            .as_ref()
    }
}

impl DataSource for GroupSource {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Counters> {
        self.finest_holding(|s| s.counter_names().iter().any(|n| n == name))
            .counters(name, filter, start_ns, end_ns)
    }
    fn counter_streams<'s>(
        &'s self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<CounterStream<'s>>> {
        self.finest_holding(|s| s.counter_names().iter().any(|n| n == name))
            .counter_streams(name, filter, start_ns, end_ns)
    }
    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        self.finest_holding(|s| s.gauge_names().iter().any(|n| n == name))
            .gauges(name, filter, start_ns, end_ns)
    }
    fn gauge_parts(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
    ) -> Option<Vec<(Gauges, f64)>> {
        self.finest_holding(|s| s.gauge_names().iter().any(|n| n == name))
            .gauge_parts(name, filter, start_ns, end_ns, step_ns)
    }
    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        // Until histograms combine per source: the source holding the
        // histogram whose span covers the most of the read, ties to the
        // finest, so the group counts once and the range is read whole where
        // one source holds it.
        let overlap = |s: &dyn DataSource| {
            s.time_range()
                .map_or(0, |(a, b)| b.min(end_ns).saturating_sub(a.max(start_ns)))
        };
        let mut best: Option<(&dyn DataSource, u64)> = None;
        for s in &self.sources {
            let s = s.as_ref();
            if !s.histogram_names().iter().any(|n| n == name) {
                continue;
            }
            let o = overlap(s);
            if best.is_none_or(|(_, b)| o > b) {
                best = Some((s, o));
            }
        }
        best.map_or(self.sources[0].as_ref(), |(s, _)| s)
            .histogram_stream(name, filter, start_ns, end_ns)
    }
    /// Every source's rows, merged: a series held by several sources is one
    /// series, and a gauge histogram is read as the latest row at each time
    /// (`super::gauge_histogram`), so the group counts once and no value
    /// is a difference between rows of two sources. A reader evaluates
    /// [`gauge_histogram_parts`](DataSource::gauge_histogram_parts), which
    /// holds each source's rows for that source's interval; this merged
    /// stream is for a caller that reads one stream.
    fn gauge_histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        HistogramStream::merge(
            self.sources
                .iter()
                .filter_map(|s| s.gauge_histogram_stream(name, filter, start_ns, end_ns))
                .collect(),
        )
    }
    /// Every source's parts, each at its source's own interval: a series
    /// two sources hold is one series, answered at each time by the latest
    /// row of either that its own source still holds
    /// (`super::gauge_histogram`). So a fine source's row is not held past
    /// its own interval into a gap where a coarse source has no row, and a
    /// coarse source's rows reach every step after a fine one ends.
    fn gauge_histogram_parts(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<(HistogramStream, f64)>> {
        let parts: Vec<(HistogramStream, f64)> = self
            .sources
            .iter()
            .filter_map(|s| s.gauge_histogram_parts(name, filter, start_ns, end_ns))
            .flatten()
            .collect();
        (!parts.is_empty()).then_some(parts)
    }
    /// The longest interval among the sources holding `name`: how far back
    /// a read of the group's parts starts.
    fn gauge_histogram_interval(&self, name: &str) -> f64 {
        self.sources
            .iter()
            .filter(|s| s.gauge_histogram_names().iter().any(|n| n == name))
            .map(|s| s.gauge_histogram_interval(name))
            .max_by(f64::total_cmp)
            .unwrap_or_else(|| self.interval())
    }
    fn gauge_histogram_names(&self) -> Vec<String> {
        self.names(|s| s.gauge_histogram_names())
    }
    fn gauge_histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.listing(|s| s.gauge_histogram_labels(name))
    }
    fn interval(&self) -> f64 {
        self.sources
            .iter()
            .map(|s| s.interval())
            .fold(f64::INFINITY, f64::min)
    }
    fn counter_interval(&self, name: &str) -> f64 {
        self.finest_holding(|s| s.counter_names().iter().any(|n| n == name))
            .counter_interval(name)
    }
    fn gauge_interval(&self, name: &str) -> f64 {
        self.finest_holding(|s| s.gauge_names().iter().any(|n| n == name))
            .gauge_interval(name)
    }
    fn histogram_interval(&self, name: &str) -> f64 {
        self.finest_holding(|s| s.histogram_names().iter().any(|n| n == name))
            .histogram_interval(name)
    }
    fn time_range(&self) -> Option<(u64, u64)> {
        self.sources
            .iter()
            .filter_map(|s| s.time_range())
            .reduce(|(a0, a1), (b0, b1)| (a0.min(b0), a1.max(b1)))
    }
    fn counter_names(&self) -> Vec<String> {
        self.names(|s| s.counter_names())
    }
    fn gauge_names(&self) -> Vec<String> {
        self.names(|s| s.gauge_names())
    }
    fn histogram_names(&self) -> Vec<String> {
        self.names(|s| s.histogram_names())
    }
    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.listing(|s| s.counter_labels(name))
    }
    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.listing(|s| s.gauge_labels(name))
    }
    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.listing(|s| s.histogram_labels(name))
    }
    fn file_metadata(&self) -> HashMap<String, String> {
        self.sources[0].file_metadata()
    }
    fn metadata_get(&self, key: &str) -> Option<String> {
        self.sources[0].metadata_get(key)
    }
    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        self.sources[0].column_map()
    }
    fn recording_label_keys(&self) -> Vec<String> {
        self.names(|s| s.recording_label_keys())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    /// S3: a 5 s gap among 1 s rows is a hole (5 s against a 2 s threshold);
    /// a change of spacing is not, because the larger side's median counts.
    #[test]
    fn a_gap_is_a_hole_against_twice_the_larger_median() {
        let mut ts: Vec<u64> = (0..10).map(|i| i * S).collect();
        ts.extend((0..10).map(|i| 14 * S + i * S));
        assert_eq!(runs(&ts), vec![(0, 9 * S), (14 * S, 23 * S)]);

        // 100 ms rows, then 1 s rows: no hole at the change.
        let mut ts: Vec<u64> = (0..20).map(|i| i * S / 10).collect();
        ts.extend((1..10).map(|i| 19 * S / 10 + i * S));
        assert_eq!(runs(&ts).len(), 1);

        // One side only: three rows 1 s apart, then one 3 s later.
        assert_eq!(
            runs(&[0, S, 2 * S, 5 * S]),
            vec![(0, 2 * S), (5 * S, 5 * S)]
        );
        // Two rows: no gap to compare with.
        assert_eq!(runs(&[0, 9 * S]), vec![(0, 9 * S)]);
        assert!(runs(&[]).is_empty());
    }

    fn pt(t: u64, v: f64) -> Point {
        Point::at(t, v)
    }

    fn member(points: Vec<Point>, rows: Vec<u64>, interval_ns: u64) -> MemberSeries {
        let labels = Labels::default();
        MemberSeries::new(
            vec![(labels.clone(), points)],
            [(labels, rows)].into(),
            interval_ns,
        )
    }

    /// The finer member answers where its rows cover the window, the coarser
    /// elsewhere; a point is never taken from a member whose rows start
    /// inside the window while another covers it.
    #[test]
    fn the_finer_member_answers_where_it_covers_the_window() {
        let coarse_rows: Vec<u64> = (0..=60).map(|i| i * S).collect();
        let fine_rows: Vec<u64> = (200..=400).map(|i| i * S / 10).collect();
        let grid: Vec<u64> = (0..=60).map(|i| i * S).collect();
        let coarse = member(grid.iter().map(|&t| pt(t, 1.0)).collect(), coarse_rows, S);
        let fine = member(
            grid.iter()
                .filter(|&&t| (20 * S..=40 * S).contains(&t))
                .map(|&t| pt(t, 2.0))
                .collect(),
            fine_rows,
            S / 10,
        );
        let out = combine(vec![fine, coarse], &|_, _| 5 * S, false);
        let values: Vec<(u64, f64)> = out[0].1.iter().map(|p| (p.t / S, p.v)).collect();
        for (t, v) in values {
            let want = if (25..=40).contains(&t) { 2.0 } else { 1.0 };
            assert_eq!(v, want, "at {t} s");
        }
    }

    /// S5: where no member holds rows, there is no point, even where a
    /// member's own evaluation gave one.
    #[test]
    fn a_span_no_member_holds_has_no_point() {
        let rows: Vec<u64> = (0..10).chain(15..25).map(|i| i * S).collect();
        let grid: Vec<u64> = (0..25).map(|i| i * S).collect();
        let a = member(grid.iter().map(|&t| pt(t, 1.0)).collect(), rows.clone(), S);
        let b = member(grid.iter().map(|&t| pt(t, 1.0)).collect(), rows, S);
        let out = combine(vec![a, b], &|_, _| 0, false);
        let times: Vec<u64> = out[0].1.iter().map(|p| p.t / S).collect();
        assert_eq!(
            times,
            (0..=10).chain(15..25).collect::<Vec<_>>(),
            "11 to 14 s are in the hole (one interval past the last row is held)"
        );
    }
    /// The streaming track answers as the runs of all the rows do, at every
    /// time it calls ready, for rows with holes, cadence changes and
    /// duplicates, offered with rows before and after.
    #[test]
    fn a_row_track_answers_as_the_runs_of_every_row() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _ in 0..300 {
            let mut ts: Vec<u64> = Vec::new();
            let mut t = next(5) * S;
            let mut every = [S / 4, S, 2 * S, 5 * S][next(4) as usize];
            for _ in 0..(5 + next(120)) {
                ts.push(t);
                if next(15) == 0 {
                    every = [S / 4, S, 2 * S, 5 * S][next(4) as usize];
                }
                t += if next(10) == 0 {
                    every * (2 + next(20))
                } else {
                    every
                };
            }
            // Split into rows before, scanned, after, with overlaps.
            let n = ts.len();
            let (a, b) = (
                next(n as u64 / 2 + 1) as usize,
                n - next(n as u64 / 2 + 1) as usize,
            );
            let before: Vec<u64> = ts[..a].to_vec();
            // The rows after can begin inside the scanned rows, as the
            // context rows after a read can repeat its last rows.
            let after: Vec<u64> = ts[b.max(a).saturating_sub(next(3) as usize)..].to_vec();
            let scanned: Vec<u64> = ts[a.saturating_sub(next(3) as usize)..b.max(a)].to_vec();
            let interval = [S / 4, S, 2 * S][next(3) as usize];
            let want = Coverage {
                runs: runs(&ts),
                interval_ns: interval,
            };
            let mut track = RowTrack::new(interval, before, after);
            let window = [0, S, 5 * S, 30 * S][next(4) as usize];
            let probe: Vec<u64> = (0..200).map(|i| i * S / 2).collect();
            let mut asked = 0;
            for &x in &scanned {
                track.push(x);
                while asked < probe.len() && track.ready(probe[asked]) {
                    let t = probe[asked];
                    assert_eq!(track.holds(t), want.holds(t), "holds at {t} of {ts:?}");
                    assert_eq!(
                        track.covers(t, window),
                        want.covers(t, window),
                        "covers at {t}"
                    );
                    // As the stage does once it has answered `t`.
                    track.forget_before(t);
                    asked += 1;
                }
            }
            track.finish();
            for &t in &probe[asked..] {
                assert!(track.ready(t));
                assert_eq!(track.holds(t), want.holds(t), "holds at {t} of {ts:?}");
                assert_eq!(
                    track.covers(t, window),
                    want.covers(t, window),
                    "covers at {t}"
                );
                track.forget_before(t);
            }
        }
    }
}
