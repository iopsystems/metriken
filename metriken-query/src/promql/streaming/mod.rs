//! Streaming time-series query pipeline.
//!
//! A naïve evaluator materialises every intermediate stage as
//! `Vec<(f64, f64)>`. For the WASM viewer that means a typical
//! `sum by (label) (irate(metric[5s]))` over many series produces a
//! transient `O(stages × points × series)` heap footprint just to be
//! reduced down to `O(stages × points)` at the boundary.
//!
//! This module replaces the in-flight matrices with iterator pipelines:
//!
//! * [`Point`] — the single sample carried through the pipeline.
//! * [`LabeledSeries`] — labelset + boxed iterator yielding `Point`.
//! * Operators (e.g. `CounterIrate`, `MergeReduce`) wrap upstream
//!   iterators and pull lazily, holding only their own windowed state.
//!
//! Wired-up shapes:
//!
//! * Counter producers: `CounterGridRate` (rate/irate — grid-aligned,
//!   interval-attributable), `CounterPairwiseRate` (Raw mode).
//! * Gauge producers: `GaugeStepGrid` (bare selector),
//!   `GaugeAvgOverTime`, `GaugeIdelta`, `GaugeDeriv`.
//! * Aggregations: `MergeReduce` reducer driven by [`AggOp`]
//!   (sum/avg/min/max/count) with [`GroupBy`] (by/without).
//! * Binary ops: `ScalarBroadcast` (matrix×scalar) and
//!   `matrix_matrix_op` (matrix×matrix with on/ignoring).
//! * Histogram quantiles: `histogram::quantiles`.
//!
//! The dispatcher (`dispatch::try_streaming`) walks the parsed AST
//! and assembles the pipeline for each recognised shape. Eager
//! handlers in `promql::mod` cover the residual cases (heatmaps,
//! one-side-eager binary ops, group_left/right, scalar/vector
//! wrappers).

use std::collections::HashMap;

use crate::labels::Labels;
use crate::promql::MatrixSample;

mod aggregate;
mod binary;
pub(crate) mod conflicts;
pub(crate) mod dispatch;
mod gauge;
pub(crate) mod gauge_histogram;
pub(crate) mod histogram;
pub(crate) mod per_source;
mod rate;

#[cfg(test)]
mod tests;

pub(crate) use aggregate::derive_group_labels;
pub(crate) use aggregate::{aggregate, AggOp, GroupBy};
pub(crate) use binary::{
    interval_binop, matrix_matrix_op, matrix_scalar_op, scalar_point, BinOp, MatchSpec,
};
pub(crate) use gauge::{AtPoints, GaugeAvgOverTime, GaugeDeriv, GaugeIdelta, GaugeStepGrid};
pub(crate) use rate::{range_start, CounterGridRate, CounterPairwiseRate, SPACING_PROBE};

#[cfg(test)]
pub(crate) use aggregate::sum_by;

/// An uncertainty interval `(lo, hi)` — either an acquisition-window bound
/// (rate/irate) or a histogram bucket-resolution band. Aliased so the nested
/// `Option<Band>` / `Vec<Vec<Option<Band>>>` shapes that thread it through the
/// pipeline stay readable (and clear clippy's `type_complexity`).
pub(crate) type Band = (f64, f64);

/// The acquisition edges a rate band was derived from.
///
/// Kept on the point so a later binary op can RE-DERIVE the band over a wider
/// span. When two operands come from different tables they were read at
/// different instants, and combining them as if simultaneous costs accuracy
/// that their own bands do not contain — see
/// `docs/journal/2026-08-21-cross-table-uncertainty.md` in rezolus.
///
/// Equality of the edges is the same-read test, and it is exact rather than a
/// heuristic: an acquisition group is one read with one window, so two points
/// carrying identical edges came from the same read and need no widening at
/// all.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct RateEdges {
    /// Interpolated acquisition window at the range's left edge.
    pub left: (f64, f64),
    /// ... and at its right edge.
    pub right: (f64, f64),
}

/// A single sample emitted through a streaming pipeline.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Point {
    pub t: u64,
    pub v: f64,
    /// Uncertainty interval (lo, hi) from acquisition windows; originated by
    /// rate()/irate() and propagated through scalar ops, aggregation, and
    /// series-op-series binary ops. None means exact / not applicable.
    pub bounds: Option<Band>,
    /// Where `bounds` came from, when it came from a rate over acquisition
    /// windows. `None` for every other producer — and for aggregated points,
    /// which no longer correspond to a single read.
    pub(crate) edges: Option<RateEdges>,
    /// This point's interval is not bounded by real acquisitions at both ends:
    /// its value is interpolated across a span in which the producer did not
    /// read.
    ///
    /// A counter that is null for a stretch — a device that appears at runtime,
    /// a partially-populated group, a read that failed — leaves a hole that
    /// `rate`/`irate` still span (except a raw `irate` across a gap longer
    /// than four of its interval, which gives a timing point), because the
    /// total across the hole is known even though its distribution inside is
    /// not. Such a point carries a value, and no `bounds`: an uncertainty
    /// band answers "how precisely do we know the average over THIS
    /// interval", and for an interval nobody observed the honest answer is
    /// unbounded, not a number. This flag is what says so, and it is what
    /// lets a consumer render the difference (a desaturated connector, a
    /// dashed segment) rather than showing an interpolated point as though
    /// it had been measured.
    ///
    /// Propagates through operators the way `bounds` does: any operand being
    /// interpolated makes the result interpolated.
    pub interpolated: bool,
    /// A timing point: in raw mode, a sample read whose function gives no
    /// value there (its window holds too little), the first sample a raw
    /// `irate` reads or one further from the sample before it than its gap
    /// limit, and a join's or an aggregate's time where an operand has only
    /// a timing point. A join holds a point for the gap before it
    /// ([`Held`]), and a timing point is what that gap is measured from; it
    /// is never a value. A result leaves timing points out.
    pub(crate) timing: bool,
}
impl Point {
    /// A point with no uncertainty bound (the default for every producer/operator
    /// except rate/irate).
    pub fn at(t: u64, v: f64) -> Self {
        Self {
            t,
            v,
            bounds: None,
            edges: None,
            interpolated: false,
            timing: false,
        }
    }

    /// A timing point at `t`; see [`Point::timing`](Point#structfield.timing).
    pub(crate) fn timing_at(t: u64) -> Self {
        Self {
            timing: true,
            ..Self::at(t, f64::NAN)
        }
    }
}

/// How an aggregation or a binary operator lines up the points of the
/// series it combines.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Join {
    /// At the timestamps the series share. Every producer on the grid (or on
    /// explicit evaluation timestamps) places its points at the same
    /// instants, so this is the join for [`crate::RateMode::Grid`].
    Exact,
    /// At every timestamp any of them has, each series contributing its
    /// latest point while that point is no older than the gap that preceded
    /// it (the series' own spacing), and at most the bound given, in
    /// nanoseconds ([`Held`]). For [`crate::RateMode::Raw`], where each
    /// series' points are at its own sample times: two acquisition groups,
    /// or two recordings, read at different instants and share none.
    Latest(u64),
}

impl Join {
    /// The join for `mode`; `hold_ns` bounds a raw join's hold.
    pub(crate) fn for_mode(mode: crate::RateMode, hold_ns: u64) -> Self {
        match mode {
            crate::RateMode::Grid => Join::Exact,
            crate::RateMode::Raw => Join::Latest(hold_ns),
        }
    }
}

/// A series sampled at instants taken from other series ([`Join::Latest`]):
/// its latest point at or before the instant, held while the instant is no
/// further past it than the gap from the point before, and at most
/// `max_hold_ns`. A series' first point is held for `max_hold_ns`. A
/// timing point ([`Point::timing`](Point#structfield.timing)) is a point the
/// gap is measured from and never a value.
///
/// The bound is what lets a read that starts a bounded time before the
/// range give every point its hold: the dispatcher reads back twice the
/// bound for each nested join, and the window (`raw_read_back`), so a point
/// whose hold can reach the range's start has the point before it read, or
/// is more than the bound past it.
pub(crate) struct Held<'a> {
    iter: std::iter::Peekable<Box<dyn Iterator<Item = Point> + 'a>>,
    latest: Option<(Point, u64)>,
    max_hold_ns: u64,
}

impl<'a> Held<'a> {
    pub(crate) fn new(iter: Box<dyn Iterator<Item = Point> + 'a>, max_hold_ns: u64) -> Self {
        Self {
            iter: iter.peekable(),
            latest: None,
            max_hold_ns,
        }
    }

    /// The time of the series' next point not yet taken.
    pub(crate) fn peek_t(&mut self) -> Option<u64> {
        self.iter.peek().map(|p| p.t)
    }

    /// Take every point at or before `t`.
    pub(crate) fn advance_to(&mut self, t: u64) {
        while let Some(p) = self.iter.next_if(|p| p.t <= t) {
            let hold = match self.latest {
                Some((q, _)) => p.t.saturating_sub(q.t).min(self.max_hold_ns),
                None => self.max_hold_ns,
            };
            self.latest = Some((p, hold));
        }
    }

    /// The series' value at `t`, after [`advance_to`](Self::advance_to) it.
    /// A timing point is never a value.
    pub(crate) fn at(&self, t: u64) -> Option<Point> {
        self.latest
            .filter(|(p, hold)| !p.timing && t.saturating_sub(p.t) <= *hold)
            .map(|(p, _)| p)
    }

    /// Whether the series' latest point is a timing point at `t`.
    pub(crate) fn timing_at(&self, t: u64) -> bool {
        self.latest.is_some_and(|(p, _)| p.timing && p.t == t)
    }

    /// Drop the rest of the series.
    pub(crate) fn clear(&mut self) {
        self.iter =
            (Box::new(std::iter::empty()) as Box<dyn Iterator<Item = Point> + 'a>).peekable();
        self.latest = None;
    }
}

/// A labeled, lazily-produced time series.
pub struct LabeledSeries<'a> {
    pub labels: Labels,
    pub iter: Box<dyn Iterator<Item = Point> + 'a>,
}

impl<'a> LabeledSeries<'a> {
    pub fn new<I>(labels: Labels, iter: I) -> Self
    where
        I: Iterator<Item = Point> + 'a,
    {
        Self {
            labels,
            iter: Box::new(iter),
        }
    }
}

/// Output of a streaming evaluation stage.
pub type SeriesSet<'a> = Vec<LabeledSeries<'a>>;

/// A series' labels and its points, computed in one pass by a source; see
/// [`crate::batch_rate`].
pub(crate) type LabeledPoints = (crate::labels::Labels, Vec<Point>);

/// Boundary collector for display mode, for a query over `[start_s,
/// end_s]` at `step_s`: each series is reduced as its points are pulled,
/// holding one bucket of them, so the full-resolution matrix is never built.
/// Gives what [`collect_to_matrix`] followed by
/// [`crate::display::display_from_result`] gives.
pub(crate) fn collect_to_display(
    streaming: SeriesSet<'_>,
    metric_name: Option<&str>,
    start_s: f64,
    end_s: f64,
    step_s: f64,
    opts: &crate::DisplayOptions,
) -> Vec<crate::DisplaySeries> {
    let width = crate::display::bucket_width(start_s, end_s, step_s, opts.budget);
    streaming
        .into_iter()
        .filter_map(|ls| {
            let mut r = crate::display::BucketReducer::new(width, opts.band);
            for p in ls.iter {
                r.push(p.t as f64 / 1e9, p.v, p.bounds, p.interpolated);
            }
            if r.is_empty() {
                return None;
            }
            let mut metric: HashMap<String, String> = HashMap::new();
            if let Some(name) = metric_name {
                metric.insert("__name__".to_string(), name.to_string());
            }
            for (k, v) in ls.labels.inner.iter() {
                metric.insert(k.clone(), v.clone());
            }
            Some(r.finish(metric, step_s, opts))
        })
        .collect()
}

/// Boundary collector: drain a streaming result into the same
/// `MatrixSample` shape the eager engine returns.
pub fn collect_to_matrix(streaming: SeriesSet<'_>, metric_name: Option<&str>) -> Vec<MatrixSample> {
    streaming
        .into_iter()
        .filter_map(|ls| {
            #[allow(clippy::type_complexity)]
            let points: Vec<((f64, f64), Option<(f64, f64)>, bool)> = ls
                .iter
                .map(|p| ((p.t as f64 / 1e9, p.v), p.bounds, p.interpolated))
                .collect();
            if points.is_empty() {
                return None;
            }
            let values: Vec<(f64, f64)> = points.iter().map(|(v, _, _)| *v).collect();
            // Emit intervals only when every point carries a band; else None.
            // Bands originate at rate()/irate() (and histogram value bands) and
            // propagate through scalar ops, sum/avg aggregation, and
            // series-op-series binary ops — but an unsupported operator upstream
            // (e.g. min/max) drops them, making the series non-uniform.
            //
            // This field's all-or-nothing rule is kept as it was, for consumers
            // that predate `bands`. It is lossy for a series where only some
            // points carry a band — which is what `bands` below is for.
            let intervals: Option<Vec<(f64, f64)>> = if points.iter().all(|(_, b, _)| b.is_some()) {
                Some(points.iter().map(|(_, b, _)| b.unwrap()).collect())
            } else {
                None
            };
            // The lossless form: present when ANY point has a band, `None` at
            // the points that do not.
            let bands: Option<Vec<Option<(f64, f64)>>> = points
                .iter()
                .any(|(_, b, _)| b.is_some())
                .then(|| points.iter().map(|(_, b, _)| *b).collect());
            let interpolated: Option<Vec<bool>> = points
                .iter()
                .any(|(_, _, i)| *i)
                .then(|| points.iter().map(|(_, _, i)| *i).collect());
            let mut metric: HashMap<String, String> = HashMap::new();
            if let Some(name) = metric_name {
                metric.insert("__name__".to_string(), name.to_string());
            }
            for (k, v) in ls.labels.inner.iter() {
                metric.insert(k.clone(), v.clone());
            }
            Some(
                MatrixSample::new(metric, values)
                    .with_intervals(intervals)
                    .with_bands(bands)
                    .with_interpolated(interpolated),
            )
        })
        .collect()
}
