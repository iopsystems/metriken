//! Baseline memory benchmark using metriken-query 0.10.8 (Tsdb-based).
//!
//! Run with:
//! ```bash
//! METRIKEN_TEST_PARQUET=/path/to/parquet \
//!   cargo run --release -p metriken-query-baseline-bench --bin baseline-memory
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use metriken_query::{QueryEngine, Tsdb};
use sysinfo::{Pid, ProcessesToUpdate, System};

fn rss_bytes() -> u64 {
    let mut sys = System::new();
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid).map(|p| p.memory()).unwrap_or(0)
}

fn fmt_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn pick_queries(tsdb: &Tsdb) -> Vec<(String, String)> {
    let mut queries = Vec::new();
    if let Some(c) = tsdb.counter_names().into_iter().next() {
        queries.push((format!("rate({c}[1m])"), "counter rate".to_string()));
    }
    if let Some(g) = tsdb.gauge_names().into_iter().next() {
        queries.push((g.to_string(), "gauge raw".to_string()));
    }
    if let Some(h) = tsdb.histogram_names().into_iter().next() {
        queries.push((
            format!("histogram_quantile(0.99, {h})"),
            "histogram p99".to_string(),
        ));
    }
    queries
}

fn bench_path(path: &PathBuf) {
    let size = std::fs::metadata(path).expect("stat").len();
    println!("\nFile: {} ({})", path.display(), fmt_size(size));

    let rss_before = rss_bytes();
    let t0 = Instant::now();
    let tsdb = Tsdb::load(path).expect("load");
    let load_elapsed = t0.elapsed();
    let rss_after_load = rss_bytes();
    let tsdb = Arc::new(tsdb);

    println!(
        "  Load:                  {:>10.2?}  RSS delta: +{}",
        load_elapsed,
        fmt_size(rss_after_load.saturating_sub(rss_before)),
    );

    let engine = QueryEngine::new(Arc::clone(&tsdb));
    let (start_ns, end_ns) = tsdb.time_range().expect("time range");
    let start = start_ns as f64 / 1e9;
    let end = end_ns as f64 / 1e9 + 1.0;

    let queries = pick_queries(&tsdb);
    for (query, label) in queries {
        let rss_before_query = rss_bytes();
        let t0 = Instant::now();
        let result = engine.query_range(&query, start, end, 1.0);
        let elapsed = t0.elapsed();
        let rss_after = rss_bytes();
        let status = match result {
            Ok(_) => "ok",
            Err(_) => "err",
        };
        println!(
            "  {:<22} {:>10.2?}  RSS delta: +{}  [{status}]  ({query})",
            label,
            elapsed,
            fmt_size(rss_after.saturating_sub(rss_before_query)),
        );
    }

    let rss_total = rss_bytes();
    println!(
        "  Cumulative RSS delta:  {}",
        fmt_size(rss_total.saturating_sub(rss_before)),
    );

    drop(engine);
    drop(tsdb);
}

fn main() {
    println!("=== Baseline (Tsdb 0.10.8) Memory Benchmark ===");
    let path = std::env::var("METRIKEN_TEST_PARQUET")
        .map(PathBuf::from)
        .expect("set METRIKEN_TEST_PARQUET=/path/to/parquet (real or augmented)");

    bench_path(&path);
    println!("\nDone.");
}
