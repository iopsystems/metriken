//! The per-source union against an oracle: each source of a process group
//! read alone over the whole range, by the engine, and the results combined
//! point by point by S3 and S9 of
//! `docs/journal/2026-10-19-recording-identity-and-views.md`, here in the
//! test (rank by interval, then uuid, then catalog order; at each point the
//! first source that has a point and whose rows cover the window, else the
//! first that has a point and holds rows there, else none). Deterministic
//! cases where reading a later source over part of the range changed
//! points, and a seeded random set of fixtures.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, SystemTime};

use dendro::seal::SealPolicy;
use metriken_exposition::{Counter, Gauge, Snapshot, SnapshotV2};
use metriken_query::{MetricsSource, QueryOptions, QueryResult};
use metriken_storage::{ArchiveReader, ArchiveWriter, BufferPool, DendroCatalog, WriterConfig};

const MS: u64 = 1_000_000;
const S: u64 = 1_000 * MS;
const BASE: u64 = 1_700_000_000 * S;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len() as u64) as usize]
    }
}

#[derive(Clone, Debug)]
struct Src {
    rows: Vec<u64>,
    epochs: Vec<(String, u64)>,
    /// (from, to): cpu=1 absent in this interval (per-series hole).
    absent1: Option<(u64, u64)>,
    window: u64,
}

fn snap(ts: u64, src: &Src) -> Snapshot {
    let ms = (ts - BASE) / MS;
    let s = (ts - BASE) / S;
    let w = Some(metriken::Window::new(ts - src.window, ts));
    let mut counters = Vec::new();
    let mut gauges = Vec::new();
    for cpu in 0..2u64 {
        if cpu == 1 && src.absent1.is_some_and(|(a, b)| a <= ts && ts <= b) {
            continue;
        }
        let meta: HashMap<String, String> = [("metric", "c"), ("sampler", "cpu")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .chain([("cpu".to_string(), cpu.to_string())])
            .collect();
        // Nonlinear so each source's sampling gives a different rate.
        let v = ms * (cpu + 1) + (ms * ms) / 50_000;
        counters.push(Counter::new(format!("c{cpu}"), v, meta.clone()).with_window(w));
        let mut gm = meta;
        gm.insert("metric".into(), "g".into());
        gauges.push(
            Gauge::new(
                format!("g{cpu}"),
                ((ms / 37) % 1000) as i64 + cpu as i64 * 5000 + (s % 7) as i64,
                gm,
            )
            .with_window(w),
        );
    }
    Snapshot::V2(SnapshotV2 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_nanos(ts),
        duration: Duration::ZERO,
        metadata: HashMap::new(),
        counters,
        gauges,
        histograms: Vec::new(),
    })
}

fn write(path: &Path, srcs: &[Src]) {
    let config = WriterConfig {
        seal: SealPolicy {
            max_rows: 50,
            ..SealPolicy::default()
        },
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(path, config).unwrap();
    for src in srcs {
        let labels: BTreeMap<String, String> = [("host".to_string(), "a".to_string())].into();
        let mut metadata = BTreeMap::new();
        if src.epochs.len() == 1 {
            metadata.insert("producer_epoch".to_string(), src.epochs[0].0.clone());
        } else {
            let list: Vec<String> = src
                .epochs
                .iter()
                .map(|(e, f)| format!(r#"{{"epoch":"{e}","from_ts":{f}}}"#))
                .collect();
            metadata.insert(
                "producer_epochs".to_string(),
                format!("[{}]", list.join(",")),
            );
            metadata.insert(
                "producer_epoch".to_string(),
                src.epochs.last().unwrap().0.clone(),
            );
        }
        let mut source = writer.add_source(labels, metadata, BASE).unwrap();
        for &ts in &src.rows {
            let staged = source.stage(&snap(ts, src), ts, 0).unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
        }
        source.finalize((*src.rows.last().unwrap(), 0)).unwrap();
    }
    writer.join().unwrap();
}

fn open(path: &Path) -> ArchiveReader {
    let catalog = DendroCatalog::open(path).unwrap();
    let recordings =
        ArchiveReader::from_catalog(Box::new(catalog), None, BufferPool::new(64 << 20), None)
            .unwrap();
    ArchiveReader::flatten(recordings.into_iter().map(|(_, r)| r).collect(), None)
}

fn src(from_s: f64, to_s: f64, every_ms: u64, holes: &[(f64, f64)]) -> Src {
    let from = BASE + (from_s * 1e9) as u64;
    let to = BASE + (to_s * 1e9) as u64;
    let mut rows: Vec<u64> = (0..)
        .map(|i| from + i * every_ms * MS)
        .take_while(|t| *t <= to)
        .collect();
    for &(a, b) in holes {
        let (a, b) = (BASE + (a * 1e9) as u64, BASE + (b * 1e9) as u64);
        rows.retain(|t| !(a..b).contains(t));
    }
    Src {
        rows,
        epochs: vec![("e".into(), BASE)],
        absent1: None,
        window: MS,
    }
}

/// A query range: start and end in seconds, and the step.
type Window = (f64, f64, f64);

/// A series' points by labels (without `__name__`).
type Series = BTreeMap<BTreeMap<String, String>, Vec<(u64, f64)>>;

fn series(r: &ArchiveReader, q: &str, (s, e, step): (f64, f64, f64)) -> Series {
    let res = r
        .query_range_opts(q, s, e, step, &QueryOptions::default())
        .unwrap_or_else(|err| panic!("{q}: {err}"));
    let QueryResult::Matrix { result } = res else {
        panic!("{q}: not a matrix");
    };
    result
        .into_iter()
        .map(|m| {
            let labels = m
                .metric
                .into_iter()
                .filter(|(k, _)| k != "__name__")
                .collect();
            let points = m
                .values
                .into_iter()
                .map(|(t, v)| {
                    (
                        BASE + ((t - (BASE / S) as f64) * 1e6).round() as u64 * 1_000,
                        v,
                    )
                })
                .collect();
            (labels, points)
        })
        .collect()
}

/// S3's runs of a series' rows: split where a gap is longer than twice the
/// larger of the medians of the up to eight gaps before and after it.
fn runs(ts: &[u64]) -> Vec<(u64, u64)> {
    let Some(&first) = ts.first() else {
        return Vec::new();
    };
    let gaps: Vec<u64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
    let median = |g: &[u64]| -> Option<u64> {
        let mut g = g.to_vec();
        g.sort_unstable();
        (!g.is_empty()).then(|| g[g.len() / 2])
    };
    let mut out = Vec::new();
    let mut start = first;
    for i in 0..gaps.len() {
        let before = median(&gaps[i.saturating_sub(8)..i]);
        let after = median(&gaps[i + 1..(i + 9).min(gaps.len())]);
        let typical = match (before, after) {
            (Some(b), Some(a)) => b.max(a),
            (Some(x), None) | (None, Some(x)) => x,
            (None, None) => continue,
        };
        if gaps[i] > 2 * typical {
            out.push((start, ts[i]));
            start = ts[i + 1];
        }
    }
    out.push((start, *ts.last().unwrap()));
    out
}

/// What the oracle knows of one source: its points per series read alone,
/// its rows per series, interval and rank.
struct Alone {
    points: Series,
    rows: [Vec<u64>; 2],
    interval_ns: u64,
    rank: (u64, String, usize),
}

/// One source as the oracle combines it: its points of one series, the
/// runs of that series' rows, and its interval.
type Member<'a> = (&'a Vec<(u64, f64)>, Vec<(u64, u64)>, u64);

/// The window a point of `q` at a grid of `step_ns` reads, for a source of
/// `interval_ns`.
fn window(q: &str, step_ns: u64, interval_ns: u64) -> u64 {
    let range = |s: &str| -> u64 {
        let r = s.split('[').nth(1).unwrap().split(']').next().unwrap();
        r.trim_end_matches('s').parse::<u64>().unwrap() * S
    };
    if q.starts_with("irate(") {
        step_ns
    } else if q.contains('[') {
        range(q)
    } else {
        step_ns.max(interval_ns)
    }
}

/// The union of `srcs` as the oracle computes it, for the series of the
/// union `got` names (and any the sources name).
fn oracle(dir: &Path, srcs: &[Src], union: &ArchiveReader, q: &str, w: (f64, f64, f64)) -> Series {
    let infos = union.sources();
    let mut alone: Vec<Alone> = srcs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let path = dir.join(format!("alone{i}.dendro"));
            if !path.exists() {
                write(&path, std::slice::from_ref(s));
            }
            let r = open(&path);
            let rows1 = s
                .rows
                .iter()
                .copied()
                .filter(|&t| !s.absent1.is_some_and(|(a, b)| a <= t && t <= b))
                .collect();
            let interval_ns = (MetricsSource::interval(&r) * 1e9) as u64;
            Alone {
                points: series(&r, q, w),
                rows: [s.rows.clone(), rows1],
                interval_ns,
                rank: (
                    interval_ns,
                    infos[i].uuid.clone().unwrap_or_default(),
                    infos[i].catalog_index,
                ),
            }
        })
        .collect();
    alone.sort_by(|a, b| a.rank.cmp(&b.rank));
    let step_ns = (w.2 * 1e9) as u64;
    let mut out = Series::new();
    let labels: std::collections::BTreeSet<_> = alone
        .iter()
        .flat_map(|a| a.points.keys().cloned())
        .collect();
    for l in labels {
        let cpu: usize = l["cpu"].parse().unwrap();
        let members: Vec<Member<'_>> = alone
            .iter()
            .filter_map(|a| {
                a.points
                    .get(&l)
                    .map(|p| (p, runs(&a.rows[cpu]), a.interval_ns))
            })
            .collect();
        let mut times: Vec<u64> = members
            .iter()
            .flat_map(|(p, _, _)| p.iter().map(|x| x.0))
            .collect();
        times.sort_unstable();
        times.dedup();
        let mut points = Vec::new();
        for t in times {
            let run_at = |runs: &[(u64, u64)], iv: u64| {
                runs.iter().find(|r| r.0 <= t && t <= r.1 + iv).copied()
            };
            let at = |p: &Vec<(u64, f64)>| p.iter().find(|x| x.0 == t).map(|x| x.1);
            let covers = |(p, runs, iv): &Member<'_>| {
                at(p).is_some()
                    && run_at(runs, *iv).is_some_and(|r| {
                        t.checked_sub(window(q, step_ns, *iv))
                            .is_some_and(|s| s >= r.0)
                    })
            };
            let holds = |(p, runs, iv): &Member<'_>| at(p).is_some() && run_at(runs, *iv).is_some();
            let pick = members
                .iter()
                .find(|m| covers(m))
                .or_else(|| members.iter().find(|m| holds(m)));
            if let Some((p, _, _)) = pick {
                points.push((t, at(p).unwrap()));
            }
        }
        if !points.is_empty() {
            out.insert(l, points);
        }
    }
    out
}

/// The differences between the union and the oracle, for a message.
fn differences(got: &Series, want: &Series) -> Vec<String> {
    let mut out = Vec::new();
    for l in got
        .keys()
        .chain(want.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let (g, w) = (got.get(l), want.get(l));
        let (g, w) = (
            g.cloned().unwrap_or_default(),
            w.cloned().unwrap_or_default(),
        );
        if g.len() != w.len() {
            out.push(format!("{l:?}: {} points against {}", g.len(), w.len()));
            continue;
        }
        for (a, b) in g.iter().zip(&w) {
            let close = a.0 == b.0
                && ((a.1.is_nan() && b.1.is_nan())
                    || (a.1 - b.1).abs() <= 1e-9 * a.1.abs().max(b.1.abs()).max(1.0));
            if !close {
                out.push(format!(
                    "{l:?} at {:.3}: {} against {}",
                    (a.0 - BASE) as f64 / 1e9,
                    a.1,
                    b.1
                ));
                break;
            }
        }
    }
    out
}

const QUERIES: &[&str] = &[
    "rate(c[1s])",
    "rate(c[5s])",
    "rate(c[30s])",
    "irate(c[5s])",
    "g",
    "deriv(g[20s])",
    "avg_over_time(g[10s])",
    "idelta(g[10s])",
    "deriv(c[10s])",
];

/// Cases where a later source read over part of the range changed points: a
/// 250 ms source with a hole over [200, 210) s beside a 2 s source, and a
/// 1 s source with a hole over [200, 230) s beside a 10 s source. At the
/// grid times the first does not cover, the union is the coarser source
/// alone, read over the whole range.
#[test]
fn a_later_source_answers_as_it_does_alone() {
    let dir = tempfile::tempdir().unwrap();
    let b = (BASE / S) as f64;
    for (name, fine, coarse, q, w, ts) in [
        (
            "250 ms hole beside 2 s",
            src(100.0, 300.0, 250, &[(200.0, 210.0)]),
            src(100.0, 300.0, 2000, &[]),
            "rate(c[1s])",
            (150.0, 260.0, 0.25),
            vec![210.25, 210.5, 210.75],
        ),
        (
            "1 s hole beside 10 s",
            src(100.0, 300.0, 1000, &[(200.0, 230.0)]),
            src(100.0, 300.0, 10000, &[]),
            "rate(c[30s])",
            (150.0, 260.0, 0.5),
            vec![250.5, 251.0, 255.0],
        ),
    ] {
        let u = dir.path().join(format!("{name}-u.dendro"));
        write(&u, &[fine.clone(), coarse.clone()]);
        let c = dir.path().join(format!("{name}-c.dendro"));
        write(&c, &[coarse]);
        let w = (b + w.0, b + w.1, w.2);
        let cpu0 = |s: &Series| {
            s.iter()
                .find(|(l, _)| l.get("cpu").map(String::as_str) == Some("0"))
                .map(|(_, p)| p.clone())
                .unwrap()
        };
        let union = cpu0(&series(&open(&u), q, w));
        let alone = cpu0(&series(&open(&c), q, w));
        for t in ts {
            let t = BASE + (t * 1e6_f64).round() as u64 * 1_000;
            let got = union.iter().find(|p| p.0 == t).map(|p| p.1);
            let want = alone.iter().find(|p| p.0 == t).map(|p| p.1);
            assert!(want.is_some(), "{name}: the coarse source has a point");
            assert_eq!(got, want, "{name} {q} at {:.2}", (t - BASE) as f64 / 1e9);
        }
    }
}

/// The reviewer's deterministic fixtures, each against the oracle.
#[test]
fn deterministic_cases_match_the_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let b = (BASE / S) as f64;
    let lacks1 = |mut s: Src, from: u64, to: u64| {
        s.absent1 = Some((BASE + from * S, BASE + to * S));
        s
    };
    let cases: Vec<(&str, Vec<Src>, Window)> = vec![
        (
            "250 ms hole beside 2 s",
            vec![
                src(100.0, 300.0, 250, &[(200.0, 210.0)]),
                src(100.0, 300.0, 2000, &[]),
            ],
            (150.0, 260.0, 0.25),
        ),
        (
            "1 s hole beside 1 s offset",
            vec![
                src(100.0, 300.0, 1000, &[(200.0, 210.0)]),
                src(100.5, 300.5, 1000, &[]),
            ],
            (150.0, 260.0, 1.0),
        ),
        (
            "dump before record",
            vec![src(200.0, 300.0, 1000, &[]), src(100.0, 300.0, 1000, &[])],
            (100.0, 300.0, 1.0),
        ),
        (
            "1 s hole beside 10 s",
            vec![
                src(100.0, 300.0, 1000, &[(200.0, 230.0)]),
                src(100.0, 300.0, 10000, &[]),
            ],
            (150.0, 260.0, 0.5),
        ),
        (
            "first lacks a series",
            vec![
                lacks1(src(100.0, 300.0, 500, &[]), 140, 280),
                src(100.0, 300.0, 1000, &[]),
            ],
            (150.0, 260.0, 1.0),
        ),
    ];
    for (i, (name, srcs, w)) in cases.into_iter().enumerate() {
        let sub = dir.path().join(format!("c{i}"));
        std::fs::create_dir_all(&sub).unwrap();
        let path = sub.join("union.dendro");
        write(&path, &srcs);
        let union = open(&path);
        assert_eq!(union.read_groups().len(), 1, "{name}");
        let w = (b + w.0, b + w.1, w.2);
        for q in QUERIES {
            let got = series(&union, q, w);
            let want = oracle(&sub, &srcs, &union, q, w);
            let d = differences(&got, &want);
            assert!(d.is_empty(), "{name} {q}: {d:?}");
        }
    }
}

/// Seeded random fixtures of two or three sources of one process (1 s,
/// 250 ms, 500 ms or 2 s rows, offsets, holes, a series absent for a while),
/// each query at four windows and steps, against the oracle. `UNION_ORACLE_N`
/// sets the number of fixtures (default 12) and `UNION_ORACLE_SEED` the seed.
#[test]
fn random_fixtures_match_the_oracle() {
    let n: u64 = std::env::var("UNION_ORACLE_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    let seed: u64 = std::env::var("UNION_ORACLE_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let dir = tempfile::tempdir().unwrap();
    let b = (BASE / S) as f64;
    let mut failures = Vec::new();
    let mut checked = 0;
    for f in 0..n {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (seed * 1_000_003 + f));
        let count = 2 + rng.below(2) as usize;
        let srcs: Vec<Src> = (0..count)
            .map(|_| {
                let every = rng.pick(&[250 * MS, 500 * MS, S, 2 * S]);
                let offset = rng.pick(&[0, 137 * MS, 500 * MS, 0]);
                let from = BASE + rng.below(120) * S + offset;
                let to = from + (30 + rng.below(240)) * S;
                let mut rows: Vec<u64> = (0..)
                    .map(|i| from + i * every)
                    .take_while(|t| *t <= to)
                    .collect();
                for _ in 0..rng.below(3) {
                    let h0 = from + rng.below(to - from);
                    let h1 = h0 + (2 + rng.below(40)) * S;
                    rows.retain(|t| !(h0..h1).contains(t));
                }
                if rows.len() < 3 {
                    rows = vec![from, from + every, from + 2 * every];
                }
                let absent1 = (f % 3 == 1 && rng.below(2) == 0).then(|| {
                    let a = from + rng.below(to - from);
                    (a, a + (5 + rng.below(100)) * S)
                });
                Src {
                    rows,
                    epochs: vec![("e".to_string(), BASE)],
                    absent1,
                    window: rng.pick(&[MS, 10 * MS]),
                }
            })
            .collect();
        let sub = dir.path().join(format!("f{f}"));
        std::fs::create_dir_all(&sub).unwrap();
        let path = sub.join("union.dendro");
        write(&path, &srcs);
        let union = open(&path);
        if union.read_groups().len() != 1 {
            continue;
        }
        for _ in 0..4 {
            let step = rng.pick(&[0.25, 0.5, 1.0, 2.0, 3.0]);
            let start = b + rng.below(200) as f64 + rng.pick(&[0.0, 0.0, 0.3, 0.75]);
            let end = start + 10.0 + rng.below(300) as f64;
            for q in QUERIES {
                let got = series(&union, q, (start, end, step));
                let want = oracle(&sub, &srcs, &union, q, (start, end, step));
                checked += 1;
                let d = differences(&got, &want);
                if !d.is_empty() {
                    failures.push(format!(
                        "fixture {f} {q} [{:.2}, {:.2}] step {step}: {:?}",
                        start - b,
                        end - b,
                        &d[..d.len().min(2)]
                    ));
                }
            }
        }
    }
    assert!(checked > 200, "only {checked} queries checked");
    assert!(
        failures.is_empty(),
        "{} of {checked} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
