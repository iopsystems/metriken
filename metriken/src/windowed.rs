use crate::window_cell::WindowCell;
use crate::{
    Counter, CounterGroup, Gauge, GaugeGroup, Lazy, LazyCounter, LazyGauge, Metric, Value,
};
use metriken_core::Window;
use std::collections::HashMap;

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

/// A [`CounterGroup`] restricted to torn-safe windowed access.
///
/// Holds an inner base [`CounterGroup`] and exposes **only** the windowed
/// writer/reader ([`set_with_window`]/[`load_with_window`]), the lock-free
/// read accessors, and metadata — **not** the base group's lock-free mutators
/// (`set`/`add`/`increment`). Because no lock-free mutator is reachable, a
/// windowless write is unrepresentable and torn-safety is enforced by the
/// type. The window store and the atomic `(value, window)` pairing live on the
/// inner base group (Phase-1 `GroupWindows` lock); this wrapper only narrows
/// the API.
///
/// [`set_with_window`]: WindowedCounterGroup::set_with_window
/// [`load_with_window`]: WindowedCounterGroup::load_with_window
pub struct WindowedCounterGroup {
    inner: CounterGroup,
}

impl WindowedCounterGroup {
    /// Create a windowed counter group with the given number of entries.
    pub const fn new(entries: usize) -> Self {
        Self {
            inner: CounterGroup::new(entries),
        }
    }

    /// Return the number of entries in this group.
    pub fn entries(&self) -> usize {
        self.inner.entries()
    }

    /// Set the counter at `idx` to `value` and record its acquisition window
    /// as a torn-safe pair. Returns `false` if `idx` is out of bounds.
    pub fn set_with_window(&self, idx: usize, value: u64, window: Window) -> bool {
        self.inner.set_with_window(idx, value, window)
    }

    /// Load the counter at `idx` and its acquisition window as a torn-safe
    /// pair. Returns `(None, None)` if `idx` is out of bounds.
    pub fn load_with_window(&self, idx: usize) -> (Option<u64>, Option<Window>) {
        self.inner.load_with_window(idx)
    }

    /// Load the current value of the counter at `idx`.
    pub fn value(&self, idx: usize) -> Option<u64> {
        self.inner.value(idx)
    }

    /// Set metadata for the entry at `idx`.
    pub fn set_metadata(&self, idx: usize, metadata: HashMap<String, String>) {
        self.inner.set_metadata(idx, metadata)
    }

    /// Set a single metadata key-value pair for the entry at `idx`.
    pub fn insert_metadata(&self, idx: usize, key: String, value: String) {
        self.inner.insert_metadata(idx, key, value)
    }

    /// Load metadata for the entry at `idx`.
    pub fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
        self.inner.load_metadata(idx)
    }

    /// Snapshot all metadata.
    pub fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
        self.inner.metadata_snapshot()
    }

    /// Remove metadata for the entry at `idx`.
    pub fn clear_metadata(&self, idx: usize) {
        self.inner.clear_metadata(idx)
    }
}

impl Metric for WindowedCounterGroup {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        // Expose the inner base group, whose `CounterGroupMetric` impl provides
        // the atomic `load_with_window(idx)` the exposition snapshotter calls.
        Some(Value::CounterGroup(&self.inner))
    }
}

/// A [`GaugeGroup`] restricted to torn-safe windowed access.
///
/// Signed mirror of [`WindowedCounterGroup`]: exposes **only** the windowed
/// writer/reader, the read accessors, and metadata — **not** the base group's
/// lock-free mutators (`set`/`add`/`sub`/`increment`/`decrement`). The window
/// store and the atomic pairing live on the inner base [`GaugeGroup`].
pub struct WindowedGaugeGroup {
    inner: GaugeGroup,
}

impl WindowedGaugeGroup {
    /// Create a windowed gauge group with the given number of entries.
    pub const fn new(entries: usize) -> Self {
        Self {
            inner: GaugeGroup::new(entries),
        }
    }

    /// Return the number of entries in this group.
    pub fn entries(&self) -> usize {
        self.inner.entries()
    }

    /// Set the gauge at `idx` to `value` and record its acquisition window as
    /// a torn-safe pair. Returns `false` if `idx` is out of bounds.
    pub fn set_with_window(&self, idx: usize, value: i64, window: Window) -> bool {
        self.inner.set_with_window(idx, value, window)
    }

    /// Load the gauge at `idx` and its acquisition window as a torn-safe pair.
    /// Returns `(None, None)` if `idx` is out of bounds or unset.
    pub fn load_with_window(&self, idx: usize) -> (Option<i64>, Option<Window>) {
        self.inner.load_with_window(idx)
    }

    /// Load the current value of the gauge at `idx`.
    pub fn value(&self, idx: usize) -> Option<i64> {
        self.inner.value(idx)
    }

    /// Set metadata for the entry at `idx`.
    pub fn set_metadata(&self, idx: usize, metadata: HashMap<String, String>) {
        self.inner.set_metadata(idx, metadata)
    }

    /// Set a single metadata key-value pair for the entry at `idx`.
    pub fn insert_metadata(&self, idx: usize, key: String, value: String) {
        self.inner.insert_metadata(idx, key, value)
    }

    /// Load metadata for the entry at `idx`.
    pub fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
        self.inner.load_metadata(idx)
    }

    /// Snapshot all metadata.
    pub fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
        self.inner.metadata_snapshot()
    }

    /// Remove metadata for the entry at `idx`.
    pub fn clear_metadata(&self, idx: usize) {
        self.inner.clear_metadata(idx)
    }
}

impl Metric for WindowedGaugeGroup {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::GaugeGroup(&self.inner))
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
    fn counter_group_round_trip() {
        use metriken_core::Window;

        let g = WindowedCounterGroup::new(4);
        assert!(g.set_with_window(1, 55, Window::new(10, 20)));
        assert_eq!(g.load_with_window(1), (Some(55), Some(Window::new(10, 20))));
        assert_eq!(g.value(1), Some(55));
        assert_eq!(g.entries(), 4);
        assert!(!g.set_with_window(9, 1, Window::new(1, 2)));
        assert_eq!(g.load_with_window(9), (None, None));

        if let Some(Value::CounterGroup(inner)) = <WindowedCounterGroup as Metric>::value(&g) {
            assert_eq!(
                inner.load_with_window(1),
                (Some(55), Some(Window::new(10, 20)))
            );
        } else {
            panic!("expected Value::CounterGroup");
        }
    }

    #[test]
    fn counter_group_metadata_round_trips() {
        let g = WindowedCounterGroup::new(2);
        g.insert_metadata(0, "cpu".into(), "0".into());
        assert_eq!(g.load_metadata(0).unwrap().get("cpu").unwrap(), "0");
        assert_eq!(g.metadata_snapshot().len(), 1);
    }

    #[test]
    fn counter_group_torn_read_stress() {
        use metriken_core::Window;
        use std::sync::Arc;
        use std::thread;

        const ITERS: u64 = 200_000;
        let g = Arc::new(WindowedCounterGroup::new(1));
        g.set_with_window(0, 0, Window::new(0, 1));

        let writer = {
            let g = g.clone();
            thread::spawn(move || {
                for v in 1..ITERS {
                    g.set_with_window(0, v, Window::new(v, v + 1));
                }
            })
        };
        let reader = {
            let g = g.clone();
            thread::spawn(move || {
                for _ in 0..ITERS {
                    let (v, w) = g.load_with_window(0);
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
    fn round_trip() {
        let c = WindowedLazyCounter::new(Counter::new);
        c.set_with_window(42, Window::new(10, 20));
        assert_eq!(c.load_with_window(), (Some(42), Some(Window::new(10, 20))));
        assert!(matches!(
            <WindowedLazyCounter as Metric>::value(&c),
            Some(Value::Counter(42))
        ));
        assert_eq!(
            <WindowedLazyCounter as Metric>::load_window(&c),
            Some(Window::new(10, 20))
        );
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
        assert_eq!(
            g.load_with_window(),
            (Some(-7), Some(Window::new(100, 250)))
        );
        assert_eq!(
            <WindowedLazyGauge as Metric>::load_window(&g),
            Some(Window::new(100, 250))
        );
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

    #[test]
    fn gauge_group_round_trip() {
        use metriken_core::Window;

        let g = WindowedGaugeGroup::new(4);
        assert!(g.set_with_window(1, -12, Window::new(10, 20)));
        assert_eq!(
            g.load_with_window(1),
            (Some(-12), Some(Window::new(10, 20)))
        );
        assert_eq!(g.value(1), Some(-12));
        assert_eq!(g.entries(), 4);
        assert!(!g.set_with_window(9, 1, Window::new(1, 2)));
        assert_eq!(g.load_with_window(9), (None, None));

        if let Some(Value::GaugeGroup(inner)) = <WindowedGaugeGroup as Metric>::value(&g) {
            assert_eq!(
                inner.load_with_window(1),
                (Some(-12), Some(Window::new(10, 20)))
            );
        } else {
            panic!("expected Value::GaugeGroup");
        }
    }

    #[test]
    fn gauge_group_metadata_round_trips() {
        let g = WindowedGaugeGroup::new(2);
        g.insert_metadata(0, "cpu".into(), "0".into());
        assert_eq!(g.load_metadata(0).unwrap().get("cpu").unwrap(), "0");
        assert_eq!(g.metadata_snapshot().len(), 1);
    }
}
