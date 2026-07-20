//! `irate` as a pull-based iterator over a counter sample slice.
//!
//! At each step tick, locate the last two samples in `[t - range, t]`
//! and emit `(t, delta / duration)`. The only state held is the
//! cursor; the sample slice is borrowed from the upstream source.

use super::Point;

pub struct CounterIrate<'a> {
    timestamps: &'a [u64],
    values: &'a [u64],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
    windows: Option<&'a [(u64, u64)]>,
    done: bool,
}

impl<'a> CounterIrate<'a> {
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

impl<'a> Iterator for CounterIrate<'a> {
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

            let ts_cur = self.timestamps[hi - 1];
            let v_cur = self.values[hi - 1];
            let ts_prev = self.timestamps[hi - 2];
            let v_prev = self.values[hi - 2];
            let delta = if v_cur >= v_prev {
                (v_cur - v_prev) as f64
            } else {
                v_cur as f64
            };
            let dur_s = (ts_cur - ts_prev) as f64 / 1e9;
            if dur_s <= 0.0 {
                continue;
            }

            let v = delta / dur_s;
            let bounds = self
                .windows
                .and_then(|w| {
                    let (b_prev, e_prev) = *w.get(hi - 2)?;
                    let (b_cur, e_cur) = *w.get(hi - 1)?;
                    let elapsed_max = e_cur.saturating_sub(b_prev) as f64 / 1e9;
                    let elapsed_min = b_cur.saturating_sub(e_prev) as f64 / 1e9;
                    if elapsed_min > 0.0 && elapsed_max > 0.0 {
                        Some((delta / elapsed_max, delta / elapsed_min))
                    } else {
                        None
                    }
                })
                // Widen the band to always contain the nominal (see rate.rs).
                .map(|(lo, hi)| (lo.min(v), hi.max(v)));
            return Some(Point { t, v, bounds });
        }
        None
    }
}
