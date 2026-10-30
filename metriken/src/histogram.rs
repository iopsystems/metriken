use std::sync::OnceLock;

pub use histogram::{Bucket, Config, Error, Histogram};
use metriken_core::Window;
use parking_lot::RwLock;

use crate::{HistogramMetric, Metric, Value};

/// A histogram that uses free-running atomic counters to track the distribution
/// of values. They are only useful for recording values and producing
/// [`crate::Snapshot`]s of the histogram state which can then be used for
/// reporting.
///
/// The `AtomicCounterHistogram` should be preferred when individual events are being
/// recorded. The `RwLockCounterHistogram` should be preferred when bulk-updating the
/// histogram from pre-aggregated data with a compatible layout.
pub struct AtomicCounterHistogram {
    inner: OnceLock<histogram::AtomicHistogram>,
    config: Config,
}

impl AtomicCounterHistogram {
    /// Create a new [`::histogram::AtomicHistogram`] with the given parameters.
    ///
    /// # Panics
    /// This will panic if the `grouping_power` and `max_value_power` do not
    /// adhere to the following constraints:
    ///
    /// - `max_value_power` must be in the range 1..=64
    /// - `grouping_power` must be in the range `0..=(max_value_power - 1)`
    pub const fn new(grouping_power: u8, max_value_power: u8) -> Self {
        let config = match ::histogram::Config::new(grouping_power, max_value_power) {
            Ok(c) => c,
            Err(_) => panic!("invalid histogram config"),
        };

        Self {
            inner: OnceLock::new(),
            config,
        }
    }

    /// Increments the bucket for a corresponding value.
    #[inline]
    pub fn increment(&self, value: u64) -> Result<(), Error> {
        self.get_or_init().increment(value)
    }

    pub fn config(&self) -> Config {
        self.config
    }

    /// Loads and returns the histogram. Returns `None` if the histogram has
    /// never been incremented.
    pub fn load(&self) -> Option<Histogram> {
        self.inner.get().map(|h| h.load())
    }

    #[inline]
    fn get_or_init(&self) -> &::histogram::AtomicHistogram {
        self.inner
            .get_or_init(|| ::histogram::AtomicHistogram::with_config(&self.config))
    }
}

impl HistogramMetric for AtomicCounterHistogram {
    fn config(&self) -> Config {
        self.config
    }

    fn load(&self) -> Option<Histogram> {
        self.load()
    }
}

impl Metric for AtomicCounterHistogram {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::Histogram(self))
    }
}

/// The former name of [`AtomicCounterHistogram`].
#[deprecated(
    since = "0.11.4",
    note = "renamed `AtomicCounterHistogram`, the counting kind beside `GaugeHistogram`"
)]
pub type AtomicHistogram = AtomicCounterHistogram;

/// The contents guarded by [`RwLockCounterHistogram`]'s single `RwLock`: the bucket
/// histogram and its acquisition window. Colocating them makes the
/// `(buckets, window)` pair atomic for free — a reader under the read guard
/// can never pair buckets from one write with a window from another.
struct HistogramState {
    histogram: histogram::Histogram,
    window: Option<Window>,
}

/// A histogram that uses free-running non-atomic counters to track the
/// distribution of values. They are only useful for bulk recording of values
/// and producing [`crate::Snapshot`]s of the histogram state which can then be
/// used for reporting.
///
/// The `AtomicCounterHistogram` should be preferred when individual events are being
/// recorded. The `RwLockCounterHistogram` should be preferred when bulk-updating the
/// histogram from pre-aggregated data with a compatible layout.
pub struct RwLockCounterHistogram {
    inner: OnceLock<RwLock<HistogramState>>,
    config: Config,
}

impl RwLockCounterHistogram {
    /// Create a new [`::histogram::AtomicHistogram`] with the given parameters.
    ///
    /// # Panics
    /// This will panic if the `grouping_power` and `max_value_power` do not
    /// adhere to the following constraints:
    ///
    /// - `max_value_power` must be in the range 1..=64
    /// - `grouping_power` must be in the range `0..=(max_value_power - 1)`
    pub const fn new(grouping_power: u8, max_value_power: u8) -> Self {
        let config = match ::histogram::Config::new(grouping_power, max_value_power) {
            Ok(c) => c,
            Err(_e) => panic!("invalid histogram config"),
        };

        Self {
            inner: OnceLock::new(),
            config,
        }
    }

    /// Updates the histogram counts from raw data.
    ///
    /// This is a windowless write: it updates the bucket data but does not
    /// touch the stored window. Callers that record acquisition windows must
    /// use [`set_with_window`] exclusively — mixing `update_from` and
    /// `set_with_window` on the same instance will produce stale or absent
    /// windows for otherwise-valid bucket data.
    ///
    /// [`set_with_window`]: RwLockCounterHistogram::set_with_window
    pub fn update_from(&self, data: &[u64]) -> Result<(), Error> {
        if data.len() != self.config.total_buckets() {
            return Err(Error::IncompatibleParameters);
        }

        let mut state = self.get_or_init().write();
        state.histogram.as_mut_slice().copy_from_slice(data);

        Ok(())
    }

    pub fn config(&self) -> Config {
        self.config
    }

    /// Loads and returns the histogram. Returns `None` if the histogram has
    /// never been written to.
    pub fn load(&self) -> Option<Histogram> {
        self.inner.get().map(|h| h.read().histogram.clone())
    }

    /// Write bucket data and its acquisition window as a torn-safe pair.
    ///
    /// Both the buckets and the window are updated under one `RwLock` write
    /// guard, so a concurrent [`load_with_window`] never observes buckets from
    /// one call paired with a window from another.
    ///
    /// # Errors
    /// Returns [`Error::IncompatibleParameters`] if `data.len()` does not equal
    /// [`Config::total_buckets`]; in that case neither the buckets nor the
    /// window are modified.
    ///
    /// # Torn-safety note
    /// Torn-safety is only guaranteed when **all** writes on this instance go
    /// through `set_with_window`. Mixing `set_with_window` with [`update_from`]
    /// (which does not update the window) will produce stale windows for some
    /// snapshots.
    ///
    /// [`load_with_window`]: RwLockCounterHistogram::load_with_window
    /// [`update_from`]: RwLockCounterHistogram::update_from
    pub fn set_with_window(&self, data: &[u64], window: Window) -> Result<(), Error> {
        if data.len() != self.config.total_buckets() {
            return Err(Error::IncompatibleParameters);
        }
        let mut state = self.get_or_init().write();
        state.histogram.as_mut_slice().copy_from_slice(data);
        state.window = Some(window);
        Ok(())
    }

    /// Load the bucket histogram and its acquisition window as a torn-safe pair.
    ///
    /// Both values are read under one `RwLock` read guard, so the returned
    /// `(histogram, window)` pair is always self-consistent provided all writes
    /// went through [`set_with_window`].
    ///
    /// Returns `(None, None)` if the histogram has never been written to.
    ///
    /// [`set_with_window`]: RwLockCounterHistogram::set_with_window
    pub fn load_with_window(&self) -> (Option<Histogram>, Option<Window>) {
        match self.inner.get() {
            Some(lock) => {
                let state = lock.read();
                (Some(state.histogram.clone()), state.window)
            }
            None => (None, None),
        }
    }

    fn get_or_init(&self) -> &RwLock<HistogramState> {
        self.inner.get_or_init(|| {
            RwLock::new(HistogramState {
                histogram: ::histogram::Histogram::with_config(&self.config),
                window: None,
            })
        })
    }
}

impl HistogramMetric for RwLockCounterHistogram {
    fn config(&self) -> Config {
        self.config
    }

    fn load(&self) -> Option<Histogram> {
        self.load()
    }
}

impl Metric for RwLockCounterHistogram {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::Histogram(self))
    }

    fn load_window(&self) -> Option<Window> {
        self.inner.get().and_then(|h| h.read().window)
    }

    fn value_with_window(&self) -> (Option<Value<'_>>, Option<Window>) {
        let window = self.inner.get().and_then(|h| h.read().window);
        (Some(Value::Histogram(self)), window)
    }
}

/// The former name of [`RwLockCounterHistogram`].
#[deprecated(
    since = "0.11.4",
    note = "renamed `RwLockCounterHistogram`, the counting kind beside `GaugeHistogram`"
)]
pub type RwLockHistogram = RwLockCounterHistogram;

/// A histogram whose buckets are the current state of something: how many
/// items of each size exist now (free extents per size, files per size
/// class, tasks per queue length), not how many events have been recorded.
///
/// Every write replaces every bucket, so a bucket can go down between two
/// reads, and there is no `increment`. Exposition sends it in a snapshot's
/// or group's `gauge_histograms` list, storage records it as a column whose
/// `metric_type` is `gauge_histogram`, and readers take each reading as it
/// is instead of differencing consecutive readings. Use
/// [`AtomicCounterHistogram`] or [`RwLockCounterHistogram`] for a distribution of events.
///
/// ```
/// use metriken::{GaugeHistogram, Metric, Value};
///
/// // Grouping power 0: one bucket for each power of two (65 buckets).
/// let free_extents = GaugeHistogram::new(0, 64);
/// let mut buckets = vec![0u64; free_extents.config().total_buckets()];
/// buckets[1] = 120; // 120 extents of size 1
/// buckets[2] = 31; // 31 extents of size 2 or 3
/// free_extents.set(&buckets).unwrap();
///
/// // The next read replaces the state; nothing accumulates.
/// buckets[1] = 100;
/// free_extents.set(&buckets).unwrap();
/// assert_eq!(free_extents.load().unwrap().as_slice()[1], 100);
/// assert!(matches!(free_extents.value(), Some(Value::GaugeHistogram(_))));
/// ```
pub struct GaugeHistogram {
    inner: OnceLock<RwLock<HistogramState>>,
    config: Config,
}

impl GaugeHistogram {
    /// Create a new, unset gauge histogram with the given parameters.
    ///
    /// # Panics
    /// This will panic if the `grouping_power` and `max_value_power` do not
    /// adhere to the following constraints:
    ///
    /// - `max_value_power` must be in the range 1..=64
    /// - `grouping_power` must be in the range `0..=(max_value_power - 1)`
    pub const fn new(grouping_power: u8, max_value_power: u8) -> Self {
        let config = match ::histogram::Config::new(grouping_power, max_value_power) {
            Ok(c) => c,
            Err(_e) => panic!("invalid histogram config"),
        };

        Self {
            inner: OnceLock::new(),
            config,
        }
    }

    pub fn config(&self) -> Config {
        self.config
    }

    /// Replace the state with `buckets`, one count per bucket of
    /// [`config`](Self::config), and clear the acquisition window.
    ///
    /// # Errors
    /// [`Error::IncompatibleParameters`] if `buckets.len()` is not
    /// [`Config::total_buckets`]; the state is then unchanged.
    pub fn set(&self, buckets: &[u64]) -> Result<(), Error> {
        self.write(buckets, None)
    }

    /// Replace the state with `buckets` and record the interval it was read
    /// over, both under one write lock, so [`load_with_window`] never pairs
    /// one call's buckets with another's window.
    ///
    /// # Errors
    /// As [`set`](Self::set); neither the buckets nor the window change.
    ///
    /// [`load_with_window`]: GaugeHistogram::load_with_window
    pub fn set_with_window(&self, buckets: &[u64], window: Window) -> Result<(), Error> {
        self.write(buckets, Some(window))
    }

    fn write(&self, buckets: &[u64], window: Option<Window>) -> Result<(), Error> {
        if buckets.len() != self.config.total_buckets() {
            return Err(Error::IncompatibleParameters);
        }
        let lock = self.inner.get_or_init(|| {
            RwLock::new(HistogramState {
                histogram: ::histogram::Histogram::with_config(&self.config),
                window: None,
            })
        });
        let mut state = lock.write();
        state.histogram.as_mut_slice().copy_from_slice(buckets);
        state.window = window;
        Ok(())
    }

    /// The current state. `None` until the first write.
    pub fn load(&self) -> Option<Histogram> {
        self.inner.get().map(|h| h.read().histogram.clone())
    }

    /// The current state and its acquisition window, read under one lock.
    /// `(None, None)` until the first write.
    pub fn load_with_window(&self) -> (Option<Histogram>, Option<Window>) {
        match self.inner.get() {
            Some(lock) => {
                let state = lock.read();
                (Some(state.histogram.clone()), state.window)
            }
            None => (None, None),
        }
    }
}

impl HistogramMetric for GaugeHistogram {
    fn config(&self) -> Config {
        self.config
    }

    fn load(&self) -> Option<Histogram> {
        self.load()
    }
}

impl Metric for GaugeHistogram {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::GaugeHistogram(self))
    }

    fn load_window(&self) -> Option<Window> {
        self.inner.get().and_then(|h| h.read().window)
    }

    fn value_with_window(&self) -> (Option<Value<'_>>, Option<Window>) {
        let window = self.inner.get().and_then(|h| h.read().window);
        (Some(Value::GaugeHistogram(self)), window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metriken_core::Window;

    #[test]
    fn round_trip() {
        let h = RwLockCounterHistogram::new(7, 64);
        let buckets = vec![0u64; h.config().total_buckets()];
        h.set_with_window(&buckets, Window::new(10, 20)).unwrap();

        let (hist, window) = h.load_with_window();
        assert!(hist.is_some());
        assert_eq!(window, Some(Window::new(10, 20)));
        assert_eq!(
            <RwLockCounterHistogram as Metric>::load_window(&h),
            Some(Window::new(10, 20))
        );
    }

    #[test]
    fn unset_window_is_none() {
        let h = RwLockCounterHistogram::new(7, 64);
        let (hist, window) = h.load_with_window();
        assert!(hist.is_none());
        assert!(window.is_none());
        assert!(<RwLockCounterHistogram as Metric>::load_window(&h).is_none());
    }

    #[test]
    fn wrong_length_is_rejected_and_records_no_window() {
        let h = RwLockCounterHistogram::new(7, 64);
        assert!(h.set_with_window(&[1, 2, 3], Window::new(1, 2)).is_err());
        assert!(<RwLockCounterHistogram as Metric>::load_window(&h).is_none());
    }

    #[test]
    fn update_from_is_windowless() {
        let h = RwLockCounterHistogram::new(7, 64);
        let buckets = vec![0u64; h.config().total_buckets()];
        h.update_from(&buckets).unwrap();
        assert!(h.load().is_some());
        let (_hist, window) = h.load_with_window();
        assert!(window.is_none());
    }

    #[test]
    fn value_with_window_returns_window_with_histogram_ref() {
        let h = RwLockCounterHistogram::new(7, 64);
        let buckets = vec![0u64; h.config().total_buckets()];
        h.set_with_window(&buckets, Window::new(10, 20)).unwrap();
        let (value, window) = <RwLockCounterHistogram as Metric>::value_with_window(&h);
        assert!(matches!(value, Some(Value::Histogram(_))));
        assert_eq!(window, Some(Window::new(10, 20)));
    }

    #[test]
    fn a_gauge_histogram_write_replaces_the_state() {
        let h = GaugeHistogram::new(0, 64);
        assert_eq!(h.config().total_buckets(), 65);
        assert!(h.load().is_none());
        let mut buckets = vec![0u64; 65];
        buckets[3] = 7;
        h.set_with_window(&buckets, Window::new(10, 20)).unwrap();
        buckets[3] = 2;
        buckets[4] = 1;
        h.set(&buckets).unwrap();
        let (hist, window) = h.load_with_window();
        let hist = hist.unwrap();
        assert_eq!(hist.as_slice()[3], 2, "replaced, not added to");
        assert_eq!(hist.as_slice()[4], 1);
        assert_eq!(window, None, "a windowless write clears the window");
        assert!(matches!(
            <GaugeHistogram as Metric>::value(&h),
            Some(Value::GaugeHistogram(_))
        ));
    }

    #[test]
    fn a_gauge_histogram_refuses_the_wrong_length() {
        let h = GaugeHistogram::new(0, 64);
        h.set_with_window(&[1; 65], Window::new(1, 2)).unwrap();
        assert!(h.set_with_window(&[5; 64], Window::new(3, 4)).is_err());
        let (hist, window) = h.load_with_window();
        assert_eq!(hist.unwrap().as_slice()[0], 1);
        assert_eq!(window, Some(Window::new(1, 2)));
    }

    #[test]
    fn a_gauge_histogram_exposes_its_window() {
        let h = GaugeHistogram::new(2, 8);
        let n = h.config().total_buckets();
        h.set_with_window(&vec![0; n], Window::new(5, 9)).unwrap();
        let (value, window) = <GaugeHistogram as Metric>::value_with_window(&h);
        assert!(matches!(value, Some(Value::GaugeHistogram(_))));
        assert_eq!(window, Some(Window::new(5, 9)));
        assert_eq!(
            <GaugeHistogram as Metric>::load_window(&h),
            Some(Window::new(5, 9))
        );
    }

    #[test]
    fn set_with_window_torn_read_stress() {
        use std::sync::Arc;
        use std::thread;

        const ITERS: u64 = 50_000;
        let h = Arc::new(RwLockCounterHistogram::new(4, 16));
        let total = h.config().total_buckets();
        {
            let mut buckets = vec![0u64; total];
            buckets[0] = 0;
            h.set_with_window(&buckets, Window::new(0, 1)).unwrap();
        }

        let writer = {
            let h = h.clone();
            thread::spawn(move || {
                let mut buckets = vec![0u64; total];
                for v in 1..ITERS {
                    buckets[0] = v;
                    h.set_with_window(&buckets, Window::new(v, v + 1)).unwrap();
                }
            })
        };
        let reader = {
            let h = h.clone();
            thread::spawn(move || {
                for _ in 0..ITERS {
                    let (hist, w) = h.load_with_window();
                    if let (Some(hist), Some(w)) = (hist, w) {
                        let v = hist.as_slice()[0];
                        assert_eq!(w.begin_ns, v, "torn read: buckets {v} paired with {w:?}");
                        assert_eq!(w.end_ns, v + 1, "torn read: buckets {v} paired with {w:?}");
                    }
                }
            })
        };
        writer.join().unwrap();
        reader.join().unwrap();
    }
}
