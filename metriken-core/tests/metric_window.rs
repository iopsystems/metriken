use metriken_core::{Metric, Value, Window};

struct Dummy;

impl Metric for Dummy {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
    fn value(&self) -> Option<Value<'_>> {
        Some(Value::Counter(0))
    }
}

#[test]
fn default_load_window_is_none() {
    assert!(Dummy.load_window().is_none());
}

struct Windowed;

impl Metric for Windowed {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
    fn value(&self) -> Option<Value<'_>> {
        Some(Value::Counter(0))
    }
    fn load_window(&self) -> Option<Window> {
        Some(Window::new(5, 9))
    }
}

#[test]
fn override_load_window_is_returned() {
    assert_eq!(Windowed.load_window(), Some(Window::new(5, 9)));
}

#[test]
fn default_value_with_window_pairs_value_and_none() {
    // The default atomic accessor pairs `value()` with `None` (no window).
    let (value, window) = Dummy.value_with_window();
    assert!(matches!(value, Some(Value::Counter(0))));
    assert!(window.is_none());
}
