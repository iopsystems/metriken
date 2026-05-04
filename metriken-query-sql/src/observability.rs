//! In-process counters + timers exposed by `DuckDbBackend`.
//!
//! Wall-clock cost of a query inside `run` splits into a few phases:
//!
//! - `gen_sql` — `wide_form::try_generate` (string construction).
//! - `slot_lock` — wait time on the slot mutex (i.e. contention with
//!   other in-flight queries on the same pool slot).
//! - `prepare` — `Connection::prepare_cached` (cache hit ≈ 0; cache miss
//!   pays parse + plan).
//! - `execute` — `query_arrow` (DuckDB execution producing the first
//!   batch).
//! - `extract` — RecordBatch / Row iteration loop in Rust.
//!
//! For each phase we track `count`, `sum_ns`, `max_ns` as atomics.
//! Mean = `sum_ns / count`; max gives the worst-case outlier. No
//! percentile histogram yet — add only if these three numbers prove
//! insufficient. Counters are lock-free; reading a snapshot is a few
//! relaxed atomic loads.

use std::sync::atomic::{AtomicU64, Ordering};

/// Per-phase aggregate. All fields are `u64` for atomic ops; durations
/// are nanoseconds. Reading a snapshot is `Relaxed` because we don't
/// need cross-counter consistency — a `sum_ns` that's a touch lower
/// than `count` × `mean` because of a concurrent update is fine.
#[derive(Default, Debug)]
pub struct PhaseStats {
    pub count: AtomicU64,
    pub sum_ns: AtomicU64,
    pub max_ns: AtomicU64,
}

impl PhaseStats {
    #[inline]
    pub fn record(&self, ns: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
        // CAS loop for max. Contention is bounded by pool size (8) and
        // the loop converges in O(log N) iterations on the worst path.
        let mut prev = self.max_ns.load(Ordering::Relaxed);
        while ns > prev {
            match self.max_ns.compare_exchange_weak(
                prev,
                ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => prev = actual,
            }
        }
    }

    fn snapshot(&self) -> PhaseSnapshot {
        let count = self.count.load(Ordering::Relaxed);
        let sum_ns = self.sum_ns.load(Ordering::Relaxed);
        let max_ns = self.max_ns.load(Ordering::Relaxed);
        let mean_ns = if count > 0 { sum_ns / count } else { 0 };
        PhaseSnapshot {
            count,
            sum_ns,
            mean_ns,
            max_ns,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PhaseSnapshot {
    pub count: u64,
    pub sum_ns: u64,
    pub mean_ns: u64,
    pub max_ns: u64,
}

/// One per `DuckDbBackend`. Cheap to construct; all increments are
/// lock-free.
#[derive(Default, Debug)]
pub struct BackendStats {
    pub gen_sql: PhaseStats,
    pub slot_lock: PhaseStats,
    pub prepare: PhaseStats,
    pub execute: PhaseStats,
    pub extract: PhaseStats,
    pub total: PhaseStats,
}

impl BackendStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            gen_sql: self.gen_sql.snapshot(),
            slot_lock: self.slot_lock.snapshot(),
            prepare: self.prepare.snapshot(),
            execute: self.execute.snapshot(),
            extract: self.extract.snapshot(),
            total: self.total.snapshot(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StatsSnapshot {
    pub gen_sql: PhaseSnapshot,
    pub slot_lock: PhaseSnapshot,
    pub prepare: PhaseSnapshot,
    pub execute: PhaseSnapshot,
    pub extract: PhaseSnapshot,
    pub total: PhaseSnapshot,
}

impl std::fmt::Display for StatsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let row = |name: &str, p: &PhaseSnapshot| -> String {
            let to_us = |ns: u64| ns as f64 / 1_000.0;
            format!(
                "  {name:>10}  count={count:>8}  mean={mean:>8.2}µs  max={max:>10.2}µs  total={total:>8.2}ms",
                count = p.count,
                mean = to_us(p.mean_ns),
                max = to_us(p.max_ns),
                total = p.sum_ns as f64 / 1_000_000.0,
            )
        };
        writeln!(f, "DuckDbBackend stats:")?;
        writeln!(f, "{}", row("gen_sql", &self.gen_sql))?;
        writeln!(f, "{}", row("slot_lock", &self.slot_lock))?;
        writeln!(f, "{}", row("prepare", &self.prepare))?;
        writeln!(f, "{}", row("execute", &self.execute))?;
        writeln!(f, "{}", row("extract", &self.extract))?;
        write!(f, "{}", row("total", &self.total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_updates_count_sum_max() {
        let s = PhaseStats::default();
        s.record(100);
        s.record(50);
        s.record(200);
        let snap = s.snapshot();
        assert_eq!(snap.count, 3);
        assert_eq!(snap.sum_ns, 350);
        assert_eq!(snap.max_ns, 200);
        assert_eq!(snap.mean_ns, 350 / 3);
    }

    #[test]
    fn empty_snapshot_zero_mean() {
        let s = PhaseStats::default();
        let snap = s.snapshot();
        assert_eq!(snap.count, 0);
        assert_eq!(snap.mean_ns, 0);
    }
}
