use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use crate::group::metadata::GroupMetadata;
use crate::{CounterGroupMetric, Metric, Value};

const CACHE_LINE: usize = 64;
const SLOTS_PER_LINE: usize = CACHE_LINE / 8;
const NUM_SHARDS: usize = 64;

thread_local! {
    static SHARD_ID: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Set the shard ID for the current thread.
///
/// Call this at the start of each worker thread to ensure deterministic
/// shard assignment and avoid false sharing between workers.
pub fn set_thread_shard(id: usize) {
    SHARD_ID.set(Some(id % NUM_SHARDS));
}

#[inline]
fn shard_index() -> usize {
    SHARD_ID.get().unwrap_or_else(|| {
        thread_local! {
            static ID: u8 = const { 0 };
        }
        ID.with(|x| x as *const u8 as usize) % NUM_SHARDS
    })
}

#[repr(C, align(64))]
struct CacheLine {
    slots: [AtomicU64; SLOTS_PER_LINE],
}

/// A group of sharded counters for high-throughput workloads.
///
/// Each thread writes to its own cache-line-aligned shard, avoiding
/// contention. Related counters are packed into the same cache line(s)
/// within a shard for spatial locality. Reading sums a given slot across
/// all shards.
///
/// Use [`set_thread_shard`] at thread startup for deterministic shard
/// assignment. Without it, a hash of a thread-local address is used.
///
/// # Example
/// ```
/// use metriken::{metric, ShardedCounterGroup};
///
/// #[metric(name = "requests")]
/// static REQUESTS: ShardedCounterGroup = ShardedCounterGroup::new(3);
///
/// // 0 = reads, 1 = writes, 2 = deletes
/// REQUESTS.increment(0);
/// REQUESTS.add(1, 5);
///
/// assert_eq!(REQUESTS.value(0), Some(1));
/// assert_eq!(REQUESTS.value(1), Some(5));
/// ```
pub struct ShardedCounterGroup {
    values: OnceLock<Vec<CacheLine>>,
    metadata: GroupMetadata,
    entries: usize,
    lines_per_shard: usize,
}

unsafe impl Send for ShardedCounterGroup {}
unsafe impl Sync for ShardedCounterGroup {}

impl ShardedCounterGroup {
    /// Create a new sharded counter group with the given number of entries.
    pub const fn new(entries: usize) -> Self {
        let lines_per_shard = entries.div_ceil(SLOTS_PER_LINE);
        Self {
            values: OnceLock::new(),
            metadata: GroupMetadata::new(),
            entries,
            lines_per_shard,
        }
    }

    /// Return the number of entries in this group.
    pub fn entries(&self) -> usize {
        self.entries
    }

    fn get_or_init(&self) -> &[CacheLine] {
        self.values.get_or_init(|| {
            let total_lines = NUM_SHARDS * self.lines_per_shard;
            let mut v = Vec::with_capacity(total_lines);
            for _ in 0..total_lines {
                #[allow(clippy::declare_interior_mutable_const)]
                const ZERO: AtomicU64 = AtomicU64::new(0);
                v.push(CacheLine {
                    slots: [ZERO; SLOTS_PER_LINE],
                });
            }
            v
        })
    }

    #[inline]
    fn slot(&self, shard: usize, idx: usize) -> &AtomicU64 {
        let line = shard * self.lines_per_shard + idx / SLOTS_PER_LINE;
        let slot = idx % SLOTS_PER_LINE;
        &self.get_or_init()[line].slots[slot]
    }

    /// Increment the counter at `idx` by 1.
    ///
    /// Returns `false` if `idx` is out of bounds.
    #[inline]
    pub fn increment(&self, idx: usize) -> bool {
        self.add(idx, 1)
    }

    /// Add `value` to the counter at `idx`.
    ///
    /// Returns `false` if `idx` is out of bounds.
    #[inline]
    pub fn add(&self, idx: usize, value: u64) -> bool {
        if idx >= self.entries {
            return false;
        }
        self.slot(shard_index(), idx)
            .fetch_add(value, Ordering::Relaxed);
        true
    }

    /// Load the current value of the counter at `idx` (summed across shards).
    ///
    /// Returns `None` if `idx` is out of bounds or values haven't been
    /// initialized.
    pub fn value(&self, idx: usize) -> Option<u64> {
        if idx >= self.entries {
            return None;
        }
        let lines = self.values.get()?;
        let line_in_shard = idx / SLOTS_PER_LINE;
        let slot_in_line = idx % SLOTS_PER_LINE;
        let mut sum: u64 = 0;
        for shard in 0..NUM_SHARDS {
            let line = shard * self.lines_per_shard + line_in_shard;
            sum += lines[line].slots[slot_in_line].load(Ordering::Relaxed);
        }
        Some(sum)
    }

    /// Load all counter values as a snapshot (each summed across shards).
    ///
    /// Returns `None` if the group hasn't been initialized yet.
    pub fn load(&self) -> Option<Vec<u64>> {
        self.values.get()?;
        let mut result = Vec::with_capacity(self.entries);
        for idx in 0..self.entries {
            result.push(self.value(idx).unwrap_or(0));
        }
        Some(result)
    }

    /// Set metadata for the entry at `idx`.
    pub fn set_metadata(&self, idx: usize, metadata: HashMap<String, String>) {
        if idx < self.entries {
            self.metadata.insert(idx, metadata);
        }
    }

    /// Set a single metadata key-value pair for the entry at `idx`.
    pub fn insert_metadata(&self, idx: usize, key: String, value: String) {
        if idx < self.entries {
            self.metadata.insert_kv(idx, key, value);
        }
    }

    /// Load metadata for the entry at `idx`.
    pub fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
        self.metadata.load(idx)
    }

    /// Remove metadata for the entry at `idx`.
    pub fn clear_metadata(&self, idx: usize) {
        self.metadata.remove(idx);
    }

    /// Snapshot all metadata.
    pub fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
        self.metadata.snapshot()
    }
}

impl CounterGroupMetric for ShardedCounterGroup {
    fn entries(&self) -> usize {
        self.entries
    }

    fn counter_value(&self, idx: usize) -> Option<u64> {
        self.value(idx)
    }

    fn load_counters(&self) -> Option<Vec<u64>> {
        self.load()
    }

    fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
        self.metadata.load(idx)
    }

    fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
        self.metadata.snapshot()
    }
}

impl Metric for ShardedCounterGroup {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::CounterGroup(self))
    }
}

impl Default for ShardedCounterGroup {
    fn default() -> Self {
        Self::new(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        static GROUP: ShardedCounterGroup = ShardedCounterGroup::new(3);
        assert_eq!(GROUP.value(0), None);
        GROUP.increment(0);
        assert_eq!(GROUP.value(0), Some(1));
        GROUP.add(1, 10);
        assert_eq!(GROUP.value(1), Some(10));
        assert_eq!(GROUP.value(2), Some(0));

        // out of bounds
        assert!(!GROUP.increment(3));
        assert_eq!(GROUP.value(3), None);
    }

    #[test]
    fn thread_distribution() {
        use std::sync::Arc;
        use std::thread;

        static GROUP: ShardedCounterGroup = ShardedCounterGroup::new(2);
        let group = Arc::new(&GROUP);
        let iterations: u64 = 1000;
        let num_threads: u64 = 4;

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                let g = Arc::clone(&group);
                thread::spawn(move || {
                    set_thread_shard(i as usize);
                    for _ in 0..iterations {
                        g.increment(0);
                        g.add(1, 2);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(GROUP.value(0), Some(iterations * num_threads));
        assert_eq!(GROUP.value(1), Some(iterations * num_threads * 2));
    }

    #[test]
    fn packing() {
        // 8 counters should fit in one cache line per shard
        static GROUP: ShardedCounterGroup = ShardedCounterGroup::new(8);
        assert_eq!(GROUP.lines_per_shard, 1);

        // 9 counters needs two cache lines per shard
        static GROUP2: ShardedCounterGroup = ShardedCounterGroup::new(9);
        assert_eq!(GROUP2.lines_per_shard, 2);
    }

    #[test]
    fn load_snapshot() {
        static GROUP: ShardedCounterGroup = ShardedCounterGroup::new(3);
        GROUP.increment(0);
        GROUP.add(1, 5);
        GROUP.add(2, 10);

        let snap = GROUP.load().unwrap();
        assert_eq!(snap, vec![1, 5, 10]);
    }

    #[test]
    fn metadata() {
        static GROUP: ShardedCounterGroup = ShardedCounterGroup::new(4);
        GROUP.insert_metadata(0, "op".into(), "read".into());
        let meta = GROUP.load_metadata(0).unwrap();
        assert_eq!(meta.get("op").unwrap(), "read");
        assert!(GROUP.load_metadata(1).is_none());
    }

    #[test]
    fn metriken_trait() {
        use crate::Metric;

        static GROUP: ShardedCounterGroup = ShardedCounterGroup::new(2);
        GROUP.increment(0);

        let value = Metric::value(&GROUP);
        assert!(matches!(value, Some(Value::CounterGroup(_))));
    }
}
