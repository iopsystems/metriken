use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Sparse metadata storage for group metrics.
///
/// Only allocates metadata for indices that have been explicitly set.
/// Suitable for both small dense groups (per-CPU) and large sparse groups
/// (per-cgroup, per-task) since the overhead for small N is negligible.
///
/// Carries a [`version`](Self::version) that changes on every mutation, so a
/// reader that caches something derived from the metadata can ask "has any
/// of it changed?" in O(1) instead of re-reading every entry. It is kept
/// here, on the store itself, because every path that mutates metadata
/// goes through this type; a counter kept by a caller would miss any write
/// that did not go through that caller.
pub(crate) struct GroupMetadata {
    inner: OnceLock<RwLock<HashMap<usize, HashMap<String, String>>>>,
    /// Bumped, under the write lock, by every mutation. Never reset.
    version: AtomicU64,
}

impl GroupMetadata {
    pub(crate) const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
            version: AtomicU64::new(0),
        }
    }

    /// A value that differs from every earlier value whenever any entry's
    /// metadata has been set, added to or removed since.
    ///
    /// Read it BEFORE reading the metadata it is meant to validate. A
    /// mutation that lands between the two then shows as a changed version
    /// on the next read and costs one spurious re-read; the other order
    /// could pair a new version with old data and be believed indefinitely.
    pub(crate) fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    fn get_or_init(&self) -> &RwLock<HashMap<usize, HashMap<String, String>>> {
        self.inner.get_or_init(|| RwLock::new(HashMap::new()))
    }

    /// Set metadata for a given index. Replaces any existing metadata.
    pub(crate) fn insert(&self, idx: usize, metadata: HashMap<String, String>) {
        let mut store = self.get_or_init().write();
        store.insert(idx, metadata);
        self.version.fetch_add(1, Ordering::Release);
    }

    /// Set a single key-value pair for a given index.
    pub(crate) fn insert_kv(&self, idx: usize, key: String, value: String) {
        let mut store = self.get_or_init().write();
        store.entry(idx).or_default().insert(key, value);
        self.version.fetch_add(1, Ordering::Release);
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
            let mut store = m.write();
            if store.remove(&idx).is_some() {
                self.version.fetch_add(1, Ordering::Release);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mutation moves the version; no read does. A reader validating a
    /// cache by the version relies on both halves: a mutation that did not
    /// move it would be served stale, and a read that moved it would
    /// invalidate on every tick.
    #[test]
    fn the_version_moves_on_every_mutation_and_no_read() {
        let m = GroupMetadata::new();
        let v0 = m.version();

        m.insert(
            3,
            HashMap::from([("comm".to_string(), "redis".to_string())]),
        );
        let v1 = m.version();
        assert_ne!(v1, v0, "set");

        m.insert_kv(3, "pid".to_string(), "4112".to_string());
        let v2 = m.version();
        assert_ne!(v2, v1, "insert of one key");

        // Replacing with identical content is still a mutation: the store
        // does not compare, and a reader that needs "unchanged" to mean
        // "identical" gets a spurious re-read rather than a missed change.
        m.insert(
            3,
            HashMap::from([
                ("comm".to_string(), "redis".to_string()),
                ("pid".to_string(), "4112".to_string()),
            ]),
        );
        let v3 = m.version();
        assert_ne!(v3, v2, "set to identical content");

        let _ = m.load(3);
        m.with(3, |_| ());
        m.for_each(&mut |_, _| ());
        let _ = m.snapshot();
        assert_eq!(m.version(), v3, "reads do not move it");

        m.remove(3);
        let v4 = m.version();
        assert_ne!(v4, v3, "clear");
        m.remove(3);
        assert_eq!(m.version(), v4, "clearing an absent index changed nothing");
        m.remove(99);
        assert_eq!(m.version(), v4);
    }
}
