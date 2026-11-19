//! AST → streaming-pipeline dispatcher.
//!
//! This is the only PromQL evaluator in the engine — every shape it
//! doesn't recognise becomes `QueryError::Unsupported`.
//!
//! Recognised AST shapes:
//!
//! * `metric{matchers}` — gauge VectorSelector at step grid.
//! * `rate(metric[range])` — the rate over `[t - range, t]`;
//!   `irate(metric[range])` — the rate over one step, or between consecutive
//!   samples in raw mode.
//! * `avg_over_time(metric[range])` / `idelta(metric[range])` — gauge
//!   producers; `deriv(metric[range])` — the slope of a gauge's or a
//!   counter's values.
//! * `histogram_quantile(q, metric)` — single-quantile histogram
//!   extraction. Output is `Built::Materialized` since the streaming
//!   histogram pipeline emits ready-made `MatrixSample`s.
//! * `sum/avg/min/max/count [by | without (..)] (...)` — aggregator
//!   over any series-typed inner.
//! * `lhs OP rhs` where OP ∈ {`+`, `-`, `*`, `/`} — binary ops
//!   between two series sets or between a series set and a number
//!   literal. Honors `on(..)` and `ignoring(..)` matching modifiers.
//!   With no modifier, an unmatched left set is broadcast against a
//!   single unmatched right series (eager-engine parity). `group_left`
//!   and `group_right` match many series of one side to one of the other.
//! * `label_replace(v, dst, replacement, src, regex)` — sets or removes a
//!   label on each series of `v`.
//! * `NumberLiteral` — produces a `Built::Scalar`; usable on either
//!   side of a binary op.
//! * Parenthesised expressions are unwrapped.

use super::histogram::HistogramOps;
use super::per_source::{self, MemberSeries, SourceGroups, Tap, Window};
use promql_parser::parser::{self, Expr};

use crate::promql::streaming::{
    aggregate, braces, collect_to_matrix, distinct_label_sets, interval_binop, matrix_grouped_op,
    matrix_matrix_op, matrix_scalar_op, AggOp, AtPoints, BinOp, CounterGridRate,
    CounterPairwiseRate, GaugeDeriv, GaugeIdelta, GaugeOverTime, GaugeStepGrid, GroupBy, Join,
    LabeledSeries, MatchSpec, OneSide, OverTime, SeriesSet,
};
use crate::promql::Selection;
use crate::promql::{nothing_read, MatrixSample, MetricKind, QueryError, QueryResult};
use crate::{DataSource, QueryOptions, RateMode};

/// Evaluate `expr` via the streaming pipeline. Returns
/// `QueryError::Unsupported` for any AST shape the dispatcher doesn't
/// recognise; this is now the only PromQL evaluator in the engine.
pub fn try_streaming(
    source: &dyn DataSource,
    groups: Option<&SourceGroups>,
    expr: &Expr,
    start: f64,
    end: f64,
    step: f64,
    opts: &QueryOptions,
) -> Result<QueryResult, QueryError> {
    let ctx = make_ctx(source, groups, expr, start, end, step, opts);
    let result = match build(&ctx, expr)? {
        Built::Series {
            series,
            metric_name,
            metric_name_for_error,
        } => {
            let collected = collect_to_matrix(clip_raw(&ctx, series), metric_name);
            if collected.is_empty() {
                if let Some(name) = metric_name_for_error {
                    held_somewhere(source, &name)?;
                }
            }
            QueryResult::Matrix {
                result: collected,
                notices: Vec::new(),
            }
        }
        Built::Materialized { result, name } => {
            if result.is_empty() {
                held_somewhere(source, &name)?;
            }
            QueryResult::Matrix {
                result,
                notices: Vec::new(),
            }
        }
        Built::Scalar(v) => QueryResult::Scalar {
            result: (start, v),
            notices: Vec::new(),
        },
    };
    Ok(result)
}

/// The evaluation context for `[start, end]` at `step`.
fn make_ctx<'a>(
    source: &'a dyn DataSource,
    groups: Option<&'a SourceGroups>,
    expr: &Expr,
    start: f64,
    end: f64,
    step: f64,
    opts: &QueryOptions,
) -> Ctx<'a> {
    let rate_mode = opts.rate_mode;
    let step_ns = (step * 1e9) as u64;
    let raw_start_ns = crate::promql::range_start_ns(start);
    // Grid mode fixes the evaluation-grid phase to the step boundary so two
    // recordings on the same step share a grid (A/B alignment) and gauge/rate
    // labels land on round step multiples. Raw keeps the caller's start, and
    // so does a range of one point (an instant query), which is evaluated at
    // the time asked for. Snap once here so every downstream producer
    // inherits the fixed phase.
    let start_ns = match rate_mode {
        RateMode::Grid if step_ns > 0 && end > start => (raw_start_ns / step_ns) * step_ns,
        _ => raw_start_ns,
    };
    Ctx {
        source,
        next_source: None,
        groups,
        start_ns,
        end_ns: crate::promql::range_end_ns(end),
        step_ns,
        rate_mode,
        eval_timestamps: opts.eval_timestamps.clone(),
        per_series_rates: opts.per_series_rates,
        recording_keys: source.recording_label_keys(),
        raw_hold_ns: match rate_mode {
            RateMode::Raw => raw_hold(source, expr, step_ns),
            RateMode::Grid => 0,
        },
        raw_depth: join_depth(expr),
    }
}

/// The longest a raw join holds a point ([`Join::Latest`]): two steps, or
/// two of the longest interval of a metric the query reads, whichever is
/// longer.
fn raw_hold(source: &dyn DataSource, expr: &Expr, step_ns: u64) -> u64 {
    let interval = metric_names(expr)
        .iter()
        .map(|n| {
            source
                .counter_interval(n)
                .max(source.gauge_interval(n))
                .max(source.histogram_interval(n))
        })
        .fold(source.interval(), f64::max);
    step_ns.saturating_mul(2).max((interval * 2e9) as u64)
}

/// The names of the metrics `expr`'s selectors read.
fn metric_names(expr: &Expr) -> Vec<String> {
    fn walk(expr: &Expr, names: &mut Vec<String>) {
        match expr {
            Expr::Aggregate(a) => walk(&a.expr, names),
            Expr::Unary(u) => walk(&u.expr, names),
            Expr::Binary(b) => {
                walk(&b.lhs, names);
                walk(&b.rhs, names);
            }
            Expr::Paren(p) => walk(&p.expr, names),
            Expr::Call(call) => {
                for arg in &call.args.args {
                    walk(arg, names);
                }
            }
            Expr::VectorSelector(sel) => names.extend(sel.name.clone()),
            Expr::MatrixSelector(sel) => names.extend(sel.vs.name.clone()),
            Expr::Subquery(s) => walk(&s.expr, names),
            Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::Extension(_) => {}
        }
    }
    let mut names = Vec::new();
    walk(expr, &mut names);
    names
}

/// The name of a histogram function other than `histogram_quantile`
/// called in `expr`, which a subquery does not evaluate.
fn histogram_call_besides_quantile(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Call(call) => {
            if call.func.name.starts_with("histogram_") && call.func.name != "histogram_quantile" {
                return Some(call.func.name);
            }
            call.args
                .args
                .iter()
                .find_map(|a| histogram_call_besides_quantile(a))
        }
        Expr::Aggregate(a) => histogram_call_besides_quantile(&a.expr),
        Expr::Unary(u) => histogram_call_besides_quantile(&u.expr),
        Expr::Binary(b) => histogram_call_besides_quantile(&b.lhs)
            .or_else(|| histogram_call_besides_quantile(&b.rhs)),
        Expr::Paren(p) => histogram_call_besides_quantile(&p.expr),
        Expr::Subquery(s) => histogram_call_besides_quantile(&s.expr),
        _ => None,
    }
}

/// How many joins (aggregations and operators between two series sets) are
/// nested in `expr`, at the deepest.
fn join_depth(expr: &Expr) -> u64 {
    match expr {
        Expr::Aggregate(a) => 1 + join_depth(&a.expr),
        Expr::Binary(b) => {
            let (l, r) = (join_depth(&b.lhs), join_depth(&b.rhs));
            fn scalar(e: &Expr) -> bool {
                match e {
                    Expr::NumberLiteral(_) => true,
                    Expr::Paren(p) => scalar(&p.expr),
                    _ => false,
                }
            }
            if scalar(&b.lhs) || scalar(&b.rhs) {
                l.max(r)
            } else {
                1 + l.max(r)
            }
        }
        Expr::Unary(u) => join_depth(&u.expr),
        Expr::Paren(p) => join_depth(&p.expr),
        Expr::Call(call) => call
            .args
            .args
            .iter()
            .map(|a| join_depth(a))
            .max()
            .unwrap_or(0),
        Expr::Subquery(s) => join_depth(&s.expr),
        _ => 0,
    }
}

/// How far before the range a raw read starts, for points over `window_ns`:
/// twice the hold bound for each nested join, and the window. A series'
/// value at `t` in a join is its latest point, within the bound before `t`,
/// held for the gap from the point before it, which matters only within the
/// bound again: so a join's points from `t - 2L` need its operands' points
/// from `t - 4L`, and each join below adds `2L` more.
fn raw_read_back(ctx: &Ctx<'_>, window_ns: u64) -> u64 {
    ctx.raw_hold_ns
        .saturating_mul(2 * ctx.raw_depth)
        .saturating_add(window_ns)
}

/// How far before a `rate(name[range])` window's left edge a sample is
/// interpolated from, for a series that does not carry its table's
/// interval: two of `counter_interval`, which no series of the source
/// exceeds (`DataSource::counter_interval`). A window with no sample that
/// close before its left edge starts at the first sample after it
/// (`range_start`).
fn rate_reach(ctx: &Ctx<'_>, name: &str) -> u64 {
    crate::batch_rate::interval_reach(ctx.source.counter_interval(name))
}

/// The first sample after `after_ns` of each series of counter `name` in
/// `want`, read from `source` within `span` (inclusive), by labels: what a
/// grid `rate`/`irate` point between a series' last row at or before a
/// range's end and that end interpolates to, so that the point has the
/// value it has in a longer range.
///
/// The read is in windows ending 16, 32, 64, ... of the counter's interval
/// after `after_ns`, or after the start of `span` when that is later, each
/// starting after the last, until every series in `want` has a sample or a
/// window reaches the span's end or the source's last row; a series with no row
/// before then has none. A series in `want` with no row in that part of the
/// span makes the read go to that bound. A window pulls one sample from the
/// stream of each series still without one. `SegmentedSource` opens a series'
/// segments in a window in order until one yields a row, skipping those whose
/// footer statistics say its column is null throughout; a source using the
/// default `counter_streams` reads every row of the matched series in the
/// window.
fn next_samples(
    source: &dyn DataSource,
    name: &str,
    filter: &crate::labels::Labels,
    (lo, hi): (u64, u64),
    after_ns: u64,
    want: &std::collections::HashSet<&crate::labels::Labels>,
) -> std::collections::HashMap<crate::labels::Labels, crate::types::CounterSample> {
    let mut found = std::collections::HashMap::new();
    let after = after_ns.max(lo.saturating_sub(1));
    if after == u64::MAX {
        return found;
    }
    let until = source.time_range().map_or(0, |(_, last)| last.min(hi));
    let mut from = after + 1;
    let mut width = ((source.counter_interval(name) * 1e9) as u64)
        .max(1)
        .saturating_mul(16);
    while from <= until && found.len() < want.len() {
        let to = after.saturating_add(width).min(until);
        if let Some(streams) = source.counter_streams(name, filter, from, to) {
            for mut stream in streams {
                if !want.contains(&stream.labels) || found.contains_key(&stream.labels) {
                    continue;
                }
                if let Some(first) = stream.samples.next() {
                    found.insert(stream.labels, first);
                }
            }
        }
        if to >= until {
            break;
        }
        from = to + 1;
        width = width.saturating_mul(2);
    }
    found
}

/// [`next_samples`] for a scan's series, in its order, as `grid_rates`
/// takes them.
fn next_for_scan(
    scan: &crate::scan::CounterScan<'_>,
    source: &dyn DataSource,
    name: &str,
    filter: &crate::labels::Labels,
    span: (u64, u64),
    after_ns: u64,
) -> Vec<Option<crate::batch_rate::Sample>> {
    let want = scan.series.iter().map(|s| &s.labels).collect();
    let mut next = next_samples(source, name, filter, span, after_ns, &want);
    scan.series
        .iter()
        .map(|s| {
            next.remove(&s.labels).map(|x| crate::batch_rate::Sample {
                ts: x.ts,
                value: x.value,
                window: x.window,
            })
        })
        .collect()
}

/// How far before a `rate(name[range])` point's time its read starts: the
/// `[range]`, the reach before it ([`rate_reach`]), and one reach more for
/// a point held past a series' last sample, whose window ends at that
/// sample, up to the series' typical spacing before the point.
fn rate_lookback(ctx: &Ctx<'_>, name: &str, range_ns: u64) -> u64 {
    range_ns.saturating_add(rate_reach(ctx, name).saturating_mul(2))
}

/// How far before the range's start a grid `irate(name[range])` reads:
/// [`rate_lookback`] over the span its first point is over (the step, a
/// reach to the row at or before the span's start, and a reach for a point
/// held past a series' last sample), or the `[range]` when that is further.
/// With explicit evaluation timestamps a point is over the gap from the
/// timestamp before it, so the span is from the last timestamp before the
/// range's start, when there is one, a timestamp falls in the range, and it
/// is further back than a step.
fn irate_lookback(ctx: &Ctx<'_>, name: &str, range_ns: u64) -> u64 {
    let before = ctx.eval_timestamps.as_deref().and_then(|ts| {
        let first = ts.partition_point(|t| *t < ctx.start_ns);
        ts.get(first).filter(|t| **t <= ctx.end_ns)?;
        first.checked_sub(1).map(|i| ctx.start_ns - ts[i])
    });
    let span = before.unwrap_or(0).max(ctx.step_ns);
    range_ns.max(rate_lookback(ctx, name, span))
}

/// In raw mode, `series` without the points before the range's start and
/// without timing points.
///
/// Raw producers emit a point at every sample they read, from before the
/// range, so that a join holds each point for the gap before it
/// (`Join::Latest`).
fn clip_raw<'a>(ctx: &Ctx<'_>, series: SeriesSet<'a>) -> SeriesSet<'a> {
    if !matches!(ctx.rate_mode, RateMode::Raw) {
        return series;
    }
    let start = ctx.start_ns;
    series
        .into_iter()
        .map(|ls| {
            LabeledSeries::new(
                ls.labels,
                ls.iter.filter(move |p| p.t >= start && !p.timing),
            )
        })
        .collect()
}

/// [`try_streaming`] in display form: each series is reduced per `display`
/// as it is collected (see `collect_to_display`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_streaming_display(
    source: &dyn DataSource,
    groups: Option<&SourceGroups>,
    expr: &Expr,
    start: f64,
    end: f64,
    step: f64,
    opts: &QueryOptions,
    display: &crate::DisplayOptions,
) -> Result<crate::DisplayResult, QueryError> {
    let ctx = make_ctx(source, groups, expr, start, end, step, opts);
    if let Some(result) = batch_display(&ctx, expr, start, end, step, display)? {
        return Ok(result);
    }
    let result = match build(&ctx, expr)? {
        Built::Series {
            series,
            metric_name,
            metric_name_for_error,
        } => {
            let collected = super::collect_to_display(
                clip_raw(&ctx, series),
                metric_name,
                start,
                end,
                step,
                display,
            );
            if collected.is_empty() {
                if let Some(name) = metric_name_for_error {
                    held_somewhere(source, &name)?;
                }
            }
            crate::DisplayResult::Series {
                result: collected,
                budget: display.budget as u32,
                notices: Vec::new(),
            }
        }
        Built::Materialized { result, name } => {
            if result.is_empty() {
                held_somewhere(source, &name)?;
            }
            crate::display::display_from_result(
                QueryResult::Matrix {
                    result,
                    notices: Vec::new(),
                },
                start,
                end,
                step,
                display,
            )
        }
        Built::Scalar(v) => crate::DisplayResult::Scalar {
            result: (start, v),
            notices: Vec::new(),
        },
    };
    Ok(result)
}

struct Ctx<'a> {
    source: &'a dyn DataSource,
    /// Where each series' first sample after the end is read
    /// ([`next_samples`]), and within what span; `None` for `source`, over
    /// all its rows.
    next_source: Option<(&'a dyn DataSource, (u64, u64))>,
    /// The process groups of more than one member among the sources, which
    /// each leaf reads per member and combines ([`per_source`]); `None` when
    /// there are none, and in the context a member is read in.
    groups: Option<&'a SourceGroups>,
    start_ns: u64,
    end_ns: u64,
    step_ns: u64,
    rate_mode: RateMode,
    /// Explicit evaluation timestamps; see [`QueryOptions::eval_timestamps`].
    eval_timestamps: Option<std::sync::Arc<[u64]>>,
    /// See [`QueryOptions::per_series_rates`].
    per_series_rates: bool,
    /// See [`DataSource::recording_label_keys`].
    recording_keys: Vec<String>,
    /// In raw mode, the longest a join holds a point ([`raw_hold`]).
    raw_hold_ns: u64,
    /// How many joins the query nests ([`join_depth`]).
    raw_depth: u64,
}

impl<'a> Ctx<'a> {
    /// This context over `source` alone, which forms no group.
    fn over<'s>(&self, source: &'s dyn DataSource) -> Ctx<'s> {
        Ctx {
            source,
            next_source: None,
            groups: None,
            start_ns: self.start_ns,
            end_ns: self.end_ns,
            step_ns: self.step_ns,
            rate_mode: self.rate_mode,
            eval_timestamps: self.eval_timestamps.clone(),
            per_series_rates: self.per_series_rates,
            recording_keys: source.recording_label_keys(),
            raw_hold_ns: self.raw_hold_ns,
            raw_depth: self.raw_depth,
        }
    }

    /// Where each series' first sample after the end is read.
    fn next_from(&self) -> (&'a dyn DataSource, (u64, u64)) {
        self.next_source.unwrap_or((self.source, (0, u64::MAX)))
    }

    /// Apply the caller's explicit evaluation timestamps to a gauge producer,
    /// leaving its default placement alone when there are none.
    ///
    /// Every gauge producer goes through here, because a binary op joins its
    /// sides on timestamp: if one producer moved to the explicit points and
    /// another stayed on the grid, the two would stop intersecting and the
    /// query would return empty rather than wrong.
    fn place<P: AtPoints>(&self, producer: P) -> P {
        match &self.eval_timestamps {
            Some(points) => producer.at_points(points.clone()),
            None => producer,
        }
    }
}

/// `rate`/`irate`/`increase` of a matrix selector on the grid, computed by the source in
/// one pass when it can (see [`crate::batch_rate`]). `None` for any other
/// expression, for raw mode or explicit evaluation timestamps, and for a
/// source that does not compute it: the caller then builds the per-series
/// path.
fn batch_rates<'a>(
    ctx: &'a Ctx<'a>,
    call: &parser::Call,
    group: Option<(AggOp, GroupBy<'_>)>,
) -> Option<(SeriesSet<'a>, String)> {
    let (name, filter, request) = grid_request(ctx, call, group, None)?;
    let crate::batch_rate::GridRates::Points(results) =
        scan_rates(ctx, call, name, &filter, request)?
    else {
        return None;
    };
    #[cfg(any(test, feature = "fixtures"))]
    BATCH_RATES.with(|n| n.set(n.get() + 1));
    let series = results
        .into_iter()
        .map(|(labels, points)| LabeledSeries::new(labels, points.into_iter()))
        .collect();
    Some((series, name.to_string()))
}

/// `request` computed over the source's scan of `name`; over process groups,
/// over every recording's scan at once, each group's members combined per
/// point (`batch_rate::UnionPlan`). `None` where a scan is not available:
/// the caller then evaluates the query per series, or per member.
fn scan_rates(
    ctx: &Ctx<'_>,
    call: &parser::Call,
    name: &str,
    filter: &crate::labels::Labels,
    request: crate::batch_rate::GridRateRequest<'_>,
) -> Option<crate::batch_rate::GridRates> {
    let Some(groups) = ctx.groups else {
        let scan = ctx
            .source
            .counter_scan(name, filter, request.data_start, request.end_ns)?;
        let (from, span) = ctx.next_from();
        let next = next_for_scan(&scan, from, name, filter, span, request.end_ns);
        return crate::batch_rate::grid_rates(scan, &request, next);
    };
    // An increase over a group of several members sums its members' parts
    // of the window (`group_increase`), which the union of the scan does
    // not.
    if call.func.name == "increase"
        && groups
            .parts
            .iter()
            .any(|p| matches!(p, per_source::Part::Group(m) if m.len() > 1))
    {
        return None;
    }
    let range = matches!(call.func.name, "rate" | "increase");
    let range_ns = match call.args.args.first().map(|a| &**a) {
        Some(Expr::MatrixSelector(sel)) => sel.range.as_nanos() as u64,
        _ => return None,
    };
    // What a part's read starts at: as `grid_request` computes it over that
    // part alone.
    let data_start = |source: &dyn DataSource| -> u64 {
        let reach = crate::batch_rate::interval_reach(source.counter_interval(name));
        let lookback = if range {
            range_ns.saturating_add(reach.saturating_mul(2))
        } else {
            range_ns.max(ctx.step_ns.saturating_add(reach.saturating_mul(2)))
        };
        ctx.start_ns.saturating_sub(lookback)
    };
    let holds = |source: &dyn DataSource| source.counter_names().iter().any(|n| n == name);
    let mut scans = Vec::new();
    // Per scanned series, its first sample after the end (`next_samples`).
    let mut next = Vec::new();
    let mut unit = Vec::new();
    let mut units: Vec<crate::batch_rate::UnionUnit> = Vec::new();
    let mut series = Vec::new();
    for part in &groups.parts {
        match part {
            per_source::Part::Single(source) => {
                if !holds(source.as_ref()) {
                    continue;
                }
                let from = data_start(source.as_ref());
                let mut scan = source.counter_scan(name, filter, from, request.end_ns)?;
                next.extend(next_for_scan(
                    &scan,
                    source.as_ref(),
                    name,
                    filter,
                    (0, u64::MAX),
                    request.end_ns,
                ));
                let interval = source.counter_interval(name);
                for s in &mut scan.series {
                    s.interval.get_or_insert(interval);
                    unit.push(units.len());
                    units.push(crate::batch_rate::UnionUnit {
                        labels: s.labels.clone(),
                        combine: false,
                    });
                    series.push(crate::batch_rate::UnionMember {
                        data_start: from,
                        rows: (0, u64::MAX),
                        interval_ns: (interval * 1e9) as u64,
                        rank: 0,
                        before: Vec::new(),
                        after: Vec::new(),
                        source: None,
                    });
                }
                scans.push(scan);
            }
            per_source::Part::Group(members) => {
                let mut ranked: Vec<(u64, &per_source::Member)> = members
                    .iter()
                    .filter(|m| holds(m.source.as_ref()))
                    .map(|m| ((m.source.counter_interval(name) * 1e9) as u64, m))
                    .collect();
                ranked.sort_by(|(ia, a), (ib, b)| {
                    ia.cmp(ib).then_with(|| a.cmp_ties(b)).then(a.lo.cmp(&b.lo))
                });
                // Each group series' unit, by labels.
                let mut unit_of: std::collections::HashMap<crate::labels::Labels, usize> =
                    std::collections::HashMap::new();
                for (rank, (interval_ns, m)) in ranked.into_iter().enumerate() {
                    let from = data_start(m.source.as_ref());
                    // A member clipped to `[m.lo, m.hi]` (one producer
                    // epoch, a scope's span, or both) scans only the segments
                    // its rows can be in; the batch path drops the scan's
                    // rows outside the span (`UnionMember::rows`). When all
                    // its rows lie outside the read, `.max(scan_from)` keeps
                    // the range from inverting: the member scans one instant
                    // and contributes no rows.
                    let scan_from = from.max(m.lo);
                    let scan_to = request.end_ns.min(m.hi).max(scan_from);
                    let mut scan = m.raw.counter_scan(name, filter, scan_from, scan_to)?;
                    // The member's first row after the end, within its span.
                    next.extend(next_for_scan(
                        &scan,
                        m.raw.as_ref(),
                        name,
                        filter,
                        (m.lo, m.hi),
                        request.end_ns,
                    ));
                    let interval = m.source.counter_interval(name);
                    let labels: Vec<crate::labels::Labels> =
                        scan.series.iter().map(|s| s.labels.clone()).collect();
                    let tap = Tap::new(m.raw.clone(), m.lo, m.hi);
                    let (mut before, mut after) =
                        tap.context_rows(true, name, filter, (from, request.end_ns), &labels);
                    for s in &mut scan.series {
                        s.interval.get_or_insert(interval);
                        let u = *unit_of.entry(s.labels.clone()).or_insert_with(|| {
                            units.push(crate::batch_rate::UnionUnit {
                                labels: s.labels.clone(),
                                combine: true,
                            });
                            units.len() - 1
                        });
                        unit.push(u);
                        series.push(crate::batch_rate::UnionMember {
                            data_start: from,
                            rows: (m.lo, m.hi),
                            interval_ns,
                            rank,
                            before: before.remove(&s.labels).unwrap_or_default(),
                            after: after.remove(&s.labels).unwrap_or_default(),
                            source: Some((
                                m.source_name(),
                                if m.first_ns == u64::MAX {
                                    0
                                } else {
                                    m.first_ns
                                },
                            )),
                        });
                    }
                    scans.push(scan);
                }
            }
        }
    }
    if scans.is_empty() {
        return None;
    }
    let plan = crate::batch_rate::UnionPlan {
        unit,
        units,
        series,
        window: if range {
            crate::batch_rate::UnionWindow::Range(range_ns)
        } else {
            crate::batch_rate::UnionWindow::Step(ctx.step_ns)
        },
        conflicts: Some((std::sync::Arc::clone(&groups.conflicts), name.to_string())),
    };
    let request = crate::batch_rate::GridRateRequest {
        union: Some(&plan),
        ..request
    };
    crate::batch_rate::grid_rates(metriken_storage::scan::interleave(scans), &request, next)
}

/// The metric, filter and request for a batch `rate`/`irate`/`increase` of `call`, or
/// `None` where the batch path does not apply; see [`batch_rates`].
fn grid_request<'c, 'g>(
    ctx: &Ctx<'_>,
    call: &'c parser::Call,
    group: Option<(AggOp, GroupBy<'g>)>,
    display: Option<&'g crate::batch_rate::GridDisplay>,
) -> Option<(
    &'c str,
    crate::labels::Labels,
    crate::batch_rate::GridRateRequest<'g>,
)> {
    if ctx.per_series_rates
        || ctx.eval_timestamps.is_some()
        || !matches!(ctx.rate_mode, RateMode::Grid)
        || ctx.step_ns == 0
    {
        return None;
    }
    if !matches!(call.func.name, "rate" | "irate" | "increase") || call.args.args.len() != 1 {
        return None;
    }
    let Expr::MatrixSelector(sel) = &*call.args.args[0] else {
        return None;
    };
    if sel.vs.offset.is_some() || sel.vs.at.is_some() {
        return None;
    }
    let name = sel.vs.name.as_deref()?;
    // A scan reads the series its filter matches; the per-series path
    // applies the branches of a selector with `or`.
    let selection = Selection::new(Some(name), &sel.vs.matchers);
    if selection.has_branches() {
        return None;
    }
    let filter = selection.filter;
    let range_ns = sel.range.as_nanos() as u64;
    // `rate` and `increase` are over their `[range]`; `irate` over one step.
    let range = matches!(call.func.name, "rate" | "increase");
    let (lookback, span_ns) = if range {
        (rate_lookback(ctx, name, range_ns), range_ns)
    } else {
        (irate_lookback(ctx, name, range_ns), ctx.step_ns)
    };
    let request = crate::batch_rate::GridRateRequest {
        data_start: ctx.start_ns.saturating_sub(lookback),
        start_ns: ctx.start_ns,
        end_ns: ctx.end_ns,
        step_ns: ctx.step_ns,
        span_ns,
        range,
        increase: call.func.name == "increase",
        reach_ns: rate_reach(ctx, name),
        group,
        display,
        union: None,
    };
    Some((name, filter, request))
}

#[cfg(test)]
thread_local! {
    /// Display queries [`batch_display`] answered on this thread.
    pub(crate) static BATCH_DISPLAYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "fixtures"))]
thread_local! {
    /// Queries [`batch_rates`] answered on this thread.
    pub(crate) static BATCH_RATES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A display query computed by the source as it reads, holding one bucket
/// per series, or one accumulator per group and grid point for an
/// aggregate: `expr` must be `rate`/`irate` of a selector, or `sum`, `avg`,
/// `min`, `max` or `count` of one, optionally under scalar ops against
/// number literals. `None` for any other expression or where the batch path
/// does not apply; the caller then builds it and reduces each series as it
/// is collected.
fn batch_display(
    ctx: &Ctx<'_>,
    expr: &Expr,
    start: f64,
    end: f64,
    step: f64,
    display: &crate::DisplayOptions,
) -> Result<Option<crate::DisplayResult>, QueryError> {
    let mut ops = Vec::new();
    let mut e = expr;
    let call = loop {
        match e {
            Expr::Paren(p) => e = &p.expr,
            Expr::Binary(bin) if bin.modifier.is_none() => {
                // A comparison keeps or drops points, which the batched
                // display reduction does not.
                let Some(op) =
                    BinOp::from_token(&bin.op, false).filter(|op| !matches!(op, BinOp::Cmp { .. }))
                else {
                    return Ok(None);
                };
                match (&*bin.lhs, &*bin.rhs) {
                    (Expr::NumberLiteral(_), Expr::NumberLiteral(_)) => return Ok(None),
                    (Expr::NumberLiteral(n), other) => {
                        ops.push((op, n.val, true));
                        e = other;
                    }
                    (other, Expr::NumberLiteral(n)) => {
                        ops.push((op, n.val, false));
                        e = other;
                    }
                    _ => return Ok(None),
                }
            }
            Expr::Call(call) => break (call, None),
            Expr::Aggregate(agg) => {
                let op = match agg.op.to_string().as_str() {
                    "sum" => AggOp::Sum,
                    "avg" => AggOp::Avg,
                    "min" => AggOp::Min,
                    "max" => AggOp::Max,
                    "count" => AggOp::Count,
                    _ => return Ok(None),
                };
                let group_by: GroupBy<'_> = match &agg.modifier {
                    None => GroupBy::Include(&[]),
                    Some(parser::LabelModifier::Include(ls)) => {
                        GroupBy::Include(ls.labels.as_slice())
                    }
                    Some(parser::LabelModifier::Exclude(ls)) => {
                        GroupBy::Exclude(ls.labels.as_slice())
                    }
                };
                let mut inner: &Expr = &agg.expr;
                while let Expr::Paren(p) = inner {
                    inner = &p.expr;
                }
                let Expr::Call(call) = inner else {
                    return Ok(None);
                };
                break (call, Some((op, group_by)));
            }
            _ => return Ok(None),
        }
    };
    let (call, group) = call;
    ops.reverse();
    let scaled = !ops.is_empty();
    // As the per-series path names the result: `rate` keeps the metric's
    // name, an aggregation or a scalar op drops it.
    let named = !scaled && group.is_none();
    let grid_display = crate::batch_rate::GridDisplay {
        width: crate::display::bucket_width(start, end, step, display.budget),
        band: display.band,
        ops,
    };
    let Some((name, filter, request)) = grid_request(ctx, call, group, Some(&grid_display)) else {
        return Ok(None);
    };
    let Some(crate::batch_rate::GridRates::Display(results)) =
        scan_rates(ctx, call, name, &filter, request)
    else {
        return Ok(None);
    };
    let series: Vec<crate::DisplaySeries> = results
        .into_iter()
        .filter(|(_, r)| !r.is_empty())
        .map(|(labels, r)| {
            let mut metric: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            if named {
                metric.insert("__name__".to_string(), name.to_string());
            }
            for (k, v) in labels.inner {
                metric.insert(k, v);
            }
            r.finish(metric, step, display)
        })
        .collect();
    if series.is_empty() && !scaled {
        held_somewhere(ctx.source, name)?;
    }
    #[cfg(test)]
    BATCH_DISPLAYS.with(|n| n.set(n.get() + 1));
    Ok(Some(crate::DisplayResult::Series {
        result: series,
        budget: display.budget as u32,
        notices: Vec::new(),
    }))
}

/// `MetricNotFound` for a name the source holds as no kind at all. A
/// source whose read of a name it does not hold returns an empty set rather
/// than `None` reaches here with an empty result.
fn held_somewhere(source: &dyn DataSource, name: &str) -> Result<(), QueryError> {
    let held = source.counter_names().iter().any(|n| n == name)
        || source.gauge_names().iter().any(|n| n == name)
        || source.histogram_names().iter().any(|n| n == name)
        || source.gauge_histogram_names().iter().any(|n| n == name);
    if held {
        Ok(())
    } else {
        Err(QueryError::MetricNotFound(name.to_string()))
    }
}

/// One step of recursion.
///
/// * `Series` — lazy iterator chain plus `__name__` plumbing.
/// * `Scalar` — the AST `NumberLiteral` representation; usable on
///   either side of a binary op.
/// * `Materialized` — pre-collected `MatrixSample`s for shapes whose
///   streaming output doesn't fit `SeriesSet` cleanly (currently
///   only `histogram_quantile`, whose pipeline goes through the
///   hand-rolled per-tick loop in `streaming::histogram::quantiles`).
enum Built<'a> {
    Series {
        series: SeriesSet<'a>,
        metric_name: Option<&'a str>,
        metric_name_for_error: Option<String>,
    },
    Scalar(f64),
    Materialized {
        result: Vec<MatrixSample>,
        name: String,
    },
}

fn build<'a, 'expr>(ctx: &'a Ctx<'a>, expr: &'expr Expr) -> Result<Built<'a>, QueryError>
where
    'expr: 'a,
{
    match expr {
        Expr::Paren(p) => build(ctx, &p.expr),
        Expr::Aggregate(agg) => build_aggregate(ctx, agg),
        Expr::Call(call) => build_call(ctx, call),
        Expr::VectorSelector(sel) => build_vector_selector(ctx, sel),
        Expr::Binary(bin) => build_binary(ctx, bin),
        Expr::NumberLiteral(num) => Ok(Built::Scalar(num.val)),
        Expr::MatrixSelector(_) => Err(QueryError::Unsupported(
            "bare matrix selector cannot be evaluated; wrap in rate/irate/etc.".to_string(),
        )),
        other => Err(QueryError::Unsupported(format!(
            "expression shape not supported: {other:?}"
        ))),
    }
}

fn build_aggregate<'a, 'expr>(
    ctx: &'a Ctx<'a>,
    agg: &'expr parser::AggregateExpr,
) -> Result<Built<'a>, QueryError>
where
    'expr: 'a,
{
    let op = match agg.op.to_string().as_str() {
        "sum" => AggOp::Sum,
        "avg" => AggOp::Avg,
        "min" => AggOp::Min,
        "max" => AggOp::Max,
        "count" => AggOp::Count,
        other => {
            return Err(QueryError::Unsupported(format!(
                "aggregation operator not supported: {other}"
            )))
        }
    };

    let group_by: GroupBy<'_> = match &agg.modifier {
        None => GroupBy::Include(&[]),
        Some(parser::LabelModifier::Include(ls)) => GroupBy::Include(ls.labels.as_slice()),
        Some(parser::LabelModifier::Exclude(ls)) => GroupBy::Exclude(ls.labels.as_slice()),
    };

    let mut inner: &Expr = &agg.expr;
    while let Expr::Paren(p) = inner {
        inner = &p.expr;
    }
    // Over process groups the scan combines each group's members (S9)
    // before the aggregation (`scan_rates`).
    let batched = match inner {
        Expr::Call(call) => batch_rates(ctx, call, Some((op, group_by))),
        _ => None,
    };
    if let Some((series, name)) = batched {
        return Ok(Built::Series {
            series,
            metric_name: None,
            metric_name_for_error: Some(name),
        });
    }

    let Built::Series {
        series: inner_series,
        metric_name_for_error,
        ..
    } = build(ctx, &agg.expr)?
    else {
        return Err(QueryError::Unsupported(
            "aggregation requires a series-typed inner expression".to_string(),
        ));
    };
    let series = aggregate(
        inner_series,
        op,
        group_by,
        Join::for_mode(ctx.rate_mode, ctx.raw_hold_ns),
    );
    Ok(Built::Series {
        series,
        metric_name: None,
        metric_name_for_error,
    })
}

fn build_binary<'a, 'expr>(
    ctx: &'a Ctx<'a>,
    bin: &'expr parser::BinaryExpr,
) -> Result<Built<'a>, QueryError>
where
    'expr: 'a,
{
    let return_bool = bin.modifier.as_ref().is_some_and(|m| m.return_bool);
    let Some(op) = BinOp::from_token(&bin.op, return_bool) else {
        return Err(QueryError::Unsupported(format!(
            "binary operator not supported: {}",
            bin.op
        )));
    };

    let grouped = match bin.modifier.as_ref().map(|m| &m.card) {
        None | Some(parser::VectorMatchCardinality::OneToOne) => None,
        Some(parser::VectorMatchCardinality::ManyToOne(include)) => {
            Some((OneSide::Right, include.labels.as_slice()))
        }
        Some(parser::VectorMatchCardinality::OneToMany(include)) => {
            Some((OneSide::Left, include.labels.as_slice()))
        }
        // The parser gives this only to `and`, `or` and `unless`, which
        // `BinOp::from_token` has refused above.
        Some(parser::VectorMatchCardinality::ManyToMany) => {
            return Err(QueryError::Unsupported(
                "many-to-many matching (and, or, unless) not supported".to_string(),
            ));
        }
    };

    let spec = match bin.modifier.as_ref().and_then(|m| m.matching.as_ref()) {
        None => MatchSpec::Default,
        Some(parser::LabelModifier::Include(ls)) => MatchSpec::Include(ls.labels.as_slice()),
        Some(parser::LabelModifier::Exclude(ls)) => MatchSpec::Exclude(ls.labels.as_slice()),
    };

    let lhs = build(ctx, &bin.lhs)?;
    let rhs = build(ctx, &bin.rhs)?;

    // A comparison without `bool` passes the vector operand's points on
    // unchanged, so they keep its metric name (Prometheus keeps the name
    // through a comparison too); every other operator gives a value of its
    // own, without one. Between vectors the name is the many side's under
    // `group_right`, as the labels are, and the left side's otherwise.
    let keep_name = |name: Option<&'a str>| if op.is_filter() { name } else { None };
    let raw = matches!(ctx.rate_mode, RateMode::Raw);
    match (lhs, rhs) {
        (
            Built::Series {
                series,
                metric_name,
                ..
            },
            Built::Scalar(s),
        ) => Ok(Built::Series {
            series: matrix_scalar_op(series, op, s, false, raw),
            metric_name: keep_name(metric_name),
            metric_name_for_error: None,
        }),
        (
            Built::Scalar(s),
            Built::Series {
                series,
                metric_name,
                ..
            },
        ) => Ok(Built::Series {
            series: matrix_scalar_op(series, op, s, true, raw),
            metric_name: keep_name(metric_name),
            metric_name_for_error: None,
        }),
        (
            Built::Series {
                series: left_series,
                metric_name: left_name,
                ..
            },
            Built::Series {
                series: right_series,
                metric_name: right_name,
                ..
            },
        ) => Ok(Built::Series {
            series: match grouped {
                None => matrix_matrix_op(
                    left_series,
                    right_series,
                    op,
                    spec,
                    &ctx.recording_keys,
                    Join::for_mode(ctx.rate_mode, ctx.raw_hold_ns),
                    ctx.start_ns,
                )?,
                Some((one_side, include)) => matrix_grouped_op(
                    left_series,
                    right_series,
                    op,
                    spec,
                    one_side,
                    include,
                    Join::for_mode(ctx.rate_mode, ctx.raw_hold_ns),
                    ctx.start_ns,
                )?,
            },
            metric_name: keep_name(match grouped {
                Some((OneSide::Left, _)) => right_name,
                _ => left_name,
            }),
            metric_name_for_error: None,
        }),
        (Built::Scalar(a), Built::Scalar(b)) => {
            Ok(Built::Scalar(op.apply(a, b).unwrap_or(f64::NAN)))
        }
        // A materialized histogram result (e.g. histogram_quantile) against a
        // scalar — the ns→s unit conversion the latency panels use. Scale the
        // values AND the bucket bands so the band survives. (Previously outright
        // unsupported, forcing an eager fallback with no bands.)
        (Built::Materialized { result, name }, Built::Scalar(s)) => Ok(Built::Materialized {
            result: scalar_op_matrix(result, op, s, false),
            name,
        }),
        (Built::Scalar(s), Built::Materialized { result, name }) => Ok(Built::Materialized {
            result: scalar_op_matrix(result, op, s, true),
            name,
        }),
        _ => Err(QueryError::Unsupported(
            "binary op against a histogram_quantile result not supported".to_string(),
        )),
    }
}

/// Apply a scalar op to each materialized `MatrixSample`'s values and their
/// uncertainty bands, keeping `values`, `intervals`, `bands` and
/// `interpolated` aligned point for point. An arithmetic op takes a band
/// through interval arithmetic against the scalar as exact `[scalar,
/// scalar]`, so a spanning-zero denominator under `scalar / value` gives no
/// band instead of a bogus narrow one, and drops a point on division by
/// zero. A comparison keeps or drops each point, or gives 1 or 0 with
/// `bool` (see [`binary`](super::binary)). A series left without points is
/// dropped.
fn scalar_op_matrix(
    mut result: Vec<crate::promql::MatrixSample>,
    op: BinOp,
    scalar: f64,
    scalar_first: bool,
) -> Vec<crate::promql::MatrixSample> {
    let sb = (scalar, scalar);
    let ordered = |x: super::Band| if scalar_first { (sb, x) } else { (x, sb) };
    for sample in &mut result {
        let n = sample.values.len();
        let band_at = |i: usize| -> Option<super::Band> {
            match &sample.bands {
                Some(bands) => bands.get(i).copied().flatten(),
                None => sample.intervals.as_ref().and_then(|iv| iv.get(i)).copied(),
            }
        };
        let interpolated_at = |i: usize| {
            sample
                .interpolated
                .as_ref()
                .is_some_and(|f| f.get(i) == Some(&true))
        };
        let mut values = Vec::with_capacity(n);
        let mut bands: Vec<Option<super::Band>> = Vec::with_capacity(n);
        let mut interpolated = Vec::with_capacity(n);
        for (i, &(t, v)) in sample.values.iter().enumerate() {
            let (l, r) = if scalar_first {
                (scalar, v)
            } else {
                (v, scalar)
            };
            let band = band_at(i);
            let out = match op {
                BinOp::Cmp { cmp, bool: false } => cmp.holds(l, r).then_some((v, band)),
                BinOp::Cmp { cmp, bool: true } => {
                    let holds = cmp.holds(l, r);
                    let band = band.map(|b| {
                        let (lb, rb) = ordered(b);
                        super::binary::compare_band(cmp, lb, rb, holds)
                    });
                    Some((f64::from(u8::from(holds)), band))
                }
                _ => op.apply(l, r).map(|nv| {
                    let band = band.and_then(|b| {
                        let (lb, rb) = ordered(b);
                        interval_binop(op, lb, rb)
                    });
                    (nv, band)
                }),
            };
            let Some((nv, band)) = out else { continue };
            values.push((t, nv));
            bands.push(band);
            interpolated.push(interpolated_at(i));
        }
        let any_band = bands.iter().any(Option::is_some);
        sample.intervals = (any_band && bands.iter().all(Option::is_some))
            .then(|| bands.iter().map(|b| b.unwrap()).collect());
        sample.bands = any_band.then_some(bands);
        sample.interpolated = interpolated.iter().any(|f| *f).then_some(interpolated);
        sample.values = values;
    }
    // A series the op left without points is not in the result, as a
    // streamed series without points is not.
    result.retain(|s| !s.values.is_empty());
    result
}

/// `label_replace(v, dst, replacement, src, regex)`: each series of `v`
/// whose `src` value (`""` where it has none) matches `regex`, anchored at
/// both ends, gets `dst` set to `replacement` with `$1` and `${name}`
/// expanded from the match, or removed where that expands to `""`. Other
/// series pass unchanged, and values and bands are untouched. Two series
/// left with one label set and points anywhere in the range are an error,
/// where Prometheus checks each step. The regex and the expansion are the
/// `regex` crate's: `.` does not match a newline, `\d` and `\w` are
/// Unicode classes, and `$01` is group 1. `dst` or `src` naming `__name__`
/// is not supported.
fn build_label_replace<'a, 'expr>(
    ctx: &'a Ctx<'a>,
    call: &'expr parser::Call,
) -> Result<Built<'a>, QueryError>
where
    'expr: 'a,
{
    let args = &call.args.args;
    let string = |i: usize| match args.get(i).map(|a| &**a) {
        Some(Expr::StringLiteral(s)) => Ok(s.val.as_str()),
        _ => Err(QueryError::ParseError(format!(
            "label_replace takes a vector and four strings; argument {} is not a string",
            i + 1
        ))),
    };
    if args.len() != 5 {
        return Err(QueryError::ParseError(format!(
            "label_replace takes 5 arguments, not {}",
            args.len()
        )));
    }
    let (dst, replacement, src, pattern) = (string(1)?, string(2)?, string(3)?, string(4)?);
    let valid_name = |name: &str| {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if !valid_name(dst) {
        return Err(QueryError::ParseError(format!(
            "label_replace: invalid destination label name {dst:?}"
        )));
    }
    if dst == "__name__" || src == "__name__" {
        return Err(QueryError::Unsupported(
            "label_replace reading or writing __name__ is not supported".to_string(),
        ));
    }
    let regex = regex::Regex::new(&format!("^(?:{pattern})$")).map_err(|e| {
        QueryError::ParseError(format!(
            "label_replace: invalid regular expression {pattern:?}: {e}"
        ))
    })?;
    let Built::Series {
        series,
        metric_name,
        metric_name_for_error,
    } = build(ctx, &args[0])?
    else {
        return Err(QueryError::Unsupported(
            "label_replace requires a series-typed first argument".to_string(),
        ));
    };
    let series = series
        .into_iter()
        .map(|mut ls| {
            let value = ls.labels.inner.get(src).map_or("", String::as_str);
            if let Some(captures) = regex.captures(value) {
                let mut expanded = String::new();
                captures.expand(replacement, &mut expanded);
                if expanded.is_empty() {
                    ls.labels.inner.remove(dst);
                } else {
                    ls.labels.inner.insert(dst.to_string(), expanded);
                }
            }
            ls
        })
        .collect();
    let hold = match Join::for_mode(ctx.rate_mode, ctx.raw_hold_ns) {
        Join::Latest(hold) => hold,
        Join::Exact => 0,
    };
    let series = distinct_label_sets(series, ctx.start_ns, hold).map_err(|labels| {
        QueryError::EvaluationError(format!(
            "label_replace: vector cannot contain metrics with the same labelset {}",
            braces(&labels.inner)
        ))
    })?;
    Ok(Built::Series {
        series,
        metric_name,
        metric_name_for_error,
    })
}

/// The most inner points one series of a subquery may have.
const MAX_SUBQUERY_POINTS: u64 = 100_000;

/// A `*_over_time` function of a subquery `expr[range:step]`: `expr`
/// evaluated on the grid at the multiples of `step` from the first after
/// the range's start less `range` to its end, then aggregated at each of the
/// query's points over the inner points in `(t − range, t]`, as Prometheus
/// evaluates one. Without `step` it is the query's step (for an instant,
/// the source's interval, at least a second; Prometheus uses its
/// evaluation interval). A `histogram_quantile`'s points, one per row, are
/// taken onto the inner grid ([`on_grid`]), read from a lookback before it;
/// the other histogram functions are refused inside a subquery.
/// The points carry no band and no metric name (Prometheus keeps the name
/// for `last_over_time` of a subquery whose points carry one); one
/// aggregating an
/// interpolated inner point is interpolated. In raw mode the points are on
/// the grid too. A subquery with `offset` or `@`, of a scalar, or of more
/// than [`MAX_SUBQUERY_POINTS`] inner points per series is refused.
fn build_subquery_over_time<'a, 'expr>(
    ctx: &'a Ctx<'a>,
    call: &'expr parser::Call,
    sq: &'expr parser::SubqueryExpr,
) -> Result<Built<'a>, QueryError>
where
    'expr: 'a,
{
    if sq.offset.is_some() || sq.at.is_some() {
        return Err(QueryError::Unsupported(
            "a subquery with offset or @ is not supported".to_string(),
        ));
    }
    let agg = over_time_agg(call)?;
    let range_ns = sq.range.as_nanos() as u64;
    let step_ns = sq
        .step
        .map(|d| d.as_nanos() as u64)
        .filter(|s| *s > 0)
        .unwrap_or(if ctx.step_ns > 0 {
            ctx.step_ns
        } else {
            1_000_000_000
        });
    // The first multiple of the step after the first window's start.
    let inner_start = (ctx.start_ns.saturating_sub(range_ns) / step_ns + 1).saturating_mul(step_ns);
    let per_series = ctx.end_ns.saturating_sub(inner_start) / step_ns + 1;
    if per_series > MAX_SUBQUERY_POINTS {
        return Err(QueryError::Unsupported(format!(
            "this subquery evaluates {per_series} points per series, more than \
             {MAX_SUBQUERY_POINTS}; use a larger subquery step or a shorter range"
        )));
    }
    if let Some(name) = histogram_call_besides_quantile(&sq.expr) {
        return Err(QueryError::Unsupported(format!(
            "{name} is not evaluated inside a subquery; histogram_quantile is"
        )));
    }
    // The inner expression is read from a lookback before the grid's first
    // point (the range, at least a step, and at least the row interval of a
    // histogram it reads, in whole steps), so a histogram's row before it is
    // there to take onto the grid; the points before the grid are dropped.
    let histogram_ns = metric_names(&sq.expr)
        .iter()
        .map(|n| (ctx.source.histogram_interval(n).max(0.0) * 1e9) as u64)
        .max()
        .unwrap_or(0);
    let lookback = range_ns
        .max(step_ns)
        .max(histogram_ns)
        .div_ceil(step_ns)
        .saturating_mul(step_ns);
    let inner = Ctx {
        source: ctx.source,
        next_source: ctx.next_source,
        groups: ctx.groups,
        start_ns: inner_start.saturating_sub(lookback),
        end_ns: ctx.end_ns,
        step_ns,
        rate_mode: RateMode::Grid,
        eval_timestamps: None,
        per_series_rates: ctx.per_series_rates,
        recording_keys: ctx.recording_keys.clone(),
        raw_hold_ns: 0,
        raw_depth: ctx.raw_depth,
    };
    /// One inner series: its labels, and its points' times, values and
    /// interpolated flags.
    type Inner = (crate::labels::Labels, Vec<u64>, Vec<f64>, Vec<bool>);
    let series: Vec<Inner> = if inner_start > ctx.end_ns {
        Vec::new()
    } else {
        match build(&inner, &sq.expr)? {
            Built::Series { series, .. } => series
                .into_iter()
                .map(|ls| {
                    let (mut ts, mut vs, mut fs) = (Vec::new(), Vec::new(), Vec::new());
                    for p in ls.iter.filter(|p| !p.timing && p.t >= inner_start) {
                        ts.push(p.t);
                        vs.push(p.v);
                        fs.push(p.interpolated);
                    }
                    (ls.labels, ts, vs, fs)
                })
                .collect(),
            Built::Materialized { result, name } => result
                .into_iter()
                .map(|m| {
                    let labels = crate::labels::Labels {
                        inner: m
                            .metric
                            .into_iter()
                            .filter(|(k, _)| k != "__name__")
                            .collect(),
                    };
                    let rows: Vec<(u64, f64)> = m
                        .values
                        .iter()
                        .map(|&(t, v)| ((t * 1e9).round() as u64, v))
                        .collect();
                    let lone = (ctx
                        .source
                        .histogram_interval(&name)
                        .max(ctx.source.interval())
                        .max(0.0)
                        * 1e9) as u64;
                    let (ts, vs) = on_grid(&rows, inner_start, ctx.end_ns, step_ns, lone);
                    let fs = vec![false; ts.len()];
                    (labels, ts, vs, fs)
                })
                .collect(),
            Built::Scalar(_) => {
                return Err(QueryError::Unsupported(
                    "a subquery of a scalar is not supported".to_string(),
                ))
            }
        }
    };
    let out: SeriesSet<'a> = series
        .into_iter()
        .map(|(labels, timestamps, values, interpolated)| {
            // `(t − range, t]`: a window of `range` less a nanosecond, both
            // ends included.
            let producer = ctx.place(GaugeOverTime::new(
                timestamps,
                values,
                interpolated,
                ctx.start_ns,
                ctx.end_ns,
                ctx.step_ns,
                range_ns.saturating_sub(1),
                false,
                agg,
            ));
            LabeledSeries::new(labels, producer)
        })
        .collect();
    // A function of a subquery has no metric name; the plain functions
    // keep theirs, as `rate` does here.
    Ok(Built::Series {
        series: out,
        metric_name: None,
        metric_name_for_error: None,
    })
}

/// `rows` (time, value), one per histogram row, taken onto the multiples of
/// `step_ns` from `start_ns` to `end_ns`: at each, the latest row at or
/// before it. The last row is held for less than its spacing from the row
/// before it, and a lone row for less than `lone_ns` (the histogram's row
/// interval).
fn on_grid(
    rows: &[(u64, f64)],
    start_ns: u64,
    end_ns: u64,
    step_ns: u64,
    lone_ns: u64,
) -> (Vec<u64>, Vec<f64>) {
    let (mut ts, mut vs) = (Vec::new(), Vec::new());
    if rows.is_empty() {
        return (ts, vs);
    }
    let mut g = start_ns;
    while g <= end_ns {
        let i = rows.partition_point(|r| r.0 <= g);
        if let Some(k) = i.checked_sub(1) {
            let spacing = match (rows.get(k + 1), k.checked_sub(1).map(|j| rows[j])) {
                (Some(next), _) => next.0 - rows[k].0,
                (None, Some(prev)) => rows[k].0 - prev.0,
                (None, None) => lone_ns,
            };
            if g - rows[k].0 < spacing.max(1) {
                ts.push(g);
                vs.push(rows[k].1);
            }
        }
        match g.checked_add(step_ns) {
            Some(next) => g = next,
            None => break,
        }
    }
    (ts, vs)
}

/// Whether `name` is a `*_over_time` function this engine evaluates.
fn is_over_time(name: &str) -> bool {
    name == "quantile_over_time" || OverTime::of(name).is_some()
}

/// The argument of `call` holding its range: the second of
/// `quantile_over_time`, whose first is the quantile, and the first of
/// every other function.
fn range_arg(call: &parser::Call) -> Option<&Expr> {
    let at = usize::from(call.func.name == "quantile_over_time");
    call.args.args.get(at).map(|a| &**a)
}

/// The aggregation of the `*_over_time` call `call`. A
/// `quantile_over_time`'s quantile must be a number.
fn over_time_agg(call: &parser::Call) -> Result<OverTime, QueryError> {
    if let Some(agg) = OverTime::of(call.func.name) {
        return Ok(agg);
    }
    let mut q = call.args.args.first().map(|a| &**a);
    while let Some(Expr::Paren(p)) = q {
        q = Some(&p.expr);
    }
    match q {
        Some(Expr::NumberLiteral(n)) => Ok(OverTime::Quantile(n.val)),
        _ => Err(QueryError::Unsupported(
            "quantile_over_time takes a number as its quantile".to_string(),
        )),
    }
}

fn build_call<'a, 'expr>(
    ctx: &'a Ctx<'a>,
    call: &'expr parser::Call,
) -> Result<Built<'a>, QueryError>
where
    'expr: 'a,
{
    if call.func.name == "histogram_quantile" {
        return build_histogram_quantile(ctx, call);
    }
    if call.func.name == "label_replace" {
        return build_label_replace(ctx, call);
    }
    if let Some(Expr::Subquery(sq)) = range_arg(call) {
        if is_over_time(call.func.name) {
            return build_subquery_over_time(ctx, call, sq);
        }
        return Err(QueryError::Unsupported(format!(
            "function {} does not take a subquery; the *_over_time functions do",
            call.func.name
        )));
    }
    if let Some(groups) = ctx.groups {
        if matches!(
            call.func.name,
            "rate" | "irate" | "increase" | "idelta" | "deriv"
        ) || is_over_time(call.func.name)
        {
            return build_per_source(ctx, groups, Leaf::Call(call));
        }
    }

    let Some(first) = range_arg(call) else {
        return Err(QueryError::Unsupported(format!(
            "function {} requires arguments",
            call.func.name
        )));
    };
    let Expr::MatrixSelector(sel) = first else {
        return Err(QueryError::Unsupported(format!(
            "function {} requires a matrix-selector argument",
            call.func.name
        )));
    };
    let metric_name = sel
        .vs
        .name
        .as_deref()
        .ok_or_else(|| QueryError::ParseError("Matrix selector missing name".to_string()))?;
    let selection = Selection::new(Some(metric_name), &sel.vs.matchers);
    let filter = &selection.filter;
    let range_ns = sel.range.as_nanos() as u64;
    // A windowed function's point at `t` reads `[t - range, t]`. Raw reads
    // as `raw_read_back` says.
    let data_start = match ctx.rate_mode {
        RateMode::Grid => ctx.start_ns.saturating_sub(range_ns),
        RateMode::Raw => ctx.start_ns.saturating_sub(raw_read_back(ctx, range_ns)),
    };

    match call.func.name {
        // `rate` is the rate over its `[range]` before each point and
        // `increase` the increase across it; `irate` ignores the `[range]`
        // and is the rate over one step, or between consecutive samples in
        // raw mode (see `RateMode`).
        "rate" | "irate" | "increase" => {
            let range = matches!(call.func.name, "rate" | "increase");
            let increase = call.func.name == "increase";
            if let Some((series, _)) = batch_rates(ctx, call, None) {
                return Ok(Built::Series {
                    series,
                    metric_name: Some(metric_name),
                    metric_name_for_error: Some(metric_name.to_string()),
                });
            }
            // A `rate` reads back its window (`rate_lookback`). An `irate`
            // on the grid reads back its step and two reaches, to bracket
            // the first interval's left edge (`irate_lookback`). Raw reads
            // as `raw_read_back` says: a raw `rate` over its window, a raw
            // `irate` over the pair, no further apart than four of its
            // interval (`max_gap`).
            let lookback = match (ctx.rate_mode, range) {
                (RateMode::Grid, true) => rate_lookback(ctx, metric_name, range_ns),
                (RateMode::Grid, false) => irate_lookback(ctx, metric_name, range_ns),
                (RateMode::Raw, true) => {
                    raw_read_back(ctx, rate_lookback(ctx, metric_name, range_ns))
                }
                (RateMode::Raw, false) => {
                    raw_read_back(ctx, rate_reach(ctx, metric_name).saturating_mul(2))
                }
            };
            let data_start = ctx.start_ns.saturating_sub(lookback);
            let Some(mut streams) =
                ctx.source
                    .counter_streams(metric_name, filter, data_start, ctx.end_ns)
            else {
                nothing_read(ctx.source, metric_name, MetricKind::Counter)?;
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
            selection.retain(&mut streams, |s| &s.labels);
            // On the grid each series' stream ends with its first sample
            // after the range (`next_samples`); raw points are at samples, so
            // a raw read ends at the range's end.
            if ctx.rate_mode == RateMode::Grid {
                let (from, span) = ctx.next_from();
                let want = streams.iter().map(|s| &s.labels).collect();
                let mut next = next_samples(from, metric_name, filter, span, ctx.end_ns, &want);
                for stream in &mut streams {
                    if let Some(after) = next.remove(&stream.labels) {
                        let samples =
                            std::mem::replace(&mut stream.samples, Box::new(std::iter::empty()));
                        stream.samples = Box::new(samples.chain(std::iter::once(after)));
                    }
                }
            }
            // Each series is its producer over its own sample stream, pulled
            // by whatever consumes it: an aggregate holds one buffered point
            // per series and each producer one interval's worth of samples,
            // not every series' every sample. Collecting here used to be
            // most of a query's memory on a wide table.
            let default_reach = rate_reach(ctx, metric_name);
            let series: SeriesSet<'a> = streams
                .into_iter()
                .map(|stream| {
                    (
                        stream
                            .interval
                            .map_or(default_reach, crate::batch_rate::interval_reach),
                        stream,
                    )
                })
                .map(|(reach, stream)| match ctx.rate_mode {
                    RateMode::Grid => {
                        let rate = CounterGridRate::from_stream(
                            stream.samples,
                            stream.windowed,
                            ctx.start_ns,
                            ctx.end_ns,
                            ctx.step_ns,
                            if range { range_ns } else { ctx.step_ns },
                        );
                        let rate = if range { rate.over_range(reach) } else { rate };
                        let rate = if increase { rate.over_increase() } else { rate };
                        // Explicit timestamps replace the grid, so a value
                        // lands on a slow source's real reading instead of
                        // being interpolated to a grid point that source
                        // never observed. An `irate`'s window is then the gap
                        // from the preceding timestamp.
                        match &ctx.eval_timestamps {
                            Some(points) => {
                                LabeledSeries::new(stream.labels, rate.at_points(points.clone()))
                            }
                            None => LabeledSeries::new(stream.labels, rate),
                        }
                    }
                    // A point at every sample read, from before the range;
                    // the result is cut at `start` (`clip_raw`).
                    RateMode::Raw if range => {
                        let rate = CounterGridRate::from_stream(
                            stream.samples,
                            stream.windowed,
                            ctx.start_ns,
                            ctx.end_ns,
                            ctx.step_ns,
                            range_ns,
                        )
                        .over_range(reach)
                        .at_samples();
                        let rate = if increase { rate.over_increase() } else { rate };
                        LabeledSeries::new(stream.labels, rate)
                    }
                    RateMode::Raw => LabeledSeries::new(
                        stream.labels,
                        // A pair spans up to four of the series' interval
                        // (two reaches): a missed tick or two with jitter.
                        CounterPairwiseRate::from_stream(stream.samples, ctx.end_ns)
                            .max_gap(reach.saturating_mul(2)),
                    ),
                })
                .collect();
            Ok(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            })
        }
        name if is_over_time(name) => {
            let agg = over_time_agg(call)?;
            let Some(mut gauges) = ctx
                .source
                .gauges(metric_name, filter, data_start, ctx.end_ns)
            else {
                nothing_read(ctx.source, metric_name, MetricKind::Gauge)?;
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
            selection.retain(&mut gauges.series, |g| &g.labels);
            let series: SeriesSet<'a> = gauges
                .series
                .into_iter()
                .map(|g| {
                    let values = GaugeOverTime::gauge_values(&g.values);
                    let producer = ctx.place(GaugeOverTime::new(
                        g.timestamps,
                        values,
                        Vec::new(),
                        ctx.start_ns,
                        ctx.end_ns,
                        ctx.step_ns,
                        range_ns,
                        matches!(ctx.rate_mode, RateMode::Raw),
                        agg,
                    ));
                    LabeledSeries::new(g.labels, producer)
                })
                .collect();
            Ok(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            })
        }
        "idelta" => {
            let Some(mut gauges) = ctx
                .source
                .gauges(metric_name, filter, data_start, ctx.end_ns)
            else {
                nothing_read(ctx.source, metric_name, MetricKind::Gauge)?;
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
            selection.retain(&mut gauges.series, |g| &g.labels);
            let series: SeriesSet<'a> = gauges
                .series
                .into_iter()
                .map(|g| {
                    let producer = ctx.place(GaugeIdelta::new(
                        g.timestamps,
                        g.values,
                        ctx.start_ns,
                        ctx.end_ns,
                        ctx.step_ns,
                        range_ns,
                        matches!(ctx.rate_mode, RateMode::Raw),
                    ));
                    LabeledSeries::new(g.labels, producer)
                })
                .collect();
            Ok(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            })
        }
        "deriv" => {
            // The slope of the values in `[t - range, t]`: a gauge's, or a
            // counter's as read (no reset handling), as in PromQL.
            let mut values: Vec<(crate::labels::Labels, Vec<u64>, Vec<f64>)> = if let Some(gauges) =
                ctx.source
                    .gauges(metric_name, filter, data_start, ctx.end_ns)
            {
                gauges
                    .series
                    .into_iter()
                    .map(|g| (g.labels, g.timestamps, g.values.to_f64()))
                    .collect()
            } else if let Some(counters) =
                ctx.source
                    .counters(metric_name, filter, data_start, ctx.end_ns)
            {
                counters
                    .series
                    .into_iter()
                    .map(|c| (c.labels, c.timestamps, c.values.to_f64()))
                    .collect()
            } else {
                // Nothing matched: empty, unless the source holds the name
                // as neither a gauge nor a counter.
                let kind = if ctx.source.counter_names().iter().any(|n| n == metric_name) {
                    MetricKind::Counter
                } else {
                    MetricKind::Gauge
                };
                nothing_read(ctx.source, metric_name, kind)?;
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
            selection.retain(&mut values, |(labels, _, _)| labels);
            let series: SeriesSet<'a> = values
                .into_iter()
                .map(|(labels, timestamps, values)| {
                    let producer = ctx.place(GaugeDeriv::new(
                        timestamps,
                        values,
                        ctx.start_ns,
                        ctx.end_ns,
                        ctx.step_ns,
                        range_ns,
                        matches!(ctx.rate_mode, RateMode::Raw),
                    ));
                    LabeledSeries::new(labels, producer)
                })
                .collect();
            Ok(Built::Series {
                series,
                metric_name: Some("deriv"),
                metric_name_for_error: Some(metric_name.to_string()),
            })
        }
        other => Err(QueryError::Unsupported(format!(
            "function not supported: {other}"
        ))),
    }
}

/// `histogram_quantile(q, metric{matchers})` — single-quantile case
/// of the histogram quantile pipeline.
fn build_histogram_quantile<'a, 'expr>(
    ctx: &'a Ctx<'a>,
    call: &'expr parser::Call,
) -> Result<Built<'a>, QueryError>
where
    'expr: 'a,
{
    if call.args.args.len() < 2 {
        return Err(QueryError::ParseError(
            "histogram_quantile requires 2 arguments".to_string(),
        ));
    }
    let Expr::NumberLiteral(num) = &*call.args.args[0] else {
        return Err(QueryError::ParseError(
            "histogram_quantile first argument must be a number".to_string(),
        ));
    };
    let quantile = num.val;
    if !(0.0..=1.0).contains(&quantile) {
        return Err(QueryError::ParseError(format!(
            "histogram_quantile quantile must be between 0.0 and 1.0, got {quantile}"
        )));
    }
    let Expr::VectorSelector(sel) = &*call.args.args[1] else {
        return Err(QueryError::ParseError(
            "histogram_quantile second argument must be a metric name".to_string(),
        ));
    };
    let metric_name = sel
        .name
        .as_deref()
        .ok_or_else(|| QueryError::ParseError("Vector selector missing name".to_string()))?;
    let selection = Selection::new(Some(metric_name), &sel.matchers);
    if crate::promql::reads_gauge_histogram(ctx.source, metric_name)? {
        let read = crate::promql::read_gauge_histogram(
            ctx.source,
            metric_name,
            &selection,
            (ctx.start_ns, ctx.end_ns, ctx.step_ns),
            ctx.eval_timestamps.as_deref(),
        );
        let result = match read {
            Some((parts, times)) => {
                super::gauge_histogram::quantiles(parts, &[quantile], &times, metric_name)
            }
            None => {
                nothing_read(ctx.source, metric_name, MetricKind::GaugeHistogram)?;
                Vec::new()
            }
        };
        return Ok(Built::Materialized {
            result,
            name: metric_name.to_string(),
        });
    }
    let Some(stream) = crate::promql::read_histogram(
        ctx.source,
        metric_name,
        &selection,
        crate::promql::histogram_read_start(ctx.source, metric_name, ctx.start_ns),
        ctx.end_ns,
    ) else {
        nothing_read(ctx.source, metric_name, MetricKind::Histogram)?;
        return Ok(Built::Materialized {
            result: Vec::new(),
            name: metric_name.to_string(),
        });
    };
    let (stream, scales) = crate::promql::scale::Scales::watch(stream);
    let mut result = stream.quantiles(&[quantile], ctx.start_ns, ctx.end_ns, None, metric_name);
    let scale = scales.scale("histogram_quantile", metric_name)?;
    crate::promql::scale::unscale_samples(&mut result, scale);
    Ok(Built::Materialized {
        result,
        name: metric_name.to_string(),
    })
}

fn build_vector_selector<'a, 'expr>(
    ctx: &'a Ctx<'a>,
    sel: &'expr parser::VectorSelector,
) -> Result<Built<'a>, QueryError>
where
    'expr: 'a,
{
    if let Some(groups) = ctx.groups {
        return build_per_source(ctx, groups, Leaf::Selector(sel));
    }
    let metric_name = sel
        .name
        .as_deref()
        .ok_or_else(|| QueryError::ParseError("Vector selector missing name".to_string()))?;
    let selection = Selection::new(Some(metric_name), &sel.matchers);

    // A gauge is held a step, or its sampling interval when that is longer,
    // past each sample. The interval is the series' own: a source joining
    // recordings of different intervals hands each recording's series over
    // with its interval (`gauge_parts`), so a 10 s recording's gauge is not
    // dropped between its rows beside a 1 s recording's.
    // In raw mode the read starts as `raw_read_back` says.
    let read_start = match ctx.rate_mode {
        RateMode::Grid => ctx.start_ns,
        RateMode::Raw => ctx.start_ns.saturating_sub(raw_read_back(ctx, 0)),
    };
    let Some(mut parts) = ctx.source.gauge_parts(
        metric_name,
        &selection.filter,
        read_start,
        ctx.end_ns,
        ctx.step_ns,
    ) else {
        // A bare selector reads gauges. A counter is read through rate() or
        // irate(), and saying so beats reporting a metric the source holds as
        // missing. Asked only on this path, so a found gauge costs nothing.
        nothing_read(ctx.source, metric_name, MetricKind::Gauge)?;
        return Ok(Built::Series {
            series: Vec::new(),
            metric_name: Some(metric_name),
            metric_name_for_error: None,
        });
    };
    for (gauges, _) in &mut parts {
        selection.retain(&mut gauges.series, |g| &g.labels);
    }

    let series: SeriesSet<'a> = parts
        .into_iter()
        .flat_map(|(gauges, interval_s)| {
            let staleness_ns = ctx.step_ns.max((interval_s * 1e9) as u64);
            gauges.series.into_iter().map(move |g| {
                let producer = ctx.place(GaugeStepGrid::new(
                    g.timestamps,
                    g.values,
                    ctx.start_ns,
                    ctx.end_ns,
                    ctx.step_ns,
                    staleness_ns,
                    matches!(ctx.rate_mode, RateMode::Raw),
                ));
                LabeledSeries::new(g.labels, producer)
            })
        })
        .collect();
    Ok(Built::Series {
        series,
        metric_name: Some(metric_name),
        metric_name_for_error: Some(metric_name.to_string()),
    })
}

/// A leaf a process group's members are read for one by one.
#[derive(Clone, Copy)]
enum Leaf<'e> {
    Call(&'e parser::Call),
    Selector(&'e parser::VectorSelector),
}

impl<'e> Leaf<'e> {
    /// The metric the leaf reads.
    fn metric(self) -> Option<&'e str> {
        match self {
            Leaf::Call(call) => match range_arg(call) {
                Some(Expr::MatrixSelector(sel)) => sel.vs.name.as_deref(),
                _ => None,
            },
            Leaf::Selector(sel) => sel.name.as_deref(),
        }
    }

    /// What a point of the leaf reads (`per_source::Window`).
    fn window(self, mode: RateMode) -> Window {
        let raw = matches!(mode, RateMode::Raw);
        match self {
            Leaf::Call(call) => {
                let range = match range_arg(call) {
                    Some(Expr::MatrixSelector(sel)) => sel.range.as_nanos() as u64,
                    _ => 0,
                };
                match (call.func.name, raw) {
                    ("irate", false) => Window::Step,
                    ("irate", true) => Window::Point,
                    _ => Window::Range(range),
                }
            }
            Leaf::Selector(_) if raw => Window::Point,
            Leaf::Selector(_) => Window::Stale,
        }
    }

    /// Whether the leaf reads `name` as a counter in `source`.
    fn reads_counter(self, source: &dyn DataSource, name: &str) -> bool {
        match self {
            Leaf::Call(call) => match call.func.name {
                "rate" | "irate" | "increase" => true,
                "deriv" => source.counter_names().iter().any(|n| n == name),
                _ => false,
            },
            Leaf::Selector(_) => false,
        }
    }

    /// Whether `source` holds `name` as the kind the leaf reads, so reading
    /// it can give series rather than "not found" or "wrong kind".
    fn held_by(self, source: &dyn DataSource, name: &str) -> bool {
        let counter = || source.counter_names().iter().any(|n| n == name);
        let gauge = || source.gauge_names().iter().any(|n| n == name);
        match self {
            Leaf::Call(call) => match call.func.name {
                "rate" | "irate" | "increase" => counter(),
                "deriv" => counter() || gauge(),
                _ => gauge(),
            },
            Leaf::Selector(_) => gauge(),
        }
    }

    fn build<'a>(self, ctx: &'a Ctx<'a>) -> Result<Built<'a>, QueryError>
    where
        'e: 'a,
    {
        match self {
            Leaf::Call(call) => build_call(ctx, call),
            Leaf::Selector(sel) => build_vector_selector(ctx, sel),
        }
    }
}

/// `leaf`'s series over sources some of which form process groups: the
/// recordings of one member as one evaluation, and each group's members one
/// by one, combined per series (S9, [`per_source`]).
fn build_per_source<'a, 'e>(
    ctx: &'a Ctx<'a>,
    groups: &'a SourceGroups,
    leaf: Leaf<'e>,
) -> Result<Built<'a>, QueryError>
where
    'e: 'a,
{
    if let Leaf::Call(call) = leaf {
        if let Some((series, _)) = batch_rates(ctx, call, None) {
            return Ok(Built::Series {
                series,
                metric_name: leaf.metric(),
                metric_name_for_error: leaf.metric().map(str::to_string),
            });
        }
    }
    let Some(name) = leaf.metric() else {
        // The errors the single-source path gives these shapes.
        return Err(match leaf {
            Leaf::Call(call) => match range_arg(call) {
                None => QueryError::Unsupported(format!(
                    "function {} requires arguments",
                    call.func.name
                )),
                Some(Expr::MatrixSelector(_)) => {
                    QueryError::ParseError("Matrix selector missing name".to_string())
                }
                Some(_) => QueryError::Unsupported(format!(
                    "function {} requires a matrix-selector argument",
                    call.func.name
                )),
            },
            Leaf::Selector(_) => QueryError::ParseError("Vector selector missing name".to_string()),
        });
    };
    let collect = |built: Built<'_>| -> Vec<(crate::labels::Labels, Vec<super::Point>)> {
        match built {
            Built::Series { series, .. } => series
                .into_iter()
                .map(|ls| (ls.labels, ls.iter.collect()))
                .collect(),
            Built::Scalar(_) | Built::Materialized { .. } => Vec::new(),
        }
    };
    let mut out: Vec<(crate::labels::Labels, Vec<super::Point>)> = Vec::new();
    // A recording that does not hold the leaf's metric as the kind it reads
    // adds no series and is not read: read alone, it would answer "not
    // found" for a name other recordings hold. When none holds it, the
    // whole source answers, with the error a single source would give.
    let held = |source: &dyn DataSource| leaf.held_by(source, name);
    let any = groups.parts.iter().any(|p| match p {
        per_source::Part::Single(s) => held(s.as_ref()),
        per_source::Part::Group(members) => members.iter().any(|m| held(m.source.as_ref())),
    });
    if !any {
        out.extend(collect(leaf.build(&ctx.over(ctx.source))?));
    }
    let window = leaf.window(ctx.rate_mode);
    let step_ns = ctx.step_ns;
    let eval = ctx.eval_timestamps.clone();
    let window_ns = move |t: u64, interval_ns: u64| -> u64 {
        match window {
            Window::Range(r) => r,
            Window::Step => match &eval {
                Some(points) => {
                    let i = points.partition_point(|&p| p < t);
                    i.checked_sub(1).map_or(step_ns, |j| t - points[j])
                }
                None => step_ns,
            },
            Window::Stale => step_ns.max(interval_ns),
            Window::Point => 0,
        }
    };
    for part in groups.parts.iter().filter(|_| any) {
        let members: Vec<&per_source::Member> = match part {
            per_source::Part::Single(source) => {
                if held(source.as_ref()) {
                    out.extend(collect(leaf.build(&ctx.over(source.as_ref()))?));
                }
                continue;
            }
            per_source::Part::Group(members) => {
                members.iter().filter(|m| held(m.source.as_ref())).collect()
            }
        };
        if members.is_empty() {
            continue;
        }
        let interval_ns = |m: &per_source::Member| -> u64 {
            let s = if leaf.reads_counter(m.source.as_ref(), name) {
                m.source.counter_interval(name)
            } else {
                m.source.gauge_interval(name)
            };
            (s * 1e9) as u64
        };
        // S3's rank: finest first, then the tie rule (`Member::cmp_ties`).
        let mut ranked: Vec<(u64, &per_source::Member)> =
            members.iter().map(|m| (interval_ns(m), *m)).collect();
        ranked.sort_by(|(ia, a), (ib, b)| {
            ia.cmp(ib).then_with(|| a.cmp_ties(b)).then(a.lo.cmp(&b.lo))
        });
        if let Some((range_ns, offset_ns)) =
            group_increase_range(ctx, leaf).filter(|_| ranked.len() > 1)
        {
            let selection = match leaf {
                Leaf::Call(call) => match call.args.args.first().map(|a| &**a) {
                    Some(Expr::MatrixSelector(sel)) => Selection::new(Some(name), &sel.vs.matchers),
                    _ => unreachable!("group_increase_range takes a matrix selector"),
                },
                Leaf::Selector(_) => unreachable!("group_increase_range takes a call"),
            };
            let times: Vec<u64> = match &ctx.eval_timestamps {
                Some(points) => points
                    .iter()
                    .copied()
                    .filter(|t| (ctx.start_ns..=ctx.end_ns).contains(t))
                    .collect(),
                None if ctx.step_ns == 0 => vec![ctx.start_ns],
                None => (ctx.start_ns..=ctx.end_ns)
                    .step_by(usize::try_from(ctx.step_ns).unwrap_or(usize::MAX))
                    .collect(),
            };
            // `offset`: each point is evaluated `offset` before its time
            // and stamped at its time.
            let shift = |t: u64| -> Option<u64> {
                u64::try_from(i128::from(t) - i128::from(offset_ns)).ok()
            };
            let (shifted, stamps): (Vec<u64>, Vec<u64>) = times
                .iter()
                .filter_map(|&t| shift(t).map(|s| (s, t)))
                .unzip();
            let (Some(&first), Some(&last)) = (shifted.first(), shifted.last()) else {
                continue;
            };
            let rows = ranked
                .iter()
                .map(|&(interval_ns, m)| {
                    group_increase_rows(m, name, &selection, (first, last), range_ns, interval_ns)
                })
                .collect();
            for (labels, mut points) in super::group_increase::increase(rows, &shifted, range_ns) {
                for p in points.iter_mut() {
                    let k = shifted.partition_point(|s| *s < p.t);
                    p.t = stamps[k];
                }
                out.push((labels, points));
            }
            continue;
        }
        // Every member is read over the whole range: a grid point
        // interpolates between rows on either side of its window's edges,
        // however far apart they are, so a narrower read can change a point.
        let mut evaluated = Vec::with_capacity(ranked.len());
        for (interval_ns, m) in ranked {
            let tap = Tap::new(m.source.clone(), m.lo, m.hi).with_context(m.raw.clone());
            // Each series' first row after the end is read from the member's
            // source within its span, not through `tap`: S3 bounds the rows
            // past a read that decide coverage, and the tap would take it as
            // one of them.
            let member = Ctx {
                next_source: Some((m.source.as_ref(), (m.lo, m.hi))),
                ..ctx.over(&tap)
            };
            let series = collect(leaf.build(&member)?);
            evaluated.push(MemberSeries::new(series, tap.take_rows(), interval_ns));
        }
        out.extend(per_source::combine(
            evaluated,
            &window_ns,
            matches!(ctx.rate_mode, RateMode::Raw),
        ));
    }
    let metric_name = match leaf {
        Leaf::Call(call) if call.func.name == "deriv" => Some("deriv"),
        _ => Some(name),
    };
    Ok(Built::Series {
        series: out
            .into_iter()
            .map(|(labels, points)| LabeledSeries::new(labels, points.into_iter()))
            .collect(),
        metric_name,
        metric_name_for_error: Some(name.to_string()),
    })
}

/// The `[range]` and `offset` (positive looking back) of `leaf` when it is
/// a grid `increase` of a matrix selector without `@`, which over a group
/// of several members is evaluated by [`super::group_increase`].
fn group_increase_range(ctx: &Ctx<'_>, leaf: Leaf<'_>) -> Option<(u64, i64)> {
    let Leaf::Call(call) = leaf else {
        return None;
    };
    if call.func.name != "increase" || !matches!(ctx.rate_mode, RateMode::Grid) {
        return None;
    }
    match call.args.args.first().map(|a| &**a) {
        Some(Expr::MatrixSelector(sel)) if sel.vs.at.is_none() => {
            let ns = |d: &std::time::Duration| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
            let offset = match &sel.vs.offset {
                None => 0,
                Some(parser::Offset::Pos(d)) => ns(d),
                Some(parser::Offset::Neg(d)) => -ns(d),
            };
            Some((sel.range.as_nanos() as u64, offset))
        }
        _ => None,
    }
}

/// Member `m`'s samples of counter `name` for a group `increase` over
/// `[range]` evaluated from `start_ns` to `end_ns`: its rows within its
/// span from the start less the `[range]` and two reaches to the end, and
/// each series' first row after the end (`next_samples`).
fn group_increase_rows(
    m: &per_source::Member,
    name: &str,
    selection: &Selection,
    (start_ns, end_ns): (u64, u64),
    range_ns: u64,
    interval_ns: u64,
) -> super::group_increase::MemberRows {
    let reach = crate::batch_rate::interval_reach(interval_ns as f64 / 1e9);
    let from = start_ns
        .saturating_sub(range_ns.saturating_add(reach.saturating_mul(2)))
        .max(m.lo);
    let to = end_ns.min(m.hi);
    let mut series: Vec<(crate::labels::Labels, Vec<crate::types::CounterSample>)> = match (from
        <= to)
        .then(|| m.source.counter_streams(name, &selection.filter, from, to))
        .flatten()
    {
        Some(streams) => streams
            .into_iter()
            .filter(|s| selection.selects(&s.labels))
            .map(|s| {
                let samples = s
                    .samples
                    .filter(|x| (m.lo..=m.hi).contains(&x.ts))
                    .collect();
                (s.labels, samples)
            })
            .collect(),
        None => Vec::new(),
    };
    let labels: Vec<crate::labels::Labels> = series.iter().map(|(l, _)| l.clone()).collect();
    let want: std::collections::HashSet<&crate::labels::Labels> = labels.iter().collect();
    let mut next = next_samples(
        m.source.as_ref(),
        name,
        &selection.filter,
        (m.lo, m.hi),
        end_ns,
        &want,
    );
    for (labels, samples) in series.iter_mut() {
        if let Some(s) = next.remove(labels) {
            samples.push(s);
        }
    }
    super::group_increase::MemberRows {
        epoch: m.epoch.clone(),
        interval_ns,
        series,
    }
}

#[cfg(test)]
mod depth_tests {
    use super::join_depth;

    /// Aggregations and operators between two series sets count; a scalar
    /// operand and a function do not.
    #[test]
    fn join_depth_counts_nested_joins() {
        for (query, depth) in [
            ("rate(x[1m])", 0),
            ("x * 2", 0),
            ("sum(x)", 1),
            ("a + b", 1),
            ("2 * (a + b)", 1),
            ("(a + b) + c", 2),
            ("sum(a + b) + c", 3),
            ("sum(irate(x[1m])) / sum(irate(y[1m]))", 2),
        ] {
            let expr = promql_parser::parser::parse(query).unwrap();
            assert_eq!(join_depth(&expr), depth, "{query}");
        }
    }
}
