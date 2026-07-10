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
