//! Gauge-side streaming producers.
//!
//! All three operate on a borrowed `&[(u64, i64)]` slice (the gauge
//! sample storage) and emit `f64` values at step-aligned timestamps.
//! State is the cursor plus, for the windowed forms, the indices into
//! the source slice — no buffering of the input.
//!
//! * [`GaugeStepGrid`] — bare `metric{matchers}` selector at each tick,
//!   subject to the eager engine's staleness rule.
//! * [`GaugeAvgOverTime`] — `avg_over_time(metric[range])`.
//! * [`GaugeIdelta`] — `idelta(metric[range])`, last-two-samples delta.

use crate::tsdb::{GaugeCollection, Labels};

use super::{LabeledSeries, Point, SeriesSet};

/// `metric{matchers}` evaluated at a fixed step grid.
///
/// At every step tick, take the latest sample at-or-before the tick.
/// Skip ticks where the staleness gap (tick - sample_ts) exceeds
/// `staleness_ns`.
pub struct GaugeStepGrid<'a> {
    samples: &'a [(u64, i64)],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    staleness_ns: u64,
    done: bool,
}

impl<'a> GaugeStepGrid<'a> {
    pub fn new(
        samples: &'a [(u64, i64)],
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        staleness_ns: u64,
    ) -> Self {
        Self {
            samples,
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            staleness_ns,
            done: step_ns == 0,
        }
    }
}

impl<'a> Iterator for GaugeStepGrid<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while !self.done && self.cursor_ns <= self.end_ns {
            let t = self.cursor_ns;
            match self.cursor_ns.checked_add(self.step_ns) {
                Some(next) => self.cursor_ns = next,
                None => self.done = true,
            }

            let hi = self.samples.partition_point(|&(ts, _)| ts <= t);
            if hi == 0 {
                continue;
            }
            let (ts, val) = self.samples[hi - 1];
            if t.saturating_sub(ts) > self.staleness_ns {
                continue;
            }
            return Some((t, val as f64));
        }
        None
    }
}

/// `avg_over_time(metric[range])` evaluated at a step grid.
///
/// At every step tick, average all samples with timestamps in
/// `[t - range, t]`. Empty windows produce no output.
pub struct GaugeAvgOverTime<'a> {
    samples: &'a [(u64, i64)],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
    done: bool,
}

impl<'a> GaugeAvgOverTime<'a> {
    pub fn new(
        samples: &'a [(u64, i64)],
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        range_ns: u64,
    ) -> Self {
        Self {
            samples,
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            range_ns,
            done: step_ns == 0,
        }
    }
}

impl<'a> Iterator for GaugeAvgOverTime<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while !self.done && self.cursor_ns <= self.end_ns {
            let t = self.cursor_ns;
            match self.cursor_ns.checked_add(self.step_ns) {
                Some(next) => self.cursor_ns = next,
                None => self.done = true,
            }

            let window_start = t.saturating_sub(self.range_ns);
            let lo = self.samples.partition_point(|&(ts, _)| ts < window_start);
            let hi = self.samples.partition_point(|&(ts, _)| ts <= t);
            if hi == lo {
                continue;
            }

            let mut sum = 0.0_f64;
            let count = hi - lo;
            for (_, v) in &self.samples[lo..hi] {
                sum += *v as f64;
            }
            return Some((t, sum / count as f64));
        }
        None
    }
}

/// `idelta(metric[range])` evaluated at a step grid.
///
/// At every step tick, take the last two samples in `[t - range, t]`
/// and emit their difference (`v_cur - v_prev`). Skip ticks where the
/// window contains fewer than two samples.
pub struct GaugeIdelta<'a> {
    samples: &'a [(u64, i64)],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
    done: bool,
}

impl<'a> GaugeIdelta<'a> {
    pub fn new(
        samples: &'a [(u64, i64)],
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        range_ns: u64,
    ) -> Self {
        Self {
            samples,
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            range_ns,
            done: step_ns == 0,
        }
    }
}

impl<'a> Iterator for GaugeIdelta<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while !self.done && self.cursor_ns <= self.end_ns {
            let t = self.cursor_ns;
            match self.cursor_ns.checked_add(self.step_ns) {
                Some(next) => self.cursor_ns = next,
                None => self.done = true,
            }

            let window_start = t.saturating_sub(self.range_ns);
            let lo = self.samples.partition_point(|&(ts, _)| ts < window_start);
            let hi = self.samples.partition_point(|&(ts, _)| ts <= t);
            if hi.saturating_sub(lo) < 2 {
                continue;
            }
            let cur = self.samples[hi - 1].1 as f64;
            let prev = self.samples[hi - 2].1 as f64;
            return Some((t, cur - prev));
        }
        None
    }
}

// Pipeline-builder helpers (analogous to `irate_counters` /
// `rate_counters` for the counter side). Each filters the
// collection up front and yields one `LabeledSeries` per matching
// gauge series.

pub fn gauges_step_grid<'a>(
    collection: &'a GaugeCollection,
    filter: &Labels,
    start_ns: u64,
    end_ns: u64,
    step_ns: u64,
    staleness_ns: u64,
) -> SeriesSet<'a> {
    let mut out = Vec::new();
    for (labels, series) in collection.iter() {
        if !filter.inner.is_empty() && !labels.matches(filter) {
            continue;
        }
        let iter = GaugeStepGrid::new(series.samples(), start_ns, end_ns, step_ns, staleness_ns);
        out.push(LabeledSeries::new(labels.clone(), iter));
    }
    out
}

pub fn gauges_avg_over_time<'a>(
    collection: &'a GaugeCollection,
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
        let iter = GaugeAvgOverTime::new(series.samples(), start_ns, end_ns, step_ns, range_ns);
        out.push(LabeledSeries::new(labels.clone(), iter));
    }
    out
}

pub fn gauges_idelta<'a>(
    collection: &'a GaugeCollection,
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
        let iter = GaugeIdelta::new(series.samples(), start_ns, end_ns, step_ns, range_ns);
        out.push(LabeledSeries::new(labels.clone(), iter));
    }
    out
}

/// `deriv(metric[range])` evaluated at a step grid.
///
/// Eager semantics from `calculate_deriv_for_collection`: at each
/// emit tick `t`, take all source samples in `[t - 2*step, t + step]`
/// (a forward-leaning three-step window) and compute the
/// least-squares regression slope of `(ts, value)` over them.  Skip
/// ticks where the window has fewer than two samples.
///
/// Note the window is forward-looking by `step`. With random access
/// to the source slice via `partition_point` this is fine — no
/// look-ahead buffering is needed because we own the whole `&[(u64,
/// i64)]`.
pub struct GaugeDeriv<'a> {
    samples: &'a [(u64, i64)],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    done: bool,
}

impl<'a> GaugeDeriv<'a> {
    pub fn new(samples: &'a [(u64, i64)], start_ns: u64, end_ns: u64, step_ns: u64) -> Self {
        Self {
            samples,
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            done: step_ns == 0,
        }
    }
}

impl<'a> Iterator for GaugeDeriv<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while !self.done && self.cursor_ns <= self.end_ns {
            let t = self.cursor_ns;
            match self.cursor_ns.checked_add(self.step_ns) {
                Some(next) => self.cursor_ns = next,
                None => self.done = true,
            }

            let window_start = t.saturating_sub(self.step_ns.saturating_mul(2));
            let window_end = t.saturating_add(self.step_ns);
            let lo = self.samples.partition_point(|&(ts, _)| ts < window_start);
            let hi = self.samples.partition_point(|&(ts, _)| ts <= window_end);
            if hi.saturating_sub(lo) < 2 {
                continue;
            }

            // Allocation-free least-squares slope. `x` is in seconds
            // (the eager path runs the same conversion before
            // accumulating, so doing it inline preserves bit-for-bit
            // parity).
            let n = (hi - lo) as f64;
            let mut sum_x = 0.0_f64;
            let mut sum_y = 0.0_f64;
            let mut sum_xy = 0.0_f64;
            let mut sum_x2 = 0.0_f64;
            for &(ts, v) in &self.samples[lo..hi] {
                let x = ts as f64 / 1e9;
                let y = v as f64;
                sum_x += x;
                sum_y += y;
                sum_xy += x * y;
                sum_x2 += x * x;
            }
            let denom = n * sum_x2 - sum_x * sum_x;
            if denom.abs() < 1e-10 {
                // Mirror eager: degenerate vertical-line input emits
                // a slope of 0 rather than ±inf.
                return Some((t, 0.0));
            }
            let slope = (n * sum_xy - sum_x * sum_y) / denom;
            return Some((t, slope));
        }
        None
    }
}

pub fn gauges_deriv<'a>(
    collection: &'a GaugeCollection,
    filter: &Labels,
    start_ns: u64,
    end_ns: u64,
    step_ns: u64,
) -> SeriesSet<'a> {
    let mut out = Vec::new();
    for (labels, series) in collection.iter() {
        if !filter.inner.is_empty() && !labels.matches(filter) {
            continue;
        }
        let iter = GaugeDeriv::new(series.samples(), start_ns, end_ns, step_ns);
        out.push(LabeledSeries::new(labels.clone(), iter));
    }
    out
}
