use super::*;

/// Represents a series of counter readings.
#[derive(Default, Clone)]
pub struct CounterSeries {
    inner: BTreeMap<u64, u64>,
}

impl CounterSeries {
    pub fn insert(&mut self, timestamp: u64, value: u64) {
        self.inner.insert(timestamp, value);
    }

    /// Returns the time bounds (min, max) in nanoseconds, or None if empty.
    pub fn time_bounds(&self) -> Option<(u64, u64)> {
        let min = *self.inner.keys().next()?;
        let max = *self.inner.keys().next_back()?;
        Some((min, max))
    }

    pub fn rate(&self) -> UntypedSeries {
        let mut rates = UntypedSeries::default();
        let mut prev: Option<(u64, u64)> = None;

        for (ts, value) in self.inner.iter() {
            if let Some((prev_ts, prev_v)) = prev {
                let delta = value.wrapping_sub(prev_v);

                if delta < 1 << 63 {
                    let duration = ts.wrapping_sub(prev_ts);

                    let rate = delta as f64 / (duration as f64 / 1000000000.0);

                    rates.inner.insert(*ts, rate);
                }
            }

            prev = Some((*ts, *value));
        }

        rates
    }

    /// PromQL `rate()`: average per-second rate of increase over a window.
    ///
    /// Walks all consecutive sample pairs in `[window_start, window_end]`,
    /// accumulates counter increases (handling resets), and divides by the
    /// total time span between first and last sample.
    pub fn windowed_rate(&self, window_start: u64, window_end: u64) -> Option<f64> {
        let samples: Vec<(u64, u64)> = self
            .inner
            .range(window_start..=window_end)
            .map(|(ts, v)| (*ts, *v))
            .collect();

        if samples.len() < 2 {
            return None;
        }

        let mut total_increase = 0.0;
        for pair in samples.windows(2) {
            let prev_val = pair[0].1;
            let cur_val = pair[1].1;
            if cur_val >= prev_val {
                total_increase += (cur_val - prev_val) as f64;
            } else {
                // Counter reset: assume reset to 0, increase is the new value
                total_increase += cur_val as f64;
            }
        }

        let first_ts = samples.first().unwrap().0;
        let last_ts = samples.last().unwrap().0;
        let duration_s = (last_ts - first_ts) as f64 / 1e9;

        if duration_s <= 0.0 {
            return None;
        }

        Some(total_increase / duration_s)
    }

    /// PromQL `irate()`: instantaneous per-second rate from the last two
    /// samples in the window.
    pub fn windowed_irate(&self, window_start: u64, window_end: u64) -> Option<f64> {
        let last_two: Vec<(u64, u64)> = self
            .inner
            .range(window_start..=window_end)
            .rev()
            .take(2)
            .map(|(ts, v)| (*ts, *v))
            .collect();

        if last_two.len() < 2 {
            return None;
        }

        // last_two[0] is the latest, last_two[1] is second-to-last
        let (ts_cur, val_cur) = last_two[0];
        let (ts_prev, val_prev) = last_two[1];

        let delta = if val_cur >= val_prev {
            (val_cur - val_prev) as f64
        } else {
            // Counter reset
            val_cur as f64
        };

        let duration_s = (ts_cur - ts_prev) as f64 / 1e9;
        if duration_s <= 0.0 {
            return None;
        }

        Some(delta / duration_s)
    }
}
