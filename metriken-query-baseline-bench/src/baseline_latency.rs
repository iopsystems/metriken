//! Baseline query latency benchmark using metriken-query 0.10.8 (Tsdb-based).
//!
//! Run with:
//! ```bash
//! METRIKEN_TEST_PARQUET=/path/to/parquet \
//!   cargo run --release -p metriken-query-baseline-bench --bin baseline-latency
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use metriken_query::{QueryEngine, Tsdb};

fn fmt_dur(d: Duration) -> String {
    if d.as_secs() >= 1 {
        format!("{:>6.2} s", d.as_secs_f64())
    } else if d.as_millis() >= 1 {
        format!("{:>5} ms", d.as_millis())
    } else {
        format!("{:>5} us", d.as_micros())
    }
}

fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    samples.sort();
    let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[idx]
}

fn bench_query(
    engine: &QueryEngine<Arc<Tsdb>>,
    query: &str,
    start: f64,
    end: f64,
    runs: usize,
) -> (Duration, Duration) {
    // Warmup
    for _ in 0..2 {
        let _ = engine.query_range(query, start, end, 1.0);
    }
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t0 = Instant::now();
        let _ = engine.query_range(query, start, end, 1.0);
        samples.push(t0.elapsed());
    }
    let median = percentile(&mut samples.clone(), 0.5);
    let p99 = percentile(&mut samples, 0.99);
    (median, p99)
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}...", &s[..n - 3])
    }
}

fn main() {
    println!("=== Baseline (Tsdb 0.10.8) Query Latency Benchmark ===");
    let path = std::env::var("METRIKEN_TEST_PARQUET")
        .map(PathBuf::from)
        .expect("set METRIKEN_TEST_PARQUET");

    let size = std::fs::metadata(&path).expect("stat").len();
    println!(
        "File: {} ({:.1} MB)",
        path.display(),
        size as f64 / (1024.0 * 1024.0)
    );

    let tsdb = Arc::new(Tsdb::load(&path).expect("load"));
    let engine = QueryEngine::new(Arc::clone(&tsdb));
    let (start_ns, end_ns) = tsdb.time_range().expect("time range");
    let start = start_ns as f64 / 1e9;
    let end = end_ns as f64 / 1e9 + 1.0;

    const RUNS: usize = 10;
    let mut queries: Vec<(String, String)> = Vec::new();
    if let Some(c) = tsdb.counter_names().into_iter().next() {
        queries.push((format!("rate({c}[1m])"), format!("rate({c}[1m])")));
    }
    if let Some(g) = tsdb.gauge_names().into_iter().next() {
        queries.push((g.to_string(), g.to_string()));
    }
    if let Some(h) = tsdb.histogram_names().into_iter().next() {
        queries.push((
            format!("histogram_quantile(0.99, {h})"),
            format!("p99({h})"),
        ));
    }

    println!("{:<45}  {:>10}  {:>10}", "Query", "Median", "P99");
    for (q, label) in queries {
        let (median, p99) = bench_query(&engine, &q, start, end, RUNS);
        println!(
            "{:<45}  {}  {}",
            truncate(&label, 45),
            fmt_dur(median),
            fmt_dur(p99)
        );
    }
}
