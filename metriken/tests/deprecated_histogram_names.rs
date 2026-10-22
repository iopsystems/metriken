//! The counting histograms' former names still declare working metrics: they
//! are deprecated aliases of `AtomicCounterHistogram`,
//! `RwLockCounterHistogram` and `CounterHistogramGroup`.
#![allow(deprecated)]

use metriken::{metric, AtomicHistogram, HistogramGroup, Metric, RwLockHistogram, Value};

#[metric(name = "deprecated_atomic")]
static ATOMIC: AtomicHistogram = AtomicHistogram::new(4, 16);

#[metric(name = "deprecated_rwlock")]
static RWLOCK: RwLockHistogram = RwLockHistogram::new(4, 16);

#[metric(name = "deprecated_group")]
static GROUP: HistogramGroup = HistogramGroup::new(2, 4, 16);

#[test]
fn the_former_names_are_the_counting_types() {
    let _: &metriken::AtomicCounterHistogram = &ATOMIC;
    let _: &metriken::RwLockCounterHistogram = &RWLOCK;
    let _: &metriken::CounterHistogramGroup = &GROUP;
    ATOMIC.increment(3).unwrap();
    assert_eq!(
        ATOMIC
            .load()
            .unwrap()
            .iter()
            .map(|b| b.count())
            .sum::<u64>(),
        1
    );
    assert!(matches!(ATOMIC.value(), Some(Value::Histogram(_))));
    assert!(matches!(RWLOCK.value(), Some(Value::Histogram(_))));
    assert!(matches!(GROUP.value(), Some(Value::HistogramGroup(_))));
}
