use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use histogram::{Config, Error, Histogram};

use crate::group::metadata::GroupMetadata;
use crate::{CounterGroupMetric, HistogramGroupMetric, HistogramMetric, Metric, Value};

/// The line size shards are padded to, so that two threads' shards never share
/// a cache line or a pair of lines one prefetch pulls together. The values are
/// crossbeam-utils' `CachePadded` alignments: 128 on x86_64 (Intel's L2
/// spatial prefetcher, since Sandy Bridge, fetches 64-byte lines in pairs),
/// aarch64 (128-byte lines on Apple silicon and some big.LITTLE big cores)
/// and powerpc64; 32, 16 or 256 where those architectures' lines are that
/// size; 64 elsewhere. It must
/// match the `repr(align)` on [`CacheLine`], which is asserted below.
const CACHE_LINE: usize = if cfg!(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "arm64ec",
    target_arch = "powerpc64",
)) {
    128
} else if cfg!(any(
    target_arch = "arm",
    target_arch = "mips",
    target_arch = "mips32r6",
    target_arch = "mips64",
    target_arch = "mips64r6",
    target_arch = "sparc",
    target_arch = "hexagon",
)) {
    32
} else if cfg!(target_arch = "m68k") {
    16
} else if cfg!(target_arch = "s390x") {
    256
} else {
    64
};
const SLOTS_PER_LINE: usize = CACHE_LINE / 8;
/// Number of shards. Ids passed to [`set_thread_shard`] are taken modulo this
/// count.
const NUM_SHARDS: usize = 256;

thread_local! {
    static SHARD_ID: Cell<Option<usize>> = const { Cell::new(None) };
    /// Holds a shard taken from [`SHARD_POOL`]; dropping it, at thread exit
    /// or on [`set_thread_shard`], returns the shard to the pool.
    static SHARD_LEASE: RefCell<Option<ShardLease>> = const { RefCell::new(None) };
}

/// Shards for threads that record without calling [`set_thread_shard`],
/// shared by every sharded metric in the process.
///
/// Each shard is at any time unused, leased to one live thread, or in `free`.
/// A lease is returned when its thread exits, and returned shards are handed
/// out before unused ones, so the shards in use follow the peak number of
/// such threads alive at once, not the number ever started. Unused shards are
/// numbered down from `NUM_SHARDS - 1`, away from the low ids callers pass to
/// [`set_thread_shard`]. A thread that starts while all are leased shares a
/// shard without a lease and keeps it until it exits, so it can share with a
/// thread that later leases that shard. A thread-local destructor that
/// records after its thread's lease was returned writes to a shard another
/// thread may hold by then. Either case costs contention, not counts.
struct ShardPool {
    free: Vec<usize>,
    /// Shards handed out for the first time, at most `NUM_SHARDS`.
    fresh: usize,
    /// Threads given a shared shard because every shard was leased.
    overflow: usize,
}

impl ShardPool {
    const fn new() -> Self {
        Self {
            free: Vec::new(),
            fresh: 0,
            overflow: 0,
        }
    }

    /// A shard for a new thread, and whether the thread leases it (and must
    /// [`give`](Self::give) it back) or shares it.
    fn take(&mut self) -> (usize, bool) {
        if let Some(id) = self.free.pop() {
            (id, true)
        } else if self.fresh < NUM_SHARDS {
            self.fresh += 1;
            (NUM_SHARDS - self.fresh, true)
        } else {
            self.overflow += 1;
            (NUM_SHARDS - 1 - self.overflow % NUM_SHARDS, false)
        }
    }

    fn give(&mut self, id: usize) {
        self.free.push(id);
    }
}

static SHARD_POOL: Mutex<ShardPool> = Mutex::new(ShardPool::new());

struct ShardLease(usize);

impl Drop for ShardLease {
    fn drop(&mut self) {
        let mut pool = SHARD_POOL.lock().unwrap_or_else(|e| e.into_inner());
        pool.give(self.0);
    }
}

/// Set the shard ID for the current thread.
///
/// Call this at the start of each worker thread, with a distinct id per
/// thread, for a deterministic shard assignment. Ids are taken modulo 256.
///
/// A thread that records without calling it takes a shard on its first
/// write, from a pool that gives each live thread its own shard while no
/// more than 256 hold one, and reuses the shards of exited threads. Pooled
/// shards are numbered down from 255, so ids passed here should be low to
/// stay clear of them.
pub fn set_thread_shard(id: usize) {
    SHARD_ID.set(Some(id % NUM_SHARDS));
    // Return a shard this thread took from the pool, if any.
    let _ = SHARD_LEASE.try_with(|lease| lease.borrow_mut().take());
}

#[inline]
fn shard_index() -> usize {
    SHARD_ID.get().unwrap_or_else(assign_thread_shard)
}

/// Take a shard from [`SHARD_POOL`] for the calling thread and remember it.
#[cold]
fn assign_thread_shard() -> usize {
    let (id, leased) = SHARD_POOL.lock().unwrap_or_else(|e| e.into_inner()).take();
    SHARD_ID.set(Some(id));
    if leased {
        // If this thread's thread-locals are being destroyed the lease cannot
        // be stored, and the shard is not returned to the pool.
        let _ = SHARD_LEASE.try_with(|lease| *lease.borrow_mut() = Some(ShardLease(id)));
    }
    id
}

#[cfg_attr(
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "arm64ec",
        target_arch = "powerpc64",
    ),
    repr(C, align(128))
)]
#[cfg_attr(
    any(
        target_arch = "arm",
        target_arch = "mips",
        target_arch = "mips32r6",
        target_arch = "mips64",
        target_arch = "mips64r6",
        target_arch = "sparc",
        target_arch = "hexagon",
    ),
    repr(C, align(32))
)]
#[cfg_attr(target_arch = "m68k", repr(C, align(16)))]
#[cfg_attr(target_arch = "s390x", repr(C, align(256)))]
#[cfg_attr(
    not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "arm64ec",
        target_arch = "powerpc64",
        target_arch = "arm",
        target_arch = "mips",
        target_arch = "mips32r6",
        target_arch = "mips64",
        target_arch = "mips64r6",
        target_arch = "sparc",
        target_arch = "hexagon",
        target_arch = "m68k",
        target_arch = "s390x",
    )),
    repr(C, align(64))
)]
struct CacheLine {
    slots: [AtomicU64; SLOTS_PER_LINE],
}

const _: () = assert!(
    std::mem::align_of::<CacheLine>() == CACHE_LINE
        && std::mem::size_of::<CacheLine>() == CACHE_LINE
);

/// A group of sharded counters for high-throughput workloads.
///
/// Each thread writes to its own cache-line-aligned shard, avoiding
/// contention. Related counters are packed into the same cache line(s)
/// within a shard for spatial locality. Reading sums a given slot across
/// all shards.
///
/// Use [`set_thread_shard`] at thread startup for deterministic shard
/// assignment. Without it, a thread takes a shard from a pool on its first
/// write; live threads get distinct shards while no more than 256 hold one,
/// and exited threads' shards are reused.
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

    #[inline]
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

    /// A value that changes whenever any entry's metadata is set, added to
    /// or removed. See `CounterGroupMetric::metadata_version` for the
    /// contract and the read ordering it needs.
    pub fn metadata_version(&self) -> u64 {
        self.metadata.version()
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

    fn metadata_version(&self) -> u64 {
        ShardedCounterGroup::metadata_version(self)
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

/// Sum `histograms`, which share one config, bucket by bucket.
fn merge(histograms: impl Iterator<Item = Histogram>) -> Option<Histogram> {
    let mut histograms = histograms;
    let mut sum = histograms.next()?;
    for h in histograms {
        for (total, count) in sum.as_mut_slice().iter_mut().zip(h.as_slice()) {
            *total = total.wrapping_add(*count);
        }
    }
    Some(sum)
}

/// Whether `value` falls in a bucket of `config`. Checked before a shard is
/// allocated, so a rejected value allocates nothing.
#[inline]
fn in_range(config: &Config, value: u64) -> bool {
    config.max_value_power() == 64 || value < (1 << config.max_value_power())
}

/// A histogram sharded per thread for high-throughput workloads.
///
/// A single [`AtomicCounterHistogram`](crate::AtomicCounterHistogram) is one
/// bucket array that every recording thread writes, and a latency
/// distribution usually concentrates in a few buckets, so threads contend on
/// the same cache lines. Here each thread records into its own shard, and
/// reading merges the shards. Shards are assigned as for
/// [`ShardedCounterGroup`]: by [`set_thread_shard`], or from a pool that gives
/// each live thread its own shard while no more than 256 hold one, for a
/// thread that does not call it.
///
/// The value itself holds 256 empty shard slots, about 14 KiB. A shard's
/// buckets, `total_buckets * 8` bytes (58 KiB at grouping power 7 and max
/// value power 64), are allocated on the first in-range write to that shard
/// and kept. The shards a value allocates are bounded by the distinct ids
/// passed to [`set_thread_shard`] by threads that record into it, plus the
/// peak number of threads alive at once that recorded into any sharded
/// metric without calling it (the pool of shards for those threads is shared
/// by all sharded metrics).
///
/// # Example
/// ```
/// use metriken::{metric, ShardedCounterHistogram};
///
/// #[metric(name = "latency")]
/// static LATENCY: ShardedCounterHistogram = ShardedCounterHistogram::new(7, 64);
///
/// let _ = LATENCY.increment(1200);
///
/// let h = LATENCY.load().unwrap();
/// assert_eq!(h.as_slice().iter().sum::<u64>(), 1);
/// ```
pub struct ShardedCounterHistogram {
    shards: [OnceLock<histogram::AtomicHistogram>; NUM_SHARDS],
    config: Config,
}

impl ShardedCounterHistogram {
    /// Create a new sharded histogram.
    ///
    /// # Panics
    /// Panics if the histogram configuration is invalid. See
    /// [`histogram::Config::new`] for constraints.
    pub const fn new(grouping_power: u8, max_value_power: u8) -> Self {
        let config = match Config::new(grouping_power, max_value_power) {
            Ok(c) => c,
            Err(_) => panic!("invalid histogram config"),
        };

        #[allow(clippy::declare_interior_mutable_const)]
        const EMPTY: OnceLock<histogram::AtomicHistogram> = OnceLock::new();

        Self {
            shards: [EMPTY; NUM_SHARDS],
            config,
        }
    }

    /// Return the histogram configuration.
    pub fn config(&self) -> Config {
        self.config
    }

    /// Increment the bucket containing `value` in the calling thread's shard.
    #[inline]
    pub fn increment(&self, value: u64) -> Result<(), Error> {
        self.add(value, 1)
    }

    /// Add `count` to the bucket containing `value` in the calling thread's
    /// shard.
    #[inline]
    pub fn add(&self, value: u64, count: u64) -> Result<(), Error> {
        if !in_range(&self.config, value) {
            return Err(Error::OutOfRange);
        }
        self.shards[shard_index()]
            .get_or_init(|| histogram::AtomicHistogram::with_config(&self.config))
            .add(value, count)
    }

    /// Load the histogram (summed across shards).
    ///
    /// Returns `None` if no shard has been written to.
    pub fn load(&self) -> Option<Histogram> {
        merge(self.shards.iter().filter_map(|s| s.get()).map(|h| h.load()))
    }
}

impl HistogramMetric for ShardedCounterHistogram {
    fn config(&self) -> Config {
        self.config
    }

    fn load(&self) -> Option<Histogram> {
        self.load()
    }
}

impl Metric for ShardedCounterHistogram {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::Histogram(self))
    }
}

type HistogramShard = OnceLock<Box<[histogram::AtomicHistogram]>>;

/// A group of sharded histograms for high-throughput workloads.
///
/// Each thread records into its own shard, which holds one histogram per
/// entry. Reading sums a given entry across all shards. Shards are assigned as
/// for [`ShardedCounterGroup`]: by [`set_thread_shard`], or from a pool that
/// gives each live thread its own shard while no more than 256 hold one, for
/// a thread that does not call it.
///
/// The value itself holds 256 empty shard slots, about 6 KiB. A shard's
/// histograms, `entries * total_buckets * 8` bytes (58 KiB per entry at
/// grouping power 7 and max value power 64), are allocated on the first
/// in-range write to that shard and kept. The shards it allocates are
/// bounded as for [`ShardedCounterHistogram`].
///
/// # Example
/// ```
/// use metriken::{metric, ShardedCounterHistogramGroup};
///
/// #[metric(name = "latency")]
/// static LATENCY: ShardedCounterHistogramGroup = ShardedCounterHistogramGroup::new(2, 7, 64);
///
/// // 0 = reads, 1 = writes
/// let _ = LATENCY.increment(0, 1200);
/// let _ = LATENCY.increment(1, 500);
///
/// let reads = LATENCY.load(0).unwrap();
/// assert_eq!(reads.as_slice().iter().sum::<u64>(), 1);
/// ```
pub struct ShardedCounterHistogramGroup {
    shards: [HistogramShard; NUM_SHARDS],
    metadata: GroupMetadata,
    config: Config,
    entries: usize,
}

impl ShardedCounterHistogramGroup {
    /// Create a new sharded histogram group.
    ///
    /// All histograms share the same `grouping_power` and `max_value_power`
    /// configuration.
    ///
    /// # Panics
    /// Panics if the histogram configuration is invalid. See
    /// [`histogram::Config::new`] for constraints.
    pub const fn new(entries: usize, grouping_power: u8, max_value_power: u8) -> Self {
        let config = match Config::new(grouping_power, max_value_power) {
            Ok(c) => c,
            Err(_) => panic!("invalid histogram config"),
        };

        #[allow(clippy::declare_interior_mutable_const)]
        const EMPTY: HistogramShard = OnceLock::new();

        Self {
            shards: [EMPTY; NUM_SHARDS],
            metadata: GroupMetadata::new(),
            config,
            entries,
        }
    }

    /// Return the number of entries in this group.
    pub fn entries(&self) -> usize {
        self.entries
    }

    /// Return the histogram configuration shared by all entries.
    pub fn config(&self) -> Config {
        self.config
    }

    #[inline]
    fn shard(&self, shard: usize) -> &[histogram::AtomicHistogram] {
        self.shards[shard].get_or_init(|| {
            let mut v = Vec::with_capacity(self.entries);
            for _ in 0..self.entries {
                v.push(histogram::AtomicHistogram::with_config(&self.config));
            }
            v.into_boxed_slice()
        })
    }

    /// Record a value in the histogram at `idx`.
    ///
    /// Returns `Err` if the value is outside the histogram's range.
    /// Returns `Ok(false)` if `idx` is out of bounds.
    #[inline]
    pub fn increment(&self, idx: usize, value: u64) -> Result<bool, Error> {
        self.add(idx, value, 1)
    }

    /// Add `count` to the bucket containing `value` in the histogram at `idx`.
    ///
    /// Returns `Err` if the value is outside the histogram's range.
    /// Returns `Ok(false)` if `idx` is out of bounds.
    #[inline]
    pub fn add(&self, idx: usize, value: u64, count: u64) -> Result<bool, Error> {
        if idx >= self.entries {
            return Ok(false);
        }
        if !in_range(&self.config, value) {
            return Err(Error::OutOfRange);
        }
        self.shard(shard_index())[idx].add(value, count)?;
        Ok(true)
    }

    /// Load the histogram at `idx` (summed across shards).
    ///
    /// Returns `None` if `idx` is out of bounds or no shard has been written
    /// to.
    pub fn load(&self, idx: usize) -> Option<Histogram> {
        if idx >= self.entries {
            return None;
        }
        merge(
            self.shards
                .iter()
                .filter_map(|s| s.get())
                .map(|shard| shard[idx].load()),
        )
    }

    /// Load all histograms (each summed across shards).
    ///
    /// Returns `None` if no shard has been written to.
    pub fn load_all(&self) -> Option<Vec<Histogram>> {
        if self.shards.iter().all(|s| s.get().is_none()) {
            return None;
        }
        (0..self.entries).map(|idx| self.load(idx)).collect()
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

    /// A value that changes whenever any entry's metadata is set, added to
    /// or removed. See `CounterGroupMetric::metadata_version` for the
    /// contract and the read ordering it needs.
    pub fn metadata_version(&self) -> u64 {
        self.metadata.version()
    }

    /// Run `f` with a borrowed view of the metadata for the entry at `idx`,
    /// without cloning the underlying map.
    ///
    /// The group's metadata read lock is held for the duration of `f` —
    /// callers must not block, await, or re-enter this group's methods
    /// (e.g. `set_metadata`, `load_metadata`) inside the closure.
    pub fn with_metadata<R>(
        &self,
        idx: usize,
        f: impl FnOnce(Option<&HashMap<String, String>>) -> R,
    ) -> R {
        self.metadata.with(idx, f)
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

impl HistogramGroupMetric for ShardedCounterHistogramGroup {
    fn entries(&self) -> usize {
        self.entries
    }

    fn config(&self) -> Config {
        self.config
    }

    fn load_histogram(&self, idx: usize) -> Option<Histogram> {
        self.load(idx)
    }

    fn load_all_histograms(&self) -> Option<Vec<Histogram>> {
        self.load_all()
    }

    fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
        self.metadata.load(idx)
    }

    fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
        self.metadata.snapshot()
    }

    fn metadata_version(&self) -> u64 {
        ShardedCounterHistogramGroup::metadata_version(self)
    }

    fn with_metadata(&self, idx: usize, f: &mut dyn FnMut(Option<&HashMap<String, String>>)) {
        self.metadata.with(idx, f);
    }

    fn for_each_metadata(&self, f: &mut dyn FnMut(usize, &HashMap<String, String>)) {
        self.metadata.for_each(f);
    }
}

impl Metric for ShardedCounterHistogramGroup {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::HistogramGroup(self))
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
        // A full line of counters fits in one cache line per shard
        static GROUP: ShardedCounterGroup = ShardedCounterGroup::new(SLOTS_PER_LINE);
        assert_eq!(GROUP.lines_per_shard, 1);

        // One more needs two cache lines per shard
        static GROUP2: ShardedCounterGroup = ShardedCounterGroup::new(SLOTS_PER_LINE + 1);
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

    #[test]
    fn histogram_basic() {
        static GROUP: ShardedCounterHistogramGroup = ShardedCounterHistogramGroup::new(3, 7, 64);
        assert!(GROUP.load(0).is_none());
        assert!(GROUP.load_all().is_none());

        assert!(GROUP.increment(0, 100).unwrap());
        assert!(GROUP.add(1, 200, 5).unwrap());

        let h0 = GROUP.load(0).unwrap();
        assert_eq!(h0.as_slice().iter().sum::<u64>(), 1);
        let h1 = GROUP.load(1).unwrap();
        assert_eq!(h1.as_slice().iter().sum::<u64>(), 5);
        let h2 = GROUP.load(2).unwrap();
        assert_eq!(h2.as_slice().iter().sum::<u64>(), 0);
        assert_eq!(GROUP.load_all().unwrap().len(), 3);

        // out of bounds
        assert!(!GROUP.increment(3, 100).unwrap());
        assert!(GROUP.load(3).is_none());

        // out of range value
        static SMALL: ShardedCounterHistogramGroup = ShardedCounterHistogramGroup::new(1, 2, 4);
        assert!(SMALL.increment(0, 1 << 10).is_err());
    }

    #[test]
    fn histogram_thread_distribution() {
        use std::thread;

        static GROUP: ShardedCounterHistogramGroup = ShardedCounterHistogramGroup::new(2, 7, 64);
        let iterations: u64 = 1000;
        let num_threads: u64 = 4;

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                thread::spawn(move || {
                    set_thread_shard(i as usize);
                    for v in 0..iterations {
                        GROUP.increment(0, v).unwrap();
                        GROUP.add(1, 1000, 2).unwrap();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Each thread used its own shard.
        let written = GROUP.shards.iter().filter(|s| s.get().is_some()).count();
        assert_eq!(written, num_threads as usize);

        // The merged histogram equals a single histogram fed the same values.
        let mut expected = Histogram::with_config(&GROUP.config());
        for _ in 0..num_threads {
            for v in 0..iterations {
                expected.increment(v).unwrap();
            }
        }
        assert_eq!(GROUP.load(0).unwrap().as_slice(), expected.as_slice());

        let h1 = GROUP.load(1).unwrap();
        assert_eq!(
            h1.as_slice().iter().sum::<u64>(),
            iterations * num_threads * 2
        );
    }

    #[test]
    fn histogram_metadata() {
        static GROUP: ShardedCounterHistogramGroup = ShardedCounterHistogramGroup::new(4, 7, 64);
        GROUP.insert_metadata(0, "op".into(), "read".into());
        let meta = GROUP.load_metadata(0).unwrap();
        assert_eq!(meta.get("op").unwrap(), "read");
        assert!(GROUP.load_metadata(1).is_none());
    }

    #[test]
    fn histogram_metriken_trait() {
        use crate::Metric;

        static GROUP: ShardedCounterHistogramGroup = ShardedCounterHistogramGroup::new(2, 7, 64);
        let _ = GROUP.increment(0, 1);

        let value = Metric::value(&GROUP);
        assert!(matches!(value, Some(Value::HistogramGroup(_))));
        assert_eq!(
            HistogramGroupMetric::config(&GROUP),
            Config::new(7, 64).unwrap()
        );
    }

    #[test]
    fn single_histogram_basic() {
        static H: ShardedCounterHistogram = ShardedCounterHistogram::new(7, 64);
        assert!(H.load().is_none());
        H.increment(100).unwrap();
        H.add(200, 4).unwrap();
        assert_eq!(H.load().unwrap().as_slice().iter().sum::<u64>(), 5);

        static SMALL: ShardedCounterHistogram = ShardedCounterHistogram::new(2, 4);
        assert!(SMALL.increment(1 << 10).is_err());
    }

    #[test]
    fn single_histogram_thread_distribution() {
        use std::thread;

        static H: ShardedCounterHistogram = ShardedCounterHistogram::new(7, 64);
        let iterations: u64 = 1000;
        let num_threads: u64 = 4;

        let handles: Vec<_> = (0..num_threads)
            .map(|i| {
                thread::spawn(move || {
                    set_thread_shard(i as usize);
                    for v in 0..iterations {
                        H.increment(v).unwrap();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let written = H.shards.iter().filter(|s| s.get().is_some()).count();
        assert_eq!(written, num_threads as usize);

        let mut expected = Histogram::with_config(&H.config());
        for _ in 0..num_threads {
            for v in 0..iterations {
                expected.increment(v).unwrap();
            }
        }
        assert_eq!(H.load().unwrap().as_slice(), expected.as_slice());
    }

    #[test]
    fn single_histogram_metriken_trait() {
        use crate::Metric;

        static H: ShardedCounterHistogram = ShardedCounterHistogram::new(7, 64);
        let _ = H.increment(1);
        assert!(matches!(Metric::value(&H), Some(Value::Histogram(_))));
    }

    #[test]
    fn set_thread_shard_wraps_at_num_shards() {
        static H: ShardedCounterHistogram = ShardedCounterHistogram::new(7, 64);
        std::thread::spawn(|| {
            set_thread_shard(NUM_SHARDS + 200);
            H.increment(1).unwrap();
        })
        .join()
        .unwrap();
        assert!(H.shards[200].get().is_some());
    }

    #[test]
    fn out_of_range_value_allocates_nothing() {
        static H: ShardedCounterHistogram = ShardedCounterHistogram::new(2, 4);
        assert!(H.increment(1 << 10).is_err());
        assert!(H.load().is_none());

        static G: ShardedCounterHistogramGroup = ShardedCounterHistogramGroup::new(2, 2, 4);
        assert!(G.increment(0, 1 << 10).is_err());
        assert!(G.load(0).is_none());
        assert!(G.load_all().is_none());
    }

    #[test]
    fn empty_group_loads_none() {
        static G: ShardedCounterHistogramGroup = ShardedCounterHistogramGroup::new(0, 7, 64);
        assert!(G.load_all().is_none());
    }

    #[test]
    fn threads_without_set_thread_shard_get_distinct_shards() {
        // Distinct as long as fewer than NUM_SHARDS threads in this test
        // binary hold a pooled shard at once.
        static H: ShardedCounterHistogram = ShardedCounterHistogram::new(7, 64);
        let threads = 8;
        // Every thread stays alive until all have recorded, so none can
        // return its shard to the pool for another to reuse.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    H.increment(1).unwrap();
                    barrier.wait();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let written = H.shards.iter().filter(|s| s.get().is_some()).count();
        assert_eq!(written, threads);
    }

    #[test]
    fn exited_threads_shards_are_reused() {
        static H: ShardedCounterHistogram = ShardedCounterHistogram::new(7, 64);
        let threads = NUM_SHARDS + 44;
        for _ in 0..threads {
            std::thread::spawn(|| H.increment(1).unwrap())
                .join()
                .unwrap();
        }
        assert_eq!(
            H.load().unwrap().as_slice().iter().sum::<u64>(),
            threads as u64
        );
        // One thread at a time reuses its predecessor's shard. Other tests
        // running in parallel can take or return pooled shards in between,
        // so allow a few, far below one shard per thread started.
        let written = H.shards.iter().filter(|s| s.get().is_some()).count();
        assert!(
            written <= 16,
            "{written} shards for {threads} sequential threads"
        );
    }

    #[test]
    fn set_thread_shard_returns_a_pooled_shard() {
        std::thread::spawn(|| {
            static H: ShardedCounterHistogram = ShardedCounterHistogram::new(7, 64);
            H.increment(1).unwrap();
            assert!(SHARD_LEASE.with(|lease| lease.borrow().is_some()));
            set_thread_shard(5);
            assert!(SHARD_LEASE.with(|lease| lease.borrow().is_none()));
            assert_eq!(shard_index(), 5);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn pool_never_leases_one_shard_twice() {
        let mut pool = ShardPool::new();
        let leased: Vec<usize> = (0..NUM_SHARDS)
            .map(|_| {
                let (id, leased) = pool.take();
                assert!(leased);
                id
            })
            .collect();
        let mut distinct = leased.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), NUM_SHARDS);
        assert_eq!(leased[0], NUM_SHARDS - 1);

        // Every shard is leased: the next thread shares one and holds no lease.
        let (_, leased_overflow) = pool.take();
        assert!(!leased_overflow);

        // All leases return; the free list holds each shard once.
        for id in leased {
            pool.give(id);
        }
        let mut free = pool.free.clone();
        free.sort_unstable();
        free.dedup();
        assert_eq!(free.len(), NUM_SHARDS);
        assert_eq!(pool.free.len(), NUM_SHARDS);

        // A second generation of live threads again gets distinct shards.
        let mut again: Vec<usize> = (0..NUM_SHARDS).map(|_| pool.take().0).collect();
        again.sort_unstable();
        again.dedup();
        assert_eq!(again.len(), NUM_SHARDS);
    }
}
