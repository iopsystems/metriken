//! Streaming time-series model (prototype).
//!
//! The eager pipeline materialises every intermediate stage as
//! `Vec<(f64, f64)>`. For the WASM viewer this means a typical
//! `sum by (label) (irate(metric[5s]))` over many series produces a
//! transient `O(stages × points × series)` heap footprint just to be
//! reduced down to `O(stages × points)` at the boundary.
//!
//! This module replaces the in-flight matrices with iterator pipelines:
//!
//! * [`Point`] — the single sample carried through the pipeline.
//! * [`LabeledSeries`] — labelset + boxed iterator yielding `Point`.
//! * Operators (e.g. [`CounterIrate`], [`SumMerge`]) wrap upstream
//!   iterators and pull lazily, holding only their own windowed state.
//!
//! Wired-up shapes:
//!
//! * Counter producers: [`CounterIrate`], [`CounterRate`].
//! * Gauge producers: [`GaugeStepGrid`] (bare selector),
//!   [`GaugeAvgOverTime`], [`GaugeIdelta`].
//! * Aggregations: [`MergeReduce`] reducer driven by [`AggOp`]
//!   (sum/avg/min/max/count) with [`GroupBy`] (by/without).
//!
//! The dispatcher (see [`dispatch::try_streaming`]) walks the parsed
//! AST and assembles a producer + optional aggregator for each
//! recognised shape; everything else falls back to the eager path.

use std::collections::HashMap;

use crate::promql::MatrixSample;
use crate::tsdb::{CounterCollection, Labels};

mod aggregate;
pub(crate) mod dispatch;
mod gauge;
pub(crate) mod histogram;
mod irate;
mod rate;

#[cfg(test)]
mod tests;

pub use aggregate::{aggregate, sum_by, AggOp, GroupBy, MergeReduce};
pub use gauge::{
    gauges_avg_over_time, gauges_idelta, gauges_step_grid, GaugeAvgOverTime, GaugeIdelta,
    GaugeStepGrid,
};
pub use irate::CounterIrate;
pub use rate::CounterRate;

/// A single sample emitted through a streaming pipeline.
///
/// Timestamps are kept in raw nanoseconds — the same shape the TSDB
/// stores. Conversion to seconds happens once, at the JSON boundary,
/// to avoid the precision-loss round-trip the eager aggregator does
/// (ns → f64 sec → u64 ns key).
pub type Point = (u64, f64);

/// A labeled, lazily-produced time series.
///
/// `iter` is type-erased so heterogeneous operator chains can sit in
/// the same `Vec`. Once the API stabilises, the boxed form can be
/// replaced with a generic `S: Iterator<Item = Point>` for monomorphised
/// inlining; for the prototype, keeping things `dyn` keeps the surface
/// area small while we validate the shape.
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

/// Output of a streaming evaluation stage. Conceptually equivalent to
/// `QueryResult::Matrix` but unmaterialised.
pub type SeriesSet<'a> = Vec<LabeledSeries<'a>>;

/// Build an iterator stream of `irate` over every counter series in
/// `collection` whose labels match `filter`.
///
/// The iterators borrow the underlying counter sample slice — the
/// caller must keep `collection` alive for the lifetime of the
/// returned `SeriesSet`. This is the producer side of the pipeline:
/// no values are computed until the consumer pulls.
pub fn irate_counters<'a>(
    collection: &'a CounterCollection,
    filter: &Labels,
    start_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
) -> SeriesSet<'a> {
    let mut out = Vec::new();
    for (labels, series) in collection.iter() {
        if !filter.inner.is_empty() && !labels.matches(filter) {
            continue;
        }
        let iter = CounterIrate::new(series.samples(), start_ns, end_ns, step_ns, range_ns);
        out.push(LabeledSeries::new(labels.clone(), iter));
    }
    out
}

/// `rate(metric[range])` over every counter series in `collection`
/// whose labels match `filter`. See [`irate_counters`].
pub fn rate_counters<'a>(
    collection: &'a CounterCollection,
    filter: &Labels,
    start_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
) -> SeriesSet<'a> {
    let mut out = Vec::new();
    for (labels, series) in collection.iter() {
        if !filter.inner.is_empty() && !labels.matches(filter) {
            continue;
        }
        let iter = CounterRate::new(series.samples(), start_ns, end_ns, step_ns, range_ns);
        out.push(LabeledSeries::new(labels.clone(), iter));
    }
    out
}

/// Boundary collector: drain a streaming result into the same
/// `MatrixSample` shape the eager engine returns, so the prototype can
/// be plumbed through the existing JSON serializer unchanged.
///
/// `metric_name` is added as the `__name__` label when present;
/// callers pass `None` for aggregated results (matching the eager
/// path, which strips `__name__` after `sum`/`avg`/etc.).
///
/// Empty series (operators that never emitted) are dropped to match
/// the eager path's behaviour.
pub fn collect_to_matrix(streaming: SeriesSet<'_>, metric_name: Option<&str>) -> Vec<MatrixSample> {
    streaming
        .into_iter()
        .filter_map(|ls| {
            let values: Vec<(f64, f64)> = ls.iter.map(|(t, v)| (t as f64 / 1e9, v)).collect();
            if values.is_empty() {
                return None;
            }
            let mut metric: HashMap<String, String> = HashMap::new();
            if let Some(name) = metric_name {
                metric.insert("__name__".to_string(), name.to_string());
            }
            for (k, v) in ls.labels.inner.iter() {
                metric.insert(k.clone(), v.clone());
            }
            Some(MatrixSample { metric, values })
        })
        .collect()
}
