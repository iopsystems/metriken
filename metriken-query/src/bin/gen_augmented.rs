//! Augment a parquet file N times and save to a path. Used for benchmarking.
//!
//! Usage:
//! ```bash
//! cargo run --release -p metriken-query --bin gen-augmented --features fixtures -- \
//!   "$HOME/Downloads/metrics.parquet" 10 /tmp/bench_10x.parquet
//! ```

use std::path::PathBuf;

use metriken_query::fixtures::ParquetAugmentor;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("Usage: {} <input.parquet> <N> <output.parquet>", args[0]);
        std::process::exit(1);
    }

    let input = PathBuf::from(&args[1]);
    let n: u32 = args[2].parse().expect("N must be a positive integer");
    let output = PathBuf::from(&args[3]);

    eprintln!(
        "Augmenting {} x{} -> {}",
        input.display(),
        n,
        output.display()
    );

    let fixture = ParquetAugmentor::from_path(&input)
        .repeat(n)
        .build()
        .expect("augment failed");

    std::fs::copy(fixture.path(), &output).expect("copy failed");
    let size = std::fs::metadata(&output).expect("stat").len();
    println!(
        "Wrote {} ({:.1} MB)",
        output.display(),
        size as f64 / (1024.0 * 1024.0)
    );
}
