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
//!
//! `histogram_quantiles([qs], metric)` and `histogram_heatmap(metric)`
//! aren't standard PromQL (array literals / multi-output topology)
//! and are pre-parsed as raw query strings inside `query_range`
//! before the AST parser runs.

use promql_parser::parser::{self, Expr};

use crate::promql::extract_filter_labels;
use crate::promql::streaming::{
    aggregate, collect_to_matrix, gauges_avg_over_time, gauges_deriv, gauges_idelta,
    gauges_step_grid, irate_counters, matrix_matrix_op, matrix_scalar_op, rate_counters, AggOp,
    BinOp, CounterPairwiseRate, GroupBy, LabeledSeries, MatchSpec, SeriesSet, StreamingDeriv,
};
use crate::promql::{streaming, MatrixSample, QueryError, QueryResult};
use crate::tsdb::{Labels, Tsdb};

/// Evaluate `expr` via the streaming pipeline. Returns
/// `QueryError::Unsupported` for any AST shape the dispatcher doesn't
/// recognise; this is now the only PromQL evaluator in the engine.
pub fn try_streaming(
    tsdb: &Tsdb,
    expr: &Expr,
    start: f64,
    end: f64,
    step: f64,
) -> Result<QueryResult, QueryError> {
    let ctx = Ctx {
        tsdb,
        start_ns: (start * 1e9) as u64,
        end_ns: (end * 1e9) as u64,
        step_ns: (step * 1e9) as u64,
        interval_ns: (tsdb.interval() * 1e9) as u64,
    };

    let result = match build(&ctx, expr)? {
        Built::Series {
            series,
            metric_name,
            metric_name_for_error: _,
        } => {
            // An empty result here means the metric exists in the TSDB
            // (the upstream `counters_ref`/`gauges_ref`/`histograms_ref`
            // checks already errored out for genuinely-missing metrics)
            // but the label predicates filtered every series out. Per
            // Prometheus semantics that's an empty matrix, not an error.
            // Conflating the two surfaces as `Metric not found` for
            // viewer queries like `softirq{kind="block"}` against fixtures
            // that have softirq but no block kind, which the SQL backend
            // (correctly) returns as empty.
            let collected = collect_to_matrix(series, metric_name);
            QueryResult::Matrix { result: collected }
        }
        Built::Materialized { result } => QueryResult::Matrix { result },
        Built::Scalar(v) => QueryResult::Scalar { result: (start, v) },
    };
    Ok(result)
}

struct Ctx<'a> {
    tsdb: &'a Tsdb,
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
    Materialized { result: Vec<MatrixSample> },
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

    // Aggregating a non-series (e.g. a bare scalar) is well-defined
    // in PromQL but never useful in real dashboards; reject rather
    // than maintaining the eager passthrough.
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
        // Materialized × anything: histogram_quantile output is a
        // single ready-made series; mixing it into a binary op is
        // legal in PromQL but never used in rezolus dashboards.
        // Reject explicitly rather than hand-rolling the wrapper.
        _ => Err(QueryError::Unsupported(
            "binary op against a histogram_quantile result not supported".to_string(),
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
    // `histogram_quantile(q, metric)` has a different arg shape
    // (number literal + vector selector) from the windowed-call
    // family below, so handle it up front.
    if call.func.name == "histogram_quantile" {
        return build_histogram_quantile(ctx, call);
    }

    // Every other recognised function takes a single matrix-selector
    // argument: `func(metric[range])`.
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

    match call.func.name {
        "irate" => {
            let Some(collection) = ctx.tsdb.counters_ref(metric_name) else {
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
            let series = irate_counters(
                collection,
                &filter,
                ctx.start_ns,
                ctx.end_ns,
                ctx.step_ns,
                range_ns,
            );
            Ok(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            })
        }
        "rate" => {
            let Some(collection) = ctx.tsdb.counters_ref(metric_name) else {
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
            let series = rate_counters(
                collection,
                &filter,
                ctx.start_ns,
                ctx.end_ns,
                ctx.step_ns,
                range_ns,
            );
            Ok(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            })
        }
        "avg_over_time" => {
            let Some(collection) = ctx.tsdb.gauges_ref(metric_name) else {
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
            let series = gauges_avg_over_time(
                collection,
                &filter,
                ctx.start_ns,
                ctx.end_ns,
                ctx.step_ns,
                range_ns,
            );
            Ok(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            })
        }
        "idelta" => {
            let Some(collection) = ctx.tsdb.gauges_ref(metric_name) else {
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
            };
            let series = gauges_idelta(
                collection,
                &filter,
                ctx.start_ns,
                ctx.end_ns,
                ctx.step_ns,
                range_ns,
            );
            Ok(Built::Series {
                series,
                metric_name: Some(metric_name),
                metric_name_for_error: Some(metric_name.to_string()),
            })
        }
        "deriv" => {
            // Gauge: random-access slope over the source slice.
            // Counter (2nd-derivative case): chain a pair-wise rate
            // producer into the generic streaming deriv consumer.
            //
            // Both paths relabel `__name__` to literally "deriv"
            // rather than preserving the source metric name —
            // matching the historical PromQL behaviour.
            if let Some(collection) = ctx.tsdb.gauges_ref(metric_name) {
                let series =
                    gauges_deriv(collection, &filter, ctx.start_ns, ctx.end_ns, ctx.step_ns);
                return Ok(Built::Series {
                    series,
                    metric_name: Some("deriv"),
                    metric_name_for_error: Some(metric_name.to_string()),
                });
            }
            let Some(collection) = ctx.tsdb.counters_ref(metric_name) else {
                return Ok(Built::Series {
                    series: Vec::new(),
                    metric_name: Some(metric_name),
                    metric_name_for_error: None,
                });
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
            Ok(Built::Series {
                series: out,
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
/// of the histogram quantile pipeline. Output is one `MatrixSample`
/// labeled `{__name__: metric, quantile: q}`.
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
    let Some(collection) = ctx.tsdb.histograms_ref(metric_name) else {
        // Match Prometheus + the SQL backend: missing metric → empty
        // matrix, not an error. This is the histogram_quantile path.
        return Ok(Built::Materialized { result: Vec::new() });
    };
    let result = streaming::histogram::quantiles(
        collection,
        &Labels::default(),
        &[quantile],
        ctx.start_ns,
        ctx.end_ns,
        None,
        metric_name,
    );
    Ok(Built::Materialized { result })
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

    // Bare gauge selector. If the metric isn't present, return an empty
    // matrix (matching Prometheus + the SQL backend) rather than an
    // error — important for viewer queries that ask for metrics that
    // exist on some fixtures but not others (the dashboard renders an
    // empty panel rather than popping an error).
    let Some(collection) = ctx.tsdb.gauges_ref(metric_name) else {
        return Ok(Built::Series {
            series: Vec::new(),
            metric_name: Some(metric_name),
            metric_name_for_error: None,
        });
    };

    // Mirror the historical staleness rule: at least the step size,
    // but not less than the file's sampling interval.
    let staleness_ns = ctx.step_ns.max(ctx.interval_ns);

    let series = gauges_step_grid(
        collection,
        &filter,
        ctx.start_ns,
        ctx.end_ns,
        ctx.step_ns,
        staleness_ns,
    );
    Ok(Built::Series {
        series,
        metric_name: Some(metric_name),
        metric_name_for_error: Some(metric_name.to_string()),
    })
}
