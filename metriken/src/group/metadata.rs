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
}
