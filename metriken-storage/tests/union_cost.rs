//! What a query costs on a real archive. Ignored; needs a recording:
//! `UNION_COST_ARCHIVE=path.dendro cargo test --release -p metriken-storage
//! --test union_cost union_cost -- --ignored --nocapture`.
//! `UNION_COST_RUNS` sets the timed runs of each query (default 7), after one
//! untimed run. Prints each query's median and minimum in milliseconds. It
//! uses only API that predates process groups, so the same file builds
//! against an older tree, and two builds can be timed in interleaved rounds.
//! `UNION_COST_CASE=n` runs the n-th case alone (for a peak-RSS reading).
//! `UNION_COST_PER_SERIES=1` takes the per-series rate path
//! (`QueryOptions::with_per_series_rates`).

use std::time::{Duration, Instant};

use metriken_query::{DisplayOptions, MetricsSource, QueryOptions};
use metriken_storage::{ArchiveReader, BufferPool, DendroCatalog};

#[test]
#[ignore]
fn union_cost() {
    let path = std::env::var("UNION_COST_ARCHIVE").expect("UNION_COST_ARCHIVE");
    let runs: usize = std::env::var("UNION_COST_RUNS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(7);
    let recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(std::path::Path::new(&path)).unwrap()),
        None,
        BufferPool::new(1 << 30),
        None,
    )
    .unwrap();
    let reader = ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None);
    let reader = &reader;
    let opts = QueryOptions::default()
        .with_per_series_rates(std::env::var("UNION_COST_PER_SERIES").is_ok_and(|v| v == "1"));
    let opts = &opts;
    let (start, end) = MetricsSource::time_range(reader).unwrap();
    let hour = (start + 3600.0, start + 7200.0);
    let display = DisplayOptions {
        budget: 1000,
        ..DisplayOptions::default()
    };
    type Run<'a> = Box<dyn Fn() -> usize + 'a>;
    let count = |r: metriken_query::QueryResult| match r {
        metriken_query::QueryResult::Matrix { result } => {
            result.iter().map(|m| m.values.len()).sum::<usize>()
        }
        _ => 0,
    };
    let range = move |q: &'static str, (s, e): (f64, f64), step: f64| -> Run<'_> {
        Box::new(move || count(reader.query_range_opts(q, s, e, step, opts).unwrap()))
    };
    let display = &display;
    let shown = move |q: &'static str, (s, e): (f64, f64), step: f64| -> Run<'_> {
        Box::new(move || {
            match reader
                .query_range_display_opts(q, s, e, step, display, opts)
                .unwrap()
            {
                metriken_query::DisplayResult::Series { result, .. } => result.len(),
                _ => 0,
            }
        })
    };
    let cases: Vec<(&str, Run<'_>)> = vec![
        (
            "display sum(irate(cgroup_cpu_usage[5s])) 9.6h step 1",
            shown("sum(irate(cgroup_cpu_usage[5s]))", (start, end), 1.0),
        ),
        (
            "rate(cgroup_cpu_usage[1m]) 1h step 1",
            range("rate(cgroup_cpu_usage[1m])", hour, 1.0),
        ),
        (
            "sum(irate(cgroup_cpu_usage[5s])) 1h step 1",
            range("sum(irate(cgroup_cpu_usage[5s]))", hour, 1.0),
        ),
        (
            "sum(rate(cgroup_cpu_usage[1m])) 9.6h step 60",
            range("sum(rate(cgroup_cpu_usage[1m]))", (start, end), 60.0),
        ),
        (
            "sum(rate(cpu_usage[1m])) one point",
            range("sum(rate(cpu_usage[1m]))", (hour.0, hour.0), 1.0),
        ),
        (
            "gauge memory_free 1h step 1",
            range("memory_free", hour, 1.0),
        ),
        (
            "display sum(irate(cpu_usage[5s])) 9.6h step 1",
            shown("sum(irate(cpu_usage[5s]))", (start, end), 1.0),
        ),
    ];
    // `UNION_COST_CASE=n` runs case `n` alone, for a peak-RSS reading of it.
    let only: Option<usize> = std::env::var("UNION_COST_CASE")
        .ok()
        .and_then(|v| v.parse().ok());
    let cases: Vec<_> = cases
        .into_iter()
        .enumerate()
        .filter(|(i, _)| only.is_none_or(|o| o == *i))
        .map(|(_, c)| c)
        .collect();
    for (name, run) in &cases {
        let points = run();
        let mut times: Vec<Duration> = (0..runs)
            .map(|_| {
                let t = Instant::now();
                std::hint::black_box(run());
                t.elapsed()
            })
            .collect();
        times.sort();
        println!(
            "{name}: median {:.2} ms, min {:.2} ms ({points} points)",
            times[times.len() / 2].as_secs_f64() * 1e3,
            times[0].as_secs_f64() * 1e3
        );
    }
}
