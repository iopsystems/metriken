//! Time and memory of chart queries on a dendro archive, opened the way
//! rezolus's viewer opens one.
//!
//! ```bash
//! cargo bench -p metriken-query --bench archive_query -- RECORDING [QUERIES] [REPS] [POOL_MB]
//! ```
//!
//! - `RECORDING`: a `.dendro` archive.
//! - `QUERIES`: a file of `name|mode|query` lines, where mode is `display`
//!   (a range query at the recording's interval, reduced to 500 points, as
//!   the viewer's charts ask) or `grid` (a plain range query at a step of
//!   range / 500). Without it, the built-in list below runs.
//! - `REPS`: runs of each query, in order, in one process (default 3). The
//!   first run of a query that opens a table pays for the open.
//! - `POOL_MB`: the buffer pool budget (default 8192, the viewer's default
//!   on a large host).
//!
//! Prints, per run, the wall time and the process's resident memory after
//! it, then the peak resident memory.

use std::time::Instant;

use metriken_query::{DisplayOptions, MetricsSource, QueryOptions, Reducer};
use metriken_storage::reader::ArchiveReader;
use metriken_storage::BufferPool;
use sysinfo::{Pid, ProcessesToUpdate, System};

const DEFAULT_QUERIES: &[(&str, &str, &str)] = &[
    (
        "cpu_total",
        "display",
        "sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000",
    ),
    ("syscall_total", "display", "sum(irate(syscall[5m]))"),
    (
        "cg_syscall_by_op",
        "display",
        "sum by (op) (irate(cgroup_syscall[5m]))",
    ),
    (
        "cg_cpu_by_name",
        "display",
        "sum by (name) (irate(cgroup_cpu_usage[5m])) / 1000000000",
    ),
    (
        "task_one_pid",
        "display",
        "sum(irate(task_cpu_usage{pid=\"1\"}[5m]))",
    ),
    (
        "task_sum",
        "display",
        "sum(irate(task_cpu_usage[5m])) / 1000000000",
    ),
    (
        "cpu_by_id_heatmap",
        "grid",
        "sum by (id) (irate(cpu_usage[5m])) / 1000000000",
    ),
];

fn rss_mb(sys: &mut System) -> f64 {
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid).map(|p| p.memory()).unwrap_or(0) as f64 / (1024.0 * 1024.0)
}

fn peak_rss_mb() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    // Bytes on macOS, kilobytes on Linux.
    let raw = ru.ru_maxrss as f64;
    if cfg!(target_os = "macos") {
        raw / (1024.0 * 1024.0)
    } else {
        raw / 1024.0
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `cargo nextest` lists every bench binary with `--list`; with
    // `harness = false` there are no tests, so the listing is empty.
    if std::env::args().any(|a| a == "--list") {
        return Ok(());
    }
    // `cargo bench` passes `--bench`; skip flags.
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let path = args
        .first()
        .ok_or("usage: archive_query RECORDING [QUERIES] [REPS] [POOL_MB]")?;
    let queries: Vec<(String, String, String)> =
        match args.get(1).filter(|s| !s.is_empty() && *s != "-") {
            Some(f) => std::fs::read_to_string(f)?
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
                .map(|l| {
                    let mut p = l.splitn(3, '|');
                    (
                        p.next().unwrap_or_default().to_string(),
                        p.next().unwrap_or_default().to_string(),
                        p.next().unwrap_or_default().to_string(),
                    )
                })
                .collect(),
            None => DEFAULT_QUERIES
                .iter()
                .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
                .collect(),
        };
    let reps: usize = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(3);
    let pool_mb: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(8192);

    // With ARCHIVE_QUERY_DUMP set to a directory, each query's first result
    // is written there, one sorted series per line.
    let dump = std::env::var("ARCHIVE_QUERY_DUMP").ok();
    let mut sys = System::new();
    println!("recording {path}; pool {pool_mb} MB; {reps} runs per query");
    println!("rss_before_open_mb {:.1}", rss_mb(&mut sys));
    let t0 = Instant::now();
    let pool = BufferPool::new(pool_mb * 1024 * 1024);
    let groups = ArchiveReader::open_read_groups(std::path::Path::new(path), pool)?;
    let readers: Vec<ArchiveReader> = groups.into_iter().map(|(_, r)| r).collect();
    let reader = ArchiveReader::flatten(readers, None);
    println!("open_s {:.3}", t0.elapsed().as_secs_f64());
    println!("rss_after_open_mb {:.1}", rss_mb(&mut sys));

    let (start, end) = reader.time_range().ok_or("empty recording")?;
    let interval = reader.interval();
    let grid_step = (((end - start) / 500.0) / interval).ceil().max(1.0) * interval;
    println!("range {start:.3} {end:.3} interval {interval} grid_step {grid_step}");
    let display = DisplayOptions {
        budget: 500,
        reducer: Reducer::Boxplot,
        ..DisplayOptions::default()
    };
    let qopts = QueryOptions::default();

    println!("query,run,seconds,rss_mb,result");
    for (name, mode, q) in &queries {
        for run in 0..reps {
            let t = Instant::now();
            // Each series' Debug form, sorted, so two builds' results can be
            // compared value by value whatever order a map gave them in. The
            // time is taken before formatting.
            let (secs, outcome): (f64, Result<Vec<String>, _>) = if mode == "grid" {
                let r = reader.query_range_opts(q, start, end, grid_step, &qopts);
                let secs = t.elapsed().as_secs_f64();
                let out = r.map(|r| match r {
                    metriken_query::QueryResult::Matrix { result, .. } => {
                        result.iter().map(|s| format!("{s:?}")).collect()
                    }
                    other => vec![format!("{other:?}")],
                });
                (secs, out)
            } else {
                let r = reader.query_range_display_opts(q, start, end, interval, &display, &qopts);
                let secs = t.elapsed().as_secs_f64();
                let out = r.map(|r| match r {
                    metriken_query::DisplayResult::Series { result, .. } => {
                        result.iter().map(|s| format!("{s:?}")).collect()
                    }
                    other => vec![format!("{other:?}")],
                });
                (secs, out)
            };
            let outcome = match outcome {
                Ok(mut series) => {
                    let n = series.len();
                    if let (Some(dir), 0) = (&dump, run) {
                        series.sort();
                        std::fs::write(format!("{dir}/{name}.txt"), series.join("\n"))?;
                    }
                    format!("{n} series")
                }
                Err(e) => format!("error: {e}"),
            };
            println!("{name},{run},{secs:.4},{:.1},{outcome}", rss_mb(&mut sys));
        }
    }
    println!("peak_rss_mb {:.1}", peak_rss_mb());
    Ok(())
}
