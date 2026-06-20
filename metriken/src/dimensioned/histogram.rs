use std::any::Any;
use std::marker::PhantomData;
use std::sync::OnceLock;

use histogram::Error;
use metriken_core::{Metric, Value};

use crate::group::HistogramGroup;
use crate::MetricDimension;

/// A group of histograms indexed by a compile-time enum dimension.
///
/// All histograms share the same configuration (grouping_power, max_value_power).
/// All dimension variants are always exported. Metadata is initialized lazily
/// on first `Metric::value()` or write call.
pub struct DimensionedHistogram<D: MetricDimension> {
    pub(crate) group: HistogramGroup,
    init: OnceLock<()>,
    _dim: PhantomData<fn() -> D>,
}

impl<D: MetricDimension> DimensionedHistogram<D> {
    /// Create a new dimensioned histogram group.
    ///
    /// # Panics
    /// Panics if the histogram configuration is invalid (see `histogram::Config::new`).
    pub const fn new(grouping_power: u8, max_value_power: u8) -> Self {
        Self {
            group: HistogramGroup::new(D::COUNT, grouping_power, max_value_power),
            init: OnceLock::new(),
            _dim: PhantomData,
        }
    }

    fn ensure_metadata(&self) {
        self.init.get_or_init(|| {
            for (idx, labels) in D::all_labels().into_iter().enumerate() {
                self.group.set_metadata(idx, labels);
            }
        });
    }

    /// Record `value` in the histogram for `dim`.
    ///
    /// Returns `Err` if the value is outside the histogram's configured range.
    /// Returns `Ok(false)` if the index is out of bounds (cannot happen with
    /// a correctly derived `MetricDimension`).
    pub fn increment(&self, dim: D, value: u64) -> Result<bool, Error> {
        self.ensure_metadata();
        self.group.increment(dim.index(), value)
    }

    /// Load a snapshot of the histogram for `dim`.
    ///
    /// Returns `None` if never written. This is a pure read.
    pub fn load(&self, dim: D) -> Option<histogram::Histogram> {
        self.group.load(dim.index())
    }
}

impl<D: MetricDimension> Metric for DimensionedHistogram<D> {
    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        self.ensure_metadata();
        Some(Value::HistogramGroup(&self.group))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    enum Op {
        Read,
        Write,
    }

    impl MetricDimension for Op {
        const COUNT: usize = 2;

        fn index(&self) -> usize {
            match self {
                Op::Read => 0,
                Op::Write => 1,
            }
        }

        fn labels(&self) -> HashMap<String, String> {
            let mut map = HashMap::new();
            let v = match self {
                Op::Read => "read",
                Op::Write => "write",
            };
            map.insert("op".to_string(), v.to_string());
            map
        }

        fn all_labels() -> Vec<HashMap<String, String>> {
            ["read", "write"]
                .into_iter()
                .map(|v| {
                    let mut m = HashMap::new();
                    m.insert("op".to_string(), v.to_string());
                    m
                })
                .collect()
        }
    }

    #[test]
    fn basic_increment_and_load() {
        static H: DimensionedHistogram<Op> = DimensionedHistogram::new(7, 64);

        assert!(H.load(Op::Read).is_none()); // not initialized yet
        H.increment(Op::Read, 1000).unwrap();
        assert!(H.load(Op::Read).is_some());
        // Both are now Some since the group was initialized; Write is just zero
        assert!(H.load(Op::Write).is_some());
    }

    #[test]
    fn metadata_initialized_on_metric_value_call() {
        static H: DimensionedHistogram<Op> = DimensionedHistogram::new(7, 64);
        let _ = Metric::value(&H);

        let meta = H.group.load_metadata(0).unwrap();
        assert_eq!(meta.get("op").unwrap(), "read");
        let meta = H.group.load_metadata(1).unwrap();
        assert_eq!(meta.get("op").unwrap(), "write");
    }

    #[test]
    fn value_returns_histogram_group_variant() {
        static H: DimensionedHistogram<Op> = DimensionedHistogram::new(7, 64);
        assert!(matches!(Metric::value(&H), Some(Value::HistogramGroup(_))));
    }
}
