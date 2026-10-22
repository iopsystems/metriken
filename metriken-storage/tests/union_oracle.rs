//! The per-source union against an oracle: each source of a process group
//! (each producer epoch of it) read alone over the whole range by the
//! engine, and the results combined point by point by S3 and S9 of
//! `docs/journal/2026-10-19-recording-identity-and-views.md`, here in the
//! test: rank by the metric's own table spacing, then uuid (a source with
//! none last), then catalog order; at each point the first source that has a
//! point and whose rows cover the window, else the first that has a point
//! and holds rows there, else none. Deterministic cases where an earlier
//! version answered differently, and seeded random fixtures with holes,
//! cadence changes, cadences from 250 ms to 15 s, a faster second table, a
//! series absent for a while.
#![cfg(feature = "write")]

use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    /// rows of the cpu sampler (c, g)
    rows: Vec<u64>,
    /// rows of a fast sampler (z only), may be empty
    fast: Vec<u64>,
    epochs: Vec<(String, u64)>,
    /// Where `cpu=1` has no rows, as inclusive spans.
    absent: Vec<(u64, u64)>,
    window: u64,
}

fn epoch_start(src: &Src, ts: u64) -> u64 {
    src.epochs
        .iter()
        .rev()
        .find(|(_, f)| *f <= ts)
        .map(|e| e.1)
        .unwrap_or(BASE)
}

fn snap(ts: u64, src: &Src, cpu_row: bool, fast_row: bool) -> Snapshot {
    let e0 = epoch_start(src, ts);
    let ms = (ts - e0) / MS;
    let s = (ts - BASE) / S;
    let w = Some(metriken::Window::new(ts - src.window, ts));
    let mut counters = Vec::new();
    let mut gauges = Vec::new();
    if cpu_row {
        for cpu in 0..2u64 {
            if cpu == 1 && src.absent.iter().any(|&(a, b)| a <= ts && ts <= b) {
                continue;
            }
            let meta: HashMap<String, String> = [("metric", "c"), ("sampler", "cpu")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .chain([("cpu".to_string(), cpu.to_string())])
                .collect();
            let v = ms * (cpu + 1) + (ms * ms) / 50_000;
            counters.push(Counter::new(format!("c{cpu}"), v, meta.clone()).with_window(w));
            let mut gm = meta;
            gm.insert("metric".into(), "g".into());
            let gms = (ts - BASE) / MS;
            gauges.push(
                Gauge::new(
                    format!("g{cpu}"),
                    ((gms / 37) % 1000) as i64 + cpu as i64 * 5000 + (s % 7) as i64,
                    gm,
                )
                .with_window(w),
            );
        }
    }
    if fast_row {
        let meta: HashMap<String, String> = [("metric", "z"), ("sampler", "fast")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        counters.push(Counter::new("z".to_string(), ms, meta).with_window(w));
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
        let ticks: BTreeSet<u64> = src.rows.iter().chain(&src.fast).copied().collect();
        let rows: BTreeSet<u64> = src.rows.iter().copied().collect();
        let fast: BTreeSet<u64> = src.fast.iter().copied().collect();
        for &ts in &ticks {
            let staged = source
                .stage(
                    &snap(ts, src, rows.contains(&ts), fast.contains(&ts)),
                    ts,
                    0,
                )
                .unwrap();
            writer.commit(vec![staged]).unwrap();
            source.maybe_seal().unwrap();
        }
        source.finalize((*ticks.last().unwrap(), 0)).unwrap();
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

type Series = BTreeMap<BTreeMap<String, String>, Vec<(u64, f64)>>;

fn to_ns(t: f64) -> u64 {
    BASE + ((t - (BASE / S) as f64) * 1e6).round() as u64 * 1_000
}
fn matrix(res: QueryResult) -> Series {
    match res {
        QueryResult::Matrix { result } => result
            .into_iter()
            .map(|m| {
                (
                    m.metric
                        .into_iter()
                        .filter(|(k, _)| k != "__name__")
                        .collect(),
                    m.values.into_iter().map(|(t, v)| (to_ns(t), v)).collect(),
                )
            })
            .collect(),
        QueryResult::Vector { result } => result
            .into_iter()
            .map(|m| {
                (
                    m.metric
                        .into_iter()
                        .filter(|(k, _)| k != "__name__")
                        .collect(),
                    vec![(to_ns(m.value.0), m.value.1)],
                )
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn range(
    r: &ArchiveReader,
    q: &str,
    (s, e, step): (f64, f64, f64),
    ps: bool,
) -> Result<Series, String> {
    let o = QueryOptions::default().with_per_series_rates(ps);
    r.query_range_opts(q, s, e, step, &o)
        .map(matrix)
        .map_err(|e| e.to_string())
}
/// Independent S3 runs.
fn runs(ts: &[u64]) -> Vec<(u64, u64)> {
    if ts.is_empty() {
        return vec![];
    }
    let gaps: Vec<u64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
    let med = |g: &[u64]| {
        if g.is_empty() {
            None
        } else {
            let mut v = g.to_vec();
            v.sort();
            Some(v[v.len() / 2])
        }
    };
    let mut out = vec![];
    let mut st = ts[0];
    for i in 0..gaps.len() {
        let b = med(&gaps[i.saturating_sub(8)..i]);
        let a = med(&gaps[i + 1..gaps.len().min(i + 9)]);
        let typ = match (b, a) {
            (Some(x), Some(y)) => x.max(y),
            (Some(x), None) | (None, Some(x)) => x,
            _ => continue,
        };
        if gaps[i] > 2 * typ {
            out.push((st, ts[i]));
            st = ts[i + 1];
        }
    }
    out.push((st, *ts.last().unwrap()));
    out
}

struct Q {
    text: &'static str,
    /// range in ns of the matrix selector; None for a plain selector
    range: Option<u64>,
    irate: bool,
    counter: bool,
}

const QS: &[&str] = &[
    "rate(c[1s])",
    "rate(c[5s])",
    "rate(c[30s])",
    "rate(c[2m])",
    "irate(c[5s])",
    "irate(c[30s])",
    "g",
    "deriv(g[20s])",
    "avg_over_time(g[10s])",
    "idelta(g[10s])",
    "deriv(c[10s])",
];

fn parse(q: &'static str) -> Q {
    let dur = |s: &str| -> u64 {
        if let Some(m) = s.strip_suffix('m') {
            m.parse::<u64>().unwrap() * 60 * S
        } else {
            s.trim_end_matches('s').parse::<u64>().unwrap() * S
        }
    };
    let range = q
        .split('[')
        .nth(1)
        .map(|r| dur(r.split(']').next().unwrap()));
    let counter = q.contains("(c[");
    Q {
        text: q,
        range,
        irate: q.starts_with("irate("),
        counter,
    }
}

struct MemberRef {
    points: Series,
    rows: [Vec<u64>; 2],
    iv: u64,
    rank: (u64, Option<String>, usize, usize, u64),
}

/// A member's cpu rows clipped to [lo, hi].
fn clip(s: &Src, lo: u64, hi: u64) -> Src {
    let mut c = s.clone();
    c.rows.retain(|&t| lo <= t && t <= hi);
    c
}

/// The rows of the engine's read of `q` over `w` for a member of
/// `interval_ns` (its read starts as far before the range as the function
/// looks back), as `[first, last]`.
fn read_span(q: &Q, (s, e, step): (f64, f64, f64), interval_ns: u64) -> (u64, u64) {
    let step_ns = (step * 1e9) as u64;
    let start = ((s * 1e9).round() as u64 / step_ns) * step_ns;
    let end = (e * 1e9).round() as u64 + 1_000;
    let back = match q.range {
        // `rate`: its window and two reaches of two intervals.
        Some(r) if !q.irate && q.text.starts_with("rate(") => r + 4 * interval_ns,
        // `irate`: its window or a step.
        Some(r) if q.irate => r.max(step_ns),
        // `avg_over_time`, `idelta`, `deriv`: their window.
        Some(r) => r,
        // A gauge read: a step, or the interval when that is longer.
        None => step_ns.max(interval_ns),
    };
    (start.saturating_sub(back), end)
}

/// `rows` as the engine sees them for a read of `[lo, hi]`: every row in it,
/// and on each side the nearest nine rows within 64 intervals (S3's bound
/// on the rows a coverage decision reads beyond a read).
fn bounded(rows: &[u64], lo: u64, hi: u64, interval_ns: u64, intervals: u64) -> Vec<u64> {
    let reach = interval_ns.max(1).saturating_mul(intervals);
    let before: Vec<u64> = rows
        .iter()
        .copied()
        .filter(|&t| t < lo && t >= lo.saturating_sub(reach))
        .collect();
    let after: Vec<u64> = rows
        .iter()
        .copied()
        .filter(|&t| t > hi && t <= hi.saturating_add(reach))
        .take(9)
        .collect();
    let mut out: Vec<u64> = before[before.len().saturating_sub(9)..].to_vec();
    out.extend(rows.iter().copied().filter(|&t| lo <= t && t <= hi));
    out.extend(after);
    out
}

/// [`reference_within`] with the engine's bound: nine rows within 64
/// intervals on each side of a read.
fn reference(
    dir: &Path,
    srcs: &[Src],
    union: &ArchiveReader,
    q: &Q,
    w: ((f64, f64, f64), bool),
    cache: &mut HashMap<String, ArchiveReader>,
) -> Series {
    reference_within(dir, srcs, union, q, w, cache, 64)
}

/// One member as the oracle combines it: its points of one series, the
/// runs of that series' rows, and its interval.
type Combined<'a> = (&'a Vec<(u64, f64)>, Vec<(u64, u64)>, u64);

/// The oracle's answer for `q` over `srcs` in `w`, on the per-series path
/// when `ps`; `union` supplies the sources' uuids, catalog indices and
/// epochs, `cache` the archives of each source and member alone. A member's
/// rows beyond its read count as the engine reads them: the nearest nine
/// within `reach` of the member's intervals on each side.
#[allow(clippy::too_many_arguments)]
fn reference_within(
    dir: &Path,
    srcs: &[Src],
    union: &ArchiveReader,
    q: &Q,
    (w, ps): ((f64, f64, f64), bool),
    cache: &mut HashMap<String, ArchiveReader>,
    reach: u64,
) -> Series {
    let infos = union.sources().to_vec();
    let mut members: Vec<MemberRef> = Vec::new();
    for (i, s) in srcs.iter().enumerate() {
        let info = &infos[i];
        let whole_key = format!("w{i}");
        if !cache.contains_key(&whole_key) {
            let p = dir.join(format!("{whole_key}.dendro"));
            write(&p, std::slice::from_ref(s));
            cache.insert(whole_key.clone(), open(&p));
        }
        let iv = {
            let w = &cache[&whole_key];
            let name = if q.counter { "c" } else { "g" };
            let owner = w.owners_of(&[name.to_string()])[0];
            let f = owner.interval();
            (f * 1e9) as u64
        };
        let n = info.epochs.len().max(1);
        for k in 0..n {
            let lo = if k == 0 { 0 } else { info.epochs[k].first_ns };
            let hi = info
                .epochs
                .get(k + 1)
                .map(|e| e.first_ns - 1)
                .unwrap_or(u64::MAX);
            let m = clip(s, lo, hi);
            if m.rows.is_empty() {
                continue;
            }
            let key = format!("m{i}-{k}");
            if !cache.contains_key(&key) {
                let p = dir.join(format!("{key}.dendro"));
                write(&p, std::slice::from_ref(&m));
                cache.insert(key.clone(), open(&p));
            }
            let r = &cache[&key];
            // A member clipped to one epoch of its source is read on the
            // per-series path by the engine (a scan cannot be clipped), so
            // the oracle reads it there too.
            let points = range(r, q.text, w, ps || n > 1).unwrap_or_default();
            let rows1 = m
                .rows
                .iter()
                .copied()
                .filter(|&t| !m.absent.iter().any(|&(a, b)| a <= t && t <= b))
                .collect();
            members.push(MemberRef {
                points,
                rows: [m.rows.clone(), rows1],
                iv,
                rank: (iv, info.uuid.clone(), info.catalog_index, i, lo),
            });
        }
    }
    members.sort_by(|a, b| {
        let ua = (a.rank.1.is_none(), a.rank.1.clone());
        let ub = (b.rank.1.is_none(), b.rank.1.clone());
        a.rank
            .0
            .cmp(&b.rank.0)
            .then(ua.cmp(&ub))
            .then(a.rank.2.cmp(&b.rank.2))
            .then(a.rank.3.cmp(&b.rank.3))
            .then(a.rank.4.cmp(&b.rank.4))
    });
    let step_ns = (w.2 * 1e9) as u64;
    let labels: BTreeSet<_> = members
        .iter()
        .flat_map(|m| m.points.keys().cloned())
        .collect();
    let mut out = Series::new();
    for l in labels {
        let cpu: usize = l["cpu"].parse().unwrap();
        let ms: Vec<Combined<'_>> = members
            .iter()
            .filter_map(|m| {
                m.points.get(&l).map(|p| {
                    let (lo, hi) = read_span(q, w, m.iv);
                    (p, runs(&bounded(&m.rows[cpu], lo, hi, m.iv, reach)), m.iv)
                })
            })
            .collect();
        let times: BTreeSet<u64> = ms
            .iter()
            .flat_map(|(p, _, _)| p.iter().map(|x| x.0))
            .collect();
        let mut pts = vec![];
        for t in times {
            let te = t;
            let at = |p: &Vec<(u64, f64)>| p.iter().find(|x| x.0 == t).map(|x| x.1);
            let run = |rs: &[(u64, u64)], iv: u64| {
                rs.iter().find(|r| r.0 <= te && te <= r.1 + iv).copied()
            };
            let win = |iv: u64| match q.range {
                Some(r) if !q.irate => r,
                Some(_) => step_ns,
                None => step_ns.max(iv),
            };
            let covers = |(p, rs, iv): &&Combined<'_>| {
                at(p).is_some()
                    && run(rs, *iv)
                        .is_some_and(|r| te.checked_sub(win(*iv)).is_some_and(|s| s >= r.0))
            };
            let holds = |(p, rs, iv): &&Combined<'_>| at(p).is_some() && run(rs, *iv).is_some();
            if let Some(m) = ms
                .iter()
                .find(|m| covers(m))
                .or_else(|| ms.iter().find(|m| holds(m)))
            {
                pts.push((t, at(m.0).unwrap()));
            }
        }
        if !pts.is_empty() {
            out.insert(l, pts);
        }
    }
    out
}

fn close(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

/// Differences; `within` restricts comparison to points in [lo, hi].
fn differences(got: &Series, want: &Series, within: Option<(u64, u64)>) -> Vec<String> {
    let mut out = vec![];
    let keep = |t: u64| within.is_none_or(|(a, b)| a <= t && t <= b);
    for l in got.keys().chain(want.keys()).collect::<BTreeSet<_>>() {
        let g: BTreeMap<u64, f64> = got
            .get(l)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| keep(p.0))
            .collect();
        let w: BTreeMap<u64, f64> = want
            .get(l)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| keep(p.0))
            .collect();
        let mut bad = vec![];
        for t in g.keys().chain(w.keys()).collect::<BTreeSet<_>>() {
            match (g.get(t), w.get(t)) {
                (Some(a), Some(b)) if close(*a, *b) => {}
                (a, b) => bad.push(format!(
                    "{:.3}: {:?} vs {:?}",
                    (*t - BASE) as f64 / 1e9,
                    a,
                    b
                )),
            }
        }
        let big = g
            .keys()
            .chain(w.keys())
            .any(|t| match (g.get(t), w.get(t)) {
                (Some(a), Some(b)) => (a - b).abs() > 1e-5 * a.abs().max(b.abs()).max(1.0),
                (None, None) => false,
                _ => true,
            });
        if !bad.is_empty() {
            out.push(format!(
                "{}cpu={} {} pts e.g. {:?}",
                if big { "BIG " } else { "small " },
                l.get("cpu").cloned().unwrap_or_default(),
                bad.len(),
                &bad[..bad.len().min(3)]
            ));
        }
    }
    out
}

/// Two or three sources of one process for fixture `f`: cadences from
/// 250 ms to 15 s, a change of cadence partway, holes, a faster second
/// table, a series absent for a while.
///
/// No epoch chains. The oracle reads a member clipped to one epoch as an
/// archive of its own, at that epoch's measured row spacing (and staleness);
/// S9 gives each epoch member its whole table's interval, which is what the
/// engine uses, so it is the oracle that would be off, not the engine. A
/// reference reading the whole source clipped to the epoch would need the
/// engine's clipping wrapper, which is not public; epoch chains are tested
/// in `union_queries.rs` instead.
fn gen(rng: &mut Rng, f: u64) -> Vec<Src> {
    let disagree = f % 3 == 1;
    let n = 2 + rng.below(2) as usize;
    let mut out = vec![];
    let cad = [250 * MS, 500 * MS, S, S, 2 * S, 5 * S, 10 * S, 15 * S];
    for _ in 0..n {
        let every = rng.pick(&cad);
        let offset = rng.pick(&[0, 137 * MS, 500 * MS, 0, 3 * S]);
        let from = BASE + rng.below(60) * S + offset;
        let to = from + (120 + rng.below(240)) * S;
        let switch = (rng.below(4) == 0).then(|| (from + rng.below(to - from), rng.pick(&cad)));
        let mut rows = vec![];
        let mut t = from;
        while t <= to {
            rows.push(t);
            let e = match switch {
                Some((at, e2)) if t >= at => e2,
                _ => every,
            };
            t += e;
        }
        for _ in 0..rng.below(3) {
            let h0 = from + rng.below(to - from);
            let h1 = h0 + (2 + rng.below(40)) * S + every * rng.below(6);
            rows.retain(|t| !(h0..h1).contains(t));
        }
        if rows.len() < 3 {
            rows = vec![from, from + every, from + 2 * every];
        }
        let fast = if rng.below(3) == 0 {
            let fe = rng.pick(&[250 * MS, S]);
            (0..)
                .map(|i| from + i * fe)
                .take_while(|t| *t <= to)
                .collect()
        } else {
            vec![]
        };
        let absent1 = (disagree && rng.below(2) == 0).then(|| {
            let a = from + rng.below(to - from);
            (a, a + (5 + rng.below(100)) * S)
        });
        out.push(Src {
            rows,
            fast,
            epochs: vec![("e".to_string(), BASE)],
            absent: absent1.into_iter().collect(),
            window: rng.pick(&[MS, 10 * MS]),
        });
    }
    out
}
fn seg_src(parts: &[(f64, f64, u64)], fast_ms: Option<u64>) -> Src {
    let mut rows = vec![];
    for &(a, b, every_ms) in parts {
        let (a, b) = (BASE + (a * 1e9) as u64, BASE + (b * 1e9) as u64);
        let mut t = a;
        while t <= b {
            rows.push(t);
            t += every_ms * MS;
        }
    }
    rows.sort();
    rows.dedup();
    let fast = fast_ms
        .map(|e| {
            (0..)
                .map(|i| rows[0] + i * e * MS)
                .take_while(|t| t <= rows.last().unwrap())
                .collect()
        })
        .unwrap_or_default();
    Src {
        rows,
        fast,
        epochs: vec![("e".into(), BASE)],
        absent: Vec::new(),
        window: MS,
    }
}

fn src(from_s: f64, to_s: f64, every_ms: u64, holes: &[(f64, f64)]) -> Src {
    let mut s = seg_src(&[(from_s, to_s, every_ms)], None);
    for &(a, b) in holes {
        let (a, b) = (BASE + (a * 1e9) as u64, BASE + (b * 1e9) as u64);
        s.rows.retain(|t| !(a..b).contains(t));
    }
    s
}

/// A query range: start and end in seconds since `BASE`, and the step.
type Window = (f64, f64, f64);

fn abs((s, e, step): Window) -> Window {
    let b = (BASE / S) as f64;
    (b + s, b + e, step)
}

/// The union of `srcs` against the oracle for every query on both paths.
fn check(dir: &Path, name: &str, srcs: &[Src], w: Window) {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("union.dendro");
    write(&path, srcs);
    let union = open(&path);
    assert_eq!(union.read_groups().len(), 1, "{name}");
    let w = abs(w);
    let mut cache = HashMap::new();
    for q in QS {
        for ps in [false, true] {
            let got = range(&union, q, w, ps).unwrap();
            let want = reference(dir, srcs, &union, &parse(q), (w, ps), &mut cache);
            let d = differences(&got, &want, None);
            assert!(d.is_empty(), "{name} {q} per-series {ps}: {d:?}");
        }
    }
}

/// Cases where reading a later source over part of the range changed
/// points: a 250 ms source with a hole over [200, 210) s beside a 2 s
/// source, and a 1 s source with a hole over [200, 230) s beside a 10 s
/// source. At the grid times the first does not cover, the union is the
/// coarser source alone, read over the whole range.
#[test]
fn a_later_source_answers_as_it_does_alone() {
    let dir = tempfile::tempdir().unwrap();
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
        let w = abs(w);
        let cpu0 = |s: &Series| {
            s.iter()
                .find(|(l, _)| l.get("cpu").map(String::as_str) == Some("0"))
                .map(|(_, p)| p.clone())
                .unwrap()
        };
        let union = cpu0(&range(&open(&u), q, w, false).unwrap());
        let alone = cpu0(&range(&open(&c), q, w, false).unwrap());
        for t in ts {
            let t = to_ns((BASE / S) as f64 + t);
            let got = union.iter().find(|p| p.0 == t).map(|p| p.1);
            let want = alone.iter().find(|p| p.0 == t).map(|p| p.1);
            assert!(want.is_some(), "{name}: the coarse source has a point");
            assert_eq!(got, want, "{name} {q} at {:.2}", (t - BASE) as f64 / 1e9);
        }
    }
}

/// Fixed fixtures against the oracle: holes beside a coarser or offset
/// source, a dump before its record, a series absent for a while, a source
/// whose cadence changes after a gap near the range's end or start, a slow
/// metric beside a fast second table of its source, and (F5) gaps before
/// the range that only the ninth row back tells from a hole.
#[test]
fn deterministic_cases_match_the_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let mut lacks1 = src(100.0, 300.0, 500, &[]);
    lacks1.absent = vec![(BASE + 140 * S, BASE + 280 * S)];
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
            vec![lacks1, src(100.0, 300.0, 1000, &[])],
            (150.0, 260.0, 1.0),
        ),
        (
            "1 s then a gap then 5 s, range ends after the gap",
            vec![
                seg_src(&[(100.0, 200.0, 1000), (203.0, 300.0, 5000)], None),
                seg_src(&[(100.5, 300.5, 2000)], None),
            ],
            (150.0, 204.0, 1.0),
        ),
        (
            "15 s then 1 s beside 15 s",
            vec![
                seg_src(&[(9.5, 99.5, 15000), (114.5, 300.0, 1000)], None),
                seg_src(&[(31.0, 300.0, 15000)], None),
            ],
            (87.0, 193.0, 0.25),
        ),
        (
            "20 s then 10 s with a 1 s table beside 10 s",
            vec![
                seg_src(&[(0.0, 120.0, 20000), (130.0, 300.0, 10000)], Some(1000)),
                seg_src(&[(5.0, 300.0, 10000)], None),
            ],
            (120.0, 200.0, 1.0),
        ),
        (
            "F5 three 3 s gaps then 1 s rows from the range's start",
            vec![
                seg_src(
                    &[
                        (0.0, 100.0, 1000),
                        (103.0, 109.0, 3000),
                        (112.0, 300.0, 1000),
                    ],
                    None,
                ),
                seg_src(&[(0.5, 300.5, 2000)], None),
            ],
            (112.0, 130.0, 1.0),
        ),
    ];
    for (i, (name, srcs, w)) in cases.into_iter().enumerate() {
        check(&dir.path().join(format!("c{i}")), name, &srcs, w);
    }
}

/// The last points of a range read the rows after it: 1 s rows to 200 s,
/// a gap to 203 s, then 5 s rows, beside a 2 s source, queried to 204 s. The
/// union matches the oracle there, and a gauge (whose points each source
/// alone gives whatever the range's end) reads the same to 204 s as to
/// 260 s, in a range query and instantly at 204 s. (A rate's last point
/// moves with the range's end for a source alone too, so only the oracle is
/// asked about it.)
#[test]
fn the_last_points_read_the_rows_after_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let srcs = vec![
        seg_src(&[(100.0, 200.0, 1000), (203.0, 300.0, 5000)], None),
        seg_src(&[(100.5, 300.5, 2000)], None),
    ];
    let p = dir.path().join("u.dendro");
    write(&p, &srcs);
    let u = open(&p);
    let mut cache = HashMap::new();
    let (short, long) = (abs((150.0, 204.0, 1.0)), abs((150.0, 260.0, 1.0)));
    for q in ["g", "rate(c[5s])", "avg_over_time(g[10s])", "irate(c[5s])"] {
        for ps in [false, true] {
            let a = range(&u, q, short, ps).unwrap();
            let want = reference(dir.path(), &srcs, &u, &parse(q), (short, ps), &mut cache);
            let d = differences(&a, &want, None);
            assert!(d.is_empty(), "{q} per-series {ps}: {d:?}");
        }
    }
    let a = range(&u, "g", short, false).unwrap();
    let b = range(&u, "g", long, false).unwrap();
    let d = differences(&a, &b, Some((to_ns(short.0), to_ns(short.1))));
    assert!(d.is_empty(), "g to 204 s against to 260 s: {d:?}");
    let QueryResult::Vector { result } = u.query("g", Some(short.1)).unwrap() else {
        panic!("not a vector");
    };
    assert!(!result.is_empty());
    for s in result {
        let labels: BTreeMap<String, String> = s
            .metric
            .into_iter()
            .filter(|(k, _)| k != "__name__")
            .collect();
        let want = b[&labels]
            .iter()
            .find(|p| p.0 == to_ns(s.value.0))
            .map(|p| p.1);
        assert_eq!(Some(s.value.1), want, "instant g {labels:?}");
    }
}

/// A point does not depend on where the range starts, and matches the
/// oracle, for a slow metric (15 s rows, a change to 1 s, or a 15 s metric
/// beside a 250 ms table of its source) beside another slow source, from 20
/// starts.
#[test]
fn the_range_start_does_not_move_points() {
    let dir = tempfile::tempdir().unwrap();
    let b = (BASE / S) as f64;
    let sets: Vec<(&str, Vec<Src>)> = vec![
        (
            "15 s then 1 s",
            vec![
                seg_src(&[(9.5, 99.5, 15000), (114.5, 300.0, 1000)], None),
                seg_src(&[(31.0, 300.0, 15000)], None),
            ],
        ),
        (
            "15 s with a 250 ms table beside 20 s",
            vec![
                seg_src(&[(0.0, 300.0, 15000)], Some(250)),
                seg_src(&[(7.0, 300.0, 20000)], None),
            ],
        ),
    ];
    for (i, (name, srcs)) in sets.into_iter().enumerate() {
        let sub = dir.path().join(format!("s{i}"));
        std::fs::create_dir_all(&sub).unwrap();
        let p = sub.join("u.dendro");
        write(&p, &srcs);
        let u = open(&p);
        let mut cache = HashMap::new();
        for q in [
            "g",
            "rate(c[30s])",
            "rate(c[5s])",
            "avg_over_time(g[10s])",
            "irate(c[30s])",
        ] {
            for ps in [false, true] {
                let long = range(&u, q, (b + 20.5, b + 250.0, 1.0), ps).unwrap();
                for s0 in (80..140).step_by(3) {
                    let w = (b + s0 as f64 + 0.5, b + 250.0, 1.0);
                    let got = range(&u, q, w, ps).unwrap();
                    let want = reference(&sub, &srcs, &u, &parse(q), (w, ps), &mut cache);
                    let d = differences(&got, &want, None);
                    assert!(d.is_empty(), "{name} {q} per-series {ps} from {s0}: {d:?}");
                    let d = differences(&got, &long, Some((to_ns(w.0), to_ns(w.1))));
                    assert!(
                        d.is_empty(),
                        "{name} {q} per-series {ps} from {s0} against a longer range: {d:?}"
                    );
                }
            }
        }
    }
}

/// Seeded random fixtures against the oracle (and, with
/// `UNION_ORACLE_EXTEND`, against the same query over a range extended at
/// both ends, which a source alone does not satisfy at a rate's last point). `UNION_ORACLE_N` sets the fixtures
/// per seed (default 6) and `UNION_ORACLE_SEEDS` the seeds (default 7,11).
#[test]
fn random_fixtures_match_the_oracle() {
    let n: u64 = std::env::var("UNION_ORACLE_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    let seeds: Vec<u64> = std::env::var("UNION_ORACLE_SEEDS")
        .unwrap_or_else(|_| "7,11".to_string())
        .split(',')
        .map(|s| s.parse().unwrap())
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let b = (BASE / S) as f64;
    let (mut failures, mut checked) = (Vec::new(), 0);
    for seed in seeds {
        for f in 0..n {
            let mut rng = Rng(0x51ED_2701_A5A5_0F0F ^ (seed * 1_000_003 + f));
            let srcs = gen(&mut rng, f);
            let sub = dir.path().join(format!("s{seed}f{f}"));
            std::fs::create_dir_all(&sub).unwrap();
            let p = sub.join("union.dendro");
            write(&p, &srcs);
            let union = open(&p);
            if union.read_groups().len() != 1 {
                continue;
            }
            let mut cache = HashMap::new();
            for _ in 0..3 {
                let step = rng.pick(&[0.25, 0.5, 1.0, 2.0, 3.0, 7.0]);
                let start = b + rng.below(250) as f64 + rng.pick(&[0.0, 0.0, 0.3, 0.75]);
                let end = start + 10.0 + rng.below(300) as f64;
                let w = (start, end, step);
                let k0 = (40.0 + rng.below(60) as f64) / step;
                let k1 = (40.0 + rng.below(60) as f64) / step;
                let wide = (start - k0.floor() * step, end + k1.floor() * step, step);
                for q in QS {
                    for ps in [false, true] {
                        let got = range(&union, q, w, ps).unwrap();
                        let want = reference(&sub, &srcs, &union, &parse(q), (w, ps), &mut cache);
                        let longer = range(&union, q, wide, ps).unwrap();
                        checked += 1;
                        let mut d = differences(&got, &want, None);
                        if std::env::var_os("UNION_ORACLE_EXTEND").is_some() {
                            d.extend(
                                differences(&got, &longer, Some((to_ns(start), to_ns(end))))
                                    .into_iter()
                                    .map(|x| format!("against a longer range: {x}")),
                            );
                        }
                        if !d.is_empty() {
                            failures.push(format!(
                                "seed {seed} fixture {f} {q} per-series {ps} [{:.2}, {:.2}] step {step}: {:?}",
                                start - b,
                                end - b,
                                &d[..d.len().min(2)]
                            ));
                        }
                    }
                }
            }
        }
    }
    assert!(checked > 300, "only {checked} queries checked");
    assert!(
        failures.is_empty(),
        "{} of {checked} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A fixture for the bound: its name, sources, query and range, and the
/// bounds (in intervals) whose answers must differ from the engine's.
type BoundCase = (&'static str, Vec<Src>, &'static str, Window, Vec<u64>);

/// `cpu=1` present only in `present` (inclusive spans, seconds), absent
/// elsewhere up to `until`.
fn only_present(mut s: Src, present: &[(f64, f64)], until: f64) -> Src {
    let mut edges = vec![(0.0f64, 0.0f64)];
    edges.extend_from_slice(present);
    edges.push((until, until));
    for w in edges.windows(2) {
        let (a, b) = (w[0].1, w[1].0);
        if b > a {
            s.absent
                .push((BASE + (a * 1e9) as u64 + 1, BASE + (b * 1e9) as u64 - 1));
        }
    }
    s
}

/// Fixtures whose answers depend on the bound of nine rows within 64
/// intervals: they match the bounded oracle and not the unbounded one, so a
/// change to the bound is caught. F1: `cpu=1` present for 4 s every 100 s in
/// a 1 s and a 2 s source, `rate(c[2m])` over [176, 206] s. F2: `cpu=1` 1 s
/// rows to 200 s, a hole to 203 s, rows at 203 and 204 s, then one row at
/// 290 s, beside a 2 s source, `rate(c[5s])` ending at 204 s; F2b, the same
/// with the row at 250 s, which a bound of 32 intervals would not read.
#[test]
fn the_bound_on_rows_beyond_a_read_decides() {
    let dir = tempfile::tempdir().unwrap();
    let flicker: Vec<(f64, f64)> = (0..12)
        .map(|k| (k as f64 * 100.0, k as f64 * 100.0 + 4.0))
        .collect();
    let f2 = only_present(
        seg_src(&[(100.0, 300.0, 1000)], None),
        &[(100.0, 200.0), (203.0, 204.0), (290.0, 290.0)],
        400.0,
    );
    let f2b = only_present(
        seg_src(&[(100.0, 300.0, 1000)], None),
        &[(100.0, 200.0), (203.0, 204.0), (250.0, 250.0)],
        400.0,
    );
    // Each case with the bounds (in intervals) whose answers must differ.
    let cases: Vec<BoundCase> = vec![
        (
            "F1 present 4 s every 100 s",
            vec![
                only_present(seg_src(&[(0.0, 1200.0, 1000)], None), &flicker, 1300.0),
                only_present(seg_src(&[(0.5, 1200.5, 2000)], None), &flicker, 1300.0),
            ],
            "rate(c[2m])",
            (176.0, 206.0, 1.0),
            vec![u64::MAX / 2],
        ),
        (
            "F2 one row 86 s after the range",
            vec![f2, seg_src(&[(100.5, 300.5, 2000)], None)],
            "rate(c[5s])",
            (194.0, 204.0, 1.0),
            vec![u64::MAX / 2],
        ),
        (
            "F2b one row 46 s after the range",
            vec![f2b, seg_src(&[(100.5, 300.5, 2000)], None)],
            "rate(c[5s])",
            (194.0, 204.0, 1.0),
            vec![32],
        ),
    ];
    for (i, (name, srcs, q, w, others)) in cases.into_iter().enumerate() {
        let sub = dir.path().join(format!("b{i}"));
        std::fs::create_dir_all(&sub).unwrap();
        let p = sub.join("union.dendro");
        write(&p, &srcs);
        let union = open(&p);
        assert_eq!(union.read_groups().len(), 1, "{name}");
        let w = abs(w);
        let mut cache = HashMap::new();
        let mut sensitive = false;
        for ps in [false, true] {
            let got = range(&union, q, w, ps).unwrap();
            let want = reference(&sub, &srcs, &union, &parse(q), (w, ps), &mut cache);
            let d = differences(&got, &want, None);
            assert!(d.is_empty(), "{name} per-series {ps}: {d:?}");
            for &other in &others {
                let alt =
                    reference_within(&sub, &srcs, &union, &parse(q), (w, ps), &mut cache, other);
                sensitive |= !differences(&got, &alt, None).is_empty();
            }
        }
        assert!(sensitive, "{name}: the bound decides nothing here");
    }
}
