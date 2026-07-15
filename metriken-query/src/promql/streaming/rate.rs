//! `rate` as a pull-based iterator over a counter sample slice.
//!
//! At each step tick, walk every consecutive sample pair in
//! `[t - range, t]`, accumulate counter increases (handling resets),
//! and divide by the time span between the first and last sample.
//! Like [`super::CounterIrate`] the only state is the cursor; the
//! sample slice is borrowed from the upstream source.

use super::Point;

pub struct CounterRate<'a> {
    timestamps: &'a [u64],
    values: &'a [u64],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
    windows: Option<&'a [(u64, u64)]>,
    done: bool,
}

impl<'a> CounterRate<'a> {
    pub fn new(
        timestamps: &'a [u64],
        values: &'a [u64],
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        range_ns: u64,
        windows: Option<&'a [(u64, u64)]>,
    ) -> Self {
        Self {
            timestamps,
            values,
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            range_ns,
            windows,
            done: step_ns == 0,
        }
    }
}

impl<'a> Iterator for CounterRate<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while !self.done && self.cursor_ns <= self.end_ns {
            let t = self.cursor_ns;
            match self.cursor_ns.checked_add(self.step_ns) {
                Some(next) => self.cursor_ns = next,
                None => self.done = true,
            }

            let window_start = t.saturating_sub(self.range_ns);
            let lo = self.timestamps.partition_point(|&ts| ts < window_start);
            let hi = self.timestamps.partition_point(|&ts| ts <= t);
            if hi.saturating_sub(lo) < 2 {
                continue;
            }

            let mut total_increase = 0.0;
            for i in lo..hi - 1 {
                let prev_v = self.values[i];
                let cur_v = self.values[i + 1];
                if cur_v >= prev_v {
                    total_increase += (cur_v - prev_v) as f64;
                } else {
                    total_increase += cur_v as f64;
                }
            }

            let first_ts = self.timestamps[lo];
            let last_ts = self.timestamps[hi - 1];
            let dur_s = (last_ts - first_ts) as f64 / 1e9;
            if dur_s <= 0.0 {
                continue;
            }

            let bounds = self.windows.and_then(|w| {
                let (b_first, e_first) = *w.get(lo)?;
                let (b_last, e_last) = *w.get(hi - 1)?;
                let elapsed_max = e_last.saturating_sub(b_first) as f64 / 1e9;
                let elapsed_min = b_last.saturating_sub(e_first) as f64 / 1e9;
                if elapsed_min > 0.0 && elapsed_max > 0.0 {
                    Some((total_increase / elapsed_max, total_increase / elapsed_min))
                } else {
                    None
                }
            });
            return Some(Point {
                t,
                v: total_increase / dur_s,
                bounds,
            });
        }
        None
    }
}

/// Pair-wise rate producer over a counter sample slice.
pub struct CounterPairwiseRate<'a> {
    timestamps: &'a [u64],
    values: &'a [u64],
    cursor: usize,
    end_ns: u64,
}

impl<'a> CounterPairwiseRate<'a> {
    pub fn new(timestamps: &'a [u64], values: &'a [u64], end_ns: u64) -> Self {
        Self {
            timestamps,
            values,
            cursor: 0,
            end_ns,
        }
    }
}

impl<'a> Iterator for CounterPairwiseRate<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while self.cursor + 1 < self.timestamps.len() {
            let i = self.cursor;
            self.cursor += 1;
            let ts_prev = self.timestamps[i];
            let v_prev = self.values[i];
            let ts_cur = self.timestamps[i + 1];
            let v_cur = self.values[i + 1];
            if ts_cur > self.end_ns {
                return None;
            }
            let delta = if v_cur >= v_prev {
                (v_cur - v_prev) as f64
            } else {
                v_cur as f64
            };
            let dur_s = (ts_cur - ts_prev) as f64 / 1e9;
            if dur_s <= 0.0 {
                continue;
            }
            return Some(Point::at(ts_cur, delta / dur_s));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_computes_interval_bounds_from_windows() {
        let ts = [1_000_000_000u64, 2_000_000_000];
        let vals = [100u64, 400];
        let windows = [
            (950_000_000u64, 980_000_000u64),
            (1_960_000_000u64, 1_980_000_000u64),
        ];
        let pts: Vec<Point> = CounterRate::new(
            &ts,
            &vals,
            2_000_000_000, // start = single eval point
            2_000_000_000, // end
            1_000_000_000, // step
            2_000_000_000, // range covers both samples
            Some(&windows),
        )
        .collect();
        assert_eq!(pts.len(), 1);
        let p = pts[0];
        assert!((p.v - 300.0).abs() < 1e-6, "nominal {}", p.v); // 300/1s
        let (lo, hi) = p.bounds.expect("bounds present");
        // elapsed_max=(1.98e9-0.95e9)/1e9=1.03 -> 300/1.03=291.26
        // elapsed_min=(1.96e9-0.98e9)/1e9=0.98 -> 300/0.98=306.12
        assert!((lo - 291.2621).abs() < 0.02, "lo {lo}");
        assert!((hi - 306.1224).abs() < 0.02, "hi {hi}");
        assert!(lo <= p.v && p.v <= hi);
    }

    #[test]
    fn rate_without_windows_has_no_bounds() {
        let ts = [1_000_000_000u64, 2_000_000_000];
        let vals = [100u64, 400];
        let pts: Vec<Point> =
            CounterRate::new(&ts, &vals, 2_000_000_000, 2_000_000_000, 1_000_000_000, 2_000_000_000, None)
                .collect();
        assert_eq!(pts.len(), 1);
        assert!(pts[0].bounds.is_none());
    }
}
