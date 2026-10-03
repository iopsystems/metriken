//! What a query costs on a freshly reopened reader, as a live viewer reopens
//! one every interval. Ignored; needs a recording:
//! `REOPEN_COST_ARCHIVE=path.dendro cargo test --release -p metriken-archive
//! --test reopen_cost -- --ignored --nocapture`. `REOPEN_COST_QUERY` picks
//! the query (default `sum(irate(task_cpu_usage[5s]))`).

use std::time::{Duration, Instant};

use metriken_archive::{ArchiveReader, DendroCatalog};
use metriken_query::{BufferPool, MetricsSource};

fn open(path: &str, pool: &std::sync::Arc<BufferPool>) -> ArchiveReader {
    let mut recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(std::path::Path::new(path)).unwrap()),
        None,
        std::sync::Arc::clone(pool),
        None,
    )
    .unwrap();
    recordings.remove(0).1
}

/// Each series' labels (without `__occupant__`) and values to 9 significant
/// digits, sorted.
fn canonical(r: &metriken_query::QueryResult) -> Vec<String> {
    let metriken_query::QueryResult::Matrix { result } = r else {
        return vec![format!("{r:?}")];
    };
    let mut out: Vec<String> = result
        .iter()
        .map(|m| {
            let labels: std::collections::BTreeMap<_, _> = m
                .metric
                .iter()
                .filter(|(k, _)| k.as_str() != "__occupant__")
                .collect();
            let values: Vec<String> = m
                .values
                .iter()
                .map(|(t, v)| format!("{t}:{v:.9e}"))
                .collect();
            format!("{labels:?} {}", values.join(","))
        })
        .collect();
    out.sort();
    out
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort_unstable();
    v[v.len() / 2]
}

#[test]
#[ignore]
fn reopen_cost() {
    let Ok(path) = std::env::var("REOPEN_COST_ARCHIVE") else {
        return;
    };
    let query = std::env::var("REOPEN_COST_QUERY")
        .unwrap_or_else(|_| "sum(irate(task_cpu_usage[5s]))".to_string());
    let pool_mb: usize = std::env::var("REOPEN_COST_POOL_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let pool = BufferPool::new(pool_mb << 20);
    let reader = open(&path, &pool);
    reader.keep_handover();
    let (lo, hi) = reader.time_range().unwrap();
    // `REOPEN_COST_FRAC=a,b` queries that fraction of the recording.
    let (lo, hi) = match std::env::var("REOPEN_COST_FRAC") {
        Ok(f) => {
            let (a, b) = f.split_once(',').unwrap();
            let at = |x: &str| lo + (hi - lo) * x.parse::<f64>().unwrap();
            (at(a), at(b))
        }
        Err(_) => (lo, hi),
    };
    let t = Instant::now();
    reader.query_range(&query, lo, hi, 5.0).unwrap();
    println!(
        "REOPEN cold query {:?}; pool {:?}",
        t.elapsed(),
        pool.stats()
    );
    if let Ok(out) = std::env::var("REOPEN_COST_SAVE") {
        let a = reader.query_range(&query, lo, hi, 5.0).unwrap();
        std::fs::write(&out, canonical(&a).join("\n")).unwrap();
    }
    if let Ok(other) = std::env::var("REOPEN_COST_COMPARE") {
        let a = reader.query_range(&query, lo, hi, 5.0).unwrap();
        let b = open(&other, &pool)
            .query_range(&query, lo, hi, 5.0)
            .unwrap();
        let (ca, cb) = (canonical(&a), canonical(&b));
        if ca != cb {
            let dir = std::env::var("REOPEN_COST_DUMP").unwrap_or_else(|_| "/tmp".into());
            std::fs::write(format!("{dir}/a.txt"), ca.join("\n").replace(',', "\n")).unwrap();
            std::fs::write(format!("{dir}/b.txt"), cb.join("\n").replace(',', "\n")).unwrap();
            panic!("answers differ; dumped to {dir}");
        }
        println!("REOPEN answers match {other}");
    }
    if std::env::var("REOPEN_COST_COLD_ONLY").is_ok() {
        return;
    }
    let mut warm = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        reader.query_range(&query, lo, hi, 5.0).unwrap();
        warm.push(t.elapsed());
    }
    let (mut opens, mut after) = (Vec::new(), Vec::new());
    for _ in 0..7 {
        let t = Instant::now();
        let fresh = open(&path, &pool);
        opens.push(t.elapsed());
        let t = Instant::now();
        fresh.query_range(&query, lo, hi, 5.0).unwrap();
        after.push(t.elapsed());
    }
    println!(
        "REOPEN warm query {:?}; reopen {:?}, then query {:?} ({query})",
        median(warm),
        median(opens),
        median(after)
    );

    // Each reader adopting the one before it, as a following viewer does.
    let (mut opens, mut after) = (Vec::new(), Vec::new());
    let mut previous = reader;
    for _ in 0..7 {
        let t = Instant::now();
        let fresh = open(&path, &pool);
        fresh.reuse_from(&previous);
        opens.push(t.elapsed());
        let t = Instant::now();
        let reused = fresh.query_range(&query, lo, hi, 5.0).unwrap();
        after.push(t.elapsed());
        assert_eq!(
            format!("{reused:?}"),
            format!("{:?}", previous.query_range(&query, lo, hi, 5.0).unwrap()),
            "a reused reader answers as the one it replaced"
        );
        previous = fresh;
    }
    println!(
        "REOPEN reused: reopen {:?}, then query {:?}",
        median(opens),
        median(after)
    );
}
