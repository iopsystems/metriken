use metriken_core::Window;
use parking_lot::RwLock;
use std::sync::OnceLock;

/// Lazily-allocated, torn-safe cell holding a scalar metric's acquisition
/// window. Null until the first windowed write, so a metric that never sets a
/// window pays only a pointer-sized `OnceLock` and never allocates.
///
/// The `RwLock` is stored behind a `Box`, so the cell adds only an
/// `OnceLock<Box<_>>` (pointer-sized state) to the windowed wrapper that
/// embeds it instead of storing the `RwLock` inline; the box is allocated on
/// the first `with_write` (first `set_with_window`).
///
/// The cell is the single serialization point for windowed scalar access: a
/// windowed writer runs under [`WindowCell::with_write`] and a windowed reader
/// runs under [`WindowCell::with_read`], so the (value, window) pair a reader
/// observes is always self-consistent.
#[derive(Default, Debug)]
pub(crate) struct WindowCell {
    inner: OnceLock<Box<RwLock<Option<Window>>>>,
}

impl WindowCell {
    pub(crate) const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    fn get_or_init(&self) -> &RwLock<Option<Window>> {
        // Deref-coerce `&Box<RwLock<_>>` to `&RwLock<_>` on return.
        self.inner.get_or_init(|| Box::new(RwLock::new(None)))
    }

    /// Run `f` while holding the write guard, initializing the cell. Store the
    /// value being paired with the window inside `f` to make the pair atomic.
    pub(crate) fn with_write<R>(&self, f: impl FnOnce(&mut Option<Window>) -> R) -> R {
        let mut guard = self.get_or_init().write();
        f(&mut guard)
    }

    /// Run `f` while holding the read guard if the cell has been initialized;
    /// otherwise pass `None` without allocating. Read the paired value inside
    /// `f` to make the pair atomic.
    pub(crate) fn with_read<R>(&self, f: impl FnOnce(Option<Window>) -> R) -> R {
        match self.inner.get() {
            Some(lock) => {
                let guard = lock.read();
                f(*guard)
            }
            None => f(None),
        }
    }

    /// Load the recorded window without allocating.
    pub(crate) fn load(&self) -> Option<Window> {
        self.inner.get().and_then(|lock| *lock.read())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_does_not_allocate_and_reads_none() {
        let cell = WindowCell::new();
        assert!(cell.load().is_none());
        // with_read must pass None without initializing the OnceLock.
        assert_eq!(cell.with_read(|w| w), None);
        assert!(cell.inner.get().is_none(), "with_read must not allocate");
    }

    #[test]
    fn write_then_read_round_trips() {
        let cell = WindowCell::new();
        cell.with_write(|w| *w = Some(Window::new(10, 20)));
        assert_eq!(cell.load(), Some(Window::new(10, 20)));
        assert_eq!(cell.with_read(|w| w), Some(Window::new(10, 20)));
    }
}
