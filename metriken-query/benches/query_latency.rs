//! Query latency benchmark: median + p99 across multiple runs per query.
//!
//! Run with:
//! ```bash
//! METRIKEN_TEST_PARQUET=/path/to/real.parquet \
//!   cargo run --release --bench query_latency --features fixtures
//! ```
//!
//! If `METRIKEN_TEST_PARQUET` is unset, generates a synthetic fixture.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use metriken_query::fixtures::{FixtureBuilder, ParquetAugmentor};
use metriken_query::ParquetReader;

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

fn bench_query(reader: &ParquetReader, query: &str, runs: usize) -> Option<(Duration, Duration)> {
    let (start, end) = reader.time_range()?;

    // Warmup: 2 runs
    for _ in 0..2 {
        let _ = reader.query_range(query, start, end, 1.0);
    }

    // Measure
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t0 = Instant::now();
        let _ = reader.query_range(query, start, end, 1.0);
        samples.push(t0.elapsed());
    }

    let median = percentile(&mut samples.clone(), 0.5);
    let p99 = percentile(&mut samples, 0.99);
    Some((median, p99))
}

fn run(base: &PathBuf, repetitions: u32, runs_per_query: usize) {
    let augmented = ParquetAugmentor::from_path(base)
        .repeat(repetitions)
        .build()
        .expect("augment");
    let size = std::fs::metadata(augmented.path()).expect("stat").len();
    let reader = ParquetReader::open(augmented.path()).expect("open");

    println!(
        "\n=== {repetitions}x replication ({:.1} MB) ===",
        size as f64 / (1024.0 * 1024.0),
    );

    let mut queries: Vec<(String, String)> = Vec::new();
    if let Some(c) = reader.counter_names().first().cloned() {
        queries.push((format!("rate({c}[1m])"), format!("rate({c}[1m])")));
    }
    if let Some(g) = reader.gauge_names().first().cloned() {
        queries.push((g.clone(), g.clone()));
    }
    if let Some(h) = reader.histogram_names().first().cloned() {
        queries.push((
            format!("histogram_quantile(0.99, {h})"),
            format!("p99({h})"),
        ));
    }

    println!("{:<45}  {:>10}  {:>10}", "Query", "Median", "P99");
    for (q, label) in queries {
        match bench_query(&reader, &q, runs_per_query) {
            Some((median, p99)) => {
                println!(
                    "{:<45}  {}  {}",
                    truncate(&label, 45),
                    fmt_dur(median),
                    fmt_dur(p99)
                );
            }
            None => println!("{:<45}  (no time range)", label),
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}...", &s[..n - 3])
    }
}

fn ensure_base_fixture() -> PathBuf {
    if let Ok(path) = std::env::var("METRIKEN_TEST_PARQUET") {
        return PathBuf::from(path);
    }
    eprintln!("METRIKEN_TEST_PARQUET unset; generating synthetic fallback");
    let fixture = FixtureBuilder::new()
        .samples(10_000)
        .sampling_interval_ms(1000)
        .monotonic_counter("counter_a", &[], 100)
        .gauge("gauge_a", &[], |t| t as i64 * 10)
        .point_histogram("histogram_a", &[], 4, 16, 5, 100)
        .build()
        .expect("build synthetic fallback");
    let path = fixture.path().to_path_buf();
    std::mem::forget(fixture);
    path
}

fn main() {
    println!("=== ParquetReader Query Latency Benchmark ===");
    let base = ensure_base_fixture();
    println!(
        "Base file: {} ({:.1} MB)",
        base.display(),
        std::fs::metadata(&base).expect("stat").len() as f64 / (1024.0 * 1024.0),
    );

    const RUNS: usize = 10;

    for reps in [1, 10, 25] {
        run(&base, reps, RUNS);
    }
}
