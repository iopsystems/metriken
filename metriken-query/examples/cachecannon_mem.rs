//! Peak transient-heap measurement for streaming-vs-eager `query_range`
//! on the cachecannon dashboard fixture.
//!
//! Wraps the system allocator with a counting shim that tracks current
//! and peak resident bytes. Loads the parquet once, then for each
//! dashboard query runs eager and streaming back-to-back, snapshotting
//! peak delta around each call.
//!
//! Usage:
//!
//!     CACHECANNON_PARQUET=/path/to/cachecannon.parquet \
//!       cargo run --release -p metriken-query --example cachecannon_mem
//!
//! Defaults to the rezolus dev checkout location when the env var is
//! unset. Doesn't require any extra dependencies — just the
//! `std::alloc::System` allocator and a couple of atomics.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use metriken_query::{QueryEngine, Tsdb};

/// Tracking allocator: delegates to `System` for the actual work and
/// keeps three running totals on the side.
///
/// * `current` — bytes resident right now (alloc - dealloc).
/// * `peak` — high-water mark of `current` since the last reset.
/// * `total` — cumulative bytes allocated (never decremented), useful
///   to see how much "churn" a query did even when peak is low.
struct TrackingAlloc;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for TrackingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let new = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            TOTAL.fetch_add(layout.size(), Ordering::Relaxed);
            // Update peak with a relaxed CAS loop — exactness doesn't
            // matter for the high-water mark; we just want a number
            // close to the truth without paying for a fence.
            let mut peak = PEAK.load(Ordering::Relaxed);
            while new > peak {
                match PEAK.compare_exchange_weak(peak, new, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(p) => peak = p,
                }
            }
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static GLOBAL: TrackingAlloc = TrackingAlloc;

/// Snapshot of the allocator counters at one moment.
#[derive(Copy, Clone)]
struct Snap {
    current: usize,
    peak: usize,
    total: usize,
}

fn snap() -> Snap {
    Snap {
        current: CURRENT.load(Ordering::Relaxed),
        peak: PEAK.load(Ordering::Relaxed),
        total: TOTAL.load(Ordering::Relaxed),
    }
}

/// Reset peak to current. Call right before a measured region.
/// Doesn't touch `total`, so `(after.total - before.total)` after the
/// region gives the cumulative bytes allocated during it.
fn reset_peak() {
    PEAK.store(CURRENT.load(Ordering::Relaxed), Ordering::Relaxed);
}

const QUERIES: &[&str] = &[
    // --- cachecannon dashboard (loadgen, cardinality 1 per metric) ---
    "target_rate{source=\"cachecannon\"}",
    "sum(irate(requests_sent{source=\"cachecannon\"}[5s]))",
    "sum(irate(responses_received{source=\"cachecannon\"}[5s]))",
    "sum(irate(bytes_rx{source=\"cachecannon\"}[5s]))",
    "sum(irate(bytes_tx{source=\"cachecannon\"}[5s]))",
    "sum(irate(request_errors{source=\"cachecannon\"}[5s]))",
    "sum(irate(connections_failed{source=\"cachecannon\"}[5s]))",
    "sum(irate(cache_hits{source=\"cachecannon\"}[5s]))",
    "sum(irate(cache_misses{source=\"cachecannon\"}[5s]))",
    "sum(irate(get_count{source=\"cachecannon\"}[5s]))",
    "sum(irate(set_count{source=\"cachecannon\"}[5s]))",
    "histogram_quantiles([0.5, 0.9, 0.99, 0.999], response_latency{source=\"cachecannon\"})",
    "histogram_quantiles([0.5, 0.9, 0.99, 0.999], get_latency{source=\"cachecannon\"})",
    "histogram_quantiles([0.5, 0.9, 0.99, 0.999], set_latency{source=\"cachecannon\"})",
    "histogram_heatmap(response_latency{source=\"cachecannon\"})",
    // --- rezolus system dashboard (high cardinality) ---
    // These are the queries that fill up WASM memory in practice:
    // counter aggregations across per-cpu / per-softirq / per-cgroup
    // series with cardinality 30-200.
    "sum(irate(softirq[5s]))",
    "sum(irate(softirq_time[5s]))",
    "sum(irate(cpu_cycles[5s]))",
    "sum(irate(cpu_instructions[5s]))",
    "sum(irate(cpu_usage[5s]))",
    "sum(irate(cpu_migrations[5s]))",
    "sum(irate(scheduler_context_switch[5s]))",
    "sum(irate(scheduler_runqueue_wait[5s]))",
    "sum(irate(syscall[5s]))",
    "sum(irate(cgroup_syscall[5s]))",
    "sum by (cpu) (irate(cpu_cycles[5s]))",
    "sum by (cpu) (irate(cpu_usage[5s]))",
    "sum by (id) (irate(softirq_time[5s]))",
    "sum by (name) (irate(cgroup_cpu_usage[5s]))",
    // rate (windowed average) — same producer shape as irate.
    "sum(rate(softirq[5s]))",
    "sum(rate(cpu_usage[5s]))",
    "sum by (cpu) (rate(cpu_usage[5s]))",
    // avg / min / max / count — exercise the generalised reducer.
    "avg(irate(cpu_usage[5s]))",
    "max(irate(cpu_usage[5s]))",
    "min(irate(cpu_usage[5s]))",
    "count(irate(cpu_usage[5s]))",
    // sum without (..) — exercise GroupBy::Exclude.
    "sum without (cpu) (irate(cpu_cycles[5s]))",
    "sum without (id) (irate(softirq_time[5s]))",
];

fn fixture_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CACHECANNON_PARQUET") {
        return Some(PathBuf::from(p));
    }
    let dev = PathBuf::from("/home/user/rezolus/site/viewer/data/cachecannon.parquet");
    if dev.exists() {
        return Some(dev);
    }
    None
}

fn fmt_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{:.2} MiB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Measure one query. Returns `(peak_delta_bytes, allocated_bytes)`.
/// `peak_delta` is the high-water mark above `before.current` — the
/// transient working-set size we actually care about. `allocated` is
/// cumulative bytes asked of the allocator (proxy for churn).
fn measure<F: FnOnce()>(f: F) -> (usize, usize) {
    let before = snap();
    reset_peak();
    f();
    let after = snap();
    let peak_delta = after.peak.saturating_sub(before.current);
    let allocated = after.total - before.total;
    (peak_delta, allocated)
}

fn main() {
    let Some(path) = fixture_path() else {
        eprintln!(
            "set CACHECANNON_PARQUET=/path/to/cachecannon.parquet (or check out rezolus \
             alongside metriken)"
        );
        std::process::exit(2);
    };

    println!("loading {path:?}");
    let load = snap();
    let tsdb = Arc::new(Tsdb::load(&path).expect("load parquet"));
    let after_load = snap();
    println!(
        "tsdb resident: {} (allocated during load: {})\n",
        fmt_bytes(after_load.current.saturating_sub(load.current)),
        fmt_bytes(after_load.total - load.total),
    );

    let (start, end) = QueryEngine::new(tsdb.clone()).get_time_range();
    let step = 1.0;

    println!(
        "{:<70} {:>14} {:>14} {:>14} {:>14}",
        "query", "eager peak", "stream peak", "eager alloc", "stream alloc",
    );

    let (mut eager_peak_total, mut stream_peak_total) = (0usize, 0usize);
    let (mut eager_alloc_total, mut stream_alloc_total) = (0usize, 0usize);

    for q in QUERIES {
        let mut engine = QueryEngine::new(tsdb.clone());

        // Run each path twice and keep the second run's number — the
        // first run pays for one-time map-grow / cache effects in
        // the parquet metadata path that aren't representative of
        // the hot loop.
        engine.set_streaming_enabled(false);
        let _ = engine.query_range(q, start, end, step);
        let (eager_peak, eager_alloc) = measure(|| {
            let _ = engine.query_range(q, start, end, step);
        });

        engine.set_streaming_enabled(true);
        let _ = engine.query_range(q, start, end, step);
        let (stream_peak, stream_alloc) = measure(|| {
            let _ = engine.query_range(q, start, end, step);
        });

        eager_peak_total += eager_peak;
        stream_peak_total += stream_peak;
        eager_alloc_total += eager_alloc;
        stream_alloc_total += stream_alloc;

        let q_short: String = if q.len() > 68 {
            format!("{}…", &q[..67])
        } else {
            (*q).to_string()
        };
        println!(
            "{:<70} {:>14} {:>14} {:>14} {:>14}",
            q_short,
            fmt_bytes(eager_peak),
            fmt_bytes(stream_peak),
            fmt_bytes(eager_alloc),
            fmt_bytes(stream_alloc),
        );
    }

    println!(
        "\ntotals (sum across queries):\n  eager   peak {}, alloc {}\n  stream  peak {}, alloc {}\n  \
         delta   peak {} ({:+.0}%), alloc {} ({:+.0}%)",
        fmt_bytes(eager_peak_total),
        fmt_bytes(eager_alloc_total),
        fmt_bytes(stream_peak_total),
        fmt_bytes(stream_alloc_total),
        fmt_bytes(eager_peak_total.saturating_sub(stream_peak_total)),
        100.0 * (stream_peak_total as f64 - eager_peak_total as f64) / eager_peak_total as f64,
        fmt_bytes(eager_alloc_total.saturating_sub(stream_alloc_total)),
        100.0 * (stream_alloc_total as f64 - eager_alloc_total as f64) / eager_alloc_total as f64,
    );
}
