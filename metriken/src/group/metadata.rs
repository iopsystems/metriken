use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Sparse metadata storage for group metrics.
///
/// Only allocates metadata for indices that have been explicitly set.
/// Suitable for both small dense groups (per-CPU) and large sparse groups
/// (per-cgroup, per-task) since the overhead for small N is negligible.
pub(crate) struct GroupMetadata {
    inner: OnceLock<RwLock<HashMap<usize, HashMap<String, String>>>>,
}

impl GroupMetadata {
    pub(crate) const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    fn get_or_init(&self) -> &RwLock<HashMap<usize, HashMap<String, String>>> {
        self.inner.get_or_init(|| RwLock::new(HashMap::new()))
    }

    /// Set metadata for a given index. Replaces any existing metadata.
    pub(crate) fn insert(&self, idx: usize, metadata: HashMap<String, String>) {
        self.get_or_init().write().insert(idx, metadata);
    }

    /// Set a single key-value pair for a given index.
    pub(crate) fn insert_kv(&self, idx: usize, key: String, value: String) {
        self.get_or_init()
            .write()
            .entry(idx)
            .or_default()
            .insert(key, value);
    }

    /// Load metadata for a given index.
    pub(crate) fn load(&self, idx: usize) -> Option<HashMap<String, String>> {
        self.inner.get().and_then(|m| m.read().get(&idx).cloned())
    }

    /// Run `f` with a borrowed view of the metadata for `idx`, without
    /// cloning.
    ///
    /// The read lock is held for the duration of `f` — callers must not
    /// block, await, or re-enter this group's methods inside the closure.
    pub(crate) fn with<R>(
        &self,
        idx: usize,
        f: impl FnOnce(Option<&HashMap<String, String>>) -> R,
    ) -> R {
        match self.inner.get() {
            Some(m) => {
                let guard = m.read();
                f(guard.get(&idx))
            }
            None => f(None),
        }
    }

    /// Remove metadata for a given index.
    pub(crate) fn remove(&self, idx: usize) {
        if let Some(m) = self.inner.get() {
            m.write().remove(&idx);
        }
    }

    /// Iterate over all (index, metadata) pairs.
    ///
    /// Takes a snapshot of the metadata to avoid holding the lock during
    /// iteration.
    pub(crate) fn snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
        match self.inner.get() {
            Some(m) => m.read().iter().map(|(k, v)| (*k, v.clone())).collect(),
            None => Vec::new(),
        }
    }

    /// Call `f` with each (index, metadata) pair, under one read guard,
    /// without cloning. Order is unspecified.
    ///
    /// The read lock is held for the duration of the iteration — callers
    /// must not block, await, or re-enter this group's methods inside `f`.
    /// If the store has never been initialized, `f` is never called (no
    /// allocation).
    pub(crate) fn for_each(&self, f: &mut dyn FnMut(usize, &HashMap<String, String>)) {
        if let Some(m) = self.inner.get() {
            let guard = m.read();
            for (idx, map) in guard.iter() {
                f(*idx, map);
            }
        }
    }
}
