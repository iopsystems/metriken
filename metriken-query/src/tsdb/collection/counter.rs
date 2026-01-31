use super::*;

/// Represents a collection of counter timeseries keyed on label sets.
#[derive(Default)]
pub struct CounterCollection {
    inner: HashMap<Labels, CounterSeries>,
}

impl CounterCollection {
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn entry(&mut self, labels: Labels) -> Entry<'_, Labels, CounterSeries> {
        self.inner.entry(labels)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Labels, &CounterSeries)> {
        self.inner.iter()
    }

    /// Returns the time bounds (min, max) in nanoseconds across all series, or None if empty.
    pub fn time_bounds(&self) -> Option<(u64, u64)> {
        let mut min_time: Option<u64> = None;
        let mut max_time: Option<u64> = None;

        for series in self.inner.values() {
            if let Some((series_min, series_max)) = series.time_bounds() {
                min_time = Some(min_time.map_or(series_min, |m| m.min(series_min)));
                max_time = Some(max_time.map_or(series_max, |m| m.max(series_max)));
            }
        }

        min_time.zip(max_time)
    }

    /// Old filter method that clones - kept for compatibility but should be avoided
    pub fn filter(&self, labels: &Labels) -> Self {
        let mut result = Self::default();

        for (k, v) in self.inner.iter() {
            if k.matches(labels) {
                result.inner.insert(k.clone(), v.clone());
            }
        }

        result
    }

    /// Efficiently compute rate for all series
    pub fn rate(&self) -> UntypedCollection {
        let mut result = UntypedCollection::default();

        for (labels, series) in self.inner.iter() {
            result.insert(labels.clone(), series.rate());
        }

        result
    }

    /// Efficiently compute rate only for series matching the filter
    /// This avoids cloning the time series data
    pub fn filtered_rate(&self, filter: &Labels) -> UntypedCollection {
        let mut result = UntypedCollection::default();

        for (labels, series) in self.inner.iter() {
            if labels.matches(filter) {
                result.insert(labels.clone(), series.rate());
            }
        }

        result
    }
}
