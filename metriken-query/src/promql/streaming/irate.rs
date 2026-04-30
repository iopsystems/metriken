//! `irate` as a pull-based iterator over a counter sample slice.
//!
//! Mirrors `CounterSeries::windowed_irate` exactly — at each step
//! tick, locate the last two samples in `[t - range, t]` and emit
//! `(t, delta / duration)`. The only state held is the cursor; the
//! sample slice is borrowed from the TSDB.

use super::Point;

pub struct CounterIrate<'a> {
    samples: &'a [(u64, u64)],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
    /// Set once the cursor would overflow past `end_ns`. Without this
    /// flag, a `step_ns` of 0 would loop forever; we treat that as a
    /// caller bug but still bail cleanly.
    done: bool,
}

impl<'a> CounterIrate<'a> {
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

impl<'a> Iterator for CounterIrate<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while !self.done && self.cursor_ns <= self.end_ns {
            let t = self.cursor_ns;
            // Advance for the next call. Saturating to avoid wrapping
            // past u64::MAX; the `self.cursor_ns <= self.end_ns` gate
            // above will catch the saturated case on the next tick.
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

            let (ts_cur, v_cur) = self.samples[hi - 1];
            let (ts_prev, v_prev) = self.samples[hi - 2];
            let delta = if v_cur >= v_prev {
                (v_cur - v_prev) as f64
            } else {
                // Counter reset: PromQL irate treats the reset point's
                // own value as the increase.
                v_cur as f64
            };
            let dur_s = (ts_cur - ts_prev) as f64 / 1e9;
            if dur_s <= 0.0 {
                continue;
            }

            return Some((t, delta / dur_s));
        }
        None
    }
}
