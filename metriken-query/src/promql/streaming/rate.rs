//! `rate` as a pull-based iterator over a counter sample slice.
//!
//! Mirrors `CounterSeries::windowed_rate`: at each step tick, walk
//! every consecutive sample pair in `[t - range, t]`, accumulate
//! counter increases (handling resets), divide by the time span
//! between the first and last sample. Like [`super::CounterIrate`]
//! the only state is the cursor; sample slice is borrowed.

use super::Point;

pub struct CounterRate<'a> {
    samples: &'a [(u64, u64)],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
    done: bool,
}

impl<'a> CounterRate<'a> {
    pub fn new(
        samples: &'a [(u64, u64)],
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
            let lo = self.samples.partition_point(|&(ts, _)| ts < window_start);
            let hi = self.samples.partition_point(|&(ts, _)| ts <= t);
            if hi.saturating_sub(lo) < 2 {
                continue;
            }

            let mut total_increase = 0.0;
            for win in self.samples[lo..hi].windows(2) {
                let prev_v = win[0].1;
                let cur_v = win[1].1;
                if cur_v >= prev_v {
                    total_increase += (cur_v - prev_v) as f64;
                } else {
                    // Counter reset: PromQL rate treats the post-reset
                    // value itself as the increase across that pair.
                    total_increase += cur_v as f64;
                }
            }

            let first_ts = self.samples[lo].0;
            let last_ts = self.samples[hi - 1].0;
            let dur_s = (last_ts - first_ts) as f64 / 1e9;
            if dur_s <= 0.0 {
                continue;
            }

            return Some((t, total_increase / dur_s));
        }
        None
    }
}

/// Pair-wise rate producer over a counter sample slice.
///
/// Distinct from [`CounterRate`] (windowed step-grid average): this
/// emits one `(ts_cur, rate)` per consecutive sample pair, anchored
/// at the second sample's timestamp. Mirrors the eager
/// `CounterSeries::rate()` method that produces an `UntypedSeries`
/// of pair-wise rates — used as the upstream for `deriv()` over a
/// counter (the 2nd-derivative case).
///
/// Holds two cursor positions; no buffering. Skips pairs where the
/// duration is non-positive (matching the eager skip-on-zero-dur).
pub struct CounterPairwiseRate<'a> {
    samples: &'a [(u64, u64)],
    cursor: usize,
    end_ns: u64,
}

impl<'a> CounterPairwiseRate<'a> {
    pub fn new(samples: &'a [(u64, u64)], end_ns: u64) -> Self {
        Self {
            samples,
            cursor: 0,
            end_ns,
        }
    }
}

impl<'a> Iterator for CounterPairwiseRate<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while self.cursor + 1 < self.samples.len() {
            let i = self.cursor;
            self.cursor += 1;
            let (ts_prev, v_prev) = self.samples[i];
            let (ts_cur, v_cur) = self.samples[i + 1];
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
            return Some((ts_cur, delta / dur_s));
        }
        None
    }
}
