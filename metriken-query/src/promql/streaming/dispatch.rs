//! AST → streaming-pipeline dispatcher.
//!
//! Recursively walks the parsed PromQL expression and assembles a
//! producer/aggregator pipeline for any recognised shape. Any node
//! the dispatcher doesn't yet cover causes the whole query to fall
//! back to the eager evaluator transparently.
//!
//! Currently recognised:
//!
//! * `metric{matchers}` — gauge VectorSelector at step grid.
//! * `irate(metric[range])` / `rate(metric[range])` — counter
//!   producers.
//! * `avg_over_time(metric[range])` / `idelta(metric[range])` /
//!   `deriv(metric[range])` (gauges only) — gauge producers.
//! * `sum/avg/min/max/count [by | without (..)] (...)` — aggregator
//!   over any of the above.
//! * `lhs OP rhs` where OP ∈ {`+`, `-`, `*`, `/`} — binary ops
//!   between two series sets, or between a series set and a number
//!   literal. Honors `on(..)` and `ignoring(..)` matching modifiers.
//!   Falls back to eager when the eager engine's single-right
//!   fallback is needed (matcher-less, right-side single-series
//!   broadcast) or when `group_left`/`group_right` is requested.
//! * `NumberLiteral` — produces a `Built::Scalar`; usable on either
//!   side of a binary op.
//! * Parenthesised expressions are unwrapped.

use promql_parser::parser::{self, Expr};

use crate::promql::extract_filter_labels;
use crate::promql::streaming::{
    aggregate, collect_to_matrix, gauges_avg_over_time, gauges_deriv, gauges_idelta,
    gauges_step_grid, irate_counters, matrix_matrix_op, matrix_scalar_op, rate_counters, AggOp,
    BinOp, CounterPairwiseRate, GroupBy, LabeledSeries, MatchSpec, SeriesSet, StreamingDeriv,
};
use crate::promql::{QueryError, QueryResult};
use crate::tsdb::Tsdb;

/// Try to evaluate `expr` via the streaming pipeline.
///
/// * `Ok(Some(result))` — the dispatcher matched a known shape and
///   produced a fully-materialised result.
/// * `Ok(None)` — shape not (yet) covered; caller must fall back.
/// * `Err(_)` — genuine evaluation error.
pub fn try_streaming(
    tsdb: &Tsdb,
    expr: &Expr,
    start: f64,
    end: f64,
    step: f64,
) -> Result<Option<QueryResult>, QueryError> {
    let start_ns = (start * 1e9) as u64;
    let end_ns = (end * 1e9) as u64;
    let step_ns = (step * 1e9) as u64;
    let interval_ns = (tsdb.interval() * 1e9) as u64;

    let ctx = Ctx {
        tsdb,
        start_ns,
        end_ns,
        step_ns,
        interval_ns,
    };

    let Some(built) = build(&ctx, expr)? else {
        return Ok(None);
    };
    match built {
        Built::Series {
            series,
            metric_name,
            metric_name_for_error,
        } => {
            let result = collect_to_matrix(series, metric_name);
            if result.is_empty() {
                // The eager path returns MetricNotFound when an
                // aggregation produces nothing or a metric isn't
                // present; mirror that so higher layers see
                // consistent error shapes regardless of which path
                // handled the query.
                if let Some(name) = metric_name_for_error {
                    return Err(QueryError::MetricNotFound(name));
                }
                return Ok(Some(QueryResult::Matrix { result: vec![] }));
            }
            Ok(Some(QueryResult::Matrix { result }))
        }
        Built::Scalar(v) => Ok(Some(QueryResult::Scalar { result: (start, v) })),
    }
}

struct Ctx<'a> {
    tsdb: &'a Tsdb,
    start_ns: u64,
    end_ns: u64,
    step_ns: u64,
    interval_ns: u64,
}

/// One step of recursion. The `Series` variant carries the lazy
/// iterator chain plus optional `__name__` plumbing for the boundary
/// collector and `MetricNotFound` shaping. `Scalar` is the AST
/// `NumberLiteral` representation so binary ops can take a number on
/// either side without going through a degenerate single-tick series.
enum Built<'a> {
    Series {
        series: SeriesSet<'a>,
        metric_name: Option<&'a str>,
        metric_name_for_error: Option<String>,
    },
    Scalar(f64),
}

fn build<'a, 'expr>(ctx: &'a Ctx<'a>, expr: &'expr Expr) -> Result<Option<Built<'a>>, QueryError>
where
    'expr: 'a,
{
    match expr {
        Expr::Paren(p) => build(ctx, &p.expr),
        Expr::Aggregate(agg) => build_aggregate(ctx, agg),
        Expr::Call(call) => build_call(ctx, call),
        Expr::VectorSelector(sel) => build_vector_selector(ctx, sel),
        Expr::Binary(bin) => build_binary(ctx, bin),
        Expr::NumberLiteral(num) => Ok(Some(Built::Scalar(num.val))),
        // Matrix selectors as standalone don't produce a meaningful
        // result (the eager path errors here); leave to the eager
        // engine to keep error shapes consistent.
        Expr::MatrixSelector(_) => Ok(None),
        // Unary ops, subqueries: not yet streamed.
        _ => Ok(None),
    }
}

fn build_aggregate<'a, 'expr>(
    ctx: &'a Ctx<'a>,
    agg: &'expr parser::AggregateExpr,
) -> Result<Option<Built<'a>>, QueryError>
where
    'expr: 'a,
{
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
        Some(parser::LabelModifier::Include(ls)) => GroupBy::Include(ls.labels.as_slice()),
        Some(parser::LabelModifier::Exclude(ls)) => GroupBy::Exclude(ls.labels.as_slice()),
    };

    // The inner expression must be a series — aggregating over a
    // bare scalar isn't meaningful (the eager engine returns the
    // scalar unchanged in that case, but the streaming dispatcher
    // can hand off to eager rather than reproduce the special case).
    let Some(Built::Series {
        series: inner_series,
        metric_name_for_error,
        ..
    }) = build(ctx, &agg.expr)?
    else {
        return Ok(None);
    };
    let series = aggregate(inner_series, op, group_by);
    Ok(Some(Built::Series {
        series,
        metric_name: None,
        metric_name_for_error,
    }))
}

fn build_binary<'a, 'expr>(
    ctx: &'a Ctx<'a>,
    bin: &'expr parser::BinaryExpr,
) -> Result<Option<Built<'a>>, QueryError>
where
    'expr: 'a,
{
    let Some(op) = BinOp::from_token(&bin.op) else {
        return Ok(None);
    };

    // Group-left / group-right (one-to-many) needs an iterator-tee
    // mechanism that defeats streaming; fall through to eager.
    if let Some(modifier) = &bin.modifier {
        if modifier.card != parser::VectorMatchCardinality::OneToOne {
            return Ok(None);
        }
    }

    // Translate the optional `on(..)` / `ignoring(..)` modifier into
    // the streaming pipeline's `MatchSpec`.
    let spec = match bin.modifier.as_ref().and_then(|m| m.matching.as_ref()) {
        None => MatchSpec::Default,
        Some(parser::LabelModifier::Include(ls)) => MatchSpec::Include(ls.labels.as_slice()),
        Some(parser::LabelModifier::Exclude(ls)) => MatchSpec::Exclude(ls.labels.as_slice()),
    };

    let lhs = build(ctx, &bin.lhs)?;
    let rhs = build(ctx, &bin.rhs)?;
    let (Some(lhs), Some(rhs)) = (lhs, rhs) else {
        return Ok(None);
    };

    // For binary ops, an empty result means "no matching label
    // pairs" or "all points filtered (e.g. divide-by-zero)" — both
    // of which the eager engine returns as `Ok(empty matrix)` not
    // `Err(MetricNotFound)`. Drop `metric_name_for_error` here so
    // the boundary collector doesn't promote the empty case to an
    // error.
    match (lhs, rhs) {
        (Built::Series { series, .. }, Built::Scalar(s)) => {
            let result = matrix_scalar_op(series, op, s, false);
            Ok(Some(Built::Series {
                series: result,
                metric_name: None,
                metric_name_for_error: None,
            }))
        }
        (Built::Scalar(s), Built::Series { series, .. }) => {
            let result = matrix_scalar_op(series, op, s, true);
            Ok(Some(Built::Series {
                series: result,
                metric_name: None,
                metric_name_for_error: None,
            }))
        }
        (
            Built::Series {
                series: left_series,
                ..
            },
            Built::Series {
                series: right_series,
                ..
            },
        ) => {
            // Returns `None` when the eager single-right fallback
            // is needed; defer to eager in that case.
            let Some(joined) = matrix_matrix_op(left_series, right_series, op, spec) else {
                return Ok(None);
            };
            Ok(Some(Built::Series {
                series: joined,
                metric_name: None,
                metric_name_for_error: None,
            }))
        }
        (Built::Scalar(a), Built::Scalar(b)) => {
            // Both literals — compute eagerly. Realistically never
            // hits in dashboards but cheap to handle.
            let v = op.apply(a, b).unwrap_or(f64::NAN);
            Ok(Some(Built::Scalar(v)))
        }
    }
}

fn build_call<'a, 'expr>(
    ctx: &'a Ctx<'a>,
    call: &'expr parser::Call,
) -> Result<Option<Built<'a>>, QueryError>
where
    'expr: 'a,
{
    // All calls we recognise take a single matrix-selector argument.
    let Some(first) = call.args.args.first() else {
        return Ok(None);
    };
    let Expr::MatrixSelector(sel) = &**first else {
        return Ok(None);
    };
    let metric_name = sel
        .vs
        .name
        .as_deref()
        .ok_or_else(|| QueryError::ParseError("Matrix selector missing name".to_string()))?;
    let filter = extract_filter_labels(&sel.vs.matchers.matchers);
    let range_ns = sel.range.as_nanos() as u64;

    match call.func.name {
        "irate" => {
            let Some(collection) = ctx.tsdb.counters_ref(metric_name) else {
                return Err(QueryError::MetricNotFound(metric_name.to_string()));
            };
            let series = irate_counters(
                collection,
                &filter,
                ctx.start_ns,
                ctx.end_ns,
                ctx.step_ns,
                range_ns,
            );
            Ok(Some(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            }))
        }
        "rate" => {
            let Some(collection) = ctx.tsdb.counters_ref(metric_name) else {
                return Err(QueryError::MetricNotFound(metric_name.to_string()));
            };
            let series = rate_counters(
                collection,
                &filter,
                ctx.start_ns,
                ctx.end_ns,
                ctx.step_ns,
                range_ns,
            );
            Ok(Some(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            }))
        }
        "avg_over_time" => {
            let Some(collection) = ctx.tsdb.gauges_ref(metric_name) else {
                return Err(QueryError::MetricNotFound(metric_name.to_string()));
            };
            let series = gauges_avg_over_time(
                collection,
                &filter,
                ctx.start_ns,
                ctx.end_ns,
                ctx.step_ns,
                range_ns,
            );
            Ok(Some(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            }))
        }
        "idelta" => {
            let Some(collection) = ctx.tsdb.gauges_ref(metric_name) else {
                return Err(QueryError::MetricNotFound(metric_name.to_string()));
            };
            let series = gauges_idelta(
                collection,
                &filter,
                ctx.start_ns,
                ctx.end_ns,
                ctx.step_ns,
                range_ns,
            );
            Ok(Some(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            }))
        }
        "deriv" => {
            // Gauge: random-access slope over the source slice
            // (cheap, see `gauges_deriv`). Counter (2nd-derivative
            // case): chain a pair-wise rate producer into the
            // generic streaming deriv consumer.
            //
            // Both paths relabel `__name__` to literally "deriv"
            // rather than preserving the source metric name —
            // matching the eager engine's historical behaviour so
            // JSON output is byte-for-byte identical.
            if let Some(collection) = ctx.tsdb.gauges_ref(metric_name) {
                let series =
                    gauges_deriv(collection, &filter, ctx.start_ns, ctx.end_ns, ctx.step_ns);
                return Ok(Some(Built::Series {
                    series,
                    metric_name: Some("deriv"),
                    metric_name_for_error: Some(metric_name.to_string()),
                }));
            }
            let Some(collection) = ctx.tsdb.counters_ref(metric_name) else {
                return Err(QueryError::MetricNotFound(metric_name.to_string()));
            };
            let mut out: SeriesSet<'_> = Vec::new();
            for (labels, series) in collection.iter() {
                if !filter.inner.is_empty() && !labels.matches(&filter) {
                    continue;
                }
                let rate_iter = CounterPairwiseRate::new(series.samples(), ctx.end_ns);
                let deriv_iter =
                    StreamingDeriv::new(rate_iter, ctx.start_ns, ctx.end_ns, ctx.step_ns);
                out.push(LabeledSeries::new(labels.clone(), deriv_iter));
            }
            Ok(Some(Built::Series {
                series: out,
                metric_name: Some("deriv"),
                metric_name_for_error: Some(metric_name.to_string()),
            }))
        }
        // Anything else (histogram_*, scalar, vector, ...) stays on
        // the eager path.
        _ => Ok(None),
    }
}

fn build_vector_selector<'a, 'expr>(
    ctx: &'a Ctx<'a>,
    sel: &'expr parser::VectorSelector,
) -> Result<Option<Built<'a>>, QueryError>
where
    'expr: 'a,
{
    let metric_name = sel
        .name
        .as_deref()
        .ok_or_else(|| QueryError::ParseError("Vector selector missing name".to_string()))?;
    let filter = extract_filter_labels(&sel.matchers.matchers);

    // Bare counter selectors aren't meaningful in PromQL (rate/irate
    // are required). Defer to the eager path, which returns
    // MetricNotFound consistently.
    let Some(collection) = ctx.tsdb.gauges_ref(metric_name) else {
        return Ok(None);
    };

    // Mirror the eager engine's staleness rule: at least the step
    // size, but not less than the file's sampling interval, so a
    // coarse step doesn't drop nearby samples.
    let staleness_ns = ctx.step_ns.max(ctx.interval_ns);

    let series = gauges_step_grid(
        collection,
        &filter,
        ctx.start_ns,
        ctx.end_ns,
        ctx.step_ns,
        staleness_ns,
    );
    Ok(Some(Built::Series {
        series,
        metric_name: Some(metric_name),
        metric_name_for_error: Some(metric_name.to_string()),
    }))
}
