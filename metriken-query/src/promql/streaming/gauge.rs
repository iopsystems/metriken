use super::Point;

pub struct GaugeStepGrid<'a> {
    timestamps: &'a [u64],
    values: &'a [i64],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    staleness_ns: u64,
    done: bool,
}

impl<'a> GaugeStepGrid<'a> {
    pub fn new(
        timestamps: &'a [u64],
        values: &'a [i64],
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        staleness_ns: u64,
    ) -> Self {
        Self {
            timestamps,
            values,
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

            let hi = self.timestamps.partition_point(|&ts| ts <= t);
            if hi == 0 {
                continue;
            }
            let ts = self.timestamps[hi - 1];
            let val = self.values[hi - 1];
            if t.saturating_sub(ts) > self.staleness_ns {
                continue;
            }
            return Some((t, val as f64));
        }
        None
    }
}

pub struct GaugeAvgOverTime<'a> {
    timestamps: &'a [u64],
    values: &'a [i64],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
    done: bool,
}

impl<'a> GaugeAvgOverTime<'a> {
    pub fn new(
        timestamps: &'a [u64],
        values: &'a [i64],
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
            return Some((t, sum / count as f64));
        }
        None
    }
}

pub struct GaugeIdelta<'a> {
    timestamps: &'a [u64],
    values: &'a [i64],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    range_ns: u64,
    done: bool,
}

impl<'a> GaugeIdelta<'a> {
    pub fn new(
        timestamps: &'a [u64],
        values: &'a [i64],
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
            let lo = self.timestamps.partition_point(|&ts| ts < window_start);
            let hi = self.timestamps.partition_point(|&ts| ts <= t);
            if hi.saturating_sub(lo) < 2 {
                continue;
            }
            let cur = self.values[hi - 1] as f64;
            let prev = self.values[hi - 2] as f64;
            return Some((t, cur - prev));
        }
        None
    }
}

pub struct GaugeDeriv<'a> {
    timestamps: &'a [u64],
    values: &'a [i64],
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    done: bool,
}

impl<'a> GaugeDeriv<'a> {
    pub fn new(
        timestamps: &'a [u64],
        values: &'a [i64],
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
    ) -> Self {
        Self {
            timestamps,
            values,
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
                return Some((t, 0.0));
            }
            let slope = (n * sum_xy - sum_x * sum_y) / denom;
            return Some((t, slope));
        }
        None
    }
}

