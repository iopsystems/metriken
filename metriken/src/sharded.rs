use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

const CACHE_LINE: usize = 64;
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

/// Get the shard index for the current thread.
///
/// Uses the explicitly set shard ID if available (via [`set_thread_shard`]),
/// otherwise falls back to a hash of a thread-local address.
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
struct Shard {
    value: AtomicU64,
    _pad: [u8; CACHE_LINE - 8],
}

/// A sharded counter for high-throughput workloads.
///
/// Each thread writes to its own cache-line-aligned shard, avoiding
/// contention. Reading sums across all shards.
///
/// Use [`set_thread_shard`] at thread startup for deterministic shard
/// assignment. Without it, a hash of a thread-local address is used.
///
/// # Example
/// ```
/// use metriken::{metric, ShardedCounter};
///
/// #[metric(name = "requests")]
/// static REQUESTS: ShardedCounter = ShardedCounter::new();
///
/// REQUESTS.increment();
/// assert!(REQUESTS.value() >= 1);
/// ```
pub struct ShardedCounter {
    shards: [Shard; NUM_SHARDS],
}

unsafe impl Send for ShardedCounter {}
unsafe impl Sync for ShardedCounter {}

impl ShardedCounter {
    /// Create a new sharded counter initialized to zero.
    #[allow(clippy::declare_interior_mutable_const)]
    pub const fn new() -> Self {
        const ZERO: Shard = Shard {
            value: AtomicU64::new(0),
            _pad: [0; CACHE_LINE - 8],
        };
        Self {
            shards: [ZERO; NUM_SHARDS],
        }
    }

    /// Increment the counter by 1.
    #[inline]
    pub fn increment(&self) {
        self.add(1);
    }

    /// Add a value to the counter.
    #[inline]
    pub fn add(&self, value: u64) {
        let shard = shard_index();
        self.shards[shard].value.fetch_add(value, Ordering::Relaxed);
    }

    /// Get the current value (aggregated across all shards).
    pub fn value(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.value.load(Ordering::Relaxed))
            .sum()
    }
}

impl Default for ShardedCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::Metric for ShardedCounter {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<crate::Value<'_>> {
        Some(crate::Value::Counter(self.value()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        static COUNTER: ShardedCounter = ShardedCounter::new();
        assert_eq!(COUNTER.value(), 0);
        COUNTER.increment();
        assert_eq!(COUNTER.value(), 1);
        COUNTER.add(10);
        assert_eq!(COUNTER.value(), 11);
    }

    #[test]
    fn thread_distribution() {
        use std::sync::Arc;
        use std::thread;

        static COUNTER: ShardedCounter = ShardedCounter::new();
        let counter = Arc::new(&COUNTER);
        let iterations: u64 = 1000;
        let num_threads: u64 = 4;

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                let c = Arc::clone(&counter);
                thread::spawn(move || {
                    set_thread_shard(i as usize);
                    for _ in 0..iterations {
                        c.increment();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(COUNTER.value(), iterations * num_threads);
    }

    #[test]
    fn metriken_trait() {
        use crate::Metric;

        static COUNTER: ShardedCounter = ShardedCounter::new();
        COUNTER.add(42);

        let value = Metric::value(&COUNTER);
        assert!(matches!(value, Some(crate::Value::Counter(v)) if v >= 42));
    }
}
