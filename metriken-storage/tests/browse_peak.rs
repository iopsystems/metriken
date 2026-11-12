//! Memory of a dashboard browse over one recording, for comparing builds
//! (issue #400). Ignored; needs an archive and a query list:
//!
//! `BP_ARCHIVE=in.dendro BP_QUERIES=queries.txt BP_POOL_MIB=512
//! cargo test --release -p metriken-storage --features rez --test browse_peak
//! -- --ignored --nocapture`
//!
//! Opens the archive's first recording (a dendro archive or a v3 `.rez`) and
//! runs each line of `BP_QUERIES` over the whole recording at step 1 s: a
//! query grouped `by (id)` as a range query, any other as a display query
//! with budget 293. After each query it prints the process RSS and the pool's
//! tracked bytes. Peak memory is measured around the process
//! (`/usr/bin/time -l` on macOS, `-v` on Linux).

#![cfg(feature = "rez")]

use metriken_query::{DisplayOptions, MetricsSource, QueryOptions};
use metriken_storage::BufferPool;

fn rss_mib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
        / 1024
}

#[test]
#[ignore]
fn browse_peak() {
    let path = std::env::var("BP_ARCHIVE").expect("BP_ARCHIVE names an archive");
    let queries = std::env::var("BP_QUERIES").expect("BP_QUERIES names a query list");
    let pool_mib: usize = std::env::var("BP_POOL_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048);
    let queries = std::fs::read_to_string(queries).unwrap();
    let pool = BufferPool::new(pool_mib << 20);
    let mut recordings =
        metriken_storage::rez::open::open_recordings(std::path::Path::new(&path), pool.clone())
            .unwrap();
    let reader = recordings.remove(0).1;
    let (lo, hi) = reader.time_range().unwrap();
    let (start, end) = (lo.floor(), hi.floor() + 1.0);
    println!("BP opened {:.0}s rss={} MiB", hi - lo, rss_mib());
    let opts = DisplayOptions {
        budget: 293,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let (mut ok, mut err) = (0, 0);
    for q in queries.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let t = std::time::Instant::now();
        let matrix = q.contains("by (id)") && !q.starts_with("sum(") && !q.starts_with("avg(");
        let res = if matrix {
            reader
                .query_range(q, start, end, 1.0)
                .map(|_| ())
                .map_err(|e| e.to_string())
        } else {
            reader
                .query_range_display_opts(q, start, end, 1.0, &opts, &QueryOptions::default())
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        let mark = match res {
            Ok(()) => {
                ok += 1;
                "ok "
            }
            Err(_) => {
                err += 1;
                "ERR"
            }
        };
        println!(
            "BP {:6} MiB pool={:5} MiB {:5.2}s {mark} {}",
            rss_mib(),
            pool.current_bytes() >> 20,
            t.elapsed().as_secs_f64(),
            &q[..q.len().min(100)]
        );
    }
    println!(
        "BP end ok={ok} err={err} rss={} MiB pool={} MiB in {:.1}s",
        rss_mib(),
        pool.current_bytes() >> 20,
        started.elapsed().as_secs_f64()
    );
}
