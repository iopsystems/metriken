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
    /// Representative timestamp (seconds) — the bucket's epoch-aligned boundary
    /// (a "nice" wall-clock instant), so decimated points snap to the same ticks
    /// the time axis draws. (Native/undecimated points keep their exact time.)
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
    /// Aggregated measurement-uncertainty band lower edge for the bucket: the
    /// median of the per-sample interval lows (robust, mirroring the `median`
    /// line). `None` when the series carries no acquisition-window uncertainty
    /// (gauges, non-rate queries). At native resolution (each sample its own
    /// bucket) this is exactly that sample's interval low, so zoomed-in and
    /// decimated bands are consistent. Parallel to `unc_hi` — both `Some` or
    /// both `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unc_lo: Option<f64>,
    /// Aggregated measurement-uncertainty band upper edge: the median of the
    /// per-sample interval highs. See [`unc_lo`](Self::unc_lo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unc_hi: Option<f64>,
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
    ///
    /// `intervals`, when present, is the per-sample measurement-uncertainty band
    /// `(lo, hi)` parallel to `points`; each output bucket carries the median of
    /// its lows/highs in [`EnvPoint::unc_lo`]/[`unc_hi`](EnvPoint::unc_hi). A
    /// length mismatch (or `None`) yields no uncertainty band.
    pub fn reduce(
        &self,
        points: &[(f64, f64)],
        intervals: Option<&[(f64, f64)]>,
        budget: usize,
        band: [f64; 2],
    ) -> Vec<EnvPoint> {
        match self {
            Reducer::Boxplot => reduce_boxplot(points, intervals, budget, band),
        }
    }
}

/// Boxplot decimation: split `[t0, tN]` into `budget` time-uniform buckets and
/// emit one [`EnvPoint`] per non-empty bucket carrying the bucket's
/// `{min, lo, median, hi, max}` and time midpoint, where `lo`/`hi` are the
/// `band` quantiles. Empty buckets emit nothing, so time gaps are preserved.
/// Identity (each sample → its own point, all five values equal) when
/// `budget == 0` or the series fits the budget.
fn reduce_boxplot(
    points: &[(f64, f64)],
    intervals: Option<&[(f64, f64)]>,
    budget: usize,
    band: [f64; 2],
) -> Vec<EnvPoint> {
    if points.is_empty() {
        return Vec::new();
    }
    // Only honor a uncertainty band that is parallel to `points`; a length
    // mismatch means we can't trust the alignment, so drop it.
    let intervals = intervals.filter(|iv| iv.len() == points.len());
    // No decimation needed: each sample becomes its own degenerate boxplot,
    // carrying its own exact uncertainty interval (so native and decimated
    // bands are the same shape).
    if budget == 0 || points.len() <= budget {
        return points
            .iter()
            .enumerate()
            .map(|(i, &(t, v))| EnvPoint {
                t,
                min: v,
                lo: v,
                median: v,
                hi: v,
                max: v,
                unc_lo: intervals.map(|iv| iv[i].0),
                unc_hi: intervals.map(|iv| iv[i].1),
            })
            .collect();
    }

    // Human-aligned decimation. Pick a "nice" bucket width ≥ the raw target
    // (span/budget), align buckets to epoch multiples of it, and stamp each
    // bucket at its boundary — so snap points land on the same round wall-clock
    // times the axis draws its ticks at (a 20s bucket → :00/:20/:40), instead of
    // the arbitrary sample-midpoint times a range/budget split produces.
    let t0 = points[0].0;
    let tn = points[points.len() - 1].0;
    // Guard the degenerate all-same-timestamp case against divide-by-zero.
    let span = (tn - t0).max(f64::MIN_POSITIVE);
    let bw = nice_bucket_secs(span / budget as f64);
    // Absolute (epoch-aligned) bucket index — floor(t / bw).
    let bucket_of = |t: f64| (t / bw).floor() as i64;

    // Points are time-sorted and buckets are contiguous in time, so each bucket
    // is a contiguous slice. Walk the run of equal bucket indices, summarize it,
    // and stamp the output at the bucket's aligned boundary.
    let mut out: Vec<EnvPoint> = Vec::with_capacity(budget + 1);
    let mut i = 0;
    while i < points.len() {
        let bucket = bucket_of(points[i].0);
        let mut j = i + 1;
        while j < points.len() && bucket_of(points[j].0) == bucket {
            j += 1;
        }
        let unc = intervals.map(|iv| &iv[i..j]);
        let mut ep = boxplot_of(&points[i..j], unc, band);
        ep.t = bucket as f64 * bw;
        out.push(ep);
        i = j;
    }
    out
}

/// Smallest human-friendly bucket width (seconds) ≥ `raw`. Every rung divides
/// evenly into a minute / hour / day, so epoch-aligned multiples land on round
/// wall-clock boundaries — keeping decimated snap points on the same instants
/// the time axis ticks at.
fn nice_bucket_secs(raw: f64) -> f64 {
    const NICE: &[f64] = &[
        1.0, 2.0, 5.0, 10.0, 15.0, 20.0, 30.0, // sub-minute
        60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, // 1m,2m,5m,10m,15m,30m
        3600.0, 7200.0, 10800.0, 21600.0, 43200.0, // 1h,2h,3h,6h,12h
        86400.0, // 1d
    ];
    for &n in NICE {
        if n >= raw {
            return n;
        }
    }
    // Beyond a day, round up to a whole number of days.
    (raw / 86400.0).ceil() * 86400.0
}

/// Summarize a non-empty, time-sorted bucket slice into an [`EnvPoint`], with
/// the inner band at the `band` quantiles and, when `unc` is present, the
/// bucket's aggregated measurement-uncertainty band (median of the per-sample
/// interval lows/highs — robust, like the median line).
fn boxplot_of(bucket: &[(f64, f64)], unc: Option<&[(f64, f64)]>, band: [f64; 2]) -> EnvPoint {
    debug_assert!(!bucket.is_empty());
    let t = (bucket[0].0 + bucket[bucket.len() - 1].0) / 2.0;

    let mut values: Vec<f64> = bucket.iter().map(|&(_, v)| v).collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let (unc_lo, unc_hi) = match unc {
        Some(iv) if !iv.is_empty() => {
            let mut los: Vec<f64> = iv.iter().map(|&(lo, _)| lo).collect();
            let mut his: Vec<f64> = iv.iter().map(|&(_, hi)| hi).collect();
            los.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            his.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            (
                Some(quantile_sorted(&los, 0.5)),
                Some(quantile_sorted(&his, 0.5)),
            )
        }
        _ => (None, None),
    };

    EnvPoint {
        t,
        min: values[0],
        lo: quantile_sorted(&values, band[0]),
        median: quantile_sorted(&values, 0.5),
        hi: quantile_sorted(&values, band[1]),
        max: values[values.len() - 1],
        unc_lo,
        unc_hi,
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
        assert!(reduce_boxplot(&[], None, 100, IQR).is_empty());
    }

    #[test]
    fn under_budget_is_identity() {
        let p = pts(&[(0.0, 1.0), (1.0, 2.0), (2.0, 3.0)]);
        let out = reduce_boxplot(&p, None, 100, IQR);
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
        let out = reduce_boxplot(&p, None, 0, IQR);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].median, 9.0);
    }

    #[test]
    fn over_budget_is_bounded() {
        let p: Vec<(f64, f64)> = (0..1000).map(|i| (i as f64, i as f64)).collect();
        let out = reduce_boxplot(&p, None, 50, IQR);
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
        let out = reduce_boxplot(&p, None, 4, IQR);
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
    fn decimation_snaps_bucket_times_to_human_boundaries() {
        // 200 samples at 1s spacing from a phase-offset start (1000.37s …).
        // budget 20 → raw bucket ~10s → nice 10s. Every emitted timestamp must
        // be a multiple of 10s (epoch-aligned, so it lands on the same
        // wall-clock boundaries as the axis ticks) — not the sample-midpoint
        // times (e.g. 1004.87) the old reducer produced.
        let p: Vec<(f64, f64)> = (0..200).map(|i| (1000.37 + i as f64, i as f64)).collect();
        let out = reduce_boxplot(&p, None, 20, IQR);
        assert!(out.len() > 1 && out.len() <= 21, "len {}", out.len());
        for e in &out {
            let m = e.t / 10.0;
            assert!(
                (m - m.round()).abs() < 1e-9,
                "t={} not on a 10s boundary",
                e.t
            );
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
        let out = reduce_boxplot(&p, None, 1, IQR);
        assert_eq!(out.len(), 1);
        let e = out[0];
        assert_eq!(e.min, 10.0);
        assert_eq!(e.lo, 20.0, "p25");
        assert_eq!(e.median, 30.0);
        assert_eq!(e.hi, 40.0, "p75");
        assert_eq!(e.max, 50.0);
        // Single 5s nice-bucket [0,5) containing [0,4]; stamped at its boundary.
        assert_eq!(e.t, 0.0, "aligned bucket boundary");
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
        let iqr = reduce_boxplot(&p, None, 1, [0.25, 0.75])[0];
        let wide = reduce_boxplot(&p, None, 1, [0.10, 0.90])[0];
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
        let out = reduce_boxplot(&p, None, 30, IQR);
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
        let out = reduce_boxplot(&p, None, 20, IQR);
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
            Reducer::Boxplot.reduce(&p, None, 2, IQR),
            reduce_boxplot(&p, None, 2, IQR)
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

    // ── measurement-uncertainty band aggregation ──────────────────────────────

    #[test]
    fn no_intervals_yields_no_uncertainty_band() {
        let p = pts(&[(0.0, 1.0), (1.0, 2.0), (2.0, 3.0)]);
        for e in reduce_boxplot(&p, None, 100, IQR) {
            assert_eq!((e.unc_lo, e.unc_hi), (None, None));
        }
    }

    #[test]
    fn identity_carries_each_samples_exact_interval() {
        // Under budget: every native sample is its own bucket and keeps its own
        // interval verbatim — this is what makes zoomed-in and decimated bands
        // the same shape.
        let p = pts(&[(0.0, 10.0), (1.0, 20.0), (2.0, 30.0)]);
        let iv = vec![(9.0, 11.0), (18.0, 22.0), (28.0, 33.0)];
        let out = reduce_boxplot(&p, Some(&iv), 100, IQR);
        assert_eq!(out.len(), 3);
        for (i, e) in out.iter().enumerate() {
            assert_eq!(e.unc_lo, Some(iv[i].0));
            assert_eq!(e.unc_hi, Some(iv[i].1));
        }
    }

    #[test]
    fn decimated_band_is_median_of_interval_edges() {
        // One bucket (budget 1) of 4 samples. The aggregated band is the median
        // of the lows and the median of the highs — robust, like the median line,
        // and independent of the value spread the boxplot already shows.
        let p = pts(&[(0.0, 30.0), (1.0, 10.0), (2.0, 50.0), (3.0, 20.0)]);
        // lows  {9,10,10,11} -> median 10 ; highs {11,13,14,20} -> median 13.5.
        // (Each edge is aggregated independently, so the pairing doesn't matter.)
        let iv = vec![(9.0, 11.0), (10.0, 13.0), (10.0, 14.0), (11.0, 20.0)];
        let out = reduce_boxplot(&p, Some(&iv), 1, IQR);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].unc_lo, Some(10.0));
        assert_eq!(out[0].unc_hi, Some(13.5));
    }

    #[test]
    fn mismatched_interval_length_is_ignored() {
        // A band that isn't parallel to points can't be trusted to align, so it
        // is dropped rather than mis-attributed.
        let p = pts(&[(0.0, 1.0), (1.0, 2.0), (2.0, 3.0)]);
        let iv = vec![(0.5, 1.5)]; // wrong length
        for e in reduce_boxplot(&p, Some(&iv), 100, IQR) {
            assert_eq!((e.unc_lo, e.unc_hi), (None, None));
        }
    }

    #[test]
    fn uncertainty_band_serializes_only_when_present() {
        let with = EnvPoint {
            t: 0.0,
            min: 1.0,
            lo: 1.0,
            median: 1.0,
            hi: 1.0,
            max: 1.0,
            unc_lo: Some(0.9),
            unc_hi: Some(1.1),
        };
        let without = EnvPoint {
            t: 0.0,
            min: 1.0,
            lo: 1.0,
            median: 1.0,
            hi: 1.0,
            max: 1.0,
            unc_lo: None,
            unc_hi: None,
        };
        let s_with = serde_json::to_string(&with).unwrap();
        assert!(
            s_with.contains("unc_lo") && s_with.contains("unc_hi"),
            "{s_with}"
        );
        let s_without = serde_json::to_string(&without).unwrap();
        assert!(
            !s_without.contains("unc"),
            "absent band omitted: {s_without}"
        );
    }
}
