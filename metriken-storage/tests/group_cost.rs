//! What forming process groups costs on a real archive, beside what opening
//! it costs. Ignored; needs a recording:
//! `GROUP_COST_ARCHIVE=path.dendro cargo test --release -p metriken-storage
//! --test group_cost group_cost -- --ignored --nocapture`. `GROUP_COST_RUNS`
//! sets the number of runs (default 5); each opens a fresh reader.
//!
//! `form_scale` times `process::form` alone on synthetic sources, 1000 and
//! 3000 of each shape (`GROUP_COST_N` for one count): a fleet (one shared
//! span, an epoch each), one agent recorded n times at once (one epoch, every
//! pair checked by R3), and a staircase (each overlapping the next,
//! epochless).

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

#[test]
#[ignore]
fn form_scale() {
    use std::collections::BTreeMap;

    use metriken_storage::process::{form, SourceInfo};

    let source = |i: usize, span: (u64, u64), epoch: Option<String>| {
        let metadata: BTreeMap<String, String> = epoch
            .map(|e| ("producer_epoch".to_string(), e))
            .into_iter()
            .collect();
        let labels = [("host".to_string(), format!("h{i}"))].into();
        SourceInfo::new(labels, i, Some(format!("u{i}")), Some(span), &metadata)
    };
    let counts: Vec<usize> = match std::env::var("GROUP_COST_N")
        .ok()
        .and_then(|n| n.parse().ok())
    {
        Some(n) => vec![n],
        None => vec![1000, 3000],
    };
    for n in counts {
        for shape in ["fleet", "one epoch", "staircase"] {
            let sources: Vec<SourceInfo> = (0..n)
                .map(|i| match shape {
                    "fleet" => source(i, (0, 1_000), Some(format!("e{i}"))),
                    "one epoch" => source(i, (0, 1_000), Some("e".to_string())),
                    _ => source(i, (i as u64 * 10, i as u64 * 10 + 15), None),
                })
                .collect();
            let mut calls = 0u64;
            let start = Instant::now();
            let g = form(&sources, &mut |_, _, _| {
                calls += 1;
                Some(true)
            });
            println!(
                "n={n} {shape}: form {:.1} ms, {} process groups, {} bridges, {calls} row checks",
                start.elapsed().as_secs_f64() * 1e3,
                g.groups.len(),
                g.bridges.len(),
            );
        }
    }
}
