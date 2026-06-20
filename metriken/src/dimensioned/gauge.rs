use std::any::Any;
use std::marker::PhantomData;
use std::sync::OnceLock;

use metriken_core::{Metric, Value};

use crate::group::GaugeGroup;
use crate::MetricDimension;

/// A group of gauges indexed by a compile-time enum dimension.
///
/// All dimension variants are always exported. Metadata is initialized lazily
/// on first `Metric::value()` or write call.
pub struct DimensionedGauge<D: MetricDimension> {
    pub(crate) group: GaugeGroup,
    init: OnceLock<()>,
    _dim: PhantomData<fn() -> D>,
}

impl<D: MetricDimension> DimensionedGauge<D> {
    pub const fn new() -> Self {
        Self {
            group: GaugeGroup::new(D::COUNT),
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

    /// Set the gauge for `dim` to `value`.
    pub fn set(&self, dim: D, value: i64) -> bool {
        self.ensure_metadata();
        self.group.set(dim.index(), value)
    }

    /// Add `value` to the gauge for `dim`.
    pub fn add(&self, dim: D, value: i64) -> bool {
        self.ensure_metadata();
        self.group.add(dim.index(), value)
    }

    /// Subtract `value` from the gauge for `dim`.
    pub fn sub(&self, dim: D, value: i64) -> bool {
        self.ensure_metadata();
        self.group.sub(dim.index(), value)
    }

    /// Increment the gauge for `dim` by 1.
    pub fn increment(&self, dim: D) -> bool {
        self.ensure_metadata();
        self.group.increment(dim.index())
    }

    /// Decrement the gauge for `dim` by 1.
    pub fn decrement(&self, dim: D) -> bool {
        self.ensure_metadata();
        self.group.decrement(dim.index())
    }

    /// Load the current value for `dim`.
    ///
    /// Returns `None` if no write has occurred yet (the backing store is
    /// uninitialized). This is a pure read — it does not initialize labels.
    pub fn value(&self, dim: D) -> Option<i64> {
        self.group.value(dim.index())
    }
}

impl<D: MetricDimension> Default for DimensionedGauge<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: MetricDimension> Metric for DimensionedGauge<D> {
    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        self.ensure_metadata();
        Some(Value::GaugeGroup(&self.group))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    enum Direction {
        Up,
        Down,
    }

    impl MetricDimension for Direction {
        const COUNT: usize = 2;

        fn index(&self) -> usize {
            match self {
                Direction::Up => 0,
                Direction::Down => 1,
            }
        }

        fn labels(&self) -> HashMap<String, String> {
            let mut map = HashMap::new();
            let v = match self {
                Direction::Up => "up",
                Direction::Down => "down",
            };
            map.insert("direction".to_string(), v.to_string());
            map
        }

        fn all_labels() -> Vec<HashMap<String, String>> {
            ["up", "down"]
                .into_iter()
                .map(|v| {
                    let mut m = HashMap::new();
                    m.insert("direction".to_string(), v.to_string());
                    m
                })
                .collect()
        }
    }

    #[test]
    fn basic_set_and_value() {
        static G: DimensionedGauge<Direction> = DimensionedGauge::new();

        G.set(Direction::Up, 100);
        assert_eq!(G.value(Direction::Up), Some(100));
        G.set(Direction::Down, -50);
        assert_eq!(G.value(Direction::Down), Some(-50));
    }

    #[test]
    fn add_sub_increment_decrement() {
        static G: DimensionedGauge<Direction> = DimensionedGauge::new();

        G.set(Direction::Up, 10);
        G.add(Direction::Up, 5);
        assert_eq!(G.value(Direction::Up), Some(15));
        G.sub(Direction::Up, 3);
        assert_eq!(G.value(Direction::Up), Some(12));
        G.increment(Direction::Up);
        assert_eq!(G.value(Direction::Up), Some(13));
        G.decrement(Direction::Up);
        assert_eq!(G.value(Direction::Up), Some(12));
    }

    #[test]
    fn metadata_initialized_on_metric_value_call() {
        static G: DimensionedGauge<Direction> = DimensionedGauge::new();
        let _ = Metric::value(&G);

        let meta = G.group.load_metadata(0).unwrap();
        assert_eq!(meta.get("direction").unwrap(), "up");
        let meta = G.group.load_metadata(1).unwrap();
        assert_eq!(meta.get("direction").unwrap(), "down");
    }

    #[test]
    fn value_returns_gauge_group_variant() {
        static G: DimensionedGauge<Direction> = DimensionedGauge::new();
        assert!(matches!(Metric::value(&G), Some(Value::GaugeGroup(_))));
    }
}
