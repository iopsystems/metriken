//! Time and result of one display query over a dendro archive, for comparing
//! builds (`docs/journal/2026-10-09-storage-scan.md`, GO / NO-GO). Ignored;
//! needs an archive:
//!
//! `DP_ARCHIVE=in.dendro DP_QUERY='sum by (comm) (irate(task_cpu_usage[5s]))'
//! cargo test --release -p metriken-archive --test display_peak -- --ignored
//! --nocapture`
//!
//! Prints the number of points and the query's time. `DP_OUT=file` writes
//! the result's `Debug` text, which prints each `f64` in its shortest
//! round-trip form, so equal files are bit-identical results. Peak memory is
//! measured around the process (`/usr/bin/time -l` on macOS, `-v` on
//! Linux).

use metriken_archive::{ArchiveReader, DendroCatalog};
use metriken_query::{BufferPool, DisplayOptions, DisplayResult, MetricsSource, QueryOptions};

#[test]
#[ignore]
fn display_peak() {
    let path = std::env::var("DP_ARCHIVE").expect("DP_ARCHIVE names a dendro archive");
    let query = std::env::var("DP_QUERY").expect("DP_QUERY is a PromQL query");
    let pool = BufferPool::new(16 << 20);
    let mut recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(std::path::Path::new(&path)).unwrap()),
        None,
        std::sync::Arc::clone(&pool),
        None,
    )
    .unwrap();
    let reader = recordings.remove(0).1;
    let (lo, hi) = reader.time_range().unwrap();
    let opts = DisplayOptions {
        budget: 500,
        ..Default::default()
    };
    let t = std::time::Instant::now();
    let r = reader
        .query_range_display_opts(&query, lo, hi, 1.0, &opts, &QueryOptions::default())
        .unwrap();
    let elapsed = t.elapsed();
    if let Ok(out) = std::env::var("DP_OUT") {
        std::fs::write(out, format!("{r:?}")).unwrap();
    }
    let DisplayResult::Series { result, .. } = r else {
        panic!("a display query returns series");
    };
    let n: usize = result.iter().map(|s| s.points.len()).sum();
    println!("DP {n} points in {elapsed:?}");
}
