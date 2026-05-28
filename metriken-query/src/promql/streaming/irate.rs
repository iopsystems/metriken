use super::Point;

pub struct CounterIrate<'a> {
    timestamps: &'a [u64],
    values: &'a [u64],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
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

            return Some((t, delta / dur_s));
        }
        None
    }
}
