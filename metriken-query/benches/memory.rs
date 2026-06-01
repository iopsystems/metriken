//! Memory + latency benchmark for ParquetReader.
//!
//! Run with:
//! ```bash
//! METRIKEN_TEST_PARQUET=/path/to/real.parquet \
//!   cargo run --release --bench memory --features fixtures
//! ```
//!
//! If `METRIKEN_TEST_PARQUET` is unset, generates a synthetic ~1MB fixture.

use std::path::{Path, PathBuf};
use std::time::Instant;

use metriken_query::fixtures::{FixtureBuilder, ParquetAugmentor};
use metriken_query::ParquetReader;
use sysinfo::{Pid, ProcessesToUpdate, System};

fn rss_bytes() -> u64 {
    let mut sys = System::new();
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid).map(|p| p.memory()).unwrap_or(0)
}

/// Pick a representative query for each metric kind on this file.
fn pick_queries(reader: &ParquetReader) -> Vec<(String, String)> {
    let mut queries = Vec::new();

    // Counter rate (1m window)
    if let Some(c) = reader.counter_names().first().cloned() {
        queries.push((
            format!("rate({c}[1m])"),
            "counter rate".to_string(),
        ));
    }

    // Gauge (raw)
    if let Some(g) = reader.gauge_names().first().cloned() {
        queries.push((g.clone(), "gauge raw".to_string()));
    }

    // Histogram quantile (p99)
    if let Some(h) = reader.histogram_names().first().cloned() {
        queries.push((
            format!("histogram_quantile(0.99, {h})"),
            "histogram p99".to_string(),
        ));
    }

    queries
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

fn bench_one(base_path: &Path, repetitions: u32) {
    println!("\n=== {repetitions}x replication ===");

    // Augment
    let augmented = ParquetAugmentor::from_path(base_path)
        .repeat(repetitions)
        .build()
        .expect("augment");
    let size = std::fs::metadata(augmented.path()).expect("stat").len();
    println!("File: {}", fmt_size(size));

    // Open
    let rss_before = rss_bytes();
    let t0 = Instant::now();
    let reader = ParquetReader::open(augmented.path()).expect("open");
    let open_elapsed = t0.elapsed();
    let rss_after_open = rss_bytes();

    println!(
        "  Open:                  {:>10.2?}  RSS delta: +{}",
        open_elapsed,
        fmt_size(rss_after_open.saturating_sub(rss_before)),
    );

    // Queries
    let (start, end) = reader.time_range().expect("time range");
    let queries = pick_queries(&reader);

    for (query, label) in queries {
        let rss_before_query = rss_bytes();
        let t0 = Instant::now();
        let result = reader.query_range(&query, start, end, 1.0);
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

    // Cumulative RSS - reader is held alive
    let rss_total = rss_bytes();
    println!(
        "  Cumulative RSS delta:  {}",
        fmt_size(rss_total.saturating_sub(rss_before)),
    );

    drop(reader);
    drop(augmented);
}

fn ensure_base_fixture() -> PathBuf {
    if let Ok(path) = std::env::var("METRIKEN_TEST_PARQUET") {
        return PathBuf::from(path);
    }
    eprintln!("METRIKEN_TEST_PARQUET unset; generating synthetic fallback (~1MB)");
    let fixture = FixtureBuilder::new()
        .samples(10_000)
        .sampling_interval_ms(1000)
        .metadata("source", "synthetic")
        .metadata("version", "1.0")
        .monotonic_counter("counter_a", &[("zone", "us-east")], 100)
        .monotonic_counter("counter_b", &[("zone", "us-west")], 200)
        .gauge("gauge_a", &[("host", "alpha")], |t| (t as i64) * 10)
        .point_histogram("histogram_a", &[], 4, 16, 5, 100)
        .build()
        .expect("build synthetic fallback");
    let path = fixture.path().to_path_buf();
    std::mem::forget(fixture); // keep on disk for the rest of the run
    path
}

fn main() {
    println!("=== ParquetReader Memory + Latency Benchmark ===");
    let base = ensure_base_fixture();
    let base_size = std::fs::metadata(&base).expect("stat base").len();
    println!("Base file: {} ({})", base.display(), fmt_size(base_size));

    // Open the base file once to print structure
    let r = ParquetReader::open(&base).expect("open base");
    println!(
        "Schema: {} counters, {} gauges, {} histograms",
        r.counter_names().len(),
        r.gauge_names().len(),
        r.histogram_names().len(),
    );
    drop(r);

    for reps in [1, 10, 25, 100] {
        bench_one(&base, reps);
    }

    println!("\nDone.");
}
