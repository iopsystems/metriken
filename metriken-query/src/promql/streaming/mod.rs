//! Streaming time-series query pipeline.

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
pub type Point = (u64, f64);

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
