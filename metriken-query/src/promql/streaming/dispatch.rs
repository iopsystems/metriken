//! AST → streaming-pipeline dispatcher.
//!
//! This is the only PromQL evaluator in the engine — every shape it
//! doesn't recognise becomes `QueryError::Unsupported`.
//!
//! Recognised AST shapes:
//!
//! * `metric{matchers}` — gauge VectorSelector at step grid.
//! * `irate(metric[range])` / `rate(metric[range])` — counter producers.
//! * `avg_over_time(metric[range])` / `idelta(metric[range])` /
//!   `deriv(metric[range])` — gauge / counter-rate producers.
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

use promql_parser::parser::{self, Expr};

use crate::promql::extract_filter_labels;
use crate::promql::streaming::{
    aggregate, collect_to_matrix, interval_binop, matrix_matrix_op, matrix_scalar_op, AggOp,
    AtPoints, BinOp, CounterGridRate, CounterPairwiseRate, GaugeAvgOverTime, GaugeDeriv,
    GaugeIdelta, GaugeStepGrid, GroupBy, LabeledSeries, MatchSpec, SeriesSet, StreamingDeriv,
};
use crate::promql::{nothing_read, MatrixSample, MetricKind, QueryError, QueryResult};
use crate::{DataSource, QueryOptions, RateMode};

/// Evaluate `expr` via the streaming pipeline. Returns
/// `QueryError::Unsupported` for any AST shape the dispatcher doesn't
/// recognise; this is now the only PromQL evaluator in the engine.
pub fn try_streaming(
    source: &dyn DataSource,
    expr: &Expr,
    start: f64,
    end: f64,
    step: f64,
    opts: &QueryOptions,
) -> Result<QueryResult, QueryError> {
    let ctx = make_ctx(source, start, end, step, opts);
    let result = match build(&ctx, expr)? {
        Built::Series {
            series,
            metric_name,
            metric_name_for_error,
        } => {
            let collected = collect_to_matrix(series, metric_name);
            if collected.is_empty() {
                if let Some(name) = metric_name_for_error {
                    held_somewhere(source, &name)?;
                }
            }
            QueryResult::Matrix { result: collected }
        }
        Built::Materialized { result, name } => {
            if result.is_empty() {
                held_somewhere(source, &name)?;
            }
            QueryResult::Matrix { result }
        }
        Built::Scalar(v) => QueryResult::Scalar { result: (start, v) },
    };
    Ok(result)
}

/// The evaluation context for `[start, end]` at `step`.
fn make_ctx<'a>(
    source: &'a dyn DataSource,
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
    // labels land on round step multiples. Raw keeps the caller's start. Snap
    // once here so every downstream producer inherits the fixed phase.
    let start_ns = match rate_mode {
        RateMode::Grid if step_ns > 0 => (raw_start_ns / step_ns) * step_ns,
        _ => raw_start_ns,
    };
    Ctx {
        source,
        start_ns,
        end_ns: crate::promql::range_end_ns(end),
        step_ns,
        interval_ns: (source.interval() * 1e9) as u64,
        rate_mode,
        rate_span_ns: opts.rate_span_ns,
        eval_timestamps: opts.eval_timestamps.clone(),
        per_series_rates: opts.per_series_rates,
    }
}

/// [`try_streaming`] in display form: each series is reduced per `display`
/// as it is collected (see `collect_to_display`).
pub(crate) fn try_streaming_display(
    source: &dyn DataSource,
    expr: &Expr,
    start: f64,
    end: f64,
    step: f64,
    opts: &QueryOptions,
    display: &crate::DisplayOptions,
) -> Result<crate::DisplayResult, QueryError> {
    let ctx = make_ctx(source, start, end, step, opts);
    if let Some(result) = batch_display(&ctx, expr, start, end, step, display)? {
        return Ok(result);
    }
    let result = match build(&ctx, expr)? {
        Built::Series {
            series,
            metric_name,
            metric_name_for_error,
        } => {
            let collected =
                super::collect_to_display(series, metric_name, start, end, step, display);
            if collected.is_empty() {
                if let Some(name) = metric_name_for_error {
                    held_somewhere(source, &name)?;
                }
            }
            crate::DisplayResult::Series {
                result: collected,
                budget: display.budget as u32,
            }
        }
        Built::Materialized { result, name } => {
            if result.is_empty() {
                held_somewhere(source, &name)?;
            }
            crate::display::display_from_result(
                QueryResult::Matrix { result },
                start,
                end,
                step,
                display,
            )
        }
        Built::Scalar(v) => crate::DisplayResult::Scalar { result: (start, v) },
    };
    Ok(result)
}

struct Ctx<'a> {
    source: &'a dyn DataSource,
    start_ns: u64,
    end_ns: u64,
    step_ns: u64,
    interval_ns: u64,
    rate_mode: RateMode,
    rate_span_ns: Option<u64>,
    /// Explicit evaluation timestamps; see [`QueryOptions::eval_timestamps`].
    eval_timestamps: Option<std::sync::Arc<[u64]>>,
    /// See [`QueryOptions::per_series_rates`].
    per_series_rates: bool,
}

impl<'a> Ctx<'a> {
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
        ctx.source.counter_grid_rates(name, &filter, &request)?
    else {
        return None;
    };
    let series = results
        .into_iter()
        .map(|(labels, points)| LabeledSeries::new(labels, points.into_iter()))
        .collect();
    Some((series, name.to_string()))
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
    let request = crate::batch_rate::GridRateRequest {
        data_start: ctx.start_ns.saturating_sub(range_ns.max(ctx.step_ns)),
        start_ns: ctx.start_ns,
        end_ns: ctx.end_ns,
        step_ns: ctx.step_ns,
        span_ns: ctx.rate_span_ns.unwrap_or(ctx.step_ns),
        group,
        display,
    };
    Some((name, filter, request))
}

#[cfg(test)]
thread_local! {
    /// Display queries [`batch_display`] answered on this thread.
    pub(crate) static BATCH_DISPLAYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
        width: crate::display::bucket_width(start, end, display.budget),
        band: display.band,
        ops,
    };
    let Some((name, filter, request)) = grid_request(ctx, call, group, Some(&grid_display)) else {
        return Ok(None);
    };
    let Some(crate::batch_rate::GridRates::Display(results)) =
        ctx.source.counter_grid_rates(name, &filter, &request)
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
    }))
}

/// `MetricNotFound` for a name the source holds as no kind at all. A
/// source whose read of a name it does not hold returns an empty set rather
/// than `None` reaches here with an empty result.
fn held_somewhere(source: &dyn DataSource, name: &str) -> Result<(), QueryError> {
    let held = source.counter_names().iter().any(|n| n == name)
        || source.gauge_names().iter().any(|n| n == name)
        || source.histogram_names().iter().any(|n| n == name);
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
    let series = aggregate(inner_series, op, group_by);
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
            series: matrix_matrix_op(left_series, right_series, op, spec),
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
    let data_start = ctx.start_ns.saturating_sub(range_ns);

    match call.func.name {
        // `rate` and `irate` are the same operation in this engine: the query's
        // `[range]` window is inert, and the value is the per-step rate. The
        // mode only chooses point placement (see `RateMode`).
        "rate" | "irate" => {
            if let Some((series, _)) = batch_rates(ctx, call, None) {
                return Ok(Built::Series {
                    series,
                    metric_name: Some(metric_name),
                    metric_name_for_error: Some(metric_name.to_string()),
                });
            }
            // Grid needs at least one step of lookback to bracket the first
            // interval's left edge, regardless of the (inert) query range.
            let lookback = match ctx.rate_mode {
                RateMode::Grid => range_ns.max(ctx.step_ns),
                RateMode::Raw => range_ns,
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
            let series: SeriesSet<'a> = streams
                .into_iter()
                .map(|stream| match ctx.rate_mode {
                    RateMode::Grid => {
                        let rate = CounterGridRate::from_stream(
                            stream.samples,
                            stream.windowed,
                            ctx.start_ns,
                            ctx.end_ns,
                            ctx.step_ns,
                            // Wider than the step only when the caller
                            // asked for smoothing (a cross-cadence query):
                            // points stay on the grid, each value averages
                            // over more.
                            ctx.rate_span_ns.unwrap_or(ctx.step_ns),
                        );
                        // Explicit timestamps override the grid entirely —
                        // placement AND averaging window both come from
                        // them, so a value lands on a slow source's real
                        // reading instead of being interpolated to a grid
                        // point that source never observed.
                        match &ctx.eval_timestamps {
                            Some(points) => {
                                LabeledSeries::new(stream.labels, rate.at_points(points.clone()))
                            }
                            None => LabeledSeries::new(stream.labels, rate),
                        }
                    }
                    RateMode::Raw => LabeledSeries::new(
                        stream.labels,
                        CounterPairwiseRate::from_stream(stream.samples, ctx.start_ns, ctx.end_ns),
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
            // Try gauge path first; fall back to counter 2nd-derivative.
            let deriv_data_start = ctx.start_ns.saturating_sub(ctx.step_ns.saturating_mul(2));
            if let Some(gauges) =
                ctx.source
                    .gauges(metric_name, &filter, deriv_data_start, ctx.end_ns)
            {
                let series: SeriesSet<'a> = gauges
                    .series
                    .into_iter()
                    .map(|g| {
                        let producer = ctx.place(GaugeDeriv::new(
                            g.timestamps,
                            g.values,
                            ctx.start_ns,
                            ctx.end_ns,
                            ctx.step_ns,
                            matches!(ctx.rate_mode, RateMode::Raw),
                        ));
                        LabeledSeries::new(g.labels, producer)
                    })
                    .collect();
                return Ok(Built::Series {
                    series,
                    metric_name: Some("deriv"),
                    metric_name_for_error: Some(metric_name.to_string()),
                });
            }
            let Some(counters) =
                ctx.source
                    .counters(metric_name, &filter, deriv_data_start, ctx.end_ns)
            else {
                nothing_read(ctx.source, metric_name, MetricKind::Counter)?;
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
            let series: SeriesSet<'a> = counters
                .series
                .into_iter()
                .map(|c| {
                    // deriv wraps the pairwise rate in StreamingDeriv, which does
                    // its own windowing and needs the pre-start lookback, so no
                    // start bound here (0).
                    let rate_iter = CounterPairwiseRate::new(c.timestamps, c.values, 0, ctx.end_ns);
                    LabeledSeries::new(
                        c.labels,
                        StreamingDeriv::new(rate_iter, ctx.start_ns, ctx.end_ns, ctx.step_ns),
                    )
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
    let Some(stream) = ctx
        .source
        .histogram_stream(metric_name, &filter, ctx.start_ns, ctx.end_ns)
    else {
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
    let metric_name = sel
        .name
        .as_deref()
        .ok_or_else(|| QueryError::ParseError("Vector selector missing name".to_string()))?;
    let filter = extract_filter_labels(&sel.matchers.matchers);

    let staleness_ns = ctx.step_ns.max(ctx.interval_ns);
    let data_start = ctx.start_ns.saturating_sub(staleness_ns);

    let Some(gauges) = ctx
        .source
        .gauges(metric_name, &filter, data_start, ctx.end_ns)
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

    let series: SeriesSet<'a> = gauges
        .series
        .into_iter()
        .map(|g| {
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
        .collect();
    Ok(Built::Series {
        series,
        metric_name: Some(metric_name),
        metric_name_for_error: Some(metric_name.to_string()),
    })
}
