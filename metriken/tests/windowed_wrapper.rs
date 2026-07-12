use metriken::{metric, Counter, MetricBuilder, Window, WindowedLazyCounter};

#[metric(name = "wrapper_fwd_static")]
static STATIC_WINDOWED: WindowedLazyCounter = WindowedLazyCounter::new(Counter::new);

fn value_window_of(metric_name: &str) -> Option<Option<Window>> {
    metriken::metrics()
        .iter()
        .find(|entry| entry.name() == metric_name)
        .map(|entry| entry.value_with_window().1)
}

fn load_window_of(metric_name: &str) -> Option<Option<Window>> {
    metriken::metrics()
        .iter()
        .find(|entry| entry.name() == metric_name)
        .map(|entry| entry.load_window())
}

#[test]
fn metric_wrapper_forwards_window_for_static_metric() {
    STATIC_WINDOWED.set_with_window(7, Window::new(11, 22));
    assert_eq!(value_window_of("wrapper_fwd_static"), Some(Some(Window::new(11, 22))));
    assert_eq!(load_window_of("wrapper_fwd_static"), Some(Some(Window::new(11, 22))));
}

#[test]
fn provider_metric_forwards_window_for_dynamic_metric() {
    let dynamic =
        MetricBuilder::new("wrapper_fwd_dynamic").build(WindowedLazyCounter::new(Counter::new));
    dynamic.set_with_window(9, Window::new(33, 44));
    assert_eq!(value_window_of("wrapper_fwd_dynamic"), Some(Some(Window::new(33, 44))));
    assert_eq!(load_window_of("wrapper_fwd_dynamic"), Some(Some(Window::new(33, 44))));
    drop(dynamic);
}
