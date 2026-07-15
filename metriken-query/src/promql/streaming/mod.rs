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
//! * Counter producers: `CounterIrate`, `CounterRate`.
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
#[cfg(test)]
use crate::types::Counters;

mod aggregate;
mod binary;
mod deriv;
pub(crate) mod dispatch;
mod gauge;
pub(crate) mod histogram;
mod irate;
mod rate;

#[cfg(test)]
mod tests;

pub(crate) use aggregate::derive_group_labels;
pub(crate) use aggregate::{aggregate, AggOp, GroupBy};
pub(crate) use binary::{matrix_matrix_op, matrix_scalar_op, BinOp, MatchSpec};
pub(crate) use deriv::StreamingDeriv;
pub(crate) use gauge::{GaugeAvgOverTime, GaugeDeriv, GaugeIdelta, GaugeStepGrid};
pub(crate) use irate::CounterIrate;
pub(crate) use rate::{CounterPairwiseRate, CounterRate};

#[cfg(test)]
pub(crate) use aggregate::sum_by;

/// A single sample emitted through a streaming pipeline.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Point {
    pub t: u64,
    pub v: f64,
    /// Uncertainty interval (lo, hi) from acquisition windows; set only by
    /// rate()/irate() (leaf-only). None means exact / not applicable.
    pub bounds: Option<(f64, f64)>,
}
impl Point {
    /// A point with no uncertainty bound (the default for every producer/operator
    /// except rate/irate).
    pub fn at(t: u64, v: f64) -> Self {
        Self { t, v, bounds: None }
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

// ─── Counters methods ────────────────────────────────────────────────────────

#[cfg(test)]
impl Counters {
    pub(crate) fn irate<'a>(
        &'a self,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        range_ns: u64,
    ) -> SeriesSet<'a> {
        self.series
            .iter()
            .filter(|c| filter.inner.is_empty() || c.labels.matches(filter))
            .map(|c| {
                let iter = CounterIrate::new(
                    &c.timestamps,
                    &c.values,
                    start_ns,
                    end_ns,
                    step_ns,
                    range_ns,
                    c.windows.as_deref(),
                );
                LabeledSeries::new(c.labels.clone(), iter)
            })
            .collect()
    }
}

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
            // Rate output is uniform (all points have bounds, or none do). Emit
            // intervals only when every point carries one; else None (leaf-only:
            // any operator upstream produced bounds-less points).
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
