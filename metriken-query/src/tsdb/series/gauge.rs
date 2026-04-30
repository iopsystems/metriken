use super::*;

/// Represents a series of gauge readings, stored as a sorted
/// `Vec<(timestamp_ns, value)>` ordered by timestamp.
#[derive(Default, Clone)]
pub struct GaugeSeries {
    inner: Vec<(u64, i64)>,
}

impl GaugeSeries {
    pub fn insert(&mut self, timestamp: u64, value: i64) {
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

    pub fn untyped(&self) -> UntypedSeries {
        UntypedSeries::from_sorted(self.inner.iter().map(|(t, v)| (*t, *v as f64)).collect())
    }
}
