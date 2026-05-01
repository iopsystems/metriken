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
}
