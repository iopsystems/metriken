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
//!   / `group_right` are NOT supported.
//! * `NumberLiteral` — produces a `Built::Scalar`; usable on either
//!   side of a binary op.
//! * Parenthesised expressions are unwrapped.

use super::histogram::HistogramOps;
use super::per_source::{self, MemberSeries, SourceGroups, Tap, Window};
use promql_parser::parser::{self, Expr};

use crate::promql::extract_filter_labels;
use crate::promql::streaming::{
    aggregate, collect_to_matrix, interval_binop, matrix_matrix_op, matrix_scalar_op, AggOp,
    AtPoints, BinOp, CounterGridRate, CounterPairwiseRate, GaugeAvgOverTime, GaugeDeriv,
    GaugeIdelta, GaugeStepGrid, GroupBy, Join, LabeledSeries, MatchSpec, SeriesSet,
};
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
    let interval = names
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

/// How far before a `rate(name[range])` point's time its read starts: the
/// `[range]`, the reach before it ([`rate_reach`]), and one reach more for
/// a point held past a series' last sample, whose window ends at that
/// sample, up to the series' typical spacing before the point.
fn rate_lookback(ctx: &Ctx<'_>, name: &str, range_ns: u64) -> u64 {
    range_ns.saturating_add(rate_reach(ctx, name).saturating_mul(2))
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

/// `rate`/`irate` of a matrix selector on the grid, computed by the source in
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
    #[cfg(test)]
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
        return crate::batch_rate::grid_rates(scan, &request);
    };
    let range = call.func.name == "rate";
    let range_ns = match call.args.args.first().map(|a| &**a) {
        Some(Expr::MatrixSelector(sel)) => sel.range.as_nanos() as u64,
        _ => return None,
    };
    // What a part's read starts at: as `grid_request` computes it over that
    // part alone.
    let data_start = |source: &dyn DataSource| -> u64 {
        let lookback = if range {
            range_ns.saturating_add(
                crate::batch_rate::interval_reach(source.counter_interval(name)).saturating_mul(2),
            )
        } else {
            range_ns.max(ctx.step_ns)
        };
        ctx.start_ns.saturating_sub(lookback)
    };
    let holds = |source: &dyn DataSource| source.counter_names().iter().any(|n| n == name);
    let mut scans = Vec::new();
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
                // A member clipped to one epoch cannot be scanned.
                if members.iter().any(|m| m.lo != 0 || m.hi != u64::MAX) {
                    return None;
                }
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
                    let mut scan = m.raw.counter_scan(name, filter, from, request.end_ns)?;
                    let interval = m.source.counter_interval(name);
                    let labels: Vec<crate::labels::Labels> =
                        scan.series.iter().map(|s| s.labels.clone()).collect();
                    let tap = Tap::new(m.raw.clone(), 0, u64::MAX);
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
    crate::batch_rate::grid_rates(metriken_storage::scan::interleave(scans), &request)
}

/// The metric, filter and request for a batch `rate`/`irate` of `call`, or
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
    if !matches!(call.func.name, "rate" | "irate") || call.args.args.len() != 1 {
        return None;
    }
    let Expr::MatrixSelector(sel) = &*call.args.args[0] else {
        return None;
    };
    if sel.vs.offset.is_some() || sel.vs.at.is_some() {
        return None;
    }
    let name = sel.vs.name.as_deref()?;
    let filter = extract_filter_labels(&sel.vs.matchers.matchers);
    let range_ns = sel.range.as_nanos() as u64;
    // `rate` is over its `[range]`; `irate` over one step.
    let range = call.func.name == "rate";
    let (lookback, span_ns) = if range {
        (rate_lookback(ctx, name, range_ns), range_ns)
    } else {
        (range_ns.max(ctx.step_ns), ctx.step_ns)
    };
    let request = crate::batch_rate::GridRateRequest {
        data_start: ctx.start_ns.saturating_sub(lookback),
        start_ns: ctx.start_ns,
        end_ns: ctx.end_ns,
        step_ns: ctx.step_ns,
        span_ns,
        range,
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
                let Some(op) = BinOp::from_token(&bin.op) else {
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
    let Some(op) = BinOp::from_token(&bin.op) else {
        return Err(QueryError::Unsupported(format!(
            "binary operator not supported: {}",
            bin.op
        )));
    };

    if let Some(modifier) = &bin.modifier {
        if modifier.card != parser::VectorMatchCardinality::OneToOne {
            return Err(QueryError::Unsupported(
                "group_left / group_right (one-to-many matching) not supported".to_string(),
            ));
        }
    }

    let spec = match bin.modifier.as_ref().and_then(|m| m.matching.as_ref()) {
        None => MatchSpec::Default,
        Some(parser::LabelModifier::Include(ls)) => MatchSpec::Include(ls.labels.as_slice()),
        Some(parser::LabelModifier::Exclude(ls)) => MatchSpec::Exclude(ls.labels.as_slice()),
    };

    let lhs = build(ctx, &bin.lhs)?;
    let rhs = build(ctx, &bin.rhs)?;

    match (lhs, rhs) {
        (Built::Series { series, .. }, Built::Scalar(s)) => Ok(Built::Series {
            series: matrix_scalar_op(series, op, s, false),
            metric_name: None,
            metric_name_for_error: None,
        }),
        (Built::Scalar(s), Built::Series { series, .. }) => Ok(Built::Series {
            series: matrix_scalar_op(series, op, s, true),
            metric_name: None,
            metric_name_for_error: None,
        }),
        (
            Built::Series {
                series: left_series,
                ..
            },
            Built::Series {
                series: right_series,
                ..
            },
        ) => Ok(Built::Series {
            series: matrix_matrix_op(
                left_series,
                right_series,
                op,
                spec,
                &ctx.recording_keys,
                Join::for_mode(ctx.rate_mode, ctx.raw_hold_ns),
                ctx.start_ns,
            )?,
            metric_name: None,
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

/// Apply a scalar op to each materialized `MatrixSample`'s values and, when
/// present, its uncertainty bands (interval arithmetic against a constant:
/// apply to both endpoints, `min`/`max` to normalize sign; drop on div-by-zero).
/// Keeps values and bands aligned point-for-point.
fn scalar_op_matrix(
    mut result: Vec<crate::promql::MatrixSample>,
    op: BinOp,
    scalar: f64,
    scalar_first: bool,
) -> Vec<crate::promql::MatrixSample> {
    let apply = |x: f64| -> Option<f64> {
        if scalar_first {
            op.apply(scalar, x)
        } else {
            op.apply(x, scalar)
        }
    };
    for sample in &mut result {
        let had_intervals = sample.intervals.is_some();
        let mut vals = Vec::with_capacity(sample.values.len());
        let mut ivls = Vec::with_capacity(sample.values.len());
        for (i, (t, v)) in sample.values.iter().enumerate() {
            let Some(nv) = apply(*v) else { continue };
            vals.push((*t, nv));
            // Propagate the band via the same interval arithmetic as the
            // streaming scalar op (treat the scalar as exact `[scalar, scalar]`),
            // so a spanning-zero denominator under `scalar / value` yields no
            // band instead of a bogus narrow one. Histogram bands are currently
            // non-negative so this path can't hit that case today, but it stays
            // consistent with ScalarBroadcast.
            let sb = (scalar, scalar);
            if let Some((lo, hi)) = sample.intervals.as_ref().and_then(|iv| iv.get(i)) {
                let band = if scalar_first {
                    interval_binop(op, sb, (*lo, *hi))
                } else {
                    interval_binop(op, (*lo, *hi), sb)
                };
                if let Some(bb) = band {
                    ivls.push(bb);
                }
            }
        }
        sample.intervals = if had_intervals && ivls.len() == vals.len() {
            Some(ivls)
        } else {
            None
        };
        sample.values = vals;
    }
    result
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
    if let Some(groups) = ctx.groups {
        if matches!(
            call.func.name,
            "rate" | "irate" | "avg_over_time" | "idelta" | "deriv"
        ) {
            return build_per_source(ctx, groups, Leaf::Call(call));
        }
    }

    let Some(first) = call.args.args.first() else {
        return Err(QueryError::Unsupported(format!(
            "function {} requires arguments",
            call.func.name
        )));
    };
    let Expr::MatrixSelector(sel) = &**first else {
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
    let filter = extract_filter_labels(&sel.vs.matchers.matchers);
    let range_ns = sel.range.as_nanos() as u64;
    // A windowed function's point at `t` reads `[t - range, t]`. Raw reads
    // as `raw_read_back` says.
    let data_start = match ctx.rate_mode {
        RateMode::Grid => ctx.start_ns.saturating_sub(range_ns),
        RateMode::Raw => ctx.start_ns.saturating_sub(raw_read_back(ctx, range_ns)),
    };

    match call.func.name {
        // `rate` is the rate over its `[range]` before each point; `irate`
        // ignores the `[range]` and is the rate over one step, or between
        // consecutive samples in raw mode (see `RateMode`).
        "rate" | "irate" => {
            let range = call.func.name == "rate";
            if let Some((series, _)) = batch_rates(ctx, call, None) {
                return Ok(Built::Series {
                    series,
                    metric_name: Some(metric_name),
                    metric_name_for_error: Some(metric_name.to_string()),
                });
            }
            // A `rate` reads back its window (`rate_lookback`). An `irate`
            // on the grid needs at least one step of lookback to bracket the
            // first interval's left edge. Raw reads as `raw_read_back` says:
            // a raw `rate` over its window, a raw `irate` over the pair, no
            // further apart than four of its interval (`max_gap`).
            let lookback = match (ctx.rate_mode, range) {
                (RateMode::Grid, true) => rate_lookback(ctx, metric_name, range_ns),
                (RateMode::Grid, false) => range_ns.max(ctx.step_ns),
                (RateMode::Raw, true) => {
                    raw_read_back(ctx, rate_lookback(ctx, metric_name, range_ns))
                }
                (RateMode::Raw, false) => {
                    raw_read_back(ctx, rate_reach(ctx, metric_name).saturating_mul(2))
                }
            };
            let data_start = ctx.start_ns.saturating_sub(lookback);
            let Some(streams) =
                ctx.source
                    .counter_streams(metric_name, &filter, data_start, ctx.end_ns)
            else {
                nothing_read(ctx.source, metric_name, MetricKind::Counter)?;
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
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
                    RateMode::Raw if range => LabeledSeries::new(
                        stream.labels,
                        CounterGridRate::from_stream(
                            stream.samples,
                            stream.windowed,
                            ctx.start_ns,
                            ctx.end_ns,
                            ctx.step_ns,
                            range_ns,
                        )
                        .over_range(reach)
                        .at_samples(),
                    ),
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
        "avg_over_time" => {
            let Some(gauges) = ctx
                .source
                .gauges(metric_name, &filter, data_start, ctx.end_ns)
            else {
                nothing_read(ctx.source, metric_name, MetricKind::Gauge)?;
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
            let series: SeriesSet<'a> = gauges
                .series
                .into_iter()
                .map(|g| {
                    let producer = ctx.place(GaugeAvgOverTime::new(
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
        "idelta" => {
            let Some(gauges) = ctx
                .source
                .gauges(metric_name, &filter, data_start, ctx.end_ns)
            else {
                nothing_read(ctx.source, metric_name, MetricKind::Gauge)?;
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
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
            let values: Vec<(crate::labels::Labels, Vec<u64>, Vec<f64>)> = if let Some(gauges) = ctx
                .source
                .gauges(metric_name, &filter, data_start, ctx.end_ns)
            {
                gauges
                    .series
                    .into_iter()
                    .map(|g| {
                        (
                            g.labels,
                            g.timestamps,
                            g.values.iter().map(|v| *v as f64).collect(),
                        )
                    })
                    .collect()
            } else if let Some(counters) =
                ctx.source
                    .counters(metric_name, &filter, data_start, ctx.end_ns)
            {
                counters
                    .series
                    .into_iter()
                    .map(|c| {
                        (
                            c.labels,
                            c.timestamps,
                            c.values.iter().map(|v| *v as f64).collect(),
                        )
                    })
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
    let filter = extract_filter_labels(&sel.matchers.matchers);
    if crate::promql::reads_gauge_histogram(ctx.source, metric_name)? {
        let read = crate::promql::read_gauge_histogram(
            ctx.source,
            metric_name,
            &filter,
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
    let Some(stream) = ctx.source.histogram_stream(
        metric_name,
        &filter,
        crate::promql::histogram_read_start(ctx.source, metric_name, ctx.start_ns),
        ctx.end_ns,
    ) else {
        nothing_read(ctx.source, metric_name, MetricKind::Histogram)?;
        return Ok(Built::Materialized {
            result: Vec::new(),
            name: metric_name.to_string(),
        });
    };
    let result = stream.quantiles(&[quantile], ctx.start_ns, ctx.end_ns, None, metric_name);
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
    let filter = extract_filter_labels(&sel.matchers.matchers);

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
    let Some(parts) =
        ctx.source
            .gauge_parts(metric_name, &filter, read_start, ctx.end_ns, ctx.step_ns)
    else {
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
            Leaf::Call(call) => match call.args.args.first().map(|a| &**a) {
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
                let range = match call.args.args.first().map(|a| &**a) {
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
                "rate" | "irate" => true,
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
                "rate" | "irate" => counter(),
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
            Leaf::Call(call) => match call.args.args.first().map(|a| &**a) {
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
        // Every member is read over the whole range: a grid point
        // interpolates between rows on either side of its window's edges,
        // however far apart they are, so a narrower read can change a point.
        let mut evaluated = Vec::with_capacity(ranked.len());
        for (interval_ns, m) in ranked {
            let tap = Tap::new(m.source.clone(), m.lo, m.hi).with_context(m.raw.clone());
            let series = collect(leaf.build(&ctx.over(&tap))?);
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
