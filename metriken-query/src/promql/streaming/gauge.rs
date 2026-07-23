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

/// The next evaluation timestamp for a gauge producer. `Grid` walks
/// `start + k·step` (PromQL default / [`crate::RateMode::Grid`]); `Raw`
/// ([`crate::RateMode::Raw`]) walks the actual sample timestamps so gauge
/// output lands on the same instants as counter `rate`/`irate` under Raw and
/// series-op-series (e.g. `x / cpu_cores`) aligns. Returns `None` when
/// exhausted (past `end_ns`, or grid with `step_ns == 0`).
fn next_eval_ts(
    raw: bool,
    cursor_ns: &mut u64,
    step_ns: u64,
    raw_idx: &mut usize,
    timestamps: &[u64],
    end_ns: u64,
) -> Option<u64> {
    if raw {
        if *raw_idx >= timestamps.len() {
            return None;
        }
        let t = timestamps[*raw_idx];
        *raw_idx += 1;
        if t > end_ns {
            return None;
        }
        Some(t)
    } else {
        if step_ns == 0 || *cursor_ns > end_ns {
            return None;
        }
        let t = *cursor_ns;
        // Saturating: an overflow lands past end_ns so the next call stops.
        *cursor_ns = cursor_ns.saturating_add(step_ns);
        Some(t)
    }
}

pub struct GaugeStepGrid<'a> {
    timestamps: &'a [u64],
    values: &'a [i64],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    staleness_ns: u64,
    raw: bool,
    raw_idx: usize,
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
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            staleness_ns,
            raw,
            raw_idx: 0,
        }
    }
}

impl<'a> Iterator for GaugeStepGrid<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = next_eval_ts(
                self.raw,
                &mut self.cursor_ns,
                self.step_ns,
                &mut self.raw_idx,
                self.timestamps,
                self.end_ns,
            )?;

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
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
    raw: bool,
    raw_idx: usize,
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
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            range_ns,
            raw,
            raw_idx: 0,
        }
    }
}

impl<'a> Iterator for GaugeAvgOverTime<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = next_eval_ts(
                self.raw,
                &mut self.cursor_ns,
                self.step_ns,
                &mut self.raw_idx,
                self.timestamps,
                self.end_ns,
            )?;

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
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
    raw: bool,
    raw_idx: usize,
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
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            range_ns,
            raw,
            raw_idx: 0,
        }
    }
}

impl<'a> Iterator for GaugeIdelta<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = next_eval_ts(
                self.raw,
                &mut self.cursor_ns,
                self.step_ns,
                &mut self.raw_idx,
                self.timestamps,
                self.end_ns,
            )?;

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
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    raw: bool,
    raw_idx: usize,
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
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            raw,
            raw_idx: 0,
        }
    }
}

impl<'a> Iterator for GaugeDeriv<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = next_eval_ts(
                self.raw,
                &mut self.cursor_ns,
                self.step_ns,
                &mut self.raw_idx,
                self.timestamps,
                self.end_ns,
            )?;

            let window_start = t.saturating_sub(self.step_ns.saturating_mul(2));
            let window_end = t.saturating_add(self.step_ns);
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
            &ts, &vals, 0, 3_000_000_000, 1_000_000_000, 5_000_000_000, true, // raw
        )
        .collect();
        assert_eq!(
            pts.iter().map(|p| p.t).collect::<Vec<_>>(),
            vec![500_000_000, 1_500_000_000, 2_500_000_000]
        );
        assert_eq!(pts.iter().map(|p| p.v).collect::<Vec<_>>(), vec![10.0, 20.0, 30.0]);
    }

    #[test]
    fn gauge_step_grid_grid_still_walks_the_step_grid() {
        // Grid mode (raw=false) is unchanged: points land on start+k·step.
        let ts = [500_000_000u64, 1_500_000_000, 2_500_000_000];
        let vals = [10i64, 20, 30];
        let pts: Vec<Point> = GaugeStepGrid::new(
            &ts, &vals, 0, 3_000_000_000, 1_000_000_000, 5_000_000_000, false,
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
