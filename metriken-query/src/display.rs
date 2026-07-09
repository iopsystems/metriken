//! Display-mode query results: a richer, presentation-oriented response
//! that a PromQL-compliant [`QueryResult`](crate::QueryResult) cannot express.
//!
//! A standard matrix response is a scalar per timestamp per series, so a
//! decimated point cannot carry any distribution information about the
//! bucket it stands in for — you get either full resolution or a lossy
//! scalar, with nothing in between. [`DisplayResult`] breaks that: each
//! decimated [`EnvPoint`] is a per-bucket boxplot `{min, lo, median, hi,
//! max}`. The median is a robust line (a spike does not drag it); `min`/`max`
//! are the hard extremes so a 1-in-N spike survives the downsample; and
//! `lo`/`hi` are a configurable inner band (default the interquartile range)
//! showing the typical spread. Evaluation stays native-resolution (faithful
//! rate/aggregation); only the *response* is decimated, and only for display.
//! Analysis consumers keep using [`QueryResult`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::promql::{HistogramHeatmapResult, Sample};

/// One decimated point: a boxplot over the samples in a time-bucket.
///
/// `median` is the representative (line) value — robust, so a spike stays in
/// `max` rather than pulling the line up. `lo`/`hi` are the configurable inner
/// band (the [`DisplaySeries::band`] quantiles) bounding the typical spread;
/// `min`/`max` are the hard extremes so a spike cannot be hidden. When no
/// decimation happens (`budget >= raw points`) each sample becomes its own
/// point with `min == lo == median == hi == max`, so full-resolution and
/// decimated data share one shape.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EnvPoint {
    /// Representative timestamp (seconds) — the bucket's time midpoint.
    pub t: f64,
    /// Minimum value over the bucket (lower spike bound).
    pub min: f64,
    /// Inner-band lower edge (quantile `band[0]` — typical-spread bottom).
    pub lo: f64,
    /// Median value over the bucket — the drawn line.
    pub median: f64,
    /// Inner-band upper edge (quantile `band[1]` — typical-spread top).
    pub hi: f64,
    /// Maximum value over the bucket (upper spike bound).
    pub max: f64,
}

/// A decimated series plus the provenance a client needs to decide whether
/// a zoom requires a refetch, label the inner band, and badge "downsampled".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplaySeries {
    pub metric: HashMap<String, String>,
    pub points: Vec<EnvPoint>,
    /// Evaluation step in seconds (the native sampling interval the query
    /// was run at).
    #[serde(rename = "nativeInterval")]
    pub native_interval: f64,
    /// Sample count before decimation.
    #[serde(rename = "rawPoints")]
    pub raw_points: u64,
    /// Which reducer produced `points`.
    pub reducer: Reducer,
    /// The inner-band quantile levels that `EnvPoint::lo`/`hi` were computed
    /// at (e.g. `[0.25, 0.75]`). The outer band is always min/max.
    pub band: [f64; 2],
    /// Whether `points` is a downsample of the evaluated series.
    pub decimated: bool,
}

/// Display-mode result. Parallel to [`QueryResult`](crate::QueryResult),
/// which stays the analysis/PromQL-compliant shape. Only `Matrix` is
/// decimated (into `Series`); heatmap/scalar/vector pass through unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resultType", rename_all = "camelCase")]
pub enum DisplayResult {
    /// Decimated line/scatter series as per-bucket boxplots.
    #[serde(rename = "series")]
    Series {
        result: Vec<DisplaySeries>,
        /// The requested point budget per series.
        budget: u32,
    },

    /// Heatmaps are already resolution-limited server-side; passed through.
    #[serde(rename = "histogram_heatmap")]
    HistogramHeatmap { result: HistogramHeatmapResult },

    #[serde(rename = "scalar")]
    Scalar { result: (f64, f64) },

    #[serde(rename = "vector")]
    Vector { result: Vec<Sample> },
}

/// Selects the decimation algorithm. `Boxplot` summarizes each time-bucket by
/// its `{min, lo, median, hi, max}` — a robust median line, a configurable
/// inner band, and a hard min/max envelope so a spike cannot be hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reducer {
    Boxplot,
}

/// Knobs for a display-mode query. Grouped so the query signature stays stable
/// as options are added.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayOptions {
    /// Maximum output points per series. `0` disables decimation (full res).
    pub budget: usize,
    /// Which decimation algorithm to apply.
    pub reducer: Reducer,
    /// Inner-band quantile levels `(lo, hi)` in `[0, 1]`, with `lo <= hi`.
    /// The outer band is always min/max and is not configurable — that is the
    /// invariant that keeps spikes visible.
    pub band: [f64; 2],
}

impl Default for DisplayOptions {
    fn default() -> Self {
        Self {
            budget: 0,
            reducer: Reducer::Boxplot,
            band: [0.25, 0.75],
        }
    }
}

impl Reducer {
    /// Decimate `points` (native `(timestamp_seconds, value)`, ascending by
    /// time, gaps already absent) down to at most `budget` output points,
    /// with the inner band computed at the `band` quantiles.
    pub fn reduce(&self, points: &[(f64, f64)], budget: usize, band: [f64; 2]) -> Vec<EnvPoint> {
        match self {
            Reducer::Boxplot => reduce_boxplot(points, budget, band),
        }
    }
}

/// Boxplot decimation: split `[t0, tN]` into `budget` time-uniform buckets and
/// emit one [`EnvPoint`] per non-empty bucket carrying the bucket's
/// `{min, lo, median, hi, max}` and time midpoint, where `lo`/`hi` are the
/// `band` quantiles. Empty buckets emit nothing, so time gaps are preserved.
/// Identity (each sample → its own point, all five values equal) when
/// `budget == 0` or the series fits the budget.
fn reduce_boxplot(points: &[(f64, f64)], budget: usize, band: [f64; 2]) -> Vec<EnvPoint> {
    if points.is_empty() {
        return Vec::new();
    }
    // No decimation needed: each sample becomes its own degenerate boxplot.
    if budget == 0 || points.len() <= budget {
        return points
            .iter()
            .map(|&(t, v)| EnvPoint {
                t,
                min: v,
                lo: v,
                median: v,
                hi: v,
                max: v,
            })
            .collect();
    }

    let t0 = points[0].0;
    let tn = points[points.len() - 1].0;
    // Guard the degenerate all-same-timestamp case against divide-by-zero;
    // every point then lands in bucket 0 (a single boxplot).
    let span = (tn - t0).max(f64::MIN_POSITIVE);
    let budget_f = budget as f64;
    let bucket_of = |t: f64| ((((t - t0) / span) * budget_f) as usize).min(budget - 1);

    // Points are time-sorted and buckets are time-uniform, so each bucket is a
    // contiguous slice. Walk the run of equal bucket indices, summarize it.
    let mut out: Vec<EnvPoint> = Vec::with_capacity(budget);
    let mut i = 0;
    while i < points.len() {
        let bucket = bucket_of(points[i].0);
        let mut j = i + 1;
        while j < points.len() && bucket_of(points[j].0) == bucket {
            j += 1;
        }
        out.push(boxplot_of(&points[i..j], band));
        i = j;
    }
    out
}

/// Summarize a non-empty, time-sorted bucket slice into an [`EnvPoint`], with
/// the inner band at the `band` quantiles.
fn boxplot_of(bucket: &[(f64, f64)], band: [f64; 2]) -> EnvPoint {
    debug_assert!(!bucket.is_empty());
    let t = (bucket[0].0 + bucket[bucket.len() - 1].0) / 2.0;

    let mut values: Vec<f64> = bucket.iter().map(|&(_, v)| v).collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    EnvPoint {
        t,
        min: values[0],
        lo: quantile_sorted(&values, band[0]),
        median: quantile_sorted(&values, 0.5),
        hi: quantile_sorted(&values, band[1]),
        max: values[values.len() - 1],
    }
}

/// Linear-interpolated quantile of an ascending-sorted, non-empty slice
/// (matches numpy's default "linear" / type-7 method). `q` is clamped to
/// `[0, 1]`.
fn quantile_sorted(sorted: &[f64], q: f64) -> f64 {
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let q = q.clamp(0.0, 1.0);
    let rank = q * (n - 1) as f64;
    let lo = rank.floor() as usize;
    let frac = rank - lo as f64;
    if lo + 1 < n {
        sorted[lo] + frac * (sorted[lo + 1] - sorted[lo])
    } else {
        sorted[n - 1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IQR: [f64; 2] = [0.25, 0.75];

    fn pts(vs: &[(f64, f64)]) -> Vec<(f64, f64)> {
        vs.to_vec()
    }

    #[test]
    fn empty_input_yields_empty() {
        assert!(reduce_boxplot(&[], 100, IQR).is_empty());
    }

    #[test]
    fn under_budget_is_identity() {
        let p = pts(&[(0.0, 1.0), (1.0, 2.0), (2.0, 3.0)]);
        let out = reduce_boxplot(&p, 100, IQR);
        assert_eq!(out.len(), 3);
        for (i, e) in out.iter().enumerate() {
            assert_eq!(e.t, p[i].0);
            let v = p[i].1;
            assert_eq!((e.min, e.lo, e.median, e.hi, e.max), (v, v, v, v, v));
        }
    }

    #[test]
    fn budget_zero_is_identity() {
        let p = pts(&[(0.0, 5.0), (1.0, 9.0)]);
        let out = reduce_boxplot(&p, 0, IQR);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].median, 9.0);
    }

    #[test]
    fn over_budget_is_bounded() {
        let p: Vec<(f64, f64)> = (0..1000).map(|i| (i as f64, i as f64)).collect();
        let out = reduce_boxplot(&p, 50, IQR);
        assert!(
            out.len() <= 50,
            "output bounded by budget, got {}",
            out.len()
        );
        assert!(!out.is_empty());
    }

    #[test]
    fn spike_stays_in_max_not_median() {
        // 40 flat samples at 1.0, one spike to 100.0. With budget 4 (buckets of
        // 10), the spike's bucket must keep median≈1.0 (robust) but max=100.0.
        let mut p: Vec<(f64, f64)> = (0..40).map(|i| (i as f64, 1.0)).collect();
        p[20].1 = 100.0;
        let out = reduce_boxplot(&p, 4, IQR);
        assert!(out.len() <= 4);

        let max_max = out.iter().map(|e| e.max).fold(f64::MIN, f64::max);
        assert_eq!(max_max, 100.0, "spike preserved in max");

        // Every bucket's median is the robust baseline — the spike never
        // pulls a median off 1.0.
        for e in &out {
            assert_eq!(e.median, 1.0, "median robust to the spike");
        }
    }

    #[test]
    fn known_quartiles() {
        // One bucket of exactly [10,20,30,40,50] (budget 1 forces one bucket).
        let p = pts(&[
            (0.0, 30.0),
            (1.0, 10.0),
            (2.0, 50.0),
            (3.0, 20.0),
            (4.0, 40.0),
        ]);
        let out = reduce_boxplot(&p, 1, IQR);
        assert_eq!(out.len(), 1);
        let e = out[0];
        assert_eq!(e.min, 10.0);
        assert_eq!(e.lo, 20.0, "p25");
        assert_eq!(e.median, 30.0);
        assert_eq!(e.hi, 40.0, "p75");
        assert_eq!(e.max, 50.0);
        assert_eq!(e.t, 2.0, "time midpoint of [0,4]");
    }

    #[test]
    fn custom_band_widens_inner_edges() {
        // Same bucket, but a p10/p90 band pulls lo/hi further out than IQR,
        // while min/max/median are unchanged.
        let p = pts(&[
            (0.0, 30.0),
            (1.0, 10.0),
            (2.0, 50.0),
            (3.0, 20.0),
            (4.0, 40.0),
        ]);
        let iqr = reduce_boxplot(&p, 1, [0.25, 0.75])[0];
        let wide = reduce_boxplot(&p, 1, [0.10, 0.90])[0];
        assert_eq!((wide.min, wide.median, wide.max), (10.0, 30.0, 50.0));
        assert!(
            wide.lo < iqr.lo,
            "p10 lower than p25: {} < {}",
            wide.lo,
            iqr.lo
        );
        assert!(
            wide.hi > iqr.hi,
            "p90 higher than p75: {} > {}",
            wide.hi,
            iqr.hi
        );
        // p10 of [10,20,30,40,50]: rank 0.1*4=0.4 -> 10 + 0.4*10 = 14.
        assert_eq!(wide.lo, 14.0);
        assert_eq!(wide.hi, 46.0);
    }

    #[test]
    fn timestamps_ascending_and_within_range() {
        let p: Vec<(f64, f64)> = (0..500).map(|i| (i as f64 * 2.0, (i % 7) as f64)).collect();
        let out = reduce_boxplot(&p, 30, IQR);
        assert!(out.len() <= 30);
        for w in out.windows(2) {
            assert!(w[1].t > w[0].t, "timestamps strictly ascending");
        }
        assert!(out.first().unwrap().t >= 0.0);
        assert!(out.last().unwrap().t <= 998.0);
    }

    #[test]
    fn quantile_ordering_invariant() {
        let p: Vec<(f64, f64)> = (0..300)
            .map(|i| (i as f64, ((i * 7 % 13) as f64) - 6.0))
            .collect();
        let out = reduce_boxplot(&p, 20, IQR);
        for e in &out {
            assert!(
                e.min <= e.lo && e.lo <= e.median && e.median <= e.hi && e.hi <= e.max,
                "min <= lo <= median <= hi <= max for {e:?}"
            );
        }
    }

    #[test]
    fn quantile_linear_interpolation_two_points() {
        assert_eq!(quantile_sorted(&[10.0, 20.0], 0.5), 15.0);
        assert_eq!(quantile_sorted(&[10.0, 20.0], 0.25), 12.5);
        assert_eq!(quantile_sorted(&[10.0, 20.0], 0.75), 17.5);
        assert_eq!(quantile_sorted(&[42.0], 0.5), 42.0);
    }

    #[test]
    fn reducer_dispatch_matches_free_fn() {
        let p = pts(&[(0.0, 1.0), (1.0, 2.0), (2.0, 3.0), (3.0, 4.0)]);
        assert_eq!(
            Reducer::Boxplot.reduce(&p, 2, IQR),
            reduce_boxplot(&p, 2, IQR)
        );
    }

    #[test]
    fn reducer_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Reducer::Boxplot).unwrap(),
            "\"boxplot\""
        );
    }

    #[test]
    fn default_options_are_iqr_boxplot_full_res() {
        let d = DisplayOptions::default();
        assert_eq!(d.budget, 0);
        assert_eq!(d.reducer, Reducer::Boxplot);
        assert_eq!(d.band, [0.25, 0.75]);
    }
}
