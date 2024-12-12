// Copyright 2021 Twitter, Inc.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

use std::mem::ManuallyDrop;
use std::pin::Pin;

use metriken::*;
use parking_lot::{Mutex, MutexGuard};

// All tests manipulate global state. Need a mutex to ensure test execution
// doesn't overlap.
static TEST_MUTEX: Mutex<()> = parking_lot::const_mutex(());

/// RAII guard that ensures
/// - All dynamic metrics are removed after each test
/// - No two tests run concurrently
struct TestGuard {
    _lock: MutexGuard<'static, ()>,
}

impl TestGuard {
    pub fn new() -> Self {
        Self {
            _lock: TEST_MUTEX.lock(),
        }
    }
}

#[test]
fn wrapped_register_unregister() {
    let _guard = TestGuard::new();

    let metric = MetricBuilder::new("wrapped_register_unregister").build(Counter::new());

    assert_eq!(metrics().dynamic_metrics().len(), 1);
    drop(metric);
    assert_eq!(metrics().dynamic_metrics().len(), 0);
}

#[test]
fn pinned_register_unregister() {
    let _guard = TestGuard::new();

    let (metric, entry) =
        MetricBuilder::new("pinned_register_unregister").build_pinned(Counter::new());

    let mut metric_ = ManuallyDrop::new(metric);
    let metric = unsafe { Pin::new_unchecked(&*metric_) };
    metric.register(entry);

    assert_eq!(metrics().dynamic_metrics().len(), 1);
    unsafe { ManuallyDrop::drop(&mut metric_) };
    assert_eq!(metrics().dynamic_metrics().len(), 0);
}

#[test]
fn pinned_scope() {
    let _guard = TestGuard::new();

    {
        let (metric, entry) = MetricBuilder::new("pinned_scope").build_pinned(Counter::new());

        let metric = unsafe { Pin::new_unchecked(&metric) };
        metric.register(entry);

        assert_eq!(metrics().dynamic_metrics().len(), 1);
    }
    assert_eq!(metrics().dynamic_metrics().len(), 0);
}

#[test]
fn pinned_dup_register() {
    let _guard = TestGuard::new();

    {
        let metric = MetricBuilder::new("pinned_dup")
            .build_pinned(Counter::new())
            .0;
        let metric = unsafe { Pin::new_unchecked(&metric) };
        metric.register(MetricBuilder::new("pinned_dup_1").into_entry());
        metric.register(MetricBuilder::new("pinned_dup_2").into_entry());

        assert_eq!(metrics().dynamic_metrics().len(), 1);
    }
    assert_eq!(metrics().dynamic_metrics().len(), 0);
}

#[test]
fn multi_metric() {
    let _guard = TestGuard::new();

    let m1 = MetricBuilder::new("multi_metric_1").build(Counter::new());
    let m2 = MetricBuilder::new("multi_metric_2").build(Counter::new());

    assert_eq!(metrics().dynamic_metrics().len(), 2);
    drop(m1);
    assert_eq!(metrics().dynamic_metrics().len(), 1);
    drop(m2);
    assert_eq!(metrics().dynamic_metrics().len(), 0);
}

#[test]
fn read_metadata() {
    let _guard = TestGuard::new();

    let _metric = MetricBuilder::new("metric1")
        .metadata("foo", "b")
        .metadata("bar", "c")
        .build(Counter::new());

    let metrics = metrics();
    assert_eq!(metrics.dynamic_metrics().len(), 1);
    let entry = metrics.dynamic_metrics().next().unwrap();

    assert_eq!(entry.name(), "metric1");
    assert_eq!(entry.metadata().get("foo"), Some("b"));
    assert_eq!(entry.metadata().get("bar"), Some("c"));
}
