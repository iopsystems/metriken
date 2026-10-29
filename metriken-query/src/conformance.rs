//! Conformance suite for queries over archives.
//!
//! A fixture is built from closed-form series that change once, at `K`: a
//! counter's rate doubles, a gauge's slope doubles, and a histogram's
//! observations move from one value to four times it. One counter and one gauge
//! stop at `STOP`, one counter resets at `RESET`, and one does not move before
//! `K`. A layout places the series in tables and recordings: one table, two
//! samplers at different cadences (each with a gauge and a counter, so a query
//! can combine the two samplers with either), two recordings out of phase, two
//! recordings at different cadences, and two recordings with identical labels.
//! Every row carries an acquisition window, longer on the slow sampler's rows
//! than on the others'.
//!
//! A query is an [`E`], which renders as PromQL and is also evaluated from the
//! fixture's definition ([`World::eval`]). Each point the engine returns is
//! compared with the value worked out for that point's time. Where the window a
//! point reads contains `K`, its value depends on how the function weighs the
//! two sides and is not compared ([`Val::v`] is `None`); every other point has
//! one exact answer, so a function reading the wrong rows gives a wrong value
//! near `K`. Each query runs against each layout:
//!
//! - grid, at a 1 s and a 5 s step, at a 5 s step from a start off the step
//!   grid, and to a row and to a row and a half past `STOP`, each with rates
//!   computed in one pass and per series. The label sets must equal the
//!   expected ones. Unless a query combines samplers of different cadence
//!   within one recording, every point of a non-histogram query is on a
//!   multiple of the step and the first and last points are the expected ones;
//!   elsewhere (the slow sampler's rows, histogram ticks) the points must start
//!   and end within the series' row spacing of the expected ones. No two points
//!   are further apart than the step or the series' row spacing. A band is
//!   compared with the one worked out from the windows: a rate's, over the
//!   grid's span or the slow sampler's, carried through sums, averages and
//!   constants, and combined by an operator after each operand is widened to
//!   the union of both operands' windows where they differ; a histogram
//!   function's, from its buckets. A value with no band to give (a gauge, its
//!   `avg_over_time` or `idelta`, a histogram count, or an operator over those)
//!   must have none. Any other band (a minimum, a maximum, a count) must have
//!   width and contain the value and the expected value.
//! - instant, at a row of the tables without an offset and between rows. The
//!   result's time is the time asked for, or for a histogram function the
//!   latest tick of its slowest series at or before it.
//! - raw (`RateMode::Raw`): every series, point, value and band equal to
//!   [`World::raw_series`], which builds the result from the series' rows up,
//!   each operand held for the gap before its point. Histogram functions are
//!   not run in raw mode.
//! - display: equal to reducing the grid result, and, on the one-table layout,
//!   for a gauge and a sum of rates ([`decimation_matches_the_fixture`]) equal
//!   to buckets worked out from the fixture.
//!
//! A query whose matching is many-to-many or many-to-one must be refused in
//! every mode. Every mismatch is collected, and the test fails once with all of
//! them.
//!
//! Some rules are the engine's specification rather than arithmetic, and the
//! oracle states them as the engine does: how long a raw operand's point is
//! held ([`held`]), the interpolated quantiles of a display bucket, which of
//! two equally slow histograms sets the ticks, and an operator over two rates
//! whose spans share no time (a rate held at a range's end beside a current
//! one) giving a value with no band, marked interpolated.

use std::collections::{BTreeMap, BTreeSet};

use metriken_storage::builder::GroupTableBuilder;
use metriken_storage::schema::{GroupSchema, MetricDesc};
use metriken_storage::table::write_table_parquet;
use metriken_storage::window::Window;
use metriken_storage::{ArchiveReader, BufferPool, InMemorySource};

use crate::display::{display_from_result, DisplayOptions, DisplayResult};
use crate::{MetricsSource, QueryOptions, QueryResult, RateMode};

const S: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
/// The first row's time.
const T0: u64 = 1_790_000_000 * S;
/// Rows run from `T0` to `T0 + SPAN`.
const SPAN: u64 = 120 * S;
/// Where every series changes.
const K: u64 = T0 + 60 * S;
/// Where the stopping series have their last row.
const STOP: u64 = T0 + 75 * S;
/// Each row's acquisition window ends at its time and is this long; the
/// slow sampler's rows (the gauges' table when it is apart) take longer.
const WINDOW: u64 = 20 * MS;
const SLOW_WINDOW: u64 = 50 * MS;
/// Where the resetting counter starts again from zero (at its first row
/// at or after this).
const RESET: u64 = T0 + 65 * S;
/// Histogram configuration: buckets are under 1% wide, and wider than one
/// above 256, where the histogram values are.
const GP: u8 = 7;
const MVP: u8 = 20;
/// The range of most `rate`s in the queries: five steps at the 1 s step,
/// one at the 5 s step.
const RATE: F = F::Rate(5 * S);
/// `deriv`'s and `avg_over_time`'s ranges in the queries.
const DERIV_RANGE: u64 = 20 * S;
const AVG_RANGE: u64 = 10 * S;
/// How close a band's ends must be to the expected ones, relative to the
/// larger end's magnitude (a difference of two bands cancels to an end much
/// smaller than either). The engine works out a band from window edges in
/// nanoseconds since the epoch as `f64`, whose step there is 256 ns: about
/// 1e-7 of a 1 s span.
const BAND_TOLERANCE: f64 = 1e-6;

/// Whether band `(lo, hi)` is band `(wl, wh)` within [`BAND_TOLERANCE`].
fn band_close((lo, hi): (f64, f64), (wl, wh): (f64, f64)) -> bool {
    let scale = wl.abs().max(wh.abs()).max(1.0) * BAND_TOLERANCE;
    (lo - wl).abs() <= scale && (hi - wh).abs() <= scale
}

type Labels = BTreeMap<String, String>;

#[derive(Clone, Copy, Debug)]
enum Kind {
    /// Grows by `per_s` a second before `K` and twice that after.
    Counter { per_s: u64 },
    /// `base`, rising by `per_s` a second before `K` and twice that after.
    Gauge { base: i64, per_s: i64 },
    /// `per_s` observations a second, of `value` before `K` and of
    /// `4 * value` after.
    Histogram { value: u64, per_s: u64 },
}

/// The area under a rate of `per_s` before `K` and `2 * per_s` after, from
/// `T0` to `t`, in whole units: every row is at a multiple of 100 ms and
/// every rate a multiple of 10.
fn integral(per_s: u64, t: u64) -> u64 {
    per_s * (t.min(K) - T0) / S + 2 * per_s * (t.max(K) - K) / S
}

struct Def {
    metric: &'static str,
    labels: Vec<(&'static str, String)>,
    kind: Kind,
    /// Whether the series has no value after `STOP`.
    stops: bool,
    /// Whether the counter restarts from zero at `RESET`, counting the
    /// interval before its first row there.
    resets: bool,
    /// Whether the counter does not move before `K`.
    idle: bool,
}

struct TableDef {
    name: &'static str,
    every: u64,
    offset: u64,
    /// Each row's acquisition window: `[row - window, row]`.
    window: u64,
    defs: Vec<Def>,
}

struct RecDef {
    labels: Vec<(&'static str, String)>,
    tables: Vec<TableDef>,
}

/// One series as the oracle sees it: its output labels and its rows.
struct Instance {
    metric: &'static str,
    labels: Labels,
    kind: Kind,
    every: u64,
    offset: u64,
    window: u64,
    stops: bool,
    idle: bool,
}

/// Which side of `K` a window `[a, b]` is on.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Side {
    Before,
    After,
}

fn side(a: u64, b: u64) -> Option<Side> {
    if b <= K {
        Some(Side::Before)
    } else if a >= K {
        Some(Side::After)
    } else {
        None
    }
}

/// A slope or rate of `per_s` before `K` and twice that after.
fn on_side(per_s: f64, side: Side) -> f64 {
    match side {
        Side::Before => per_s,
        Side::After => 2.0 * per_s,
    }
}

/// A row grid: first row, spacing, last row.
type Rows = (u64, u64, u64);

impl Instance {
    fn first(&self) -> u64 {
        T0 + self.offset
    }

    fn last(&self) -> u64 {
        let end = if self.stops { STOP } else { T0 + SPAN };
        self.first() + (end - self.first()) / self.every * self.every
    }

    fn rows(&self) -> Rows {
        (self.first(), self.every, self.last())
    }

    /// The latest row at or before `t`.
    fn row_at(&self, t: u64) -> Option<u64> {
        if t < self.first() {
            return None;
        }
        Some(self.first() + (t.min(self.last()) - self.first()) / self.every * self.every)
    }

    /// The first row at or after `t`.
    fn row_from(&self, t: u64) -> Option<u64> {
        if t <= self.first() {
            return Some(self.first());
        }
        let row = self.first() + (t - self.first()).div_ceil(self.every) * self.every;
        (row <= self.last()).then_some(row)
    }

    /// The row before `row`.
    fn before(&self, row: u64) -> Option<u64> {
        (row >= self.first() + self.every).then(|| row - self.every)
    }

    fn gauge(&self, row: u64) -> f64 {
        let Kind::Gauge { base, per_s } = self.kind else {
            unreachable!()
        };
        base as f64 + integral(per_s as u64, row) as f64
    }

    /// The rate or slope this series changes by.
    fn per_s(&self) -> f64 {
        match self.kind {
            Kind::Counter { per_s } => per_s as f64,
            Kind::Gauge { per_s, .. } => per_s as f64,
            Kind::Histogram { per_s, .. } => per_s as f64,
        }
    }
}

/// The bounds of the bucket holding `value`.
fn bucket_range(value: u64) -> (f64, f64) {
    let config = histogram::Config::new(GP, MVP).unwrap();
    let mut h = histogram::Histogram::with_config(&config);
    h.increment(value).unwrap();
    let b = h.iter().find(|b| b.count() > 0).unwrap();
    (b.start() as f64, b.end() as f64)
}

fn bucket_of(value: u64) -> usize {
    let config = histogram::Config::new(GP, MVP).unwrap();
    let mut h = histogram::Histogram::with_config(&config);
    h.increment(value).unwrap();
    histogram::SparseHistogram::from(&h).index()[0] as usize
}

fn table_bytes(table: &TableDef) -> Vec<u8> {
    let desc = |i: usize, d: &Def| MetricDesc {
        name: format!("{}_{i}", d.metric),
        metadata: std::iter::once(("metric".to_string(), d.metric.to_string()))
            .chain(d.labels.iter().map(|(k, v)| (k.to_string(), v.clone())))
            .collect(),
    };
    let mut schema = GroupSchema::default();
    for (i, d) in table.defs.iter().enumerate() {
        match d.kind {
            Kind::Counter { .. } => schema.counters.push(desc(i, d)),
            Kind::Gauge { .. } => schema.gauges.push(desc(i, d)),
            Kind::Histogram { .. } => schema.histograms.push(desc(i, d)),
        }
    }
    let buckets = histogram::Config::new(GP, MVP).unwrap().total_buckets();
    let mut builder = GroupTableBuilder::new(table.name.to_string());
    let rows = (0..)
        .map(|k| T0 + table.offset + k * table.every)
        .take_while(|t| *t <= T0 + SPAN);
    for row in rows {
        let mut counters = Vec::new();
        let mut gauges = Vec::new();
        let mut histograms = Vec::new();
        for d in &table.defs {
            let live = !(d.stops && row > STOP);
            match d.kind {
                Kind::Counter { per_s } => {
                    // A reset counts from the row before its first row at
                    // `RESET`, so its rate stays the same across it.
                    let reset = T0
                        + table.offset
                        + (RESET - T0 - table.offset).div_ceil(table.every) * table.every;
                    let value = if d.resets && row >= reset {
                        integral(per_s, row) - integral(per_s, reset - table.every)
                    } else if d.idle {
                        1_000_000 + 2 * per_s * (row.max(K) - K) / S
                    } else {
                        1_000_000 + integral(per_s, row)
                    };
                    counters.push(live.then_some(value))
                }
                Kind::Gauge { base, per_s } => {
                    gauges.push(live.then(|| base + integral(per_s as u64, row) as i64))
                }
                Kind::Histogram { value, per_s } => {
                    let mut b = vec![0u64; buckets];
                    b[bucket_of(value)] = per_s * (row.min(K) - T0) / S;
                    b[bucket_of(4 * value)] = per_s * (row.max(K) - K) / S;
                    histograms.push(live.then_some((GP, MVP, b)));
                }
            }
        }
        builder.push_row(
            row,
            0,
            Some(Window::new(row - table.window, row)),
            &schema,
            &counters,
            &gauges,
            &histograms,
        );
    }
    write_table_parquet(&builder.finish()).unwrap()
}

/// A fixture: its archive and the series the oracle evaluates.
struct World {
    name: &'static str,
    reader: ArchiveReader,
    instances: Vec<Instance>,
    /// The longest row spacing of any table.
    slowest: u64,
    /// Whether a grid query's points are on multiples of the step: false
    /// for one recording with samplers of different cadence, whose
    /// combinations are evaluated at the slow sampler's rows.
    aligned: bool,
    /// The longest row spacing of a table other than a histogram's: the
    /// span of a rate evaluated at the slow sampler's rows.
    slow_cadence: u64,
}

impl World {
    fn new(name: &'static str, recs: Vec<RecDef>, twins: bool) -> Self {
        let mut instances = Vec::new();
        let mut sources = Vec::new();
        let mut slowest = 0;
        let mut cadences = BTreeSet::new();
        for (index, rec) in recs.iter().enumerate() {
            let rec_labels: Labels = rec
                .labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            for table in &rec.tables {
                slowest = slowest.max(table.every);
                if !table.name.starts_with("h/") {
                    cadences.insert(table.every);
                }
                for d in &table.defs {
                    let mut labels = rec_labels.clone();
                    labels.extend(d.labels.iter().map(|(k, v)| (k.to_string(), v.clone())));
                    if twins {
                        labels.insert("__recording__".to_string(), index.to_string());
                    }
                    instances.push(Instance {
                        metric: d.metric,
                        labels,
                        kind: d.kind,
                        every: table.every,
                        offset: table.offset,
                        window: table.window,
                        stops: d.stops,
                        idle: d.idle,
                    });
                }
            }
            sources.push(InMemorySource {
                name: rec_labels.get("source").cloned().unwrap_or_default(),
                metadata: rec_labels.clone(),
                labels: rec_labels,
                complete: true,
                tables: rec
                    .tables
                    .iter()
                    .map(|t| (t.name.to_string(), vec![table_bytes(t)]))
                    .collect(),
            });
        }
        let reader =
            ArchiveReader::from_in_memory(sources, None, BufferPool::new(64 << 20)).unwrap();
        Self {
            name,
            reader,
            instances,
            slowest,
            aligned: recs.len() > 1 || cadences.len() <= 1,
            slow_cadence: cadences.last().copied().unwrap_or(S),
        }
    }
}

// ── Fixture definitions ──

fn def(metric: &'static str, labels: Vec<(&'static str, String)>, kind: Kind) -> Def {
    Def {
        metric,
        labels,
        kind,
        stops: false,
        resets: false,
        idle: false,
    }
}

/// The series of one recording, its counter and gauge values scaled by
/// `mult`, its histogram observing `hist_value` before `K`.
fn defs(mult: u64, hist_value: u64) -> (Vec<Def>, Vec<Def>, Vec<Def>) {
    let mut counters = Vec::new();
    for cpu in 0..3u64 {
        for (state, factor) in [("user", 1), ("system", 3)] {
            counters.push(def(
                "c",
                vec![("cpu", cpu.to_string()), ("state", state.to_string())],
                Kind::Counter {
                    per_s: mult * 10 * (cpu + 1) * factor,
                },
            ));
        }
    }
    counters.push(Def {
        stops: true,
        ..def("cs", vec![], Kind::Counter { per_s: mult * 10 })
    });
    counters.push(Def {
        resets: true,
        ..def("cr", vec![], Kind::Counter { per_s: mult * 10 })
    });
    // A counter that does not move before `K`.
    counters.push(Def {
        idle: true,
        ..def("ci", vec![], Kind::Counter { per_s: mult * 10 })
    });
    // A gauge read with the counters.
    counters.push(def(
        "gf",
        vec![],
        Kind::Gauge {
            base: 100 * mult as i64,
            per_s: 10,
        },
    ));
    let mut gauges = Vec::new();
    for cpu in 0..2i64 {
        gauges.push(def(
            "g",
            vec![("cpu", cpu.to_string())],
            Kind::Gauge {
                base: 1000 * (cpu + 1) * mult as i64,
                per_s: 10 * (cpu + 1),
            },
        ));
    }
    gauges.push(def(
        "cores",
        vec![],
        Kind::Gauge {
            base: 4 * mult as i64,
            per_s: 0,
        },
    ));
    // A counter read with the gauges.
    gauges.push(def("cx", vec![], Kind::Counter { per_s: mult * 20 }));
    gauges.push(Def {
        stops: true,
        ..def(
            "gs",
            vec![],
            Kind::Gauge {
                base: 500 * mult as i64,
                per_s: 10,
            },
        )
    });
    let hist = vec![def(
        "lat",
        vec![],
        Kind::Histogram {
            value: hist_value,
            per_s: 100,
        },
    )];
    (counters, gauges, hist)
}

/// One recording: counters every `fast`, gauges every `slow`, the
/// histogram every `hist`, each table its own sampler; `fast == slow` puts
/// counters and gauges in one table.
fn rec(
    labels: &[(&'static str, &str)],
    mult: u64,
    hist_value: u64,
    (fast, slow, hist): (u64, u64, u64),
    offsets: (u64, u64, u64),
) -> RecDef {
    let (counters, gauges, histograms) = defs(mult, hist_value);
    let mut tables = Vec::new();
    if fast == slow && offsets.0 == offsets.1 {
        let mut all = counters;
        all.extend(gauges);
        tables.push(TableDef {
            name: "s/all",
            every: fast,
            offset: offsets.0,
            window: WINDOW,
            defs: all,
        });
    } else {
        tables.push(TableDef {
            name: "s/fast",
            every: fast,
            offset: offsets.0,
            window: WINDOW,
            defs: counters,
        });
        tables.push(TableDef {
            name: "t/slow",
            every: slow,
            offset: offsets.1,
            window: SLOW_WINDOW,
            defs: gauges,
        });
    }
    tables.push(TableDef {
        name: "h/lat",
        every: hist,
        offset: offsets.2,
        window: WINDOW,
        defs: histograms,
    });
    RecDef {
        labels: labels.iter().map(|(k, v)| (*k, v.to_string())).collect(),
        tables,
    }
}

fn worlds() -> Vec<World> {
    let a = [("source", "a"), ("host", "ha")];
    let b = [("source", "b"), ("host", "hb")];
    vec![
        World::new(
            "one table",
            vec![rec(&a, 1, 1000, (S, S, S), (0, 0, 0))],
            false,
        ),
        World::new(
            "two cadences",
            vec![rec(
                &a,
                1,
                1000,
                (S, 10 * S, 5 * S),
                (0, 300 * MS, 100 * MS),
            )],
            false,
        ),
        World::new(
            "two recordings out of phase",
            vec![
                rec(&a, 1, 1000, (S, S, S), (0, 0, 0)),
                rec(&b, 7, 50000, (S, S, S), (500 * MS, 500 * MS, 500 * MS)),
            ],
            false,
        ),
        World::new(
            "two recordings at different cadences",
            vec![
                rec(&a, 1, 1000, (S, S, S), (0, 0, 0)),
                rec(
                    &b,
                    7,
                    50000,
                    (2 * S, 10 * S, 5 * S),
                    (300 * MS, 700 * MS, 100 * MS),
                ),
            ],
            false,
        ),
        World::new(
            "twins",
            vec![
                rec(&a, 1, 1000, (S, S, S), (0, 0, 0)),
                rec(&a, 7, 50000, (S, S, S), (500 * MS, 500 * MS, 500 * MS)),
            ],
            true,
        ),
    ]
}

// ── Queries and the oracle ──

#[derive(Clone, Copy, Debug, PartialEq)]
enum F {
    /// A gauge selector.
    Value,
    /// `rate(metric[range])`, the range in nanoseconds.
    Rate(u64),
    /// `irate(metric[1m])`: its range is not used.
    Irate,
    Idelta,
    /// `deriv(metric[20s])`, over a gauge or a counter.
    Deriv,
    /// `avg_over_time(metric[10s])`.
    AvgOverTime,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Agg {
    Sum,
    Avg,
    Min,
    Max,
    Count,
}

#[derive(Clone, Debug)]
enum Grouping {
    All,
    By(Vec<&'static str>),
    Without(Vec<&'static str>),
}

#[derive(Clone, Debug)]
enum Matching {
    Default,
    On(Vec<&'static str>),
    Ignoring(Vec<&'static str>),
}

/// A histogram function over every series of the metric, pooled.
#[derive(Clone, Copy, Debug, PartialEq)]
enum H {
    Quantile(f64),
    /// `histogram_quantiles([0.25, 0.75], ..)`: one series per quantile.
    Quartiles,
    /// The same, spelled `histogram_percentiles`.
    Percentiles,
    Count,
    Sum,
    Mean,
    Irate,
}

#[derive(Clone, Debug)]
enum E {
    /// `f(metric{matchers}[range])`, or `metric{matchers}` for `F::Value`.
    Sel {
        f: F,
        metric: &'static str,
        matchers: &'static str,
    },
    Agg {
        op: Agg,
        grouping: Grouping,
        inner: Box<E>,
    },
    /// `inner op k`, or `k op inner` with `scalar_left`.
    Scalar {
        op: char,
        k: f64,
        scalar_left: bool,
        inner: Box<E>,
    },
    Bin {
        op: char,
        l: Box<E>,
        r: Box<E>,
        matching: Matching,
    },
    Hist {
        h: H,
        metric: &'static str,
        matchers: &'static str,
    },
}

fn selector(metric: &str, matchers: &str) -> String {
    if matchers.is_empty() {
        metric.to_string()
    } else {
        format!("{metric}{{{matchers}}}")
    }
}

impl E {
    fn render(&self) -> String {
        match self {
            E::Sel {
                f,
                metric,
                matchers,
            } => {
                let sel = selector(metric, matchers);
                match f {
                    F::Value => sel,
                    F::Rate(r) if r % (60 * S) == 0 => format!("rate({sel}[{}m])", r / (60 * S)),
                    F::Rate(r) => format!("rate({sel}[{}s])", r / S),
                    F::Irate => format!("irate({sel}[1m])"),
                    F::Idelta => format!("idelta({sel}[1m])"),
                    F::Deriv => format!("deriv({sel}[20s])"),
                    F::AvgOverTime => format!("avg_over_time({sel}[10s])"),
                }
            }
            E::Agg {
                op,
                grouping,
                inner,
            } => {
                let op = format!("{op:?}").to_lowercase();
                let g = match grouping {
                    Grouping::All => String::new(),
                    Grouping::By(l) => format!(" by ({})", l.join(", ")),
                    Grouping::Without(l) => format!(" without ({})", l.join(", ")),
                };
                format!("{op}{g} ({})", inner.render())
            }
            E::Scalar {
                op,
                k,
                scalar_left,
                inner,
            } => {
                if *scalar_left {
                    format!("{k} {op} {}", inner.render())
                } else {
                    format!("{} {op} {k}", inner.render())
                }
            }
            E::Bin { op, l, r, matching } => {
                let m = match matching {
                    Matching::Default => String::new(),
                    Matching::On(l) => format!(" on ({})", l.join(", ")),
                    Matching::Ignoring(l) => format!(" ignoring ({})", l.join(", ")),
                };
                format!("({}) {op}{m} ({})", l.render(), r.render())
            }
            E::Hist {
                h,
                metric,
                matchers,
            } => {
                let sel = selector(metric, matchers);
                match h {
                    H::Quantile(q) => format!("histogram_quantile({q}, {sel})"),
                    H::Quartiles => format!("histogram_quantiles([0.25, 0.75], {sel})"),
                    H::Percentiles => format!("histogram_percentiles([0.25, 0.75], {sel})"),
                    H::Count => format!("histogram_count({sel})"),
                    H::Sum => format!("histogram_sum({sel})"),
                    H::Mean => format!("histogram_mean({sel})"),
                    H::Irate => format!("histogram_irate({sel})"),
                }
            }
        }
    }

    fn is_histogram(&self) -> bool {
        match self {
            E::Hist { .. } => true,
            E::Agg { inner, .. } | E::Scalar { inner, .. } => inner.is_histogram(),
            E::Bin { l, r, .. } => l.is_histogram() || r.is_histogram(),
            E::Sel { .. } => false,
        }
    }

    /// Whether every series the query reads is in one sampler.
    fn one_sampler(&self) -> bool {
        fn sampler(e: &E) -> BTreeSet<&'static str> {
            match e {
                E::Sel { metric, .. } => BTreeSet::from([match *metric {
                    "c" | "cs" | "cr" | "ci" | "gf" => "counters",
                    _ => "gauges",
                }]),
                E::Hist { .. } => BTreeSet::from(["histograms"]),
                E::Agg { inner, .. } | E::Scalar { inner, .. } => sampler(inner),
                E::Bin { l, r, .. } => sampler(l).union(&sampler(r)).copied().collect(),
            }
        }
        sampler(self).len() <= 1
    }

    /// How close a value must be to the expected one, relative.
    fn tolerance(&self) -> f64 {
        1e-9
    }

    /// Whether a band of no width is right: a count of series, which no
    /// read's timing can change.
    fn allows_flat_band(&self) -> bool {
        matches!(self, E::Agg { op: Agg::Count, .. })
    }
}

/// Why the oracle expects a refusal.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Refusal {
    ManyToMany,
    MultipleMatches,
}

impl Refusal {
    fn text(self) -> &'static str {
        match self {
            Refusal::ManyToMany => "many-to-many matching not allowed",
            Refusal::MultipleMatches => "multiple matches for labels",
        }
    }
}

/// `matchers` as `(key, op, value)`; `op` is `=`, `!=`, `=~` or `!~`.
fn parse_matchers(m: &str) -> Vec<(String, String, String)> {
    m.split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            let s = s.trim();
            for op in ["=~", "!~", "!=", "="] {
                if let Some((k, v)) = s.split_once(op) {
                    return (
                        k.trim().to_string(),
                        op.to_string(),
                        v.trim().trim_matches('"').to_string(),
                    );
                }
            }
            panic!("matcher {s}");
        })
        .collect()
}

fn matches(labels: &Labels, matchers: &[(String, String, String)]) -> bool {
    matchers.iter().all(|(k, op, v)| {
        let have = labels.get(k).map(String::as_str).unwrap_or("");
        let alternatives = || v.split('|').any(|a| a == have);
        match op.as_str() {
            "=" => have == v,
            "!=" => have != v,
            "=~" => alternatives(),
            "!~" => !alternatives(),
            _ => unreachable!(),
        }
    })
}

fn apply(op: char, a: f64, b: f64) -> Option<f64> {
    match op {
        '+' => Some(a + b),
        '-' => Some(a - b),
        '*' => Some(a * b),
        '/' => (b != 0.0).then(|| a / b),
        _ => unreachable!(),
    }
}

/// The labels matching uses: every label, `__recording__` included.
fn match_key(labels: &Labels, matching: &Matching) -> Labels {
    match matching {
        Matching::Default => labels.clone(),
        Matching::On(keep) => labels
            .iter()
            .filter(|(k, _)| keep.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        Matching::Ignoring(drop) => labels
            .iter()
            .filter(|(k, _)| !drop.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

/// One expected series at one time.
#[derive(Clone, Debug)]
struct Val {
    labels: Labels,
    /// `None` where the point's window contains `K`.
    v: Option<f64>,
    /// The band its windows give, where that is known exactly.
    band: Option<(f64, f64)>,
    /// Whether the value has no band to give: a gauge, or a count.
    exact: bool,
    /// For a rate's band, the windows at its span's two ends (seconds from
    /// `T0`), where every read it is made of has them alike: two operands
    /// whose edges differ are widened to the union of both before they are
    /// combined.
    edges: Option<Edges>,
    /// Whether the value combines reads too far apart to bound together
    /// (an operator whose operands' spans share no time): it has no band.
    interpolated: bool,
    /// The longest row spacing of the series it is made of.
    every: u64,
    /// The rows of the series it is made of.
    rows: Vec<Rows>,
}

/// How the oracle evaluates: in raw mode a series' value is its latest
/// point, computed at its own row time, held for the gap before it; on the
/// grid a gauge is held for the step or its spacing, and a rate reads back
/// `back`. `step` is the rate's span on the grid.
#[derive(Clone, Copy)]
struct Mode {
    raw: bool,
    back: u64,
    step: u64,
    /// The query's end: rows after it are not read.
    end: u64,
}

/// A rate's left and right windows, `(begin, end)` each.
type Edges = ((f64, f64), (f64, f64));

/// A rate's band from windows `[x - w, x]` at both ends of a span of `s`
/// nanoseconds.
fn rate_band(v: f64, s: u64, w: u64) -> (f64, f64) {
    let (s, w) = (s as f64, w as f64);
    (v * s / (s + w), v * s / (s - w))
}

/// Whether two operands' reads are too far apart to bound together: both
/// rates with edges that differ, the later left window ending after the
/// earlier right window begins.
fn apart(l: &Val, r: &Val) -> bool {
    match (l.edges, r.edges) {
        (Some(le), Some(re)) if le != re => {
            let left_end = le.0 .1.max(re.0 .1);
            let right_begin = le.1 .0.min(re.1 .0);
            right_begin - left_end <= 0.0
        }
        _ => false,
    }
}

/// Two operands' bands combined by `op`: an exact operand counts as
/// `[v, v]`; a rate band whose edges differ from the other's is first
/// widened to their union, the longer window over the same span.
fn combine(op: char, l: &Val, r: &Val) -> Option<(f64, f64)> {
    let (lv, rv) = (l.v?, r.v?);
    let (mut lb, mut rb) = (l.band, r.band);
    if lb.is_none() && rb.is_none() {
        return None;
    }
    if !l.exact && lb.is_none() || !r.exact && rb.is_none() {
        return None;
    }
    if let (Some(le), Some(re)) = (l.edges, r.edges) {
        if le != re {
            let union = (
                (le.0 .0.min(re.0 .0), le.0 .1.max(re.0 .1)),
                (le.1 .0.min(re.1 .0), le.1 .1.max(re.1 .1)),
            );
            // A band is the increase over the longest and the shortest time
            // between the reads; over the union both stretch.
            let longest = |e: Edges| e.1 .1 - e.0 .0;
            let shortest = |e: Edges| e.1 .0 - e.0 .1;
            // Spans that do not overlap (the union's shortest is not
            // positive) leave the bands alone.
            // Each end moves away from the value: a negative end by the
            // other ratio than a positive one.
            let widen = |b: (f64, f64), own: Edges, v: f64| {
                if [longest(own), shortest(own), longest(union), shortest(union)]
                    .iter()
                    .any(|x| *x <= 0.0)
                {
                    return b;
                }
                let ratios = [
                    longest(own) / longest(union),
                    shortest(own) / shortest(union),
                ];
                let lo = ratios.map(|r| b.0 * r).into_iter().fold(b.0, f64::min);
                let hi = ratios.map(|r| b.1 * r).into_iter().fold(b.1, f64::max);
                (lo.min(v), hi.max(v))
            };
            lb = lb.map(|b| widen(b, le, lv));
            rb = rb.map(|b| widen(b, re, rv));
        }
    }
    let (a, b) = lb.unwrap_or((lv, lv));
    let (c, d) = rb.unwrap_or((rv, rv));
    if op == '/' && c <= 0.0 && d >= 0.0 {
        return None;
    }
    let corners = [(a, c), (a, d), (b, c), (b, d)].map(|(x, y)| apply(op, x, y));
    let corners: Option<Vec<f64>> = corners.into_iter().collect();
    let corners = corners?;
    Some((
        corners.iter().copied().fold(f64::INFINITY, f64::min),
        corners.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    ))
}

/// `(lo, hi)` under `f`, which may reverse them.
fn map_band(band: (f64, f64), f: impl Fn(f64) -> Option<f64>) -> Option<(f64, f64)> {
    let (a, b) = (f(band.0)?, f(band.1)?);
    Some((a.min(b), a.max(b)))
}

impl World {
    /// The expected series of `e` at `t`.
    fn eval(&self, e: &E, t: u64, mode: Mode) -> Result<Vec<Val>, Refusal> {
        Ok(match e {
            E::Sel {
                f,
                metric,
                matchers,
            } => {
                let matchers = parse_matchers(matchers);
                self.instances
                    .iter()
                    .filter(|i| i.metric == *metric && matches(&i.labels, &matchers))
                    .filter_map(|i| {
                        let row = i.row_at(t)?;
                        // Held for the gap before it in raw mode; on the
                        // grid a gauge for the step or its spacing, and a
                        // rate needs a row at or after `t`.
                        // Rows after the query's end are not read: past the
                        // last one read, a rate is held at it when the next
                        // row would fall after the end, and ends otherwise
                        // (a row at the end would have been read).
                        let last_read = i.row_at(mode.end)?;
                        let held = if mode.raw {
                            i.every
                        } else if matches!(f, F::Rate(_) | F::Irate) {
                            if t > last_read && mode.end - last_read >= i.every {
                                return None;
                            }
                            u64::MAX
                        } else {
                            mode.step.max(i.every)
                        };
                        if t - row > held {
                            return None;
                        }
                        let at = if mode.raw { row } else { t };
                        let mut band = None;
                        let mut edges = None;
                        let v = match (f, i.kind) {
                            (F::Value, Kind::Gauge { .. }) => Some(i.gauge(row)),
                            (F::Rate(range), Kind::Counter { .. }) => {
                                // Over `[t - range, t]`, from the first row
                                // where that is earlier; in raw mode at the
                                // row.
                                let t = if mode.raw { row } else { t.min(last_read) };
                                if t <= i.first() {
                                    return None;
                                }
                                let left = t.saturating_sub(*range).max(i.first());
                                let a = i.row_at(left)?;
                                let b = i.row_from(t).unwrap_or(t);
                                let v = side(a, b).map(|s| match s {
                                    Side::Before if i.idle => 0.0,
                                    s => on_side(i.per_s(), s),
                                });
                                band = v.map(|v| rate_band(v, t - left, i.window));
                                if !mode.raw {
                                    let window = |x: u64| (rel(x - i.window), rel(x));
                                    edges = Some((window(left), window(t)));
                                }
                                v
                            }
                            (F::Irate, Kind::Counter { .. }) => {
                                let t = t.min(last_read);
                                let row = i.row_at(t)?;
                                // A rate needs the row before.
                                let before = i.before(row)?;
                                let (a, b, span) = if mode.raw {
                                    (before, row, row - before)
                                } else {
                                    let a = i.row_at(t.saturating_sub(mode.back)).unwrap_or(before);
                                    (a, i.row_from(t).unwrap_or(row), mode.step)
                                };
                                let v = side(a, b).map(|s| match s {
                                    Side::Before if i.idle => 0.0,
                                    s => on_side(i.per_s(), s),
                                });
                                band = v.map(|v| rate_band(v, span, i.window));
                                if !mode.raw {
                                    let window = |x: u64| (rel(x - i.window), rel(x));
                                    edges = Some((window(t - span), window(t)));
                                }
                                v
                            }
                            (F::Idelta, Kind::Gauge { .. }) => {
                                let before = i.before(row)?;
                                side(before, row)
                                    .map(|s| on_side(i.per_s(), s) * (i.every as f64 / S as f64))
                            }
                            (F::Deriv, Kind::Gauge { .. } | Kind::Counter { .. }) => {
                                let from = i.row_from(at.saturating_sub(DERIV_RANGE))?;
                                if from >= row {
                                    return None;
                                }
                                side(from, row).map(|s| on_side(i.per_s(), s))
                            }
                            (F::AvgOverTime, Kind::Gauge { .. }) => {
                                let from = i.row_from(at.saturating_sub(AVG_RANGE))?;
                                let n = (row - from) / i.every + 1;
                                let sum: f64 = (0..n).map(|k| i.gauge(from + k * i.every)).sum();
                                Some(sum / n as f64)
                            }
                            other => panic!("{other:?} on {}", i.metric),
                        };
                        Some(Val {
                            labels: i.labels.clone(),
                            v,
                            band,
                            exact: matches!(f, F::Value | F::AvgOverTime | F::Idelta),
                            edges,
                            interpolated: false,
                            every: i.every,
                            rows: vec![i.rows()],
                        })
                    })
                    .collect()
            }
            E::Agg {
                op,
                grouping,
                inner,
            } => {
                let mut groups: BTreeMap<Labels, Vec<Val>> = BTreeMap::new();
                for val in self.eval(inner, t, mode)? {
                    let key: Labels = match grouping {
                        Grouping::All => Labels::new(),
                        Grouping::By(keep) => val
                            .labels
                            .iter()
                            .filter(|(k, _)| keep.contains(&k.as_str()))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                        Grouping::Without(drop) => val
                            .labels
                            .iter()
                            .filter(|(k, _)| !drop.contains(&k.as_str()))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    };
                    groups.entry(key).or_default().push(val);
                }
                groups
                    .into_iter()
                    .map(|(labels, vals)| {
                        let every = vals.iter().map(|v| v.every).max().unwrap_or(0);
                        let rows = vals.iter().flat_map(|v| v.rows.clone()).collect();
                        let n = vals.len() as f64;
                        let bands: Option<Vec<(f64, f64)>> = vals.iter().map(|v| v.band).collect();
                        let summed = bands.map(|b| {
                            b.iter()
                                .fold((0.0, 0.0), |(lo, hi), (l, h)| (lo + l, hi + h))
                        });
                        let band = match op {
                            Agg::Sum => summed,
                            Agg::Avg => summed.map(|(lo, hi)| (lo / n, hi / n)),
                            _ => None,
                        };
                        let v = if *op == Agg::Count {
                            Some(n)
                        } else {
                            vals.iter().map(|v| v.v).collect::<Option<Vec<f64>>>().map(
                                |vs| match op {
                                    Agg::Sum => vs.iter().sum(),
                                    Agg::Avg => vs.iter().sum::<f64>() / n,
                                    Agg::Min => vs.iter().copied().fold(f64::INFINITY, f64::min),
                                    Agg::Max => {
                                        vs.iter().copied().fold(f64::NEG_INFINITY, f64::max)
                                    }
                                    Agg::Count => unreachable!(),
                                },
                            )
                        };
                        let exact = *op == Agg::Count || vals.iter().all(|v| v.exact);
                        let edges = vals[0]
                            .edges
                            .filter(|e| vals.iter().all(|v| v.edges == Some(*e)))
                            .filter(|_| band.is_some());
                        let interpolated = vals.iter().any(|v| v.interpolated);
                        Val {
                            labels,
                            v,
                            band: band.filter(|_| !interpolated),
                            exact,
                            edges,
                            interpolated,
                            every,
                            rows,
                        }
                    })
                    .collect()
            }
            E::Scalar {
                op,
                k,
                scalar_left,
                inner,
            } => {
                let f = |x: f64| {
                    if *scalar_left {
                        apply(*op, *k, x)
                    } else {
                        apply(*op, x, *k)
                    }
                };
                self.eval(inner, t, mode)?
                    .into_iter()
                    .filter_map(|val| {
                        let v = match val.v {
                            Some(v) => Some(f(v)?),
                            None => None,
                        };
                        let band = val.band.and_then(|b| map_band(b, f));
                        Some(Val { v, band, ..val })
                    })
                    .collect()
            }
            E::Bin { op, l, r, matching } => {
                let left = self.eval(l, t, mode)?;
                let right = self.eval(r, t, mode)?;
                let mut by_key: BTreeMap<Labels, Val> = BTreeMap::new();
                for val in right {
                    if by_key
                        .insert(match_key(&val.labels, matching), val)
                        .is_some()
                    {
                        return Err(Refusal::ManyToMany);
                    }
                }
                let mut used = BTreeSet::new();
                let mut out = Vec::new();
                for val in left {
                    let key = match_key(&val.labels, matching);
                    if let Some(rv) = by_key.get(&key) {
                        if !used.insert(key) {
                            return Err(Refusal::MultipleMatches);
                        }
                        let v = match (val.v, rv.v) {
                            (Some(a), Some(b)) => match apply(*op, a, b) {
                                Some(v) => Some(v),
                                None => continue,
                            },
                            _ => None,
                        };
                        let mut rows = val.rows.clone();
                        rows.extend(rv.rows.iter().copied());
                        let interpolated = val.interpolated || rv.interpolated || apart(&val, rv);
                        let band = combine(*op, &val, rv).filter(|_| !interpolated);
                        out.push(Val {
                            labels: val.labels,
                            v,
                            band,
                            exact: val.exact && rv.exact,
                            edges: if val.edges == rv.edges {
                                val.edges
                            } else {
                                None
                            },
                            interpolated,
                            every: val.every.max(rv.every),
                            rows,
                        });
                    }
                }
                out
            }
            E::Hist {
                h,
                metric,
                matchers,
            } => {
                let matchers = parse_matchers(matchers);
                let pooled: Vec<&Instance> = self
                    .instances
                    .iter()
                    .filter(|i| i.metric == *metric && matches(&i.labels, &matchers))
                    .filter(|i| i.row_at(t).is_some())
                    .collect();
                if pooled.is_empty() {
                    return Ok(Vec::new());
                }
                // Pooled on the rows of the slowest; a tick holds every
                // series' rows since the one before, which can reach back
                // another of their rows.
                let clock = pooled.iter().map(|i| i.every).max().unwrap();
                let reach = 2 * clock;
                let side = side(t.saturating_sub(reach), t);
                let rows: Vec<Rows> = pooled.iter().map(|i| i.rows()).collect();
                let quantiles: Vec<f64> = match h {
                    H::Quantile(q) => vec![*q],
                    H::Quartiles | H::Percentiles => vec![0.25, 0.75],
                    _ => vec![f64::NAN],
                };
                let per_tick: f64 = pooled
                    .iter()
                    .map(|i| i.per_s() * clock as f64 / S as f64)
                    .sum();
                quantiles
                    .into_iter()
                    .map(|q| {
                        let labels = if q.is_nan() {
                            Labels::new()
                        } else {
                            Labels::from([("quantile".to_string(), q.to_string())])
                        };
                        // Per tick, `per_s * clock` observations of each
                        // series, in its value's bucket. A quantile is its
                        // bucket's end, banded by the bucket; a mean or sum
                        // takes bucket midpoints, banded by the starts and
                        // ends.
                        let found = side.map(|side| {
                            let value = |i: &Instance| match (i.kind, side) {
                                (Kind::Histogram { value, .. }, Side::Before) => value,
                                (Kind::Histogram { value, .. }, Side::After) => 4 * value,
                                _ => unreachable!(),
                            };
                            let mut masses: Vec<(u64, f64)> = pooled
                                .iter()
                                .map(|i| (value(i), i.per_s() * clock as f64 / S as f64))
                                .collect();
                            masses.sort_by_key(|m| m.0);
                            let total: f64 = masses.iter().map(|m| m.1).sum();
                            let weighted = |f: &dyn Fn((f64, f64)) -> f64| {
                                masses
                                    .iter()
                                    .map(|(v, n)| f(bucket_range(*v)) * n)
                                    .sum::<f64>()
                            };
                            let (lo, mid, hi) = (
                                weighted(&|(a, _)| a),
                                weighted(&|(a, b)| (a + b) / 2.0),
                                weighted(&|(_, b)| b),
                            );
                            match h {
                                H::Quantile(_) | H::Quartiles | H::Percentiles => {
                                    let mut seen = 0.0;
                                    let mut at = masses.last().unwrap().0;
                                    for (v, n) in &masses {
                                        seen += n;
                                        if seen >= q * total {
                                            at = *v;
                                            break;
                                        }
                                    }
                                    let (a, b) = bucket_range(at);
                                    (b, Some((a, b)))
                                }
                                H::Count => (per_tick, None),
                                H::Sum => (mid, Some((lo, hi))),
                                H::Mean => (mid / total, Some((lo / total, hi / total))),
                                H::Irate => (per_tick * S as f64 / clock as f64, None),
                            }
                        });
                        let (v, band) = match found {
                            Some((v, band)) => (Some(v), band),
                            None => (None, None),
                        };
                        Val {
                            labels,
                            v,
                            band,
                            exact: matches!(h, H::Count),
                            edges: None,
                            interpolated: false,
                            every: clock,
                            rows: rows.clone(),
                        }
                    })
                    .collect()
            }
        })
    }

    /// The time of the tick a pooled histogram reports at or before `t`:
    /// the latest row of its slowest series (the last of equals, as the
    /// engine takes them).
    fn histogram_tick(&self, e: &E, t: u64) -> Option<u64> {
        let E::Hist {
            metric, matchers, ..
        } = e
        else {
            return None;
        };
        let matchers = parse_matchers(matchers);
        let pooled: Vec<&Instance> = self
            .instances
            .iter()
            .filter(|i| i.metric == *metric && matches(&i.labels, &matchers))
            .collect();
        let slowest = pooled.iter().map(|i| i.every).max()?;
        pooled
            .iter()
            .rfind(|i| i.every == slowest)
            .and_then(|i| i.row_at(t))
    }
}

/// A point of a raw-mode series.
#[derive(Clone, Copy, Debug)]
struct Pt {
    t: u64,
    v: Option<f64>,
    band: Option<(f64, f64)>,
    /// Whether the value has no band to give.
    exact: bool,
}

/// A raw-mode series' point at or before `t` while held: no further past
/// it than the gap from the point before, and at most `bound` (for the
/// first point, `bound`). Every row of these fixtures after a series' first
/// has a value, so the points are the rows.
fn held(series: &[Pt], t: u64, bound: u64) -> Option<Pt> {
    let i = series.partition_point(|p| p.t <= t).checked_sub(1)?;
    let gap = match i {
        0 => bound,
        _ => (series[i].t - series[i - 1].t).min(bound),
    };
    (t - series[i].t <= gap).then_some(series[i])
}

type Series = BTreeMap<Labels, Vec<Pt>>;

impl World {
    /// `e` in raw mode, as series: a function's points are at its series'
    /// rows, and an aggregation's or an operator's at every time one of its
    /// operands has a point, each operand contributing its point held there
    /// ([`held`]).
    fn raw_series(&self, e: &E) -> Series {
        self.raw_series_held(e, self.hold_bound(e))
    }

    /// How long a raw join holds a point, as the engine works it out: two
    /// steps (of 1 s), or two of the longest row spacing of a metric the
    /// query reads, whichever is longer.
    fn hold_bound(&self, e: &E) -> u64 {
        fn walk(e: &E, metrics: &mut Vec<&'static str>) {
            match e {
                E::Sel { metric, .. } | E::Hist { metric, .. } => metrics.push(metric),
                E::Agg { inner, .. } | E::Scalar { inner, .. } => walk(inner, metrics),
                E::Bin { l, r, .. } => {
                    walk(l, metrics);
                    walk(r, metrics);
                }
            }
        }
        let mut metrics = Vec::new();
        walk(e, &mut metrics);
        let every = self
            .instances
            .iter()
            .filter(|i| metrics.contains(&i.metric))
            .map(|i| i.every)
            .max()
            .unwrap_or(0);
        (2 * S).max(2 * every)
    }

    fn raw_series_held(&self, e: &E, bound: u64) -> Series {
        let raw = Mode {
            raw: true,
            back: 0,
            step: 0,
            end: u64::MAX,
        };
        match e {
            E::Sel {
                metric, matchers, ..
            } => {
                let m = parse_matchers(matchers);
                let mut out = Series::new();
                for i in self
                    .instances
                    .iter()
                    .filter(|i| i.metric == *metric && matches(&i.labels, &m))
                {
                    let mut points = Vec::new();
                    let mut row = i.first();
                    while row <= i.last() {
                        if let Some(v) = self
                            .eval(e, row, raw)
                            .unwrap()
                            .into_iter()
                            .find(|v| v.labels == i.labels)
                        {
                            points.push(Pt {
                                t: row,
                                v: v.v,
                                band: v.band,
                                exact: v.exact,
                            });
                        }
                        row += i.every;
                    }
                    if !points.is_empty() {
                        out.insert(i.labels.clone(), points);
                    }
                }
                out
            }
            E::Agg {
                op,
                grouping,
                inner,
            } => {
                let mut groups: BTreeMap<Labels, Vec<Vec<Pt>>> = BTreeMap::new();
                for (labels, points) in self.raw_series_held(inner, bound) {
                    let key: Labels = match grouping {
                        Grouping::All => Labels::new(),
                        Grouping::By(keep) => labels
                            .into_iter()
                            .filter(|(k, _)| keep.contains(&k.as_str()))
                            .collect(),
                        Grouping::Without(drop) => labels
                            .into_iter()
                            .filter(|(k, _)| !drop.contains(&k.as_str()))
                            .collect(),
                    };
                    groups.entry(key).or_default().push(points);
                }
                groups
                    .into_iter()
                    .map(|(labels, members)| {
                        let times: BTreeSet<u64> = members.iter().flatten().map(|p| p.t).collect();
                        let points = times
                            .into_iter()
                            .filter_map(|t| {
                                let at: Vec<Pt> =
                                    members.iter().filter_map(|m| held(m, t, bound)).collect();
                                if at.is_empty() {
                                    return None;
                                }
                                let n = at.len() as f64;
                                let vs: Option<Vec<f64>> = at.iter().map(|p| p.v).collect();
                                let v = match op {
                                    Agg::Count => Some(n),
                                    Agg::Sum => vs.map(|vs| vs.iter().sum()),
                                    Agg::Avg => vs.map(|vs| vs.iter().sum::<f64>() / n),
                                    Agg::Min => {
                                        vs.map(|vs| vs.into_iter().fold(f64::INFINITY, f64::min))
                                    }
                                    Agg::Max => vs
                                        .map(|vs| vs.into_iter().fold(f64::NEG_INFINITY, f64::max)),
                                };
                                let bands: Option<Vec<(f64, f64)>> =
                                    at.iter().map(|p| p.band).collect();
                                let summed = bands.map(|b| {
                                    b.iter()
                                        .fold((0.0, 0.0), |(lo, hi), (l, h)| (lo + l, hi + h))
                                });
                                let band = match op {
                                    Agg::Sum => summed,
                                    Agg::Avg => summed.map(|(lo, hi)| (lo / n, hi / n)),
                                    _ => None,
                                };
                                let exact = *op == Agg::Count || at.iter().all(|p| p.exact);
                                Some(Pt { t, v, band, exact })
                            })
                            .collect();
                        (labels, points)
                    })
                    .collect()
            }
            E::Scalar {
                op,
                k,
                scalar_left,
                inner,
            } => {
                let f = |x: f64| {
                    if *scalar_left {
                        apply(*op, *k, x)
                    } else {
                        apply(*op, x, *k)
                    }
                };
                self.raw_series_held(inner, bound)
                    .into_iter()
                    .map(|(labels, points)| {
                        let points = points
                            .into_iter()
                            .filter_map(|p| {
                                let v = match p.v {
                                    Some(v) => Some(f(v)?),
                                    None => None,
                                };
                                Some(Pt {
                                    t: p.t,
                                    v,
                                    band: p.band.and_then(|b| map_band(b, f)),
                                    exact: p.exact,
                                })
                            })
                            .collect();
                        (labels, points)
                    })
                    .collect()
            }
            E::Bin { op, l, r, matching } => {
                let right: BTreeMap<Labels, Vec<Pt>> = self
                    .raw_series(r)
                    .into_iter()
                    .map(|(labels, points)| (match_key(&labels, matching), points))
                    .collect();
                self.raw_series_held(l, bound)
                    .into_iter()
                    .filter_map(|(labels, left)| {
                        let right = right.get(&match_key(&labels, matching))?;
                        let times: BTreeSet<u64> =
                            left.iter().chain(right.iter()).map(|p| p.t).collect();
                        let points = times
                            .into_iter()
                            .filter_map(|t| {
                                let (a, b) = (held(&left, t, bound)?, held(right, t, bound)?);
                                let (v, band) = match (a.v, b.v) {
                                    (Some(x), Some(y)) => {
                                        let v = apply(*op, x, y)?;
                                        // Each operand's band as it is (a
                                        // gauge's as `[v, v]`).
                                        let band = (a.band.is_some() || b.band.is_some())
                                            .then(|| {
                                                let l = Val {
                                                    labels: Labels::new(),
                                                    v: Some(x),
                                                    band: a.band,
                                                    exact: a.exact,
                                                    edges: None,
                                                    interpolated: false,
                                                    every: 0,
                                                    rows: Vec::new(),
                                                };
                                                let r = Val {
                                                    v: Some(y),
                                                    band: b.band,
                                                    exact: b.exact,
                                                    ..l.clone()
                                                };
                                                combine(*op, &l, &r)
                                            })
                                            .flatten();
                                        (Some(v), band)
                                    }
                                    _ => (None, None),
                                };
                                Some(Pt {
                                    t,
                                    v,
                                    band,
                                    exact: a.exact && b.exact,
                                })
                            })
                            .collect::<Vec<_>>();
                        (!points.is_empty()).then_some((labels, points))
                    })
                    .collect()
            }
            E::Hist { .. } => unreachable!("histogram functions are not run in raw mode"),
        }
    }
}

fn sel(f: F, metric: &'static str, matchers: &'static str) -> E {
    E::Sel {
        f,
        metric,
        matchers,
    }
}

fn agg(op: Agg, grouping: Grouping, inner: E) -> E {
    E::Agg {
        op,
        grouping,
        inner: Box::new(inner),
    }
}

fn bin(op: char, l: E, r: E, matching: Matching) -> E {
    E::Bin {
        op,
        l: Box::new(l),
        r: Box::new(r),
        matching,
    }
}

fn scalar(op: char, k: f64, scalar_left: bool, inner: E) -> E {
    E::Scalar {
        op,
        k,
        scalar_left,
        inner: Box::new(inner),
    }
}

fn hist(h: H, matchers: &'static str) -> E {
    E::Hist {
        h,
        metric: "lat",
        matchers,
    }
}

fn cases() -> Vec<E> {
    use Grouping::*;
    let mut out = Vec::new();
    let groupings = || {
        vec![
            All,
            By(vec!["cpu"]),
            By(vec!["source"]),
            By(vec!["state", "source"]),
            Without(vec!["state"]),
            Without(vec!["cpu", "state"]),
        ]
    };
    for f in [RATE, F::Irate] {
        for m in [
            "",
            r#"state="user""#,
            r#"cpu=~"0|1""#,
            r#"state!="user""#,
            r#"cpu!~"2""#,
            r#"cpu="9""#,
        ] {
            out.push(sel(f, "c", m));
        }
        for op in [Agg::Sum, Agg::Avg, Agg::Min, Agg::Max, Agg::Count] {
            for g in groupings() {
                out.push(agg(op, g, sel(f, "c", "")));
            }
        }
    }
    let rate = || sel(RATE, "c", "");
    // Ranges under a step (part of each step unread), between the slow
    // sampler's rows, over several steps, and reaching before the first
    // row (taken from it).
    for range in [S, 3 * S, 20 * S, 60 * S] {
        let f = F::Rate(range);
        out.push(sel(f, "c", ""));
        out.push(agg(Agg::Sum, Grouping::By(vec!["cpu"]), sel(f, "c", "")));
        out.push(sel(f, "cx", ""));
        out.push(sel(f, "cr", ""));
        out.push(agg(Agg::Sum, Grouping::All, sel(f, "cs", "")));
        out.push(bin(
            '/',
            sel(f, "c", r#"state="user""#),
            sel(RATE, "c", r#"state="system""#),
            Matching::Ignoring(vec!["state"]),
        ));
    }
    out.push(scalar('*', 2.0, false, rate()));
    out.push(scalar('*', 2.0, true, rate()));
    out.push(scalar('/', 4.0, false, rate()));
    out.push(scalar('+', 1.0, false, rate()));
    out.push(scalar('-', 100.0, true, rate()));
    out.push(scalar(
        '/',
        1e9,
        false,
        agg(Agg::Sum, By(vec!["cpu"]), rate()),
    ));
    out.push(bin(
        '/',
        agg(Agg::Sum, All, rate()),
        agg(Agg::Sum, All, rate()),
        Matching::Default,
    ));
    out.push(bin(
        '/',
        sel(RATE, "c", r#"state="user""#),
        sel(RATE, "c", r#"state="system""#),
        Matching::Ignoring(vec!["state"]),
    ));
    out.push(bin(
        '/',
        agg(
            Agg::Sum,
            By(vec!["cpu", "source"]),
            sel(RATE, "c", r#"state="user""#),
        ),
        agg(Agg::Sum, By(vec!["cpu", "source"]), rate()),
        Matching::On(vec!["cpu", "source"]),
    ));
    out.push(bin(
        '/',
        agg(Agg::Sum, By(vec!["source", "host"]), rate()),
        sel(F::Value, "cores", ""),
        Matching::Default,
    ));
    out.push(bin(
        '/',
        agg(Agg::Sum, All, rate()),
        sel(F::Value, "cores", ""),
        Matching::On(vec![]),
    ));
    out.push(bin(
        '+',
        agg(Agg::Sum, By(vec!["source", "host"]), rate()),
        agg(Agg::Sum, By(vec!["source", "host"]), sel(F::Value, "g", "")),
        Matching::Default,
    ));
    out.push(bin(
        '/',
        sel(RATE, "c", r#"state="user""#),
        sel(F::Value, "g", ""),
        Matching::Ignoring(vec!["state"]),
    ));
    out.push(bin(
        '-',
        sel(F::Value, "g", ""),
        sel(F::Value, "g", ""),
        Matching::Default,
    ));
    out.push(bin(
        '*',
        sel(F::Value, "g", ""),
        agg(Agg::Sum, All, sel(F::Value, "cores", "")),
        Matching::On(vec![]),
    ));
    for f in [F::Value, F::Idelta, F::Deriv, F::AvgOverTime] {
        out.push(sel(f, "g", ""));
        out.push(sel(f, "g", r#"cpu="9""#));
        for op in [Agg::Sum, Agg::Avg, Agg::Min, Agg::Max, Agg::Count] {
            for g in [All, By(vec!["cpu"]), By(vec!["source"])] {
                out.push(agg(op, g, sel(f, "g", "")));
            }
        }
    }
    out.push(scalar('*', 2.0, false, sel(F::Value, "g", "")));
    out.push(scalar('-', 1.0, false, sel(F::AvgOverTime, "g", "")));
    out.push(sel(F::Deriv, "c", ""));
    out.push(sel(F::Deriv, "c", r#"cpu="9""#));
    out.push(agg(Agg::Sum, By(vec!["source"]), sel(F::Deriv, "c", "")));
    // Combinations of the two samplers of one kind: gauges (the second
    // reading back its range, so both sides have a value at the slow
    // sampler's rows before the range), then rates.
    out.push(bin(
        '+',
        agg(Agg::Sum, By(vec!["source", "host"]), sel(F::Value, "g", "")),
        agg(
            Agg::Sum,
            By(vec!["source", "host"]),
            sel(F::Value, "gf", ""),
        ),
        Matching::Default,
    ));
    out.push(bin(
        '+',
        agg(Agg::Sum, By(vec!["source", "host"]), sel(F::Value, "g", "")),
        agg(
            Agg::Sum,
            By(vec!["source", "host"]),
            sel(F::AvgOverTime, "gf", ""),
        ),
        Matching::Default,
    ));
    out.push(bin(
        '+',
        agg(Agg::Sum, By(vec!["source", "host"]), rate()),
        agg(Agg::Sum, By(vec!["source", "host"]), sel(RATE, "cx", "")),
        Matching::Default,
    ));
    // The same with `irate`, whose window at the slow sampler's rows is the
    // gap from the row before, and with one of each.
    out.push(bin(
        '+',
        agg(Agg::Sum, By(vec!["source", "host"]), sel(F::Irate, "c", "")),
        agg(
            Agg::Sum,
            By(vec!["source", "host"]),
            sel(F::Irate, "cx", ""),
        ),
        Matching::Default,
    ));
    out.push(bin(
        '/',
        agg(Agg::Sum, By(vec!["source", "host"]), sel(F::Irate, "c", "")),
        agg(Agg::Sum, By(vec!["source", "host"]), sel(RATE, "cx", "")),
        Matching::Default,
    ));
    // Operators between two banded operands: a difference, a product with a
    // negative side, and an operator over an operator's result.
    out.push(bin(
        '-',
        sel(RATE, "c", r#"state="system""#),
        sel(RATE, "c", r#"state="user""#),
        Matching::Ignoring(vec!["state"]),
    ));
    out.push(bin(
        '-',
        agg(Agg::Sum, By(vec!["source", "host"]), rate()),
        agg(Agg::Sum, By(vec!["source", "host"]), sel(RATE, "cx", "")),
        Matching::Default,
    ));
    out.push(bin(
        '*',
        scalar(
            '*',
            -1.0,
            true,
            agg(Agg::Sum, By(vec!["source", "host"]), rate()),
        ),
        agg(Agg::Sum, By(vec!["source", "host"]), sel(RATE, "cx", "")),
        Matching::Default,
    ));
    out.push(bin(
        '/',
        bin(
            '+',
            agg(Agg::Sum, By(vec!["source", "host"]), rate()),
            agg(Agg::Sum, By(vec!["source", "host"]), sel(RATE, "cr", "")),
            Matching::Default,
        ),
        agg(Agg::Sum, By(vec!["source", "host"]), sel(RATE, "cx", "")),
        Matching::Default,
    ));
    // Where `c` and `cx` are in different tables, the inner sum's operands
    // differ in their windows, so it carries none and the outer division
    // widens nothing.
    out.push(bin(
        '/',
        bin(
            '+',
            agg(Agg::Sum, By(vec!["source", "host"]), rate()),
            agg(Agg::Sum, By(vec!["source", "host"]), sel(RATE, "cx", "")),
            Matching::Default,
        ),
        agg(Agg::Sum, By(vec!["source", "host"]), sel(RATE, "cx", "")),
        Matching::Default,
    ));
    // A constant over a rate (the band's ends swap), a constant less a
    // gauge, a sum over an operator's result, an aggregation over an
    // aggregation.
    out.push(scalar('/', 1e6, true, rate()));
    out.push(scalar('-', 1e4, true, sel(F::Value, "g", "")));
    out.push(agg(
        Agg::Sum,
        By(vec!["cpu"]),
        bin(
            '/',
            sel(RATE, "c", r#"state="user""#),
            sel(RATE, "c", r#"state="system""#),
            Matching::Ignoring(vec!["state"]),
        ),
    ));
    out.push(agg(Agg::Max, All, agg(Agg::Sum, By(vec!["cpu"]), rate())));
    // A counter that does not move before `K`.
    out.push(sel(RATE, "ci", ""));
    out.push(agg(Agg::Sum, All, sel(RATE, "ci", "")));
    out.push(sel(F::Irate, "ci", ""));
    // A counter that resets, alone and against the other sampler's.
    out.push(sel(RATE, "cr", ""));
    out.push(agg(Agg::Sum, All, sel(RATE, "cr", "")));
    out.push(sel(F::Irate, "cr", ""));
    out.push(agg(Agg::Sum, All, sel(F::Irate, "cr", "")));
    out.push(bin(
        '+',
        agg(Agg::Sum, By(vec!["source", "host"]), sel(RATE, "cr", "")),
        agg(Agg::Sum, By(vec!["source", "host"]), sel(RATE, "cx", "")),
        Matching::Default,
    ));
    // Series that stop at `STOP`, alone and beside ones that do not.
    out.push(sel(F::Value, "gs", ""));
    out.push(agg(Agg::Sum, All, sel(F::Value, "gs", "")));
    out.push(sel(RATE, "cs", ""));
    out.push(agg(Agg::Sum, All, sel(RATE, "cs", "")));
    out.push(agg(Agg::Sum, By(vec!["source"]), sel(RATE, "cs", "")));
    out.push(bin(
        '+',
        agg(
            Agg::Sum,
            By(vec!["source", "host"]),
            sel(F::Value, "gs", ""),
        ),
        agg(Agg::Sum, By(vec!["source", "host"]), sel(F::Value, "g", "")),
        Matching::Default,
    ));
    out.push(bin(
        '/',
        agg(Agg::Sum, By(vec!["source", "host"]), sel(RATE, "cs", "")),
        agg(Agg::Sum, By(vec!["source", "host"]), rate()),
        Matching::Default,
    ));
    for q in [0.25, 0.5, 0.75, 0.99] {
        out.push(hist(H::Quantile(q), ""));
    }
    out.push(hist(H::Quantile(0.5), r#"source="a""#));
    out.push(hist(H::Quartiles, ""));
    out.push(hist(H::Percentiles, ""));
    for h in [H::Count, H::Sum, H::Mean, H::Irate] {
        out.push(hist(h, ""));
        out.push(hist(h, r#"source="a""#));
    }
    out
}

// ── Running ──

/// Whether two JSON values are equal, numbers to within 1e-12 relative:
/// summing in another order moves the last bit.
fn json_close(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            (x - y).abs() <= 1e-12 * x.abs().max(y.abs()).max(1.0)
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_close(v, w)))
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(v, w)| json_close(v, w))
        }
        _ => a == b,
    }
}

/// The first place two JSON values differ, as a path and both values.
fn first_difference(a: &serde_json::Value, b: &serde_json::Value) -> String {
    use serde_json::Value;
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                match y.get(k) {
                    Some(w) if json_close(v, w) => {}
                    Some(w) => return format!("{k}.{}", first_difference(v, w)),
                    None => return format!("{k}: only in display"),
                }
            }
            "keys differ".into()
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return format!("lengths {} and {}", x.len(), y.len());
            }
            for (i, (v, w)) in x.iter().zip(y).enumerate() {
                if !json_close(v, w) {
                    return format!("[{i}].{}", first_difference(v, w));
                }
            }
            "equal".into()
        }
        _ => format!(" {a} against {b}"),
    }
}

/// The visible labels of a result series: without `__name__`.
fn visible(metric: &std::collections::HashMap<String, String>) -> Labels {
    metric
        .iter()
        .filter(|(k, _)| k.as_str() != "__name__")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn close(a: f64, b: f64, tolerance: f64) -> bool {
    (a - b).abs() <= tolerance * b.abs().max(1.0)
}

fn to_ns(t: f64) -> u64 {
    // Rows are at whole milliseconds.
    ((t * 1e3).round() as u64) * MS
}

fn secs(t: u64) -> f64 {
    t as f64 / 1e9
}

/// Seconds since `T0`, for messages.
fn rel(t: u64) -> f64 {
    (t as i64 - T0 as i64) as f64 / 1e9
}

/// A grid range query's bounds and step, in nanoseconds.
#[derive(Clone, Copy)]
struct Span {
    start: u64,
    end: u64,
    step: u64,
}

struct Run<'a> {
    world: &'a World,
    failures: Vec<String>,
    /// Points whose band was compared with the one its windows give.
    bands: usize,
    /// Points whose value was compared.
    compared: usize,
}

impl Run<'_> {
    fn fail(&mut self, e: &E, mode: &str, what: String) {
        self.failures.push(format!(
            "[{}] {mode} `{}`: {what}",
            self.world.name,
            e.render()
        ));
    }

    fn grid(&self, step: u64, end: u64) -> Mode {
        Mode {
            raw: false,
            back: step.max(self.world.slowest),
            step,
            end,
        }
    }

    fn expect_refusal(&mut self, e: &E, mode: &str, r: Result<QueryResult, String>, why: Refusal) {
        match r {
            Err(err) if err.contains(why.text()) => {}
            Err(err) => self.fail(e, mode, format!("refused with {err}, expected {why:?}")),
            Ok(_) => self.fail(e, mode, format!("answered, expected {why:?}")),
        }
    }

    /// The times in `span` the oracle has a point of `labels` at, among the
    /// grid's (none for a histogram function) and, unless `grid_only`, the
    /// rows of the series it is made of.
    #[allow(clippy::too_many_arguments)]
    fn expected_times(
        &self,
        e: &E,
        labels: &Labels,
        rows: &[Rows],
        span: Span,
        mode: Mode,
        grid_only: bool,
    ) -> Vec<u64> {
        let mut candidates = BTreeSet::new();
        let rows_only = e.is_histogram();
        let first_allowed = if rows_only {
            span.start
        } else {
            span.start / span.step * span.step
        };
        if !rows_only {
            let mut t = first_allowed;
            while t <= span.end {
                candidates.insert(t);
                t += span.step;
            }
        }
        if !grid_only || rows_only {
            for (first, every, last) in rows {
                let mut t = *first;
                while t <= (*last).min(span.end) {
                    if t >= first_allowed {
                        candidates.insert(t);
                    }
                    t += every;
                }
            }
        }
        candidates
            .into_iter()
            .filter(|t| {
                self.world
                    .eval(e, *t, mode)
                    .is_ok_and(|vals| vals.iter().any(|v| v.labels == *labels))
            })
            .collect()
    }

    /// Check a matrix result over `span`.
    fn check_matrix(&mut self, e: &E, mode_name: &str, result: QueryResult, span: Span) {
        let Span { start, end, step } = span;
        // Points on the grid or at the series' rows, known exactly; else a
        // combination of samplers, at the slow sampler's rows, whose rates
        // span its spacing.
        let exact = (self.world.aligned || e.one_sampler()) && !e.is_histogram();
        let mode = if exact || e.is_histogram() {
            self.grid(step, end)
        } else {
            Mode {
                step: self.world.slow_cadence,
                ..self.grid(step, end)
            }
        };
        let QueryResult::Matrix { result, .. } = result else {
            self.fail(e, mode_name, "not a matrix".into());
            return;
        };
        // Label sets, against the oracle in the middle of the range.
        let mid = start + (end - start) / 2;
        let expected: BTreeMap<Labels, Val> = match self.world.eval(e, mid, mode) {
            Ok(v) => v.into_iter().map(|v| (v.labels.clone(), v)).collect(),
            Err(r) => {
                self.fail(e, mode_name, format!("answered, expected {r:?}"));
                return;
            }
        };
        let got: Vec<Labels> = result.iter().map(|m| visible(&m.metric)).collect();
        let got_set: BTreeSet<Labels> = got.iter().cloned().collect();
        if got_set.len() != got.len() {
            self.fail(e, mode_name, format!("a label set twice: {got:?}"));
        }
        let expected_set: BTreeSet<Labels> = expected.keys().cloned().collect();
        if got_set != expected_set {
            self.fail(
                e,
                mode_name,
                format!("labels {got_set:?}, expected {expected_set:?}"),
            );
            return;
        }
        let tolerance = e.tolerance();
        // A grid range starts at the step multiple at or before `start`.
        let first_allowed = start / step * step;
        for m in &result {
            let labels = visible(&m.metric);
            let model = &expected[&labels];
            let max_gap = step.max(model.every) + 10 * MS;
            let times = self.expected_times(e, &labels, &model.rows, span, mode, exact);
            for (i, (t, v)) in m.values.iter().enumerate() {
                let tn = to_ns(*t);
                if tn + 10 * MS < first_allowed || tn > end + 10 * MS {
                    self.fail(
                        e,
                        mode_name,
                        format!("{labels:?}: point at {:.3} outside the range", rel(tn)),
                    );
                    break;
                }
                if exact && !tn.is_multiple_of(step) {
                    self.fail(
                        e,
                        mode_name,
                        format!("{labels:?}: point at {:.3} off the step grid", rel(tn)),
                    );
                    break;
                }
                let want = match self.world.eval(e, tn, mode) {
                    Ok(vals) => vals.into_iter().find(|v| v.labels == labels),
                    Err(_) => None,
                };
                let Some(want) = want else {
                    self.fail(
                        e,
                        mode_name,
                        format!("{labels:?} at {:.3}: {v}, expected no point", rel(tn)),
                    );
                    break;
                };
                match want.v {
                    None => {}
                    Some(w) if close(*v, w, tolerance) => self.compared += 1,
                    Some(w) => {
                        self.fail(
                            e,
                            mode_name,
                            format!("{labels:?} at {:.3}: {v}, expected {w}", rel(tn)),
                        );
                        break;
                    }
                }
                let band = m.bands.as_ref().and_then(|b| b[i]);
                let marked = m.interpolated.as_ref().is_some_and(|f| f[i]);
                if marked != want.interpolated || want.interpolated && band.is_some() {
                    self.fail(
                        e,
                        mode_name,
                        format!(
                            "{labels:?} at {:.3}: interpolated {marked} with band {band:?}, expected interpolated {} and {}",
                            rel(tn),
                            want.interpolated,
                            if want.interpolated { "no band" } else { "a band where known" }
                        ),
                    );
                    break;
                }
                match (want.band, band) {
                    (Some((wl, wh)), Some((lo, hi))) => {
                        if !band_close((lo, hi), (wl, wh)) {
                            self.fail(
                                e,
                                mode_name,
                                format!(
                                    "{labels:?} at {:.3}: band [{lo}, {hi}], expected [{wl}, {wh}]",
                                    rel(tn)
                                ),
                            );
                            break;
                        }
                        self.bands += 1;
                    }
                    (Some(w), None) => {
                        self.fail(
                            e,
                            mode_name,
                            format!("{labels:?} at {:.3}: no band, expected {w:?}", rel(tn)),
                        );
                        break;
                    }
                    (None, Some((lo, hi))) if want.exact && !e.allows_flat_band() => {
                        self.fail(
                            e,
                            mode_name,
                            format!(
                                "{labels:?} at {:.3}: band [{lo}, {hi}] on a value that has none",
                                rel(tn)
                            ),
                        );
                        break;
                    }
                    (None, Some((lo, hi))) => {
                        let slack = tolerance * v.abs().max(1.0);
                        let contains = |x: f64| lo - slack <= x && x <= hi + slack;
                        if !contains(*v) || want.v.is_some_and(|w| !contains(w)) {
                            self.fail(e, mode_name, format!("{labels:?} at {:.3}: band [{lo}, {hi}] around {v}, expected {:?}", rel(tn), want.v));
                            break;
                        }
                        if hi <= lo && !e.allows_flat_band() {
                            self.fail(
                                e,
                                mode_name,
                                format!(
                                    "{labels:?} at {:.3}: band [{lo}, {hi}] has no width",
                                    rel(tn)
                                ),
                            );
                            break;
                        }
                    }
                    (None, None) => {}
                }
                if i > 0 {
                    let prev = to_ns(m.values[i - 1].0);
                    if tn - prev > max_gap {
                        self.fail(
                            e,
                            mode_name,
                            format!("{labels:?}: gap from {:.3} to {:.3}", rel(prev), rel(tn)),
                        );
                        break;
                    }
                }
            }
            // The first and last points: exactly the expected ones where the
            // times are known, else within the series' row spacing.
            let first = m.values.first().map(|p| to_ns(p.0));
            let last = m.values.last().map(|p| to_ns(p.0));
            let (want_first, want_last) = (times.first().copied(), times.last().copied());
            let near = |got: Option<u64>, want: Option<u64>| match (got, want) {
                (Some(g), Some(w)) if exact => g == w,
                (Some(g), Some(w)) => g.abs_diff(w) <= max_gap,
                (None, None) => true,
                _ => false,
            };
            if !near(first, want_first) || !near(last, want_last) {
                self.fail(
                    e,
                    mode_name,
                    format!(
                        "{labels:?}: points from {:?} to {:?}, expected {:?} to {:?}",
                        first.map(rel),
                        last.map(rel),
                        want_first.map(rel),
                        want_last.map(rel)
                    ),
                );
            }
        }
    }

    /// Check a raw-mode result over `[start, end]` against
    /// [`World::raw_series`] cut to the range: the same series, at the same
    /// times, with the same values and bands.
    fn check_raw(&mut self, e: &E, result: QueryResult, start: u64, end: u64) {
        let QueryResult::Matrix { result, .. } = result else {
            self.fail(e, "raw", "not a matrix".into());
            return;
        };
        let want: Series = self
            .world
            .raw_series(e)
            .into_iter()
            .map(|(labels, points)| {
                let points: Vec<Pt> = points
                    .into_iter()
                    .filter(|p| p.t >= start && p.t <= end)
                    .collect();
                (labels, points)
            })
            .filter(|(_, points)| !points.is_empty())
            .collect();
        let got: BTreeSet<Labels> = result.iter().map(|m| visible(&m.metric)).collect();
        let want_labels: BTreeSet<Labels> = want.keys().cloned().collect();
        if got != want_labels || got.len() != result.len() {
            self.fail(
                e,
                "raw",
                format!("labels {got:?}, expected {want_labels:?}"),
            );
            return;
        }
        let tolerance = e.tolerance();
        for m in &result {
            let labels = visible(&m.metric);
            let want = &want[&labels];
            let times: Vec<u64> = m.values.iter().map(|(t, _)| to_ns(*t)).collect();
            let want_times: Vec<u64> = want.iter().map(|p| p.t).collect();
            if times != want_times {
                let first_diff = times
                    .iter()
                    .zip(&want_times)
                    .position(|(a, b)| a != b)
                    .unwrap_or(times.len().min(want_times.len()));
                self.fail(
                    e,
                    "raw",
                    format!(
                        "{labels:?}: {} points, expected {}; first difference at {:?} against {:?}",
                        times.len(),
                        want_times.len(),
                        times.get(first_diff).map(|t| rel(*t)),
                        want_times.get(first_diff).map(|t| rel(*t))
                    ),
                );
                continue;
            }
            for (i, ((_, v), p)) in m.values.iter().zip(want).enumerate() {
                if let Some(w) = p.v {
                    if !close(*v, w, tolerance) {
                        self.fail(
                            e,
                            "raw",
                            format!("{labels:?} at {:.3}: {v}, expected {w}", rel(p.t)),
                        );
                        break;
                    }
                    self.compared += 1;
                }
                let band = m.bands.as_ref().and_then(|b| b[i]);
                match (p.band, band) {
                    (Some((wl, wh)), Some((lo, hi))) => {
                        if !band_close((lo, hi), (wl, wh)) {
                            self.fail(
                                e,
                                "raw",
                                format!(
                                    "{labels:?} at {:.3}: band [{lo}, {hi}], expected [{wl}, {wh}]",
                                    rel(p.t)
                                ),
                            );
                            break;
                        }
                        self.bands += 1;
                    }
                    (Some(w), None) => {
                        self.fail(
                            e,
                            "raw",
                            format!("{labels:?} at {:.3}: no band, expected {w:?}", rel(p.t)),
                        );
                        break;
                    }
                    (None, Some((lo, hi))) if p.exact && !e.allows_flat_band() => {
                        self.fail(
                            e,
                            "raw",
                            format!(
                                "{labels:?} at {:.3}: band [{lo}, {hi}] on a value that has none",
                                rel(p.t)
                            ),
                        );
                        break;
                    }
                    (None, Some((lo, hi))) => {
                        let contains = |x: f64| lo <= x && x <= hi;
                        if !contains(*v) || hi <= lo && !e.allows_flat_band() {
                            self.fail(
                                e,
                                "raw",
                                format!(
                                    "{labels:?} at {:.3}: band [{lo}, {hi}] around {v}",
                                    rel(p.t)
                                ),
                            );
                            break;
                        }
                    }
                    (None, None) => {}
                }
            }
        }
    }

    fn check(&mut self, e: &E) {
        let q = e.render();
        let reader = &self.world.reader;
        let refusal = self
            .world
            .eval(e, T0 + 60 * S, self.grid(S, T0 + 90 * S))
            .err();
        let (start, end) = (T0 + 30 * S, T0 + 90 * S);

        // Each range on the batch rate path and on the per-series one; the
        // last two end a row and a half and one row after `STOP`, where a
        // stopped series is not held (at one row, its next row would have
        // been read).
        let ranges = [
            (start, end, S),
            (start, end, 5 * S),
            (start + 400 * MS, end, 5 * S),
            (start, STOP + 1500 * MS, S),
            (start, STOP + S, S),
        ];
        for ((start, end, step), per_series) in
            ranges.into_iter().flat_map(|r| [(r, false), (r, true)])
        {
            let mode = format!(
                "grid{} from {:.1} to {:.1} step {}s",
                if per_series { " per series" } else { "" },
                rel(start),
                rel(end),
                step / S
            );
            let r = reader
                .query_range_opts(
                    &q,
                    secs(start),
                    secs(end),
                    secs(step),
                    &QueryOptions::default().with_per_series_rates(per_series),
                )
                .map_err(|e| e.to_string());
            match (refusal, r) {
                (Some(why), r) => self.expect_refusal(e, &mode, r, why),
                (None, Err(err)) => self.fail(e, &mode, err),
                (None, Ok(r)) => self.check_matrix(e, &mode, r, Span { start, end, step }),
            }
        }

        // Display: equal to reducing the grid result.
        if refusal.is_none() {
            let opts = DisplayOptions {
                budget: 12,
                ..Default::default()
            };
            let shown = reader.query_range_display_opts(
                &q,
                secs(start),
                secs(end),
                1.0,
                &opts,
                &QueryOptions::default(),
            );
            let reduced = reader
                .query_range_opts(&q, secs(start), secs(end), 1.0, &QueryOptions::default())
                .map(|r| display_from_result(r, secs(start), secs(end), 1.0, &opts));
            match (shown, reduced) {
                (Ok(a), Ok(b)) => {
                    let (a, b) = (sorted_json(&a), sorted_json(&b));
                    if !json_close(&a, &b) {
                        self.fail(
                            e,
                            "display",
                            format!(
                                "differs from reducing the grid result: {}",
                                first_difference(&a, &b)
                            ),
                        );
                    }
                }
                (a, b) => self.fail(e, "display", format!("{:?} / {:?}", a.err(), b.err())),
            }
        }

        // Instant: at a row of the tables without an offset, and between
        // rows.
        for t in [
            T0 + 40 * S,
            T0 + 61 * S + 250 * MS,
            T0 + 67 * S + 900 * MS,
            T0 + 80 * S + 500 * MS,
        ] {
            let mode = format!("instant at {:.3}", rel(t));
            let r = reader.query(&q, Some(secs(t))).map_err(|e| e.to_string());
            if let Some(why) = refusal {
                self.expect_refusal(e, &mode, r, why);
                continue;
            }
            let r = match r {
                Ok(r) => r,
                Err(err) => {
                    self.fail(e, &mode, err);
                    continue;
                }
            };
            let QueryResult::Vector { result, .. } = r else {
                self.fail(e, &mode, format!("{r:?}"));
                continue;
            };
            // The time asked for, or a histogram's latest tick.
            let tick = if e.is_histogram() {
                self.world.histogram_tick(e, t)
            } else {
                Some(t)
            };
            let times: BTreeSet<u64> = result.iter().map(|s| to_ns(s.value.0)).collect();
            if times.iter().any(|tt| Some(*tt) != tick) {
                self.fail(
                    e,
                    &mode,
                    format!(
                        "at {:?}, expected {:?}",
                        times.iter().map(|t| rel(*t)).collect::<Vec<_>>(),
                        tick.map(rel)
                    ),
                );
                continue;
            }
            let at = tick.unwrap_or(t);
            let tolerance = e.tolerance();
            let got: BTreeMap<Labels, f64> = result
                .iter()
                .map(|s| (visible(&s.metric), s.value.1))
                .collect();
            let want = self.world.eval(e, at, self.grid(S, t)).unwrap();
            let same = got.len() == want.len()
                && want.iter().all(|w| {
                    got.get(&w.labels)
                        .is_some_and(|v| w.v.is_none_or(|w| close(*v, w, tolerance)))
                });
            if !same {
                self.fail(e, &mode, format!("{got:?}, expected {want:?}"));
                continue;
            }
            // The band the instant reports, where it is known.
            for s in &result {
                let Some(w) = want.iter().find(|w| w.labels == visible(&s.metric)) else {
                    continue;
                };
                let fine = match (w.band, s.interval) {
                    (Some(wb), Some(b)) => band_close(b, wb),
                    (Some(_), None) => false,
                    (None, Some(_)) => !w.exact || e.allows_flat_band(),
                    (None, None) => true,
                };
                if !fine {
                    self.fail(
                        e,
                        &mode,
                        format!(
                            "{:?}: band {:?}, expected {:?}",
                            w.labels, s.interval, w.band
                        ),
                    );
                }
            }
        }

        // Raw: the series' own times.
        if !e.is_histogram() {
            let (start, end) = (T0 + 50 * S, T0 + 80 * S);
            let r = reader
                .query_range_opts(
                    &q,
                    secs(start),
                    secs(end),
                    1.0,
                    &QueryOptions::with_rate_mode(RateMode::Raw),
                )
                .map_err(|e| e.to_string());
            match (refusal, r) {
                (Some(why), r) => self.expect_refusal(e, "raw", r, why),
                (None, Err(err)) => self.fail(e, "raw", err),
                (None, Ok(r)) => self.check_raw(e, r, start, end),
            }
        }
    }
}

/// A display result as JSON with its series in label order: series order
/// is not part of the result.
fn sorted_json(r: &DisplayResult) -> serde_json::Value {
    let mut v = serde_json::to_value(r).unwrap();
    if let Some(series) = v.get_mut("result").and_then(|r| r.as_array_mut()) {
        series.sort_by_key(|s| s["metric"].to_string());
    }
    v
}

#[test]
fn every_query_in_every_layout_and_mode() {
    let cases = cases();
    let mut failures = Vec::new();
    let mut checked = 0;
    for world in worlds() {
        let mut run = Run {
            world: &world,
            failures: Vec::new(),
            bands: 0,
            compared: 0,
        };
        for e in &cases {
            run.check(e);
            checked += 1;
        }
        // Every row has a window, so a rate has a band; and most points are
        // away from `K`.
        if run.bands == 0 {
            run.failures
                .push(format!("[{}] no band was compared", world.name));
        }
        if run.compared == 0 {
            run.failures
                .push(format!("[{}] no point's value was compared", world.name));
        }
        failures.extend(run.failures);
    }
    assert!(
        failures.is_empty(),
        "{} failures over {checked} query and layout pairs:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The quantile `q` of sorted `values`, interpolated between neighbours.
fn quantile(values: &[f64], q: f64) -> f64 {
    let rank = q * (values.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let frac = rank - lo as f64;
    match values.get(lo + 1) {
        Some(next) => values[lo] + frac * (next - values[lo]),
        None => values[lo],
    }
}

/// Display mode over 60 s with a budget of 12 cuts the 1 s grid into 5 s
/// buckets aligned to multiples of 5 s; each bucket's minimum, quartiles,
/// median and maximum are those of its points, worked out from the
/// fixture, and its band the median of its points' bands. Buckets whose
/// points read across `K` are not compared.
#[test]
fn decimation_matches_the_fixture() {
    let worlds = worlds();
    let world = &worlds[0];
    let (start, end) = (T0 + 30 * S, T0 + 90 * S);
    let opts = DisplayOptions {
        budget: 12,
        ..Default::default()
    };
    let run = Run {
        world,
        failures: Vec::new(),
        bands: 0,
        compared: 0,
    };
    let mut compared = 0;
    for e in [
        sel(F::Value, "g", r#"cpu="1""#),
        agg(Agg::Sum, Grouping::All, sel(RATE, "c", "")),
    ] {
        let DisplayResult::Series { result, .. } = world
            .reader
            .query_range_display_opts(
                &e.render(),
                secs(start),
                secs(end),
                1.0,
                &opts,
                &QueryOptions::default(),
            )
            .unwrap()
        else {
            panic!("{}: not a series", e.render());
        };
        assert_eq!(result.len(), 1, "{}", e.render());
        let mut buckets: BTreeMap<u64, Vec<Val>> = BTreeMap::new();
        let mut t = start;
        while t <= end {
            let val = world.eval(&e, t, run.grid(S, end)).unwrap().remove(0);
            buckets.entry(t / (5 * S)).or_default().push(val);
            t += S;
        }
        let points = &result[0].points;
        assert_eq!(points.len(), buckets.len(), "{}", e.render());
        for (point, (bucket, vals)) in points.iter().zip(buckets) {
            let values: Option<Vec<f64>> = vals.iter().map(|v| v.v).collect();
            let Some(mut values) = values else {
                continue;
            };
            values.sort_by(f64::total_cmp);
            let when = if values.len() == 1 {
                secs(start.max(bucket * 5 * S))
            } else {
                (bucket * 5) as f64
            };
            let want = [
                when,
                values[0],
                quantile(&values, 0.25),
                quantile(&values, 0.5),
                quantile(&values, 0.75),
                *values.last().unwrap(),
            ];
            let got = [
                point.t,
                point.min,
                point.lo,
                point.median,
                point.hi,
                point.max,
            ];
            assert!(
                want.iter().zip(got).all(|(w, g)| close(g, *w, 1e-12)),
                "{} bucket {}: {got:?}, expected {want:?}",
                e.render(),
                bucket * 5 - T0 / S
            );
            let bands: Option<Vec<(f64, f64)>> = vals.iter().map(|v| v.band).collect();
            if let Some(bands) = bands {
                let mut lo: Vec<f64> = bands.iter().map(|b| b.0).collect();
                let mut hi: Vec<f64> = bands.iter().map(|b| b.1).collect();
                lo.sort_by(f64::total_cmp);
                hi.sort_by(f64::total_cmp);
                let (wl, wh) = (quantile(&lo, 0.5), quantile(&hi, 0.5));
                let (Some(gl), Some(gh)) = (point.unc_lo, point.unc_hi) else {
                    panic!("{} bucket {}: no band", e.render(), bucket * 5 - T0 / S);
                };
                assert!(
                    band_close((gl, gh), (wl, wh)),
                    "{} bucket {}: band [{gl}, {gh}], expected [{wl}, {wh}]",
                    e.render(),
                    bucket * 5 - T0 / S
                );
            }
            compared += 1;
        }
    }
    assert!(compared >= 16, "only {compared} buckets compared");
}

/// One counter `c`, every `every` seconds from `T0` for 120 s except in
/// `[hole.0, hole.1)` seconds after `T0`, at 10/s with 5000 more accrued in
/// the hole.
fn counter_table(name: &str, every: u64, hole: (u64, u64)) -> Vec<u8> {
    let mut schema = GroupSchema::default();
    schema.counters.push(MetricDesc {
        name: "c_0".to_string(),
        metadata: std::iter::once(("metric".to_string(), "c".to_string())).collect(),
    });
    let mut builder = GroupTableBuilder::new(name.to_string());
    for s in (0..=120).step_by(every as usize) {
        if (hole.0..hole.1).contains(&s) {
            continue;
        }
        let row = T0 + s * S;
        let value = 1_000 + s * 10 + if s >= hole.1 { 5000 } else { 0 };
        builder.push_row(
            row,
            0,
            Some(Window::new(row - WINDOW, row)),
            &schema,
            &[Some(value)],
            &[],
            &[],
        );
    }
    write_table_parquet(&builder.finish()).unwrap()
}

/// A recording's series takes the reach of its own table, on the batch
/// path as on the per-series one: across a hole in a 1 s recording, its
/// `rate` is the same whether or not the query also reaches a 10 s
/// recording.
#[test]
fn each_series_takes_its_own_tables_reach() {
    let recording = |source: &str, every: u64, hole: (u64, u64)| {
        let labels: Labels = [("source".to_string(), source.to_string())].into();
        InMemorySource {
            name: source.to_string(),
            metadata: labels.clone(),
            labels,
            complete: true,
            tables: vec![(
                "s/counters".to_string(),
                vec![counter_table("s/counters", every, hole)],
            )],
        }
    };
    let archive = |recs: Vec<InMemorySource>| {
        ArchiveReader::from_in_memory(recs, None, BufferPool::new(64 << 20)).unwrap()
    };
    let alone = archive(vec![recording("a", 1, (50, 62))]);
    let both = archive(vec![
        recording("a", 1, (50, 62)),
        recording("b", 10, (0, 0)),
    ]);
    // No matcher on `source`: a matcher the other recording cannot match
    // would leave it out of the query.
    let q = "rate(c[10s])";
    let (start, end) = ((T0 / S + 60) as f64, (T0 / S + 70) as f64);
    let values = |reader: &ArchiveReader, per_series: bool| -> Vec<(f64, f64)> {
        let opts = QueryOptions::default().with_per_series_rates(per_series);
        match reader.query_range_opts(q, start, end, 1.0, &opts).unwrap() {
            QueryResult::Matrix { result, .. } => result
                .into_iter()
                .filter(|m| m.metric.get("source").map(String::as_str) == Some("a"))
                .flat_map(|m| m.values)
                .collect(),
            other => panic!("{other:?}"),
        }
    };
    let want = values(&alone, true);
    assert!(
        want.iter()
            .filter(|(t, _)| *t >= start + 3.0)
            .all(|(_, v)| (v - 10.0).abs() < 1e-6),
        "{want:?}"
    );
    for (name, reader) in [("alone", &alone), ("both", &both)] {
        for per_series in [false, true] {
            assert_eq!(
                values(reader, per_series),
                want,
                "{name} per_series={per_series}"
            );
        }
    }
}

/// Recordings whose sources set no interval on their series: each series
/// takes its own recording's `counter_interval` on the batch path too, so
/// a rate across a hole in the faster one is the same beside a recording
/// that reports a longer interval.
#[test]
fn a_recording_without_intervals_takes_its_own_reach() {
    use metriken_storage::labeled::LabeledSource;
    use metriken_storage::recordings::RecordingsSource;
    use metriken_storage::segmented::{InMemorySegments, SegmentedSource};
    use metriken_storage::DataSource;
    use std::sync::Arc;
    let table = |every: u64, hole: (u64, u64)| -> Arc<dyn DataSource> {
        let store = Arc::new(InMemorySegments::new(vec![counter_table(
            "s/counters",
            every,
            hole,
        )]));
        SegmentedSource::open_with_pool(store, BufferPool::new(64 << 20)).unwrap()
    };
    let labels = |source: &str| -> metriken_storage::Labels {
        let map: Labels = [("source".to_string(), source.to_string())].into();
        map.into()
    };
    let recordings = |with_b: bool| {
        let mut children: Vec<Arc<dyn DataSource>> = vec![Arc::new(LabeledSource::new(
            table(1, (50, 62)),
            labels("a"),
        ))];
        if with_b {
            children.push(Arc::new(
                LabeledSource::new(table(10, (0, 0)), labels("b")).with_interval(10.0),
            ));
        }
        RecordingsSource::new(children)
    };
    let (start, end) = ((T0 / S + 60) as f64, (T0 / S + 70) as f64);
    let values = |with_b: bool, per_series: bool| -> Vec<(f64, f64)> {
        let engine = crate::promql::QueryEngine::new(Arc::new(recordings(with_b)));
        let opts = QueryOptions::default().with_per_series_rates(per_series);
        match engine
            .query_range_opts(r#"rate(c{source="a"}[10s])"#, start, end, 1.0, &opts)
            .unwrap()
        {
            QueryResult::Matrix { result, .. } => {
                result.into_iter().flat_map(|m| m.values).collect()
            }
            other => panic!("{other:?}"),
        }
    };
    let want = values(false, true);
    assert!(!want.is_empty());
    for per_series in [false, true] {
        assert_eq!(values(true, per_series), want, "per_series={per_series}");
    }
}
