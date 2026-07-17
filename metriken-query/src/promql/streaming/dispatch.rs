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
    aggregate, collect_to_matrix, interval_binop, matrix_matrix_op, matrix_scalar_op, AggOp, BinOp,
    CounterIrate, CounterPairwiseRate, CounterRate, GaugeAvgOverTime, GaugeDeriv, GaugeIdelta,
    GaugeStepGrid, GroupBy, LabeledSeries, MatchSpec, SeriesSet, StreamingDeriv,
};
use crate::promql::{MatrixSample, QueryError, QueryResult};
use crate::DataSource;

/// Evaluate `expr` via the streaming pipeline. Returns
/// `QueryError::Unsupported` for any AST shape the dispatcher doesn't
/// recognise; this is now the only PromQL evaluator in the engine.
pub fn try_streaming(
    source: &dyn DataSource,
    expr: &Expr,
    start: f64,
    end: f64,
    step: f64,
) -> Result<QueryResult, QueryError> {
    let ctx = Ctx {
        source,
        start_ns: (start * 1e9) as u64,
        end_ns: (end * 1e9) as u64,
        step_ns: (step * 1e9) as u64,
        interval_ns: (source.interval() * 1e9) as u64,
    };

    let result = match build(&ctx, expr)? {
        Built::Series {
            series,
            metric_name,
            metric_name_for_error,
        } => {
            let collected = collect_to_matrix(series, metric_name);
            if collected.is_empty() {
                if let Some(name) = metric_name_for_error {
                    return Err(QueryError::MetricNotFound(name));
                }
                QueryResult::Matrix { result: vec![] }
            } else {
                QueryResult::Matrix { result: collected }
            }
        }
        Built::Materialized { result, name } => {
            if result.is_empty() {
                return Err(QueryError::MetricNotFound(name));
            }
            QueryResult::Matrix { result }
        }
        Built::Scalar(v) => QueryResult::Scalar { result: (start, v) },
    };
    Ok(result)
}

struct Ctx<'a> {
    source: &'a dyn DataSource,
    start_ns: u64,
    end_ns: u64,
    step_ns: u64,
    interval_ns: u64,
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
        "irate" => {
            let counters = ctx
                .source
                .counters(metric_name, &filter, data_start, ctx.end_ns)
                .ok_or_else(|| QueryError::MetricNotFound(metric_name.to_string()))?;
            let series: SeriesSet<'a> = counters
                .series
                .into_iter()
                .map(|c| {
                    let pts: Vec<_> = CounterIrate::new(
                        &c.timestamps,
                        &c.values,
                        ctx.start_ns,
                        ctx.end_ns,
                        ctx.step_ns,
                        range_ns,
                        c.windows.as_deref(),
                    )
                    .collect();
                    LabeledSeries::new(c.labels, pts.into_iter())
                })
                .collect();
            Ok(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            })
        }
        "rate" => {
            let counters = ctx
                .source
                .counters(metric_name, &filter, data_start, ctx.end_ns)
                .ok_or_else(|| QueryError::MetricNotFound(metric_name.to_string()))?;
            let series: SeriesSet<'a> = counters
                .series
                .into_iter()
                .map(|c| {
                    let pts: Vec<_> = CounterRate::new(
                        &c.timestamps,
                        &c.values,
                        ctx.start_ns,
                        ctx.end_ns,
                        ctx.step_ns,
                        range_ns,
                        c.windows.as_deref(),
                    )
                    .collect();
                    LabeledSeries::new(c.labels, pts.into_iter())
                })
                .collect();
            Ok(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            })
        }
        "avg_over_time" => {
            let gauges = ctx
                .source
                .gauges(metric_name, &filter, data_start, ctx.end_ns)
                .ok_or_else(|| QueryError::MetricNotFound(metric_name.to_string()))?;
            let series: SeriesSet<'a> = gauges
                .series
                .into_iter()
                .map(|g| {
                    let pts: Vec<_> = GaugeAvgOverTime::new(
                        &g.timestamps,
                        &g.values,
                        ctx.start_ns,
                        ctx.end_ns,
                        ctx.step_ns,
                        range_ns,
                    )
                    .collect();
                    LabeledSeries::new(g.labels, pts.into_iter())
                })
                .collect();
            Ok(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            })
        }
        "idelta" => {
            let gauges = ctx
                .source
                .gauges(metric_name, &filter, data_start, ctx.end_ns)
                .ok_or_else(|| QueryError::MetricNotFound(metric_name.to_string()))?;
            let series: SeriesSet<'a> = gauges
                .series
                .into_iter()
                .map(|g| {
                    let pts: Vec<_> = GaugeIdelta::new(
                        &g.timestamps,
                        &g.values,
                        ctx.start_ns,
                        ctx.end_ns,
                        ctx.step_ns,
                        range_ns,
                    )
                    .collect();
                    LabeledSeries::new(g.labels, pts.into_iter())
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
                        let pts: Vec<_> = GaugeDeriv::new(
                            &g.timestamps,
                            &g.values,
                            ctx.start_ns,
                            ctx.end_ns,
                            ctx.step_ns,
                        )
                        .collect();
                        LabeledSeries::new(g.labels, pts.into_iter())
                    })
                    .collect();
                return Ok(Built::Series {
                    series,
                    metric_name: Some("deriv"),
                    metric_name_for_error: Some(metric_name.to_string()),
                });
            }
            let counters = ctx
                .source
                .counters(metric_name, &filter, deriv_data_start, ctx.end_ns)
                .ok_or_else(|| QueryError::MetricNotFound(metric_name.to_string()))?;
            let series: SeriesSet<'a> = counters
                .series
                .into_iter()
                .map(|c| {
                    let rate_iter = CounterPairwiseRate::new(&c.timestamps, &c.values, ctx.end_ns);
                    let pts: Vec<_> =
                        StreamingDeriv::new(rate_iter, ctx.start_ns, ctx.end_ns, ctx.step_ns)
                            .collect();
                    LabeledSeries::new(c.labels, pts.into_iter())
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
    let stream = ctx
        .source
        .histogram_stream(metric_name, &filter, ctx.start_ns, ctx.end_ns)
        .ok_or_else(|| QueryError::MetricNotFound(metric_name.to_string()))?;
    let result = stream.quantiles(&[quantile], ctx.start_ns, ctx.end_ns, None, metric_name);
    Ok(Built::Materialized {
        result,
        name: format!("No histogram data found for {metric_name}"),
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

    let gauges = ctx
        .source
        .gauges(metric_name, &filter, data_start, ctx.end_ns)
        .ok_or_else(|| QueryError::MetricNotFound(metric_name.to_string()))?;

    let series: SeriesSet<'a> = gauges
        .series
        .into_iter()
        .map(|g| {
            let pts: Vec<_> = GaugeStepGrid::new(
                &g.timestamps,
                &g.values,
                ctx.start_ns,
                ctx.end_ns,
                ctx.step_ns,
                staleness_ns,
            )
            .collect();
            LabeledSeries::new(g.labels, pts.into_iter())
        })
        .collect();
    Ok(Built::Series {
        series,
        metric_name: Some(metric_name),
        metric_name_for_error: Some(metric_name.to_string()),
    })
}
