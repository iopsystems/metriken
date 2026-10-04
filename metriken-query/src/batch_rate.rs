//! `rate`/`irate` of a counter on the evaluation grid, computed by a source
//! in one pass over its decoded columns rather than one sample stream per
//! series.
//!
//! A source that can decode a segment's columns at once (the segmented
//! reader) hands each sample to [`SeriesRate`], which computes
//! `CounterGridRate`'s points for samples pushed in increasing time order,
//! and writes them to a [`Sink`]: one vector of points per series, or
//! accumulators per group and grid point that reduce as `MergeReduce` does.
//!
//! An aggregate's values and bands can differ from the per-series path's in
//! the last bits of a float: they are summed in the order points are
//! computed rather than the order of the series.

use std::collections::VecDeque;

use crate::labels::Labels;
use crate::promql::streaming::{
    derive_group_labels, AggOp, GroupBy, Point, RateEdges, SPACING_PROBE,
};

/// What the dispatcher asks a source for.
pub(crate) struct GridRateRequest<'a> {
    /// The first sample time read: the grid's start less its lookback.
    pub data_start: u64,
    /// The first grid point.
    pub start_ns: u64,
    /// The last time a grid point can fall at.
    pub end_ns: u64,
    /// The spacing of the grid points.
    pub step_ns: u64,
    /// The averaging window per point; see `CounterGridRate`.
    pub span_ns: u64,
    /// Aggregate the series, or `None` for one result per series.
    pub group: Option<(AggOp, GroupBy<'a>)>,
}

#[derive(Clone, Copy)]
pub(crate) struct Sample {
    pub ts: u64,
    pub value: u64,
    pub window: Option<(u64, u64)>,
}

#[derive(Clone, Copy)]
struct Kept {
    ts: u64,
    cum: f64,
    window: Option<(u64, u64)>,
}

/// One series' grid rate, fed one sample at a time: `CounterGridRate`'s
/// arithmetic, with the grid advanced when a sample at or past the next point
/// arrives rather than by pulling. Expects samples in increasing time order;
/// out-of-order input gives points that differ from `CounterGridRate`'s.
pub(crate) struct SeriesRate {
    windowed: bool,
    probe: Vec<Sample>,
    settled: bool,
    typical: u64,
    pulled: usize,
    prev_value: Option<u64>,
    acc: f64,
    first_ts: Option<u64>,
    last_ts: Option<u64>,
    kept: VecDeque<Kept>,
    cursor_ns: u64,
    done: bool,
    /// The last edge evaluated: a point's right edge is the next point's
    /// left edge when the span is one step, and its value is reused unless a
    /// later sample has the edge's timestamp.
    last_edge: Option<(u64, Edge)>,
}

/// An edge's interpolated cumulative and, when real reads describe it, its
/// acquisition window.
#[derive(Clone, Copy)]
struct Edge {
    cum: Option<f64>,
    window: Option<(f64, f64)>,
}

impl SeriesRate {
    pub fn new(windowed: bool, start_ns: u64) -> Self {
        Self {
            windowed,
            probe: Vec::new(),
            settled: false,
            typical: 1,
            pulled: 0,
            prev_value: None,
            acc: 0.0,
            first_ts: None,
            last_ts: None,
            kept: VecDeque::new(),
            cursor_ns: start_ns,
            done: false,
            last_edge: None,
        }
    }

    pub fn push(&mut self, s: Sample, grid: &Grid, series: usize, sink: &mut dyn Sink) {
        if self.done {
            return;
        }
        if !self.settled {
            self.probe.push(s);
            if self.probe.len() == SPACING_PROBE {
                self.settle(grid, series, sink);
            }
            return;
        }
        self.ingest(s);
        self.advance(false, grid, series, sink);
    }

    /// The stream has ended: evaluate what the samples cover.
    pub fn finish(&mut self, grid: &Grid, series: usize, sink: &mut dyn Sink) {
        if !self.settled {
            self.settle(grid, series, sink);
        }
        self.advance(true, grid, series, sink);
    }

    fn settle(&mut self, grid: &Grid, series: usize, sink: &mut dyn Sink) {
        self.settled = true;
        let mut spacings: Vec<u64> = self
            .probe
            .windows(2)
            .map(|w| w[1].ts - w[0].ts)
            .take(SPACING_PROBE - 1)
            .collect();
        spacings.sort_unstable();
        self.typical = spacings
            .get(spacings.len() / 2)
            .copied()
            .unwrap_or(1)
            .max(1);
        if self.probe.len() < 2 || grid.step_ns == 0 {
            self.done = true;
            self.probe = Vec::new();
            return;
        }
        for s in std::mem::take(&mut self.probe) {
            self.ingest(s);
            self.advance(false, grid, series, sink);
        }
    }

    fn ingest(&mut self, s: Sample) {
        if let Some(prev) = self.prev_value {
            self.acc += if s.value >= prev {
                (s.value - prev) as f64
            } else {
                s.value as f64
            };
        }
        self.prev_value = Some(s.value);
        // A second sample at the cached edge's time is what the edge is
        // evaluated against once the first is trimmed.
        if self.last_edge.is_some_and(|(at, _)| at == s.ts) {
            self.last_edge = None;
        }
        self.first_ts.get_or_insert(s.ts);
        self.last_ts = Some(s.ts);
        self.pulled += 1;
        self.kept.push_back(Kept {
            ts: s.ts,
            cum: self.acc,
            window: s.window,
        });
    }

    fn advance(&mut self, exhausted: bool, grid: &Grid, series: usize, sink: &mut dyn Sink) {
        while !self.done {
            let t = self.cursor_ns;
            if t > grid.end_ns {
                self.done = true;
                return;
            }
            // The point needs a sample at or past it, unless there are no
            // more.
            if !exhausted && self.kept.back().is_none_or(|k| k.ts < t) {
                return;
            }
            match self.cursor_ns.checked_add(grid.step_ns) {
                Some(next) => self.cursor_ns = next,
                None => self.done = true,
            }
            let Some(left) = t.checked_sub(grid.span_ns) else {
                continue;
            };
            self.trim(left);
            if exhausted && self.last_ts.is_some_and(|last| t > last) {
                self.done = true;
                return;
            }
            if let Some(p) = self.point(t, left) {
                sink.emit(series, grid.index(t), p);
            }
        }
    }

    fn trim(&mut self, left: u64) {
        while self.kept.len() >= 2 && self.kept[1].ts <= left {
            self.kept.pop_front();
        }
    }

    fn observed(&self, edge: u64) -> bool {
        match (self.first_ts, self.last_ts) {
            (Some(first), Some(last)) => edge >= first && edge <= last,
            _ => false,
        }
    }

    /// `CounterGridRate::interp` and `sampled_window` at `edge`, from one
    /// search of the kept samples.
    fn edge(&self, edge: u64) -> Edge {
        let none = Edge {
            cum: None,
            window: None,
        };
        if !self.observed(edge) {
            return none;
        }
        let hi = self.kept.partition_point(|k| k.ts < edge);
        let Some(k_hi) = self.kept.get(hi) else {
            return none;
        };
        if k_hi.ts == edge {
            return Edge {
                cum: Some(k_hi.cum),
                window: self
                    .windowed
                    .then_some(k_hi.window)
                    .flatten()
                    .map(|(b, e)| (b as f64, e as f64)),
            };
        }
        let Some(k_lo) = hi.checked_sub(1).and_then(|lo| self.kept.get(lo)) else {
            return none;
        };
        let frac = (edge - k_lo.ts) as f64 / (k_hi.ts - k_lo.ts) as f64;
        let cum = Some(k_lo.cum + frac * (k_hi.cum - k_lo.cum));
        // A hole: the reads either side are further apart than twice the
        // typical spacing, and no read describes the edge.
        let hole = self.pulled >= 2 && k_hi.ts - k_lo.ts > self.typical.saturating_mul(2);
        let window = if !self.windowed || hole {
            None
        } else {
            k_lo.window
                .zip(k_hi.window)
                .map(|((b_lo, e_lo), (b_hi, e_hi))| {
                    (
                        b_lo as f64 + frac * (b_hi as f64 - b_lo as f64),
                        e_lo as f64 + frac * (e_hi as f64 - e_lo as f64),
                    )
                })
        };
        Edge { cum, window }
    }

    fn point(&mut self, t: u64, left: u64) -> Option<Point> {
        let left_edge = match self.last_edge {
            Some((at, e)) if at == left => e,
            _ => self.edge(left),
        };
        let right_edge = self.edge(t);
        self.last_edge = Some((t, right_edge));
        let (v_hi, v_lo) = (right_edge.cum?, left_edge.cum?);
        let step_s = (t - left) as f64 / 1e9;
        if step_s <= 0.0 {
            return None;
        }
        let increase = v_hi - v_lo;
        let v = increase / step_s;
        let window_pair = left_edge.window.zip(right_edge.window);
        let interpolated = window_pair.is_none() && self.windowed;
        let bounds = window_pair
            .and_then(|((b_left, e_left), (b_hi, e_hi))| {
                let elapsed_max = (e_hi - b_left) / 1e9;
                let elapsed_min = (b_hi - e_left) / 1e9;
                (elapsed_min > 0.0 && elapsed_max > 0.0)
                    .then(|| (increase / elapsed_max, increase / elapsed_min))
            })
            .map(|(lo, hi)| (lo.min(v), hi.max(v)));
        let edges = bounds.and_then(|_| {
            window_pair.map(|(left_w, right_w)| RateEdges {
                left: left_w,
                right: right_w,
            })
        });
        Some(Point {
            t,
            v,
            bounds,
            edges,
            interpolated,
        })
    }
}

/// The evaluation grid.
pub(crate) struct Grid {
    pub start_ns: u64,
    pub end_ns: u64,
    pub step_ns: u64,
    pub span_ns: u64,
}

impl Grid {
    pub fn from_request(r: &GridRateRequest<'_>) -> Self {
        Self {
            start_ns: r.start_ns,
            end_ns: r.end_ns,
            step_ns: r.step_ns,
            span_ns: r.span_ns.max(1),
        }
    }

    fn index(&self, t: u64) -> usize {
        ((t - self.start_ns) / self.step_ns) as usize
    }

    fn len(&self) -> usize {
        if self.end_ns < self.start_ns || self.step_ns == 0 {
            return 0;
        }
        ((self.end_ns - self.start_ns) / self.step_ns) as usize + 1
    }
}

/// The groups of `series` (their labels, in order) under `group_by`: each
/// group's labels, and each series' group.
pub(crate) fn groups(group_by: GroupBy<'_>, series: &[Labels]) -> (Vec<Labels>, Vec<usize>) {
    let mut labels: Vec<Labels> = Vec::new();
    let mut index: std::collections::HashMap<Labels, usize> = std::collections::HashMap::new();
    let group_of = series
        .iter()
        .map(|l| {
            let g = derive_group_labels(l, group_by);
            *index.entry(g.clone()).or_insert_with(|| {
                labels.push(g);
                labels.len() - 1
            })
        })
        .collect();
    (labels, group_of)
}

/// Where computed points go.
pub(crate) trait Sink {
    fn emit(&mut self, series: usize, index: usize, point: Point);
}

/// One vector of points per series.
pub(crate) struct PerSeries {
    pub points: Vec<Vec<Point>>,
}

impl Sink for PerSeries {
    fn emit(&mut self, series: usize, _index: usize, point: Point) {
        self.points[series].push(point);
    }
}

/// What `MergeReduce` accumulates for one group at one grid point.
#[derive(Clone, Copy)]
struct Slot {
    sum: f64,
    count: u32,
    min: f64,
    max: f64,
    lo: f64,
    hi: f64,
    any_bounded: bool,
    any_interpolated: bool,
    edges: Option<RateEdges>,
    unanimous: bool,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            sum: 0.0,
            count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            lo: 0.0,
            hi: 0.0,
            any_bounded: false,
            any_interpolated: false,
            edges: None,
            unanimous: true,
        }
    }
}

/// Grid points per block of accumulators.
const BLOCK: usize = 1024;

/// Accumulators per group and grid point, reducing as `MergeReduce` does. A
/// group's accumulators are allocated in blocks of [`BLOCK`] grid points, a
/// block when a point first reaches it, so a group with points in a short
/// stretch holds that stretch.
pub(crate) struct Grouped {
    op: AggOp,
    group_of: Vec<usize>,
    labels: Vec<Labels>,
    blocks: Vec<Vec<Option<Box<[Slot]>>>>,
    grid_len: usize,
}

impl Grouped {
    /// Accumulators for groups `labels`, where series `s` (an index into
    /// what this sink is fed) belongs to group `group_of[s]`.
    pub fn new(op: AggOp, labels: Vec<Labels>, group_of: Vec<usize>, grid: &Grid) -> Self {
        let grid_len = grid.len();
        Self {
            op,
            group_of,
            blocks: (0..labels.len())
                .map(|_| (0..grid_len.div_ceil(BLOCK)).map(|_| None).collect())
                .collect(),
            labels,
            grid_len,
        }
    }

    /// Fold `other`, holding the same groups for other series, into this.
    pub fn merge(&mut self, other: Grouped) {
        for (mine, theirs) in self.blocks.iter_mut().zip(other.blocks) {
            for (mine, theirs) in mine.iter_mut().zip(theirs) {
                let Some(theirs) = theirs else {
                    continue;
                };
                let Some(mine) = mine else {
                    *mine = Some(theirs);
                    continue;
                };
                merge_block(mine, &theirs);
            }
        }
    }

    /// Each group's points, as `MergeReduce` emits them.
    pub fn finish(self, grid: &Grid) -> Vec<(Labels, Vec<Point>)> {
        let op = self.op;
        self.labels
            .into_iter()
            .zip(self.blocks)
            .map(|(labels, blocks)| {
                let points = blocks
                    .into_iter()
                    .enumerate()
                    .filter_map(|(b, block)| block.map(|block| (b, block)))
                    .flat_map(|(b, block)| {
                        block
                            .into_vec()
                            .into_iter()
                            .enumerate()
                            .map(move |(i, s)| (b * BLOCK + i, s))
                    })
                    .filter(|(_, s)| s.count > 0)
                    .map(|(k, s)| {
                        let v = match op {
                            AggOp::Sum => s.sum,
                            AggOp::Avg => s.sum / s.count as f64,
                            AggOp::Min => s.min,
                            AggOp::Max => s.max,
                            AggOp::Count => s.count as f64,
                        };
                        let bounds = if s.any_bounded && !s.any_interpolated {
                            match op {
                                AggOp::Sum => Some((s.lo, s.hi)),
                                AggOp::Avg => Some((s.lo / s.count as f64, s.hi / s.count as f64)),
                                AggOp::Min | AggOp::Max | AggOp::Count => None,
                            }
                        } else {
                            None
                        };
                        Point {
                            t: grid.start_ns + k as u64 * grid.step_ns,
                            v,
                            bounds,
                            edges: if s.unanimous { s.edges } else { None },
                            interpolated: s.any_interpolated,
                        }
                    })
                    .collect();
                (labels, points)
            })
            .collect()
    }
}

/// Fold block `b` into block `a`, both covering the same grid points.
fn merge_block(a: &mut [Slot], b: &[Slot]) {
    for (a, b) in a.iter_mut().zip(b) {
        if b.count == 0 {
            continue;
        }
        if a.count == 0 {
            *a = *b;
            continue;
        }
        a.unanimous = a.unanimous && b.unanimous && a.edges == b.edges;
        a.sum += b.sum;
        a.count += b.count;
        a.min = a.min.min(b.min);
        a.max = a.max.max(b.max);
        a.lo += b.lo;
        a.hi += b.hi;
        a.any_bounded |= b.any_bounded;
        a.any_interpolated |= b.any_interpolated;
    }
}

impl Sink for Grouped {
    fn emit(&mut self, series: usize, index: usize, p: Point) {
        let block = self.blocks[self.group_of[series]][index / BLOCK].get_or_insert_with(|| {
            let len = BLOCK.min(self.grid_len - index / BLOCK * BLOCK);
            vec![Slot::default(); len].into_boxed_slice()
        });
        let s = &mut block[index % BLOCK];
        let v = p.v;
        if s.count == 0 {
            s.edges = p.edges;
        } else if s.edges != p.edges {
            s.unanimous = false;
        }
        s.sum += v;
        s.count += 1;
        s.min = s.min.min(v);
        s.max = s.max.max(v);
        let (lo, hi) = p.bounds.unwrap_or((v, v));
        s.lo += lo;
        s.hi += hi;
        s.any_bounded |= p.bounds.is_some();
        s.any_interpolated |= p.interpolated;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::promql::streaming::{aggregate, CounterGridRate, LabeledSeries};

    /// xorshift: deterministic inputs without a dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    const S: u64 = 1_000_000_000;

    fn batch(samples: &[Sample], windowed: bool, grid: &Grid) -> Vec<Point> {
        let mut sink = PerSeries {
            points: vec![Vec::new()],
        };
        let mut r = SeriesRate::new(windowed, grid.start_ns);
        for s in samples {
            r.push(*s, grid, 0, &mut sink);
        }
        r.finish(grid, 0, &mut sink);
        sink.points.pop().unwrap()
    }

    fn streamed(samples: &[Sample], windowed: bool, grid: &Grid) -> Vec<Point> {
        let ts: Vec<u64> = samples.iter().map(|s| s.ts).collect();
        let vs: Vec<u64> = samples.iter().map(|s| s.value).collect();
        let ws = windowed.then(|| samples.iter().map(|s| s.window.unwrap()).collect());
        CounterGridRate::new(
            ts,
            &vs,
            grid.start_ns,
            grid.end_ns,
            grid.step_ns,
            grid.span_ns,
            ws,
        )
        .collect()
    }

    /// Samples in increasing time order, with duplicates, resets and holes
    /// as asked.
    fn samples(
        rng: &mut Rng,
        dups: bool,
        resets: bool,
        holes: bool,
        windowed: bool,
    ) -> Vec<Sample> {
        let n = rng.below(30) as usize;
        let aligned = rng.below(2) == 0;
        let mut ts = S + rng.below(5) * S + if aligned { 0 } else { rng.below(S) };
        let mut v = rng.below(1000);
        let mut out = Vec::new();
        for _ in 0..n {
            let mut gap = if aligned { S } else { S + rng.below(S / 10) };
            if holes && rng.below(6) == 0 {
                gap *= 2 + rng.below(5);
            }
            if dups && rng.below(5) == 0 {
                gap = 0;
            }
            ts += gap;
            if resets && rng.below(7) == 0 {
                v = rng.below(10);
            } else {
                v += rng.below(100);
            }
            let window = (ts - rng.below(1000), ts + rng.below(1_000_000));
            out.push(Sample {
                ts,
                value: v,
                window: windowed.then_some(window),
            });
        }
        out
    }

    /// For samples in increasing time order, `SeriesRate` gives exactly
    /// `CounterGridRate`'s points: with duplicates at grid points, resets
    /// and holes, windowed or not, on several steps and spans.
    #[test]
    fn series_rate_matches_counter_grid_rate() {
        for (class, dups, resets, holes) in [
            ("plain", false, false, false),
            ("resets", false, true, false),
            ("holes", false, false, true),
            ("duplicates", true, false, false),
            ("all", true, true, true),
        ] {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
            for case in 0..4000 {
                let windowed = rng.below(2) == 0;
                let input = samples(&mut rng, dups, resets, holes, windowed);
                let step = [S, S / 2, 2 * S, 2_500_000_000][rng.below(4) as usize];
                let span = if rng.below(2) == 0 {
                    step
                } else {
                    step * (1 + rng.below(3))
                };
                let start = rng.below(8) * step;
                let grid = Grid {
                    start_ns: start,
                    end_ns: start + rng.below(40) * step,
                    step_ns: step,
                    span_ns: span,
                };
                assert_eq!(
                    format!("{:?}", batch(&input, windowed, &grid)),
                    format!("{:?}", streamed(&input, windowed, &grid)),
                    "{class} case {case}: step {step} span {span}"
                );
            }
        }
    }

    fn same(a: &Point, b: &Point) -> bool {
        let close = |x: f64, y: f64| {
            x == y || (x.is_nan() && y.is_nan()) || (x - y).abs() <= 1e-12 * x.abs().max(y.abs())
        };
        a.t == b.t
            && close(a.v, b.v)
            && a.interpolated == b.interpolated
            && a.edges == b.edges
            && match (a.bounds, b.bounds) {
                (Some((x0, x1)), Some((y0, y1))) => close(x0, y0) && close(x1, y1),
                (None, None) => true,
                _ => false,
            }
    }

    /// `Grouped` reduces as `aggregate()` does for every op: exactly when
    /// fed one partition in series order, and to rounding when the series
    /// are split across partitions and merged. Points carry mixed bands,
    /// edges and interpolation, and NaN and infinite values.
    #[test]
    fn grouped_matches_merge_reduce() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let grid = Grid {
            start_ns: 0,
            end_ns: 2999 * S,
            step_ns: S,
            span_ns: S,
        };
        let edges = [
            None,
            Some(RateEdges {
                left: (1.0, 2.0),
                right: (3.0, 4.0),
            }),
            Some(RateEdges {
                left: (1.5, 2.0),
                right: (3.0, 4.5),
            }),
        ];
        for case in 0..300 {
            let n = 1 + rng.below(12) as usize;
            let series: Vec<(Labels, Vec<Point>)> = (0..n)
                .map(|i| {
                    let group = (rng.below(3)).to_string();
                    let labels =
                        Labels::from([("g", group.as_str()), ("i", i.to_string().as_str())]);
                    let mut points = Vec::new();
                    for k in 0..grid.len() {
                        if rng.below(3) == 0 {
                            continue;
                        }
                        let v = match rng.below(50) {
                            0 => f64::NAN,
                            1 => f64::INFINITY,
                            _ => rng.below(1000) as f64 / 7.0,
                        };
                        let interpolated = rng.below(10) == 0;
                        points.push(Point {
                            t: k as u64 * S,
                            v,
                            bounds: (!interpolated && rng.below(2) == 0)
                                .then_some((v - 1.0, v + 1.0)),
                            edges: edges[rng.below(3) as usize],
                            interpolated,
                        });
                    }
                    (labels, points)
                })
                .collect();
            let labels: Vec<Labels> = series.iter().map(|(l, _)| l.clone()).collect();
            let by = ["g".to_string()];
            for op in [AggOp::Sum, AggOp::Avg, AggOp::Min, AggOp::Max, AggOp::Count] {
                let expected: Vec<(Labels, Vec<Point>)> = aggregate(
                    series
                        .iter()
                        .map(|(l, p)| LabeledSeries::new(l.clone(), p.clone().into_iter()))
                        .collect(),
                    op,
                    GroupBy::Include(&by),
                )
                .into_iter()
                .map(|s| (s.labels, s.iter.collect()))
                .collect();
                let (glabels, group_of) = groups(GroupBy::Include(&by), &labels);
                let reduce = |parts: usize, rng: &mut Rng| {
                    let part: Vec<usize> =
                        (0..n).map(|_| rng.below(parts as u64) as usize).collect();
                    let mut sinks: Vec<Grouped> = (0..parts)
                        .map(|p| {
                            let members: Vec<usize> = (0..n).filter(|s| part[*s] == p).collect();
                            Grouped::new(
                                op,
                                glabels.clone(),
                                members.iter().map(|s| group_of[*s]).collect(),
                                &grid,
                            )
                        })
                        .collect();
                    let mut local = vec![0usize; parts];
                    for (s, (_, points)) in series.iter().enumerate() {
                        let p = part[s];
                        for point in points {
                            sinks[p].emit(local[p], grid.index(point.t), *point);
                        }
                        local[p] += 1;
                    }
                    let mut all = sinks.remove(0);
                    for g in sinks {
                        all.merge(g);
                    }
                    all.finish(&grid)
                };
                for parts in [1, 3] {
                    let got = reduce(parts, &mut rng);
                    for (labels, points) in &expected {
                        let (_, mine) = got
                            .iter()
                            .find(|(l, _)| l == labels)
                            .expect("every group present");
                        assert_eq!(mine.len(), points.len(), "case {case} {op:?} parts {parts}");
                        for (a, b) in mine.iter().zip(points) {
                            if parts == 1 {
                                assert_eq!(
                                    format!("{a:?}"),
                                    format!("{b:?}"),
                                    "case {case} {op:?}"
                                );
                            } else {
                                assert!(
                                    same(a, b),
                                    "case {case} {op:?} parts {parts}: {a:?} != {b:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
