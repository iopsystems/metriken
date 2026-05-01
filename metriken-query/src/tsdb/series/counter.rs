use super::*;

/// Represents a series of counter readings, stored as a sorted
/// `Vec<(timestamp_ns, value)>` ordered by timestamp.  Insertion is O(1) at
/// the end (the load-time pattern) and falls back to binary-search insert for
/// out-of-order writes.
#[derive(Default, Clone)]
pub struct CounterSeries {
    inner: Vec<(u64, u64)>,
}

impl CounterSeries {
    pub fn insert(&mut self, timestamp: u64, value: u64) {
        if let Some(&(last_ts, _)) = self.inner.last() {
            if timestamp > last_ts {
                self.inner.push((timestamp, value));
                return;
            }
            if timestamp == last_ts {
                self.inner.last_mut().unwrap().1 = value;
                return;
            }
        } else {
            self.inner.push((timestamp, value));
            return;
        }
        match self.inner.binary_search_by_key(&timestamp, |&(t, _)| t) {
            Ok(idx) => self.inner[idx].1 = value,
            Err(idx) => self.inner.insert(idx, (timestamp, value)),
        }
    }

    /// Returns the time bounds (min, max) in nanoseconds, or None if empty.
    pub fn time_bounds(&self) -> Option<(u64, u64)> {
        let first = self.inner.first()?.0;
        let last = self.inner.last()?.0;
        Some((first, last))
    }

    /// Borrow the raw sample slice. Used by streaming operators that
    /// build iterator pipelines without cloning.
    pub(crate) fn samples(&self) -> &[(u64, u64)] {
        &self.inner
    }

    /// Slice covering all samples whose timestamp falls in `[start, end]`.
    fn range_window(&self, start: u64, end: u64) -> &[(u64, u64)] {
        let lo = self.inner.partition_point(|&(t, _)| t < start);
        let hi = self.inner.partition_point(|&(t, _)| t <= end);
        &self.inner[lo..hi]
    }

    pub fn rate(&self) -> UntypedSeries {
        let mut rates: Vec<(u64, f64)> = Vec::with_capacity(self.inner.len().saturating_sub(1));
        let mut prev: Option<(u64, u64)> = None;

        for &(ts, value) in self.inner.iter() {
            if let Some((prev_ts, prev_v)) = prev {
                let delta = value.wrapping_sub(prev_v);

                if delta < 1 << 63 {
                    let duration = ts.wrapping_sub(prev_ts);

                    let rate = delta as f64 / (duration as f64 / 1000000000.0);

                    rates.push((ts, rate));
                }
            }

            prev = Some((ts, value));
        }

        UntypedSeries::from_sorted(rates)
    }

    /// PromQL `rate()`: average per-second rate of increase over a window.
    ///
    /// Walks all consecutive sample pairs in `[window_start, window_end]`,
    /// accumulates counter increases (handling resets), and divides by the
    /// total time span between first and last sample.
    pub fn windowed_rate(&self, window_start: u64, window_end: u64) -> Option<f64> {
        let samples = self.range_window(window_start, window_end);

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
        let samples = self.range_window(window_start, window_end);

        if samples.len() < 2 {
            return None;
        }

        let (ts_cur, val_cur) = samples[samples.len() - 1];
        let (ts_prev, val_prev) = samples[samples.len() - 2];

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
