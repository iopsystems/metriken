use std::any::Any;
use std::marker::PhantomData;
use std::sync::OnceLock;

use metriken_core::{Metric, Value};

use crate::group::CounterGroup;
use crate::MetricDimension;

/// A group of counters indexed by a compile-time enum dimension.
///
/// All dimension variants are always exported, even when their value is zero.
/// Metadata (labels) is initialized lazily on the first call to `value()` or
/// any write method.
pub struct DimensionedCounter<D: MetricDimension> {
    pub(crate) group: CounterGroup,
    init: OnceLock<()>,
    _dim: PhantomData<fn() -> D>,
}

impl<D: MetricDimension> DimensionedCounter<D> {
    /// Create a new dimensioned counter group.
    pub const fn new() -> Self {
        Self {
            group: CounterGroup::new(D::COUNT),
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

    /// Increment the counter for `dim` by 1.
    pub fn increment(&self, dim: D) -> bool {
        self.ensure_metadata();
        self.group.increment(dim.index())
    }

    /// Add `value` to the counter for `dim`.
    pub fn add(&self, dim: D, value: u64) -> bool {
        self.ensure_metadata();
        self.group.add(dim.index(), value)
    }

    /// Load the current value for `dim`.
    ///
    /// Returns `None` if no write has occurred yet (the backing store is
    /// uninitialized). This is a pure read — it does not initialize labels;
    /// call any write method or let exposition (`Metric::value()`) trigger
    /// label initialization first.
    pub fn value(&self, dim: D) -> Option<u64> {
        self.group.value(dim.index())
    }
}

impl<D: MetricDimension> Default for DimensionedCounter<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: MetricDimension> Metric for DimensionedCounter<D> {
    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        self.ensure_metadata();
        Some(Value::CounterGroup(&self.group))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    enum Side {
        Left,
        Right,
    }

    impl MetricDimension for Side {
        const COUNT: usize = 2;

        fn index(&self) -> usize {
            match self {
                Side::Left => 0,
                Side::Right => 1,
            }
        }

        fn labels(&self) -> HashMap<String, String> {
            let mut map = HashMap::new();
            let v = match self {
                Side::Left => "left",
                Side::Right => "right",
            };
            map.insert("side".to_string(), v.to_string());
            map
        }

        fn all_labels() -> Vec<HashMap<String, String>> {
            ["left", "right"]
                .into_iter()
                .map(|v| {
                    let mut m = HashMap::new();
                    m.insert("side".to_string(), v.to_string());
                    m
                })
                .collect()
        }
    }

    #[test]
    fn basic_increment_and_value() {
        static C: DimensionedCounter<Side> = DimensionedCounter::new();

        assert_eq!(C.value(Side::Left), None); // backing not yet initialized
        C.increment(Side::Left);
        assert_eq!(C.value(Side::Left), Some(1));
        C.add(Side::Right, 5);
        assert_eq!(C.value(Side::Right), Some(5));
    }

    #[test]
    fn metadata_initialized_on_metric_value_call() {
        static C: DimensionedCounter<Side> = DimensionedCounter::new();

        let _ = Metric::value(&C); // triggers ensure_metadata()

        let left = C.group.load_metadata(0).unwrap();
        assert_eq!(left.get("side").unwrap(), "left");
        let right = C.group.load_metadata(1).unwrap();
        assert_eq!(right.get("side").unwrap(), "right");
    }

    #[test]
    fn value_returns_counter_group_variant() {
        static C: DimensionedCounter<Side> = DimensionedCounter::new();
        assert!(matches!(Metric::value(&C), Some(Value::CounterGroup(_))));
    }

    #[test]
    fn zero_values_visible_after_metadata_init() {
        static C: DimensionedCounter<Side> = DimensionedCounter::new();

        C.increment(Side::Left); // initializes backing store + metadata
                                 // Right was never written but backing is initialized — should be Some(0)
        assert_eq!(C.value(Side::Right), Some(0));
    }
}
