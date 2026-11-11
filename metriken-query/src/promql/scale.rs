//! A histogram function's values in the histogram's unit.
//!
//! A histogram row's bucket bounds and overflow bound are its values times
//! the row's [`scale`](crate::histogram_stream::HistogramRow::scale) (the
//! column's [`SCALE_KEY`](metriken_storage::SCALE_KEY)). A function that
//! reports a bucket value (a quantile and its band, a mean, a sum, a
//! heatmap's bucket bounds) divides it by that scale. The rows one function
//! pools must share one scale: their buckets cannot be added otherwise, so
//! a read whose rows have two scales is refused ([`Scales::scale`]).
//! Counts (`histogram_count`, `histogram_irate`) do not depend on the scale
//! and are neither divided nor refused.

use std::sync::{Arc, Mutex};

use crate::histogram_stream::HistogramStream;
use crate::promql::{HistogramHeatmapResult, MatrixSample, QueryError};

/// The scales of the rows a histogram stream yielded, recorded as a
/// function reads them.
#[derive(Clone, Default)]
pub(crate) struct Scales(Arc<Mutex<Seen>>);

#[derive(Default)]
struct Seen {
    /// The first row's scale.
    first: Option<f64>,
    /// The first scale that differs from `first`, if any.
    other: Option<f64>,
}

impl Scales {
    /// `stream` with each row's scale recorded in the returned `Scales`.
    pub(crate) fn watch(stream: HistogramStream) -> (HistogramStream, Scales) {
        let scales = Scales::default();
        let seen = Arc::clone(&scales.0);
        let rows = stream.rows.inspect(move |row| {
            let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
            match seen.first {
                None => seen.first = Some(row.scale),
                Some(first) if seen.other.is_none() && first.to_bits() != row.scale.to_bits() => {
                    seen.other = Some(row.scale);
                }
                Some(_) => {}
            }
        });
        let stream = HistogramStream {
            meta: stream.meta,
            rows: Box::new(rows),
        };
        (stream, scales)
    }

    /// The one scale of the rows read, 1 when none was read. An error
    /// naming `function` and `metric` when two rows' scales differ, or a
    /// combined row's is NaN (its sources' scales differed).
    pub(crate) fn scale(&self, function: &str, metric: &str) -> Result<f64, QueryError> {
        let seen = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let first = seen.first.unwrap_or(1.0);
        let other = seen.other.or(first.is_nan().then_some(first));
        match other {
            None => Ok(first),
            Some(other) => {
                let shown = |s: f64| {
                    if s.is_nan() {
                        "several".to_string()
                    } else {
                        s.to_string()
                    }
                };
                Err(QueryError::EvaluationError(format!(
                    "{function} pools series of {metric} whose buckets are stored at different \
                     scales ({} and {}; the `scale` key, a power of 10 set when a histogram's \
                     smallest bound is below 1), which cannot be added; select series of one \
                     scale with a label matcher",
                    shown(first),
                    shown(other),
                )))
            }
        }
    }
}

/// Divides each value, interval and band of `samples` by `scale`.
pub(crate) fn unscale_samples(samples: &mut [MatrixSample], scale: f64) {
    if scale == 1.0 {
        return;
    }
    for s in samples {
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
