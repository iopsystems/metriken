use crate::window_cell::WindowCell;
use crate::{Counter, Gauge, Lazy, LazyCounter, LazyGauge, Metric, Value};
use metriken_core::Window;

/// A [`LazyCounter`] paired with a torn-safe acquisition [`Window`].
///
/// The window lives in a lazily-allocated [`WindowCell`] on this wrapper, so
/// the base [`Counter`] primitive stays lean and unchanged. Producers write
/// the value and its window together via [`set_with_window`] and readers pull
/// the self-consistent pair via [`load_with_window`].
///
/// [`set_with_window`]: WindowedLazyCounter::set_with_window
/// [`load_with_window`]: WindowedLazyCounter::load_with_window
pub struct WindowedLazyCounter {
    inner: LazyCounter,
    window: WindowCell,
}

impl WindowedLazyCounter {
    /// Create a windowed lazy counter whose inner counter is produced by `f`
    /// on first write (mirrors [`LazyCounter::new`]).
    pub const fn new(f: fn() -> Counter) -> Self {
        Self {
            inner: LazyCounter::new(f),
            window: WindowCell::new(),
        }
    }

    /// Set the value and its acquisition window as a torn-safe pair.
    ///
    /// The inner value store and the window insert happen under one
    /// [`WindowCell`] write guard, so a concurrent
    /// [`load_with_window`](WindowedLazyCounter::load_with_window) never
    /// observes a value from one call paired with a window from another.
    ///
    /// Torn-safety is **enforced** by this type: it exposes no lock-free
    /// mutator and does not `Deref` to the inner counter, so a windowless
    /// write is unrepresentable and cannot bypass the window lock.
    pub fn set_with_window(&self, value: u64, window: Window) {
        self.window.with_write(|w| {
            // `set` goes through the inner Lazy's Deref, forcing init.
            self.inner.set(value);
            *w = Some(window);
        });
    }

    /// Load the value and its acquisition window as a torn-safe pair. The
    /// value is `None` while the inner lazy counter has never been written.
    pub fn load_with_window(&self) -> (Option<u64>, Option<Window>) {
        self.window
            .with_read(|w| (Lazy::get(&self.inner).map(|c| c.value()), w))
    }
}

impl Metric for WindowedLazyCounter {
    fn is_enabled(&self) -> bool {
        self.inner.is_enabled()
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }

    fn value(&self) -> Option<Value<'_>> {
        self.inner.value()
    }

    fn load_window(&self) -> Option<Window> {
        self.window.load()
    }

    fn value_with_window(&self) -> (Option<Value<'_>>, Option<Window>) {
        // One WindowCell read-lock acquisition reads both the value (via the
        // inner Lazy's non-forcing Metric::value) and the window, so
        // exposition never pairs a value with a stale window.
        self.window.with_read(|w| (self.inner.value(), w))
    }
}

/// A [`LazyGauge`] paired with a torn-safe acquisition [`Window`].
///
/// Signed mirror of [`WindowedLazyCounter`]; the base [`Gauge`] primitive is
/// left lean and unchanged.
pub struct WindowedLazyGauge {
    inner: LazyGauge,
    window: WindowCell,
}

impl WindowedLazyGauge {
    /// Create a windowed lazy gauge whose inner gauge is produced by `f` on
    /// first write (mirrors [`LazyGauge::new`]).
    pub const fn new(f: fn() -> Gauge) -> Self {
        Self {
            inner: LazyGauge::new(f),
            window: WindowCell::new(),
        }
    }

    /// Set the value and its acquisition window as a torn-safe pair.
    ///
    /// Torn-safety is **enforced** by this type: it exposes no lock-free
    /// mutator and does not `Deref` to the inner gauge, so a windowless write
    /// is unrepresentable and cannot bypass the window lock.
    pub fn set_with_window(&self, value: i64, window: Window) {
        self.window.with_write(|w| {
            self.inner.set(value);
            *w = Some(window);
        });
    }

    /// Load the value and its acquisition window as a torn-safe pair. The
    /// value is `None` while the inner lazy gauge has never been written.
    pub fn load_with_window(&self) -> (Option<i64>, Option<Window>) {
        self.window
            .with_read(|w| (Lazy::get(&self.inner).map(|g| g.value()), w))
    }
}

impl Metric for WindowedLazyGauge {
    fn is_enabled(&self) -> bool {
        self.inner.is_enabled()
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }

    fn value(&self) -> Option<Value<'_>> {
        self.inner.value()
    }

    fn load_window(&self) -> Option<Window> {
        self.window.load()
    }

    fn value_with_window(&self) -> (Option<Value<'_>>, Option<Window>) {
        self.window.with_read(|w| (self.inner.value(), w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Counter, Gauge, Metric, Value};
    use metriken_core::Window;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn round_trip() {
        let c = WindowedLazyCounter::new(Counter::new);
        c.set_with_window(42, Window::new(10, 20));
        assert_eq!(c.load_with_window(), (Some(42), Some(Window::new(10, 20))));
        assert!(matches!(<WindowedLazyCounter as Metric>::value(&c), Some(Value::Counter(42))));
        assert_eq!(<WindowedLazyCounter as Metric>::load_window(&c), Some(Window::new(10, 20)));
    }

    #[test]
    fn unset_window_is_none_and_value_uninitialized() {
        let c = WindowedLazyCounter::new(Counter::new);
        assert_eq!(c.load_with_window(), (None, None));
        assert!(<WindowedLazyCounter as Metric>::load_window(&c).is_none());
    }

    #[test]
    fn value_with_window_pairs_atomically() {
        let c = WindowedLazyCounter::new(Counter::new);
        c.set_with_window(42, Window::new(10, 20));
        let (value, window) = <WindowedLazyCounter as Metric>::value_with_window(&c);
        assert!(matches!(value, Some(Value::Counter(42))));
        assert_eq!(window, Some(Window::new(10, 20)));
    }

    #[test]
    fn base_primitives_unchanged_and_window_cell_pointer_sized() {
        use crate::window_cell::WindowCell;
        use parking_lot::RwLock;
        use std::sync::atomic::{AtomicI64, AtomicU64};
        use std::sync::OnceLock;

        assert_eq!(
            std::mem::size_of::<Counter>(),
            std::mem::size_of::<AtomicU64>(),
            "Counter must stay exactly its AtomicU64 (option A)"
        );
        assert_eq!(
            std::mem::size_of::<Gauge>(),
            std::mem::size_of::<AtomicI64>(),
            "Gauge must stay exactly its AtomicI64 (option A)"
        );
        assert_eq!(
            std::mem::size_of::<WindowCell>(),
            std::mem::size_of::<OnceLock<Box<RwLock<Option<Window>>>>>()
        );

        let c = WindowedLazyCounter::new(Counter::new);
        assert!(<WindowedLazyCounter as Metric>::load_window(&c).is_none());
    }

    #[test]
    fn torn_read_stress() {
        const ITERS: u64 = 200_000;
        let c = Arc::new(WindowedLazyCounter::new(Counter::new));
        c.set_with_window(0, Window::new(0, 1));

        let writer = {
            let c = c.clone();
            thread::spawn(move || {
                for v in 1..ITERS {
                    c.set_with_window(v, Window::new(v, v + 1));
                }
            })
        };
        let reader = {
            let c = c.clone();
            thread::spawn(move || {
                for _ in 0..ITERS {
                    let (v, w) = c.load_with_window();
                    if let (Some(v), Some(w)) = (v, w) {
                        assert_eq!(w.begin_ns, v, "torn read: value {v} paired with {w:?}");
                        assert_eq!(w.end_ns, v + 1, "torn read: value {v} paired with {w:?}");
                    }
                }
            })
        };
        writer.join().unwrap();
        reader.join().unwrap();
    }

    #[test]
    fn gauge_round_trip() {
        let g = WindowedLazyGauge::new(Gauge::new);
        g.set_with_window(-7, Window::new(100, 250));
        assert_eq!(g.load_with_window(), (Some(-7), Some(Window::new(100, 250))));
        assert_eq!(<WindowedLazyGauge as Metric>::load_window(&g), Some(Window::new(100, 250)));
    }

    #[test]
    fn gauge_unset_window_is_none() {
        let g = WindowedLazyGauge::new(Gauge::new);
        assert_eq!(g.load_with_window(), (None, None));
        assert!(<WindowedLazyGauge as Metric>::load_window(&g).is_none());
    }

    #[test]
    fn gauge_value_with_window_pairs_atomically() {
        let g = WindowedLazyGauge::new(Gauge::new);
        g.set_with_window(-7, Window::new(100, 250));
        let (value, window) = <WindowedLazyGauge as Metric>::value_with_window(&g);
        assert!(matches!(value, Some(Value::Gauge(-7))));
        assert_eq!(window, Some(Window::new(100, 250)));
    }
}
