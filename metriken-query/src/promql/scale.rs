//! A histogram function's values in the histogram's unit.
//!
//! A histogram row's bucket bounds and overflow bound are its values times
//! the row's [`scale`](crate::histogram_stream::HistogramRow::scale) (the
//! column's [`SCALE_KEY`](metriken_storage::SCALE_KEY)). A function that
//! reports a bucket value (a quantile and its band, a mean, a sum, a
//! heatmap's bucket bounds) divides each output series by the scale of the
//! rows it pools. The rows one output series pools must share one scale:
//! their buckets cannot be added otherwise, so a group whose rows have two
//! scales is refused ([`Scales::scale`], [`Scales::group_scales`]). The
//! rows read include the baseline row before the range's start, which a
//! delta is taken from. Counts (`histogram_count`, `histogram_irate`) do
//! not depend on the scale and are neither divided nor refused.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::promql::streaming::{derive_group_labels, GroupBy};
use crate::promql::{HistogramHeatmapResult, MatrixSample, QueryError};

/// The scales of the rows a histogram stream yielded, per series, recorded
/// as a function reads them.
#[derive(Clone)]
pub(crate) struct Scales {
    /// The stream's series labels, indexed by `HistogramRow::series_idx`.
    series: Arc<Vec<Labels>>,
    seen: Arc<Mutex<HashMap<usize, Seen>>>,
}

/// One series' (or group's) scales: the first, and the first that differs
/// from it, if any.
#[derive(Clone, Copy, Default)]
struct Seen {
    first: Option<f64>,
    other: Option<f64>,
}

impl Seen {
    fn add(&mut self, scale: f64) {
        match self.first {
            None => self.first = Some(scale),
            Some(first) if self.other.is_none() && first.to_bits() != scale.to_bits() => {
                self.other = Some(scale);
            }
            Some(_) => {}
        }
    }

    fn merge(&mut self, other: Seen) {
        for s in [other.first, other.other].into_iter().flatten() {
            self.add(s);
        }
    }

    /// The one scale, 1 when none was seen; an error when two differ, or
    /// the one is NaN (a combined row whose sources' scales differed).
    fn scale(&self, function: &str, metric: &str) -> Result<f64, QueryError> {
        let first = self.first.unwrap_or(1.0);
        let other = self.other.or(first.is_nan().then_some(first));
        let Some(other) = other else {
            return Ok(first);
        };
        let shown = |s: f64| {
            if s.is_nan() {
                "several".to_string()
            } else {
                s.to_string()
            }
        };
        Err(QueryError::EvaluationError(format!(
            "{function} pools rows of {metric} whose buckets are stored at different scales \
             ({} and {}; the `scale` key, the factor a histogram's bounds are stored \
             multiplied by), which cannot be added; select series of one scale with a label \
             matcher, or a time range within one bucket layout (the row before the range's \
             start counts)",
            shown(first),
            shown(other),
        )))
    }
}

impl Scales {
    /// `stream` with each row's scale recorded in the returned `Scales`.
    pub(crate) fn watch(stream: HistogramStream) -> (HistogramStream, Scales) {
        let scales = Scales {
            series: Arc::new(stream.meta.series.clone()),
            seen: Arc::default(),
        };
        let seen = Arc::clone(&scales.seen);
        let rows = stream.rows.inspect(move |row| {
            let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
            seen.entry(row.series_idx).or_default().add(row.scale);
        });
        let stream = HistogramStream {
            meta: stream.meta,
            rows: Box::new(rows),
        };
        (stream, scales)
    }

    /// The one scale of every row read, for a function that pools them all
    /// into one output (the quantiles, the heatmap).
    pub(crate) fn scale(&self, function: &str, metric: &str) -> Result<f64, QueryError> {
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = Seen::default();
        for s in seen.values() {
            all.merge(*s);
        }
        all.scale(function, metric)
    }

    /// The scale of each output group `group_by` makes of the series, keyed
    /// by the group's labels: an error when one group's rows have two
    /// scales.
    pub(crate) fn group_scales(
        &self,
        function: &str,
        metric: &str,
        group_by: GroupBy<'_>,
    ) -> Result<HashMap<Labels, f64>, QueryError> {
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let mut groups: HashMap<Labels, Seen> = HashMap::new();
        for (idx, s) in seen.iter() {
            let Some(labels) = self.series.get(*idx) else {
                continue;
            };
            groups
                .entry(derive_group_labels(labels, group_by))
                .or_default()
                .merge(*s);
        }
        groups
            .into_iter()
            .map(|(key, s)| s.scale(function, metric).map(|scale| (key, scale)))
            .collect()
    }
}

/// Divides each value, interval and band of `samples` by `scale`.
pub(crate) fn unscale_samples(samples: &mut [MatrixSample], scale: f64) {
    for s in samples {
        unscale_sample(s, scale);
    }
}

/// Divides each of `samples`, an output series per group named by its
/// group labels and `__name__`, by its group's scale in `scales`.
pub(crate) fn unscale_groups(samples: &mut [MatrixSample], scales: &HashMap<Labels, f64>) {
    for s in samples {
        let mut key = Labels::default();
        for (k, v) in &s.metric {
            if k != "__name__" {
                key.inner.insert(k.clone(), v.clone());
            }
        }
        if let Some(scale) = scales.get(&key) {
            unscale_sample(s, *scale);
        }
    }
}

fn unscale_sample(s: &mut MatrixSample, scale: f64) {
    if scale == 1.0 {
        return;
    }
    for (_, v) in &mut s.values {
        *v /= scale;
    }
    for (lo, hi) in s.intervals.iter_mut().flatten() {
        *lo /= scale;
        *hi /= scale;
    }
    for (lo, hi) in s.bands.iter_mut().flatten().flatten() {
        *lo /= scale;
        *hi /= scale;
    }
}

/// Divides a heatmap's bucket bounds by `scale`.
pub(crate) fn unscale_heatmap(heatmap: &mut HistogramHeatmapResult, scale: f64) {
    if scale == 1.0 {
        return;
    }
    for b in &mut heatmap.bucket_bounds {
        *b /= scale;
    }
}
