use metriken_core::Window;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Sparse per-index acquisition-window storage for group metrics. Only indices
/// with an explicitly recorded window consume memory. This is a *separate,
/// typed* store — it holds `Window` values, NOT stringly-typed metadata; it only
/// mirrors `GroupMetadata`'s sparse structure. The `HashMap<String, String>`
/// metadata store is untouched.
pub(crate) struct GroupWindows {
    inner: OnceLock<RwLock<HashMap<usize, Window>>>,
}

impl GroupWindows {
    pub(crate) const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    fn get_or_init(&self) -> &RwLock<HashMap<usize, Window>> {
        self.inner.get_or_init(|| RwLock::new(HashMap::new()))
    }

    /// Record the window for `idx`, replacing any existing one.
    pub(crate) fn insert(&self, idx: usize, window: Window) {
        self.get_or_init().write().insert(idx, window);
    }

    /// Run `f` while holding the write guard, initializing the store. Store the
    /// group value being paired with a window inside `f` to make the pair
    /// atomic against a concurrent [`with_read`](GroupWindows::with_read).
    pub(crate) fn with_write<R>(&self, f: impl FnOnce(&mut HashMap<usize, Window>) -> R) -> R {
        let mut guard = self.get_or_init().write();
        f(&mut guard)
    }

    /// Run `f` while holding the read guard if the store has been initialized;
    /// otherwise pass `None` without allocating. Read the paired group value
    /// inside `f` to make the pair atomic.
    pub(crate) fn with_read<R>(&self, f: impl FnOnce(Option<&HashMap<usize, Window>>) -> R) -> R {
        match self.inner.get() {
            Some(m) => {
                let guard = m.read();
                f(Some(&guard))
            }
            None => f(None),
        }
    }

    /// Load the window for `idx`.
    pub(crate) fn load(&self, idx: usize) -> Option<Window> {
        self.inner.get().and_then(|m| m.read().get(&idx).copied())
    }

    /// Snapshot all (index, window) pairs without holding the lock.
    pub(crate) fn snapshot(&self) -> Vec<(usize, Window)> {
        match self.inner.get() {
            Some(m) => m.read().iter().map(|(k, v)| (*k, *v)).collect(),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_read_unset_does_not_allocate() {
        let w = GroupWindows::new();
        // with_read must see None (no map) without initializing the OnceLock.
        let seen = w.with_read(|map| map.map(|m| m.len()));
        assert_eq!(seen, None);
        assert!(
            w.snapshot().is_empty(),
            "with_read must not allocate the map"
        );
    }

    #[test]
    fn with_write_then_with_read_round_trips() {
        let w = GroupWindows::new();
        w.with_write(|map| {
            map.insert(3, Window::new(7, 8));
        });
        let got = w.with_read(|map| map.and_then(|m| m.get(&3).copied()));
        assert_eq!(got, Some(Window::new(7, 8)));
    }

    #[test]
    fn insert_load_snapshot() {
        let w = GroupWindows::new();
        assert!(w.load(0).is_none());
        assert!(w.snapshot().is_empty());

        w.insert(2, Window::new(100, 200));
        assert_eq!(w.load(2), Some(Window::new(100, 200)));
        assert!(w.load(0).is_none());

        let snap = w.snapshot();
        assert_eq!(snap, vec![(2, Window::new(100, 200))]);
    }
}
