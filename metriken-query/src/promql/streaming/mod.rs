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
//!   interval-attributable), `CounterPairwiseRate` (Raw mode / `deriv`).
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
//! wrappers, counter-rate `deriv`).

use std::collections::HashMap;

use crate::labels::Labels;
use crate::promql::MatrixSample;

mod aggregate;
mod binary;
mod deriv;
pub(crate) mod dispatch;
mod gauge;
pub(crate) mod histogram;
mod rate;

#[cfg(test)]
mod tests;

pub(crate) use aggregate::derive_group_labels;
pub(crate) use aggregate::{aggregate, AggOp, GroupBy};
pub(crate) use binary::{interval_binop, matrix_matrix_op, matrix_scalar_op, BinOp, MatchSpec};
pub(crate) use deriv::StreamingDeriv;
pub(crate) use gauge::{GaugeAvgOverTime, GaugeDeriv, GaugeIdelta, GaugeStepGrid};
pub(crate) use rate::{CounterGridRate, CounterPairwiseRate};

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
        }
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

/// Boundary collector: drain a streaming result into the same
/// `MatrixSample` shape the eager engine returns.
pub fn collect_to_matrix(streaming: SeriesSet<'_>, metric_name: Option<&str>) -> Vec<MatrixSample> {
    streaming
        .into_iter()
        .filter_map(|ls| {
            #[allow(clippy::type_complexity)]
            let points: Vec<((f64, f64), Option<(f64, f64)>)> = ls
                .iter
                .map(|p| ((p.t as f64 / 1e9, p.v), p.bounds))
                .collect();
            if points.is_empty() {
                return None;
            }
            let values: Vec<(f64, f64)> = points.iter().map(|(v, _)| *v).collect();
            // Emit intervals only when every point carries a band; else None.
            // Bands originate at rate()/irate() (and histogram value bands) and
            // propagate through scalar ops, sum/avg aggregation, and
            // series-op-series binary ops — but an unsupported operator upstream
            // (e.g. min/max) drops them, making the series non-uniform.
            let intervals: Option<Vec<(f64, f64)>> = if points.iter().all(|(_, b)| b.is_some()) {
                Some(points.iter().map(|(_, b)| b.unwrap()).collect())
            } else {
                None
            };
            let mut metric: HashMap<String, String> = HashMap::new();
            if let Some(name) = metric_name {
                metric.insert("__name__".to_string(), name.to_string());
            }
            for (k, v) in ls.labels.inner.iter() {
                metric.insert(k.clone(), v.clone());
            }
            Some(MatrixSample {
                metric,
                values,
                intervals,
            })
        })
        .collect()
}
