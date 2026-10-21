//! What forming process groups costs on a real archive, beside what opening
//! it costs. Ignored; needs a recording:
//! `GROUP_COST_ARCHIVE=path.dendro cargo test --release -p metriken-storage
//! --test group_cost -- --ignored --nocapture`. `GROUP_COST_RUNS` sets the
//! number of runs (default 5); each opens a fresh reader.

use std::time::{Duration, Instant};

use metriken_storage::BufferPool;
use metriken_storage::{ArchiveReader, DendroCatalog};

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[test]
#[ignore]
fn group_cost() {
    let path = std::env::var("GROUP_COST_ARCHIVE").expect("GROUP_COST_ARCHIVE");
    let runs: usize = std::env::var("GROUP_COST_RUNS")
        .ok()
        .and_then(|r| r.parse().ok())
        .unwrap_or(5);
    let (mut opens, mut groups) = (Vec::new(), Vec::new());
    for run in 0..runs {
        let start = Instant::now();
        let recordings = ArchiveReader::from_catalog(
            Box::new(DendroCatalog::open(std::path::Path::new(&path)).unwrap()),
            None,
            BufferPool::new(256 << 20),
            None,
        )
        .unwrap();
        let reader = ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None);
        let opened = start.elapsed();
        let start = Instant::now();
        let g = reader.process_groups();
        let grouped = start.elapsed();
        println!(
            "run {run}: open {:.1} ms, group {:.3} ms: {} sources, {} uuid groups, {} process \
             groups, {} bridges, {} notices",
            opened.as_secs_f64() * 1e3,
            grouped.as_secs_f64() * 1e3,
            reader.sources().len(),
            g.uuid_groups.len(),
            g.groups.len(),
            g.bridges.len(),
            g.notices.len(),
        );
        opens.push(opened);
        groups.push(grouped);
    }
    println!(
        "median: open {:.1} ms, group {:.3} ms",
        median(opens).as_secs_f64() * 1e3,
        median(groups).as_secs_f64() * 1e3
    );
}

/// Build the archive `group_cost` reads from another: `GROUP_COST_COPIES`
/// copies (default 64) of `GROUP_COST_MINUTES` minutes (default 30) of
/// `GROUP_COST_FROM`, from its first row, written to `GROUP_COST_ARCHIVE`.
/// The copies keep the source's uuid; give them their own with SQL.
#[cfg(feature = "write")]
#[test]
#[ignore]
fn build_copies() {
    use dendro::archive::{Archive, ArchiveMut};
    use dendro::rewrite::{copy_sources_into, CopySpec};
    use metriken_storage::writer::Encoder;

    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
    let parse = |k: &str, d: u64| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let from = Archive::open(std::path::Path::new(&var("GROUP_COST_FROM"))).unwrap();
    let (copies, minutes) = (
        parse("GROUP_COST_COPIES", 64),
        parse("GROUP_COST_MINUTES", 30),
    );
    let first = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(std::path::Path::new(&var("GROUP_COST_FROM"))).unwrap()),
        None,
        BufferPool::new(64 << 20),
        None,
    )
    .unwrap()[0]
        .1
        .time_range_ns()
        .unwrap()
        .0;
    let spec = CopySpec {
        start: first as i64,
        end: (first + minutes * 60_000_000_000) as i64,
        ..CopySpec::everything()
    };
    let encoder = Encoder::for_archive(&from).unwrap();
    let mut out = ArchiveMut::create(std::path::Path::new(&var("GROUP_COST_ARCHIVE"))).unwrap();
    for _ in 0..copies {
        out.transaction(|tx| copy_sources_into(&from, tx, &spec, &encoder))
            .unwrap();
    }
}
