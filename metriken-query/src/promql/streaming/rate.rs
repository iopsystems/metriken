use super::Point;

pub struct CounterRate<'a> {
    timestamps: &'a [u64],
    values: &'a [u64],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
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
    ) -> Self {
        Self {
            timestamps,
            values,
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

            return Some((t, total_increase / dur_s));
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
            return Some((ts_cur, delta / dur_s));
        }
        None
    }
}
