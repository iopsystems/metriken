//! Process-wide buffer pool for decoded parquet row groups.
//!
//! Follows the standard layered-cache pattern used by ClickHouse, DuckDB,
//! PostgreSQL, and other columnar/OLAP systems. Multiple `ParquetReader`s
//! constructed with the same pool share its budget and LRU.
//!
//! # Usage
//!
//! ```rust,ignore
//! let pool = BufferPool::new(500 * 1024 * 1024); // 500 MB
//! let reader = ParquetReader::open_with_pool(&path, Arc::clone(&pool))?;
//! // Second reader shares the same pool
//! let reader2 = ParquetReader::open_with_pool(&path2, Arc::clone(&pool))?;
//! let stats = pool.stats();
//! println!("hits={} misses={}", stats.hits, stats.misses);
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::types::HistogramSnapshot;

static NEXT_SOURCE_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_source_id() -> u64 {
    NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed)
}

/// A bounded LRU cache of decoded parquet row groups.
///
/// Construct one per process (or per logical workload), share across many
/// `ParquetReader`s via `Arc`. The pool tracks RAM usage and evicts the
/// least-recently-used entries when the byte budget is exceeded.
pub struct BufferPool {
    inner: Mutex<BufferPoolInner>,
}

struct BufferPoolInner {
    // lru::LruCache::unbounded() lets us manage eviction by bytes ourselves,
    // since the crate's built-in cap is by entry count, not bytes.
    cache: lru::LruCache<CacheKey, CachedEntry>,
    current_bytes: usize,
    max_bytes: usize,
    hits: u64,
    misses: u64,
}

/// Snapshot of pool statistics at a point in time.
#[derive(Debug, Clone, Copy)]
pub struct BufferPoolStats {
    pub hits: u64,
    pub misses: u64,
    pub bytes_used: usize,
    pub entries: usize,
}

/// Identifies a single decoded column block within a parquet source.
#[derive(Hash, Eq, PartialEq, Clone, Copy)]
pub(crate) struct CacheKey {
    pub source_id: u64,
    pub column_idx: usize,
    pub row_group_idx: usize,
}

#[derive(Clone)]
struct CachedEntry {
    data: Block,
    size_bytes: usize,
}

/// Decoded row-group data, parameterized by column type.
///
/// Counter and gauge values use `Option<T>` to preserve the null/present
/// distinction from the parquet column, keeping row alignment with the
/// timestamp column consistent.
#[derive(Clone)]
pub(crate) enum Block {
    Timestamps(Arc<Vec<Option<u64>>>),
    CounterValues(Arc<Vec<Option<u64>>>),
    GaugeValues(Arc<Vec<Option<i64>>>),
    HistogramSnapshots(Arc<Vec<HistogramSnapshot>>),
}

impl BufferPool {
    /// Create a buffer pool with `max_bytes` budget, returned as `Arc<Self>`
    /// for easy sharing across readers.
    pub fn new(max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BufferPoolInner {
                cache: lru::LruCache::unbounded(),
                current_bytes: 0,
                max_bytes,
                hits: 0,
                misses: 0,
            }),
        })
    }

    /// Current number of bytes tracked in the pool.
    pub fn current_bytes(&self) -> usize {
        self.inner.lock().unwrap().current_bytes
    }

    /// Maximum byte budget.
    pub fn max_bytes(&self) -> usize {
        self.inner.lock().unwrap().max_bytes
    }

    /// Evict all entries, resetting byte usage to zero.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.cache.clear();
        inner.current_bytes = 0;
    }

    /// Return a snapshot of pool statistics.
    pub fn stats(&self) -> BufferPoolStats {
        let inner = self.inner.lock().unwrap();
        BufferPoolStats {
            hits: inner.hits,
            misses: inner.misses,
            bytes_used: inner.current_bytes,
            entries: inner.cache.len(),
        }
    }

    // ─── Timestamp column ────────────────────────────────────────────────────

    pub(crate) fn get_timestamps(&self, key: CacheKey) -> Option<Arc<Vec<Option<u64>>>> {
        let mut inner = self.inner.lock().unwrap();
        let result = inner.cache.get(&key).and_then(|entry| {
            if let Block::Timestamps(v) = &entry.data { Some(Arc::clone(v)) } else { None }
        });
        if result.is_some() { inner.hits += 1; } else { inner.misses += 1; }
        result
    }

    pub(crate) fn put_timestamps(&self, key: CacheKey, data: Arc<Vec<Option<u64>>>) {
        // Vec<Option<u64>>: 16 bytes per element on x86_64 (None + discriminant)
        let size = data.len() * std::mem::size_of::<Option<u64>>();
        self.put(key, Block::Timestamps(data), size);
    }

    // ─── Counter value column ────────────────────────────────────────────────

    pub(crate) fn get_counter_values(&self, key: CacheKey) -> Option<Arc<Vec<Option<u64>>>> {
        let mut inner = self.inner.lock().unwrap();
        let result = inner.cache.get(&key).and_then(|entry| {
            if let Block::CounterValues(v) = &entry.data { Some(Arc::clone(v)) } else { None }
        });
        if result.is_some() { inner.hits += 1; } else { inner.misses += 1; }
        result
    }

    pub(crate) fn put_counter_values(&self, key: CacheKey, data: Arc<Vec<Option<u64>>>) {
        let size = data.len() * std::mem::size_of::<Option<u64>>();
        self.put(key, Block::CounterValues(data), size);
    }

    // ─── Gauge value column ──────────────────────────────────────────────────

    pub(crate) fn get_gauge_values(&self, key: CacheKey) -> Option<Arc<Vec<Option<i64>>>> {
        let mut inner = self.inner.lock().unwrap();
        let result = inner.cache.get(&key).and_then(|entry| {
            if let Block::GaugeValues(v) = &entry.data { Some(Arc::clone(v)) } else { None }
        });
        if result.is_some() { inner.hits += 1; } else { inner.misses += 1; }
        result
    }

    pub(crate) fn put_gauge_values(&self, key: CacheKey, data: Arc<Vec<Option<i64>>>) {
        let size = data.len() * std::mem::size_of::<Option<i64>>();
        self.put(key, Block::GaugeValues(data), size);
    }

    // ─── Histogram snapshot column ───────────────────────────────────────────

    pub(crate) fn get_histogram_snapshots(&self, key: CacheKey) -> Option<Arc<Vec<HistogramSnapshot>>> {
        let mut inner = self.inner.lock().unwrap();
        let result = inner.cache.get(&key).and_then(|entry| {
            if let Block::HistogramSnapshots(v) = &entry.data { Some(Arc::clone(v)) } else { None }
        });
        if result.is_some() { inner.hits += 1; } else { inner.misses += 1; }
        result
    }

    pub(crate) fn put_histogram_snapshots(&self, key: CacheKey, data: Arc<Vec<HistogramSnapshot>>) {
        let size: usize = data.iter()
            .map(|s| {
                s.index.len() * std::mem::size_of::<u32>()
                    + s.count.len() * std::mem::size_of::<u64>()
            })
            .sum();
        self.put(key, Block::HistogramSnapshots(data), size);
    }

    // ─── Internal ────────────────────────────────────────────────────────────

    fn put(&self, key: CacheKey, data: Block, size_bytes: usize) {
        let mut inner = self.inner.lock().unwrap();

        // If the single item exceeds the total budget, skip caching it.
        if size_bytes > inner.max_bytes {
            return;
        }

        // Evict LRU entries until there is room for the new entry.
        while inner.current_bytes + size_bytes > inner.max_bytes {
            if let Some((_, evicted)) = inner.cache.pop_lru() {
                inner.current_bytes = inner.current_bytes.saturating_sub(evicted.size_bytes);
            } else {
                break;
            }
        }

        // If the same key was already present (e.g. concurrent fill), replace it.
        if let Some(prev) = inner.cache.pop(&key) {
            inner.current_bytes = inner.current_bytes.saturating_sub(prev.size_bytes);
        }

        inner.cache.put(key, CachedEntry { data, size_bytes });
        inner.current_bytes += size_bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_basic_get_put() {
        let pool = BufferPool::new(1024);
        let key = CacheKey { source_id: 1, column_idx: 0, row_group_idx: 0 };
        let data = Arc::new(vec![Some(1u64), Some(2)]);
        pool.put_timestamps(key, Arc::clone(&data));

        let got = pool.get_timestamps(key).unwrap();
        assert_eq!(*got, vec![Some(1u64), Some(2)]);
    }

    #[test]
    fn test_pool_evicts_when_full() {
        // Budget: 64 bytes. Each entry of 8 Option<u64> = 8 * 16 = 128 bytes.
        // So only one entry fits. After inserting 10, budget must be respected.
        let pool = BufferPool::new(64);
        for i in 0..10u64 {
            let key = CacheKey { source_id: 1, column_idx: 0, row_group_idx: i as usize };
            // 1-element entries: 16 bytes each
            pool.put_timestamps(key, Arc::new(vec![Some(i)]));
        }
        let stats = pool.stats();
        assert!(
            stats.bytes_used <= 64,
            "bytes_used={} exceeds budget=64",
            stats.bytes_used
        );
        assert!(stats.entries < 10, "expected evictions, got {} entries", stats.entries);
    }

    #[test]
    fn test_pool_stats_hits_misses() {
        let pool = BufferPool::new(1024);
        let key = CacheKey { source_id: 1, column_idx: 0, row_group_idx: 0 };

        assert_eq!(pool.stats().hits, 0);
        assert!(pool.get_timestamps(key).is_none());
        assert_eq!(pool.stats().misses, 1);

        pool.put_timestamps(key, Arc::new(vec![Some(1u64)]));
        assert!(pool.get_timestamps(key).is_some());
        assert_eq!(pool.stats().hits, 1);
    }

    #[test]
    fn test_pool_clear() {
        // 100 * size_of::<Option<u64>>() = 100 * 16 = 1600 bytes; give the pool 2 KB
        let pool = BufferPool::new(2048);
        let key = CacheKey { source_id: 1, column_idx: 0, row_group_idx: 0 };
        pool.put_timestamps(key, Arc::new(vec![Some(1u64); 100]));
        assert!(pool.stats().bytes_used > 0);

        pool.clear();
        assert_eq!(pool.stats().bytes_used, 0);
        assert_eq!(pool.stats().entries, 0);
        // After clear, the key is gone
        assert!(pool.get_timestamps(key).is_none());
    }

    #[test]
    fn test_pool_type_mismatch_is_miss() {
        // Inserting as timestamps then reading as counter values should be a miss.
        let pool = BufferPool::new(1024);
        let key = CacheKey { source_id: 1, column_idx: 0, row_group_idx: 0 };
        pool.put_timestamps(key, Arc::new(vec![Some(42u64)]));
        assert!(pool.get_counter_values(key).is_none());
    }

    #[test]
    fn test_pool_oversized_entry_not_cached() {
        // An entry larger than the entire budget should not be inserted.
        let pool = BufferPool::new(8); // only 8 bytes
        let key = CacheKey { source_id: 1, column_idx: 0, row_group_idx: 0 };
        // 1 Option<u64> = 16 bytes > 8 byte budget
        pool.put_timestamps(key, Arc::new(vec![Some(1u64)]));
        assert_eq!(pool.stats().entries, 0);
        assert_eq!(pool.stats().bytes_used, 0);
    }

    #[test]
    fn test_pool_counter_and_gauge_values() {
        let pool = BufferPool::new(4096);
        let ck = CacheKey { source_id: 2, column_idx: 1, row_group_idx: 0 };
        let gk = CacheKey { source_id: 2, column_idx: 2, row_group_idx: 0 };

        pool.put_counter_values(ck, Arc::new(vec![Some(100u64), None, Some(200)]));
        pool.put_gauge_values(gk, Arc::new(vec![Some(-1i64), Some(0), None]));

        let cv = pool.get_counter_values(ck).unwrap();
        assert_eq!(*cv, vec![Some(100u64), None, Some(200)]);

        let gv = pool.get_gauge_values(gk).unwrap();
        assert_eq!(*gv, vec![Some(-1i64), Some(0), None]);
    }
}
