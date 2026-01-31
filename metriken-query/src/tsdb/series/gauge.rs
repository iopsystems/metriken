use super::*;

/// Represents a series of gauge readings.
#[derive(Default, Clone)]
pub struct GaugeSeries {
    inner: BTreeMap<u64, i64>,
}

impl GaugeSeries {
    pub fn insert(&mut self, timestamp: u64, value: i64) {
        self.inner.insert(timestamp, value);
    }

    /// Returns the time bounds (min, max) in nanoseconds, or None if empty.
    pub fn time_bounds(&self) -> Option<(u64, u64)> {
        let min = *self.inner.keys().next()?;
        let max = *self.inner.keys().next_back()?;
        Some((min, max))
    }

    pub fn untyped(&self) -> UntypedSeries {
        UntypedSeries {
            inner: self.inner.iter().map(|(k, v)| (*k, *v as f64)).collect(),
        }
    }
}
