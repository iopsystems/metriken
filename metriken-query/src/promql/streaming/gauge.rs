//! Gauge-side streaming producers.
//!
//! All own their sample vectors and emit `f64` values at step-aligned
//! timestamps. State is the cursor plus, for the windowed forms, the indices
//! into the samples — no buffering beyond the input itself.
//!
//! * [`GaugeStepGrid`] — bare `metric{matchers}` selector at each tick,
//!   subject to the staleness rule.
//! * [`GaugeAvgOverTime`] — `avg_over_time(metric[range])`.
//! * [`GaugeIdelta`] — `idelta(metric[range])`, last-two-samples delta.
//! * [`GaugeDeriv`] — `deriv(metric[range])`, least-squares slope of a
//!   gauge's or a counter's values.

use super::Point;
use crate::types::GaugeValues;

/// Where a gauge producer places its output points, and how it advances.
///
/// All four gauge producers share this so a query mixing them stays
/// self-consistent: a binary op joins its two sides ON TIMESTAMP, so two
/// producers that place points differently would simply fail to intersect and
/// yield an empty series rather than a wrong one.
///
/// Three modes, in precedence order:
///
/// * `points` — the caller supplied explicit evaluation timestamps
///   ([`crate::QueryOptions::eval_timestamps`]). Used to land values on a slow
///   source's real readings, which are not evenly spaced and so cannot be hit
///   by any uniform grid.
/// * `raw` ([`crate::RateMode::Raw`]) — walk the actual sample timestamps, so
///   gauge output lands on the same instants as counter `rate`/`irate` and
///   series-op-series (e.g. `x / cpu_cores`) aligns.
/// * grid ([`crate::RateMode::Grid`], the PromQL default) — walk
///   `start + k·step`.
pub(crate) struct Placement {
    raw: bool,
    /// The range's start: explicit points before it are not evaluated.
    start_ns: u64,
    cursor_ns: u64,
    pub(crate) step_ns: u64,
    raw_idx: usize,
    points: Option<(std::sync::Arc<[u64]>, usize)>,
}

impl Placement {
    fn new(raw: bool, start_ns: u64, step_ns: u64) -> Self {
        Self {
            raw,
            start_ns,
            cursor_ns: start_ns,
            step_ns,
            raw_idx: 0,
            points: None,
        }
    }

    fn at_points(mut self, points: std::sync::Arc<[u64]>) -> Self {
        self.points = Some((points, 0));
        self
    }

    /// The next evaluation timestamp, or `None` when exhausted (past `end_ns`,
    /// out of points/samples, or grid with `step_ns == 0`).
    fn next(&mut self, timestamps: &[u64], end_ns: u64) -> Option<u64> {
        if let Some((points, idx)) = &mut self.points {
            loop {
                let t = *points.get(*idx)?;
                *idx += 1;
                if t >= self.start_ns {
                    return (t <= end_ns).then_some(t);
                }
            }
        }
        if self.raw {
            let t = *timestamps.get(self.raw_idx)?;
            self.raw_idx += 1;
            return (t <= end_ns).then_some(t);
        }
        if self.step_ns == 0 || self.cursor_ns > end_ns {
            return None;
        }
        let t = self.cursor_ns;
        // Saturating: an overflow lands past end_ns so the next call stops.
        self.cursor_ns = self.cursor_ns.saturating_add(self.step_ns);
        Some(t)
    }
}

/// A producer that can be told where to place its points. Implemented by
/// every gauge producer so the dispatcher can apply the caller's explicit
/// timestamps uniformly, without caring which one it holds. See [`Placement`].
pub(crate) trait AtPoints: Sized {
    fn at_points(self, points: std::sync::Arc<[u64]>) -> Self;
}

macro_rules! impl_at_points {
    ($($t:ident),+ $(,)?) => {$(
        impl AtPoints for $t {
            fn at_points(mut self, points: std::sync::Arc<[u64]>) -> Self {
                self.place = self.place.at_points(points);
                self
            }
        }
    )+};
}

impl_at_points!(GaugeStepGrid, GaugeOverTime, GaugeIdelta, GaugeDeriv);

/// Value `i` of a gauge series, as an `f64`: an integer gauge's converted,
/// a float gauge's as stored.
fn value_at(values: &GaugeValues, i: usize) -> f64 {
    values
        .get_f64(i)
        .expect("a series has as many values as timestamps")
}

pub struct GaugeStepGrid {
    timestamps: Vec<u64>,
    values: GaugeValues,
    end_ns: u64,
    staleness_ns: u64,
    place: Placement,
}

impl GaugeStepGrid {
    pub fn new(
        timestamps: Vec<u64>,
        values: GaugeValues,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        staleness_ns: u64,
        raw: bool,
    ) -> Self {
        Self {
            timestamps,
            values,
            end_ns,
            staleness_ns,
            place: Placement::new(raw, start_ns, step_ns),
        }
    }
}

impl Iterator for GaugeStepGrid {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = self.place.next(&self.timestamps, self.end_ns)?;

            let hi = self.timestamps.partition_point(|&ts| ts <= t);
            if hi == 0 {
                continue;
            }
            let ts = self.timestamps[hi - 1];
            let val = value_at(&self.values, hi - 1);
            if t.saturating_sub(ts) > self.staleness_ns {
                continue;
            }
            return Some(Point::at(t, val));
        }
    }
}

/// The aggregation of a `*_over_time` function over the values of a
/// window, as Prometheus defines it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum OverTime {
    Avg,
    Min,
    Max,
    Sum,
    Count,
    Last,
    Present,
    /// Population variance.
    StdVar,
    /// Population standard deviation.
    StdDev,
    /// `quantile_over_time(q, ..)`.
    Quantile(f64),
}

impl OverTime {
    /// The aggregation of the `*_over_time` function `name`, other than
    /// `quantile_over_time`, which takes its quantile as an argument.
    pub(crate) fn of(name: &str) -> Option<Self> {
        Some(match name {
            "avg_over_time" => Self::Avg,
            "min_over_time" => Self::Min,
            "max_over_time" => Self::Max,
            "sum_over_time" => Self::Sum,
            "count_over_time" => Self::Count,
            "last_over_time" => Self::Last,
            "present_over_time" => Self::Present,
            "stdvar_over_time" => Self::StdVar,
            "stddev_over_time" => Self::StdDev,
            _ => return None,
        })
    }

    /// The aggregation of `values`, which is not empty, as Prometheus
    /// computes each: sums, means and variances with its Kahan-Neumaier
    /// compensation (an infinite sum ends the compensation, and a mean
    /// whose sum overflows is taken incrementally), the variance by
    /// Welford's method, so a constant window's is 0. `min` and `max` leave
    /// out NaN unless every value is NaN. A quantile below 0 is -inf, above
    /// 1 +inf, and NaN for a NaN quantile; otherwise it interpolates
    /// linearly between the values either side of rank `q·(n − 1)`, NaN
    /// sorting first, and is the lower value where the rank is whole or the
    /// two are the same.
    pub(crate) fn reduce(self, values: &[f64]) -> f64 {
        let n = values.len() as f64;
        match self {
            Self::Avg => mean(values),
            Self::Min => values.iter().copied().fold(f64::NAN, f64::min),
            Self::Max => values.iter().copied().fold(f64::NAN, f64::max),
            Self::Sum => {
                let (sum, c) = values
                    .iter()
                    .fold((0.0, 0.0), |(sum, c), &v| kahan_inc(v, sum, c));
                if sum.is_infinite() {
                    sum
                } else {
                    sum + c
                }
            }
            Self::Count => n,
            Self::Last => *values.last().expect("not empty"),
            Self::Present => 1.0,
            Self::StdVar | Self::StdDev => {
                let var = variance(values);
                if self == Self::StdDev {
                    var.sqrt()
                } else {
                    var
                }
            }
            Self::Quantile(q) => {
                if q.is_nan() {
                    return f64::NAN;
                }
                if q < 0.0 {
                    return f64::NEG_INFINITY;
                }
                if q > 1.0 {
                    return f64::INFINITY;
                }
                let mut sorted = values.to_vec();
                sorted.sort_by(|a, b| match (a.is_nan(), b.is_nan()) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => a.total_cmp(b),
                });
                let rank = q * (n - 1.0);
                let lower = rank.floor() as usize;
                let upper = (lower + 1).min(sorted.len() - 1);
                let weight = rank - rank.floor();
                let (lo, hi) = (sorted[lower], sorted[upper]);
                if weight == 0.0 || lo.to_bits() == hi.to_bits() {
                    return lo;
                }
                lo * (1.0 - weight) + hi * weight
            }
        }
    }
}

/// `inc` added to the compensated sum `(sum, c)`: Prometheus's
/// `kahansum.Inc`.
fn kahan_inc(inc: f64, sum: f64, mut c: f64) -> (f64, f64) {
    let t = sum + inc;
    if t.is_infinite() {
        c = 0.0;
    } else if sum.abs() >= inc.abs() {
        c += (sum - t) + inc;
    } else {
        c += (inc - t) + sum;
    }
    (t, c)
}

/// The mean of `values` as Prometheus's `avg_over_time` takes it: a
/// compensated sum over the count, and an incremental mean from the point
/// the sum overflows.
fn mean(values: &[f64]) -> f64 {
    let (mut sum, mut c, mut mean, mut count) = (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
    let mut incremental = false;
    for &f in values {
        count += 1.0;
        if !incremental {
            let (new_sum, new_c) = kahan_inc(f, sum, c);
            if count == 1.0 || !new_sum.is_infinite() {
                (sum, c) = (new_sum, new_c);
                continue;
            }
            incremental = true;
            mean = sum / (count - 1.0);
            c /= count - 1.0;
        }
        let q = (count - 1.0) / count;
        (mean, c) = kahan_inc(f / count, q * mean, q * c);
    }
    if incremental {
        mean + c
    } else {
        sum / count + c / count
    }
}

/// The population variance of `values` as Prometheus's
/// `stdvar_over_time` takes it: Welford's method with compensated sums.
fn variance(values: &[f64]) -> f64 {
    let (mut count, mut mean, mut c_mean, mut aux, mut c_aux) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for &f in values {
        count += 1.0;
        let delta = f - (mean + c_mean);
        (mean, c_mean) = kahan_inc(delta / count, mean, c_mean);
        (aux, c_aux) = kahan_inc(delta * (f - (mean + c_mean)), aux, c_aux);
    }
    (aux + c_aux) / count
}

/// A `*_over_time` function of a series: at each evaluation time `t`, the
/// aggregation of its values stamped in `[t − range, t]`, both ends
/// included, each counted once. A window with no value gives no point; one
/// holding an interpolated value gives an interpolated point.
pub struct GaugeOverTime {
    timestamps: Vec<u64>,
    values: Vec<f64>,
    interpolated: Vec<bool>,
    end_ns: u64,
    range_ns: u64,
    agg: OverTime,
    place: Placement,
}

impl GaugeOverTime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        timestamps: Vec<u64>,
        values: Vec<f64>,
        interpolated: Vec<bool>,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        range_ns: u64,
        raw: bool,
        agg: OverTime,
    ) -> Self {
        Self {
            timestamps,
            values,
            interpolated,
            end_ns,
            range_ns,
            agg,
            place: Placement::new(raw, start_ns, step_ns),
        }
    }

    /// A gauge's values as `f64`.
    pub(crate) fn gauge_values(values: &GaugeValues) -> Vec<f64> {
        (0..values.len()).map(|i| value_at(values, i)).collect()
    }
}

impl Iterator for GaugeOverTime {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = self.place.next(&self.timestamps, self.end_ns)?;

            let window_start = t.saturating_sub(self.range_ns);
            let lo = self.timestamps.partition_point(|&ts| ts < window_start);
            let hi = self.timestamps.partition_point(|&ts| ts <= t);
            if hi == lo {
                continue;
            }
            let mut p = Point::at(t, self.agg.reduce(&self.values[lo..hi]));
            p.interpolated = self
                .interpolated
                .get(lo..hi)
                .is_some_and(|f| f.iter().any(|x| *x));
            return Some(p);
        }
    }
}

pub struct GaugeIdelta {
    timestamps: Vec<u64>,
    values: GaugeValues,
    end_ns: u64,
    range_ns: u64,
    place: Placement,
}

impl GaugeIdelta {
    pub fn new(
        timestamps: Vec<u64>,
        values: GaugeValues,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        range_ns: u64,
        raw: bool,
    ) -> Self {
        Self {
            timestamps,
            values,
            end_ns,
            range_ns,
            place: Placement::new(raw, start_ns, step_ns),
        }
    }
}

impl Iterator for GaugeIdelta {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = self.place.next(&self.timestamps, self.end_ns)?;

            let window_start = t.saturating_sub(self.range_ns);
            let lo = self.timestamps.partition_point(|&ts| ts < window_start);
            let hi = self.timestamps.partition_point(|&ts| ts <= t);
            if hi.saturating_sub(lo) < 2 {
                // In raw mode, a sample with no value is a timing point: a
                // join holds the next point for the gap from it.
                if self.place.raw {
                    return Some(Point::timing_at(t));
                }
                continue;
            }
            let cur = value_at(&self.values, hi - 1);
            let prev = value_at(&self.values, hi - 2);
            return Some(Point::at(t, cur - prev));
        }
    }
}

pub struct GaugeDeriv {
    timestamps: Vec<u64>,
    values: Vec<f64>,
    end_ns: u64,
    range_ns: u64,
    place: Placement,
}

impl GaugeDeriv {
    pub fn new(
        timestamps: Vec<u64>,
        values: Vec<f64>,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        range_ns: u64,
        raw: bool,
    ) -> Self {
        Self {
            timestamps,
            values,
            end_ns,
            range_ns,
            place: Placement::new(raw, start_ns, step_ns),
        }
    }
}

impl Iterator for GaugeDeriv {
    type Item = Point;

    /// The least-squares slope, per second, of the samples in
    /// `[t - range, t]`; no point where there are fewer than two.
    fn next(&mut self) -> Option<Point> {
        loop {
            let t = self.place.next(&self.timestamps, self.end_ns)?;

            let window_start = t.saturating_sub(self.range_ns);
            let lo = self.timestamps.partition_point(|&ts| ts < window_start);
            let hi = self.timestamps.partition_point(|&ts| ts <= t);
            if hi.saturating_sub(lo) < 2 {
                // In raw mode, a sample with no value is a timing point: a
                // join holds the next point for the gap from it.
                if self.place.raw {
                    return Some(Point::timing_at(t));
                }
                continue;
            }

            let n = (hi - lo) as f64;
            let mut sum_x = 0.0_f64;
            let mut sum_y = 0.0_f64;
            let mut sum_xy = 0.0_f64;
            let mut sum_x2 = 0.0_f64;
            // `x` in seconds from `t` and `y` from the first value: from the
            // epoch, `x * x` is near 3e18 and the denominator below cancels
            // to nothing, and a large counter loses the slope the same way.
            let y0 = self.values[lo];
            for i in lo..hi {
                let x = (self.timestamps[i] as i64 - t as i64) as f64 / 1e9;
                let y = self.values[i] - y0;
                sum_x += x;
                sum_y += y;
                sum_xy += x * y;
                sum_x2 += x * x;
            }
            let denom = n * sum_x2 - sum_x * sum_x;
            if denom.abs() < 1e-10 {
                return Some(Point::at(t, 0.0));
            }
            let slope = (n * sum_xy - sum_x * sum_y) / denom;
            return Some(Point::at(t, slope));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gauge_step_grid_raw_emits_at_sample_timestamps() {
        // Samples phase-offset from the grid. Raw must place points at the real
        // sample times (so a gauge aligns with Raw counter rates on the same
        // rows), not at the synthetic start+k·step grid.
        let ts = [500_000_000u64, 1_500_000_000, 2_500_000_000];
        let vals = [10i64, 20, 30];
        let pts: Vec<Point> = GaugeStepGrid::new(
            ts.to_vec(),
            vals.to_vec().into(),
            0,
            3_000_000_000,
            1_000_000_000,
            5_000_000_000,
            true, // raw
        )
        .collect();
        assert_eq!(
            pts.iter().map(|p| p.t).collect::<Vec<_>>(),
            vec![500_000_000, 1_500_000_000, 2_500_000_000]
        );
        assert_eq!(
            pts.iter().map(|p| p.v).collect::<Vec<_>>(),
            vec![10.0, 20.0, 30.0]
        );
    }

    #[test]
    fn gauge_step_grid_grid_still_walks_the_step_grid() {
        // Grid mode (raw=false) is unchanged: points land on start+k·step.
        let ts = [500_000_000u64, 1_500_000_000, 2_500_000_000];
        let vals = [10i64, 20, 30];
        let pts: Vec<Point> = GaugeStepGrid::new(
            ts.to_vec(),
            vals.to_vec().into(),
            0,
            3_000_000_000,
            1_000_000_000,
            5_000_000_000,
            false,
        )
        .collect();
        // Grid ticks 0,1,2,3s; t=0 has no sample ≤ it → skipped; 1,2,3 carry the
        // last sample ≤ t.
        assert_eq!(
            pts.iter().map(|p| p.t).collect::<Vec<_>>(),
            vec![1_000_000_000, 2_000_000_000, 3_000_000_000]
        );
    }
}
