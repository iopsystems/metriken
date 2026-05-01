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
//! * `avg_over_time(metric[range])` / `idelta(metric[range])` —
//!   gauge producers.
//! * `sum/avg/min/max/count [by | without (..)] (...)` — aggregator
//!   over any of the above.
//! * Parenthesised expressions are unwrapped.
//!
//! Histograms and binary operators stay on the eager path; they
//! either don't fit the streaming shape (heatmaps are inherently 2D)
//! or are deferred to a follow-up.

use promql_parser::parser::{self, Expr};

use crate::promql::extract_filter_labels;
use crate::promql::streaming::{
    aggregate, collect_to_matrix, gauges_avg_over_time, gauges_idelta, gauges_step_grid,
    irate_counters, rate_counters, AggOp, GroupBy, SeriesSet,
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
    let result = collect_to_matrix(built.series, built.metric_name);
    if result.is_empty() {
        // The eager path returns MetricNotFound when an aggregation
        // produces nothing or a metric isn't present; mirror that so
        // higher layers see consistent error shapes regardless of
        // which path handled the query.
        if let Some(name) = built.metric_name_for_error {
            return Err(QueryError::MetricNotFound(name));
        }
        return Ok(Some(QueryResult::Matrix { result: vec![] }));
    }
    Ok(Some(QueryResult::Matrix { result }))
}

struct Ctx<'a> {
    tsdb: &'a Tsdb,
    start_ns: u64,
    end_ns: u64,
    step_ns: u64,
    interval_ns: u64,
}

/// One step of recursion. `series` carries the lazy iterator chain;
/// `metric_name` is `Some` for raw producers (so the boundary
/// collector adds `__name__`) and `None` for aggregated results
/// (matching the eager engine's behaviour). `metric_name_for_error`
/// is the metric this stage was rooted in, used to produce a
/// meaningful `MetricNotFound` when the result ends up empty.
struct Built<'a> {
    series: SeriesSet<'a>,
    metric_name: Option<&'a str>,
    metric_name_for_error: Option<String>,
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
        // Matrix selectors as standalone don't produce a meaningful
        // result (the eager path errors here); leave to the eager
        // engine to keep error shapes consistent.
        Expr::MatrixSelector(_) => Ok(None),
        // Numbers, binary ops, unary ops, subqueries: not yet
        // streamed.
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

    let Some(inner) = build(ctx, &agg.expr)? else {
        return Ok(None);
    };
    let series = aggregate(inner.series, op, group_by);
    Ok(Some(Built {
        series,
        metric_name: None,
        metric_name_for_error: inner.metric_name_for_error,
    }))
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
            Ok(Some(Built {
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
            Ok(Some(Built {
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
            Ok(Some(Built {
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
            Ok(Some(Built {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            }))
        }
        // Anything else (deriv, histogram_*, scalar, vector, ...)
        // stays on the eager path.
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
    Ok(Some(Built {
        series,
        metric_name: Some(metric_name),
        metric_name_for_error: Some(metric_name.to_string()),
    }))
}
