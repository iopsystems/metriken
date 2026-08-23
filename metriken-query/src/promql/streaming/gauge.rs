//! Gauge-side streaming producers.
//!
//! All operate on a borrowed `&[(u64, i64)]` slice (the gauge sample
//! storage) and emit `f64` values at step-aligned timestamps. State
//! is the cursor plus, for the windowed forms, the indices into the
//! source slice — no buffering of the input.
//!
//! * [`GaugeStepGrid`] — bare `metric{matchers}` selector at each tick,
//!   subject to the staleness rule.
//! * [`GaugeAvgOverTime`] — `avg_over_time(metric[range])`.
//! * [`GaugeIdelta`] — `idelta(metric[range])`, last-two-samples delta.
//! * [`GaugeDeriv`] — `deriv(metric[range])`, least-squares slope.

use super::Point;

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
    cursor_ns: u64,
    pub(crate) step_ns: u64,
    raw_idx: usize,
    points: Option<(std::sync::Arc<[u64]>, usize)>,
}

impl Placement {
    fn new(raw: bool, start_ns: u64, step_ns: u64) -> Self {
        Self {
            raw,
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
            let t = *points.get(*idx)?;
            *idx += 1;
            return (t <= end_ns).then_some(t);
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
        impl<'a> AtPoints for $t<'a> {
            fn at_points(mut self, points: std::sync::Arc<[u64]>) -> Self {
                self.place = self.place.at_points(points);
                self
            }
        }
    )+};
}

impl_at_points!(GaugeStepGrid, GaugeAvgOverTime, GaugeIdelta, GaugeDeriv);

pub struct GaugeStepGrid<'a> {
    timestamps: &'a [u64],
    values: &'a [i64],
    end_ns: u64,
    staleness_ns: u64,
    place: Placement,
}

impl<'a> GaugeStepGrid<'a> {
    pub fn new(
        timestamps: &'a [u64],
        values: &'a [i64],
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

impl<'a> Iterator for GaugeStepGrid<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = self.place.next(self.timestamps, self.end_ns)?;

            let hi = self.timestamps.partition_point(|&ts| ts <= t);
            if hi == 0 {
                continue;
            }
            let ts = self.timestamps[hi - 1];
            let val = self.values[hi - 1];
            if t.saturating_sub(ts) > self.staleness_ns {
                continue;
            }
            return Some(Point::at(t, val as f64));
        }
    }
}

pub struct GaugeAvgOverTime<'a> {
    timestamps: &'a [u64],
    values: &'a [i64],
    end_ns: u64,
    range_ns: u64,
    place: Placement,
}

impl<'a> GaugeAvgOverTime<'a> {
    pub fn new(
        timestamps: &'a [u64],
        values: &'a [i64],
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

impl<'a> Iterator for GaugeAvgOverTime<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = self.place.next(self.timestamps, self.end_ns)?;

            let window_start = t.saturating_sub(self.range_ns);
            let lo = self.timestamps.partition_point(|&ts| ts < window_start);
            let hi = self.timestamps.partition_point(|&ts| ts <= t);
            if hi == lo {
                continue;
            }

            let mut sum = 0.0_f64;
            let count = hi - lo;
            for &v in &self.values[lo..hi] {
                sum += v as f64;
            }
            return Some(Point::at(t, sum / count as f64));
        }
    }
}

pub struct GaugeIdelta<'a> {
    timestamps: &'a [u64],
    values: &'a [i64],
    end_ns: u64,
    range_ns: u64,
    place: Placement,
}

impl<'a> GaugeIdelta<'a> {
    pub fn new(
        timestamps: &'a [u64],
        values: &'a [i64],
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

impl<'a> Iterator for GaugeIdelta<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = self.place.next(self.timestamps, self.end_ns)?;

            let window_start = t.saturating_sub(self.range_ns);
            let lo = self.timestamps.partition_point(|&ts| ts < window_start);
            let hi = self.timestamps.partition_point(|&ts| ts <= t);
            if hi.saturating_sub(lo) < 2 {
                continue;
            }
            let cur = self.values[hi - 1] as f64;
            let prev = self.values[hi - 2] as f64;
            return Some(Point::at(t, cur - prev));
        }
    }
}

pub struct GaugeDeriv<'a> {
    timestamps: &'a [u64],
    values: &'a [i64],
    end_ns: u64,
    place: Placement,
}

impl<'a> GaugeDeriv<'a> {
    pub fn new(
        timestamps: &'a [u64],
        values: &'a [i64],
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        raw: bool,
    ) -> Self {
        Self {
            timestamps,
            values,
            end_ns,
            place: Placement::new(raw, start_ns, step_ns),
        }
    }
}

impl<'a> Iterator for GaugeDeriv<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = self.place.next(self.timestamps, self.end_ns)?;

            let window_start = t.saturating_sub(self.place.step_ns.saturating_mul(2));
            let window_end = t.saturating_add(self.place.step_ns);
            let lo = self.timestamps.partition_point(|&ts| ts < window_start);
            let hi = self.timestamps.partition_point(|&ts| ts <= window_end);
            if hi.saturating_sub(lo) < 2 {
                continue;
            }

            let n = (hi - lo) as f64;
            let mut sum_x = 0.0_f64;
            let mut sum_y = 0.0_f64;
            let mut sum_xy = 0.0_f64;
            let mut sum_x2 = 0.0_f64;
            for i in lo..hi {
                let x = self.timestamps[i] as f64 / 1e9;
                let y = self.values[i] as f64;
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
            &ts,
            &vals,
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
            &ts,
            &vals,
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
