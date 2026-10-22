//! A differential harness: every answer a reader gives for an archive,
//! written to a file, so two builds can be compared byte for byte. Ignored;
//! it only uses API that predates process groups, so the same file builds
//! against an older tree.
//!
//! `ANSWERS_GEN_DIR=dir cargo test -p metriken-storage --all-features --test
//! answers_dump answers_gen -- --ignored` writes the fixtures (sources that
//! form no process group of more than one: one source; two hosts; twins of
//! two processes; an epochless source at 100 ms; epochless twins over
//! disjoint spans; one epoch over disjoint spans), and under `control/` a
//! source and its copy, which must answer differently.
//!
//! `ANSWERS_ARCHIVE=path.dendro ANSWERS_OUT=file cargo test --release -p
//! metriken-storage --all-features --test answers_dump answers_dump --
//! --ignored` writes, for every metric: range queries of rate, irate, sum,
//! sum by, a gauge read, `avg_over_time`, histogram functions, on the
//! batched, per-series, raw and display paths, over the whole archive and
//! over a 10-minute and a 1-minute window; instant queries; and the series
//! listings. `ANSWERS_MAX_METRICS` caps the metrics of each kind.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime};

use dendro::seal::SealPolicy;
use metriken_exposition::{Counter, Gauge, Snapshot, SnapshotV2};
use metriken_query::{DisplayOptions, MetricsSource, QueryOptions, RateMode};
use metriken_storage::{ArchiveReader, ArchiveWriter, BufferPool, DendroCatalog, WriterConfig};

const MS: u64 = 1_000_000;
const S: u64 = 1_000 * MS;
const BASE: u64 = 1_700_000_000 * S;

fn snapshot(ts: u64, host_scale: u64) -> Snapshot {
    let meta = |name: &str, id: &str| -> HashMap<String, String> {
        [("metric", name), ("sampler", "cpu"), ("id", id)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let window = Some(metriken::Window::new(ts - MS, ts));
    let s = (ts - BASE) / S;
    let mut counters = Vec::new();
    let mut gauges = Vec::new();
    let mut histograms = Vec::new();
    for id in 0..3u64 {
        let ids = id.to_string();
        counters.push(
            Counter::new(
                format!("cpu_cycles/{id}"),
                host_scale * (id + 1) * (ts - BASE) / MS + (s % 7) * 13,
                meta("cpu_cycles", &ids),
            )
            .with_window(window),
        );
        gauges.push(
            Gauge::new(
                format!("g/{id}"),
                (s % 11) as i64 + id as i64,
                meta("g", &ids),
            )
            .with_window(window),
        );
    }
    let mut h = histogram::Histogram::new(4, 20).unwrap();
    h.add(1000, 9 * s).unwrap();
    h.add(50_000 + (s % 5) * 1000, s).unwrap();
    let mut m = meta("latency", "0");
    m.insert("grouping_power".to_string(), "4".to_string());
    m.insert("max_value_power".to_string(), "20".to_string());
    histograms.push(metriken_exposition::Histogram::new(
        "latency".to_string(),
        h,
        m,
    ));
    Snapshot::V2(SnapshotV2 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::ZERO,
        metadata: HashMap::new(),
        counters,
        gauges,
        histograms,
        gauge_histograms: Vec::new(),
    })
}

struct Spec {
    labels: &'static [(&'static str, &'static str)],
    epoch: Option<&'static str>,
    from: u64,
    to: u64,
    every: u64,
    scale: u64,
}

fn write(path: &Path, specs: &[Spec]) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 100,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    for spec in specs {
        let labels: BTreeMap<String, String> = spec
            .labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut metadata = BTreeMap::new();
        if let Some(e) = spec.epoch {
            metadata.insert("producer_epoch".to_string(), e.to_string());
        }
        let mut source = writer.add_source(labels, metadata, BASE).unwrap();
        let mut ts = spec.from;
        while ts <= spec.to {
            let staged = source.stage(&snapshot(ts, spec.scale), ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
            ts += spec.every;
        }
        source.finalize((spec.to, 0)).unwrap();
    }
    writer.join().unwrap();
}

#[test]
#[ignore]
fn answers_gen() {
    let dir = std::path::PathBuf::from(std::env::var("ANSWERS_GEN_DIR").expect("ANSWERS_GEN_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let m = 60 * S;
    let one = |labels, epoch, from, to, every, scale| Spec {
        labels,
        epoch,
        from,
        to,
        every,
        scale,
    };
    let host_a: &[(&str, &str)] = &[("host", "a"), ("source", "rezolus")];
    let host_b: &[(&str, &str)] = &[("host", "b"), ("source", "rezolus")];
    write(
        &dir.join("one.dendro"),
        &[one(host_a, Some("e"), BASE, BASE + 10 * m, S, 1)],
    );
    write(
        &dir.join("hosts.dendro"),
        &[
            one(host_a, Some("e1"), BASE, BASE + 10 * m, S, 1),
            one(host_b, Some("e2"), BASE + 2 * m, BASE + 12 * m, S, 2),
        ],
    );
    write(
        &dir.join("twins.dendro"),
        &[
            one(host_a, Some("e1"), BASE, BASE + 10 * m, S, 1),
            one(host_a, Some("e2"), BASE + m, BASE + 9 * m, S, 3),
        ],
    );
    write(
        &dir.join("fine.dendro"),
        &[one(host_a, None, BASE, BASE + 2 * m, 100 * MS, 1)],
    );
    write(
        &dir.join("epochless_twins.dendro"),
        &[
            one(host_a, None, BASE, BASE + 5 * m, S, 1),
            one(host_a, None, BASE + 6 * m, BASE + 10 * m, S, 1),
        ],
    );
    // The control, which must answer differently: a source and its copy.
    {
        use dendro::archive::{Archive, ArchiveMut};
        use dendro::rewrite::{copy_sources_into, CopySpec};
        let src = Archive::open(&dir.join("one.dendro")).unwrap();
        let encoder = metriken_storage::writer::Encoder::for_archive(&src).unwrap();
        std::fs::create_dir_all(dir.join("control")).unwrap();
        let mut out = ArchiveMut::create(&dir.join("control").join("copies.dendro")).unwrap();
        for _ in 0..2 {
            out.transaction(|tx| copy_sources_into(&src, tx, &CopySpec::everything(), &encoder))
                .unwrap();
        }
    }
    write(
        &dir.join("one_epoch_apart.dendro"),
        &[
            one(host_a, Some("e"), BASE, BASE + 4 * m, S, 1),
            one(host_a, Some("e"), BASE + 5 * m, BASE + 8 * m, S, 1),
            one(host_b, Some("e"), BASE + 9 * m, BASE + 12 * m, S, 1),
        ],
    );
}

fn open(path: &Path) -> ArchiveReader {
    let catalog = DendroCatalog::open(path).unwrap();
    let recordings =
        ArchiveReader::from_catalog(Box::new(catalog), None, BufferPool::new(256 << 20), None)
            .unwrap();
    ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None)
}

/// `value` as JSON with every object's keys sorted and a result's series
/// sorted by their labels: labels are a `HashMap`, and an aggregate's groups
/// come out of one, so both orders change from run to run.
fn canonical(value: &impl serde::Serialize) -> String {
    let mut v = serde_json::to_value(value).unwrap();
    if let Some(serde_json::Value::Array(series)) = v.get_mut("result") {
        series.sort_by_key(|s| s.get("metric").map(|m| m.to_string()).unwrap_or_default());
    }
    v.to_string()
}

#[test]
#[ignore]
fn answers_dump() {
    let path = std::env::var("ANSWERS_ARCHIVE").expect("ANSWERS_ARCHIVE");
    let out = std::env::var("ANSWERS_OUT").expect("ANSWERS_OUT");
    let cap: usize = std::env::var("ANSWERS_MAX_METRICS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let reader = open(Path::new(&path));
    let mut out = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
    let (start, end) = MetricsSource::time_range(&reader).unwrap();
    let span = end - start;
    let full_step = (span / 600.0).max(1.0).round();
    let mid = start + span / 2.0;
    let windows = [
        (start, end, full_step),
        (mid, (mid + 600.0).min(end), 1.0),
        (mid, (mid + 60.0).min(end), 0.1),
    ];
    let mut emit = |line: String| writeln!(out, "{line}").unwrap();
    let show = |r: Result<String, metriken_query::QueryError>| match r {
        Ok(s) => s,
        Err(e) => format!("error: {e}"),
    };
    // A metric with many series (a long table's occupants) is read whole on
    // the batched grid path only, the one that scales, and one of its series
    // on every path.
    const WIDE: usize = 20;
    let one_series = |name: &str, labels: &[BTreeMap<String, String>]| -> Option<String> {
        let first = labels.first().filter(|_| labels.len() > WIDE)?;
        let matchers: Vec<String> = first
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect();
        Some(format!("{name}{{{}}}", matchers.join(", ")))
    };
    // Each query, and whether it runs on every path.
    let mut queries: Vec<(String, bool)> = Vec::new();
    for name in reader.counter_names().iter().take(cap) {
        let one = one_series(name, &reader.counter_labels(name));
        let every = one.is_none();
        for (sel, all) in [(name.clone(), every)]
            .into_iter()
            .chain(one.map(|o| (o, true)))
        {
            queries.push((format!("rate({sel}[1m])"), all));
            queries.push((format!("irate({sel}[5s])"), all));
            queries.push((format!("sum(rate({sel}[1m]))"), all));
            queries.push((format!("sum by (id) (irate({sel}[10s]))"), all));
            queries.push((format!("deriv({sel}[1m])"), all));
        }
    }
    for name in reader.gauge_names().iter().take(cap) {
        let one = one_series(name, &reader.gauge_labels(name));
        let every = one.is_none();
        for (sel, all) in [(name.clone(), every)]
            .into_iter()
            .chain(one.map(|o| (o, true)))
        {
            queries.push((sel.clone(), all));
            queries.push((format!("sum({sel})"), all));
            queries.push((format!("avg_over_time({sel}[1m])"), all));
            queries.push((format!("idelta({sel}[1m])"), all));
        }
    }
    for name in reader.histogram_names().iter().take(cap) {
        let every = reader.histogram_labels(name).len() <= WIDE;
        queries.push((format!("histogram_quantiles([0.5, 0.99], {name})"), every));
        queries.push((
            format!("histogram_quantiles([0.5, 0.99], {name}, 30)"),
            every,
        ));
        queries.push((format!("histogram_count({name})"), every));
        queries.push((format!("histogram_heatmap({name})"), every));
    }
    let raw = QueryOptions::with_rate_mode(RateMode::Raw);
    let per_series = QueryOptions::default().with_per_series_rates(true);
    let display = DisplayOptions {
        budget: 500,
        ..DisplayOptions::default()
    };
    for (q, every) in &queries {
        for (i, &(s, e, step)) in windows.iter().enumerate() {
            let json = |r: metriken_query::QueryResult| canonical(&r);
            emit(format!(
                "{q}\tw{i}\tgrid\t{}",
                show(
                    reader
                        .query_range_opts(q, s, e, step, &QueryOptions::default())
                        .map(json)
                )
            ));
            if !every {
                continue;
            }
            emit(format!(
                "{q}\tw{i}\tper_series\t{}",
                show(
                    reader
                        .query_range_opts(q, s, e, step, &per_series)
                        .map(json)
                )
            ));
            emit(format!(
                "{q}\tw{i}\traw\t{}",
                show(reader.query_range_opts(q, s, e, step, &raw).map(json))
            ));
            emit(format!(
                "{q}\tw{i}\tdisplay\t{}",
                show(
                    reader
                        .query_range_display_opts(q, s, e, step, &display, &QueryOptions::default())
                        .map(|r| canonical(&r))
                )
            ));
        }
        for t in [None, Some(mid)] {
            emit(format!(
                "{q}\tinstant {t:?}\t{}",
                show(reader.query(q, t).map(|r| canonical(&r)))
            ));
        }
    }
    for name in reader.counter_names() {
        emit(format!(
            "labels counter {name}\t{:?}",
            reader.counter_labels(&name)
        ));
    }
    for name in reader.gauge_names() {
        emit(format!(
            "labels gauge {name}\t{:?}",
            reader.gauge_labels(&name)
        ));
    }
    for name in reader.histogram_names() {
        emit(format!(
            "labels histogram {name}\t{:?}",
            reader.histogram_labels(&name)
        ));
    }
}
