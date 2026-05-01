//! AST → streaming-pipeline dispatcher.
//!
//! Pattern-matches the parsed PromQL expression against the shapes
//! the streaming pipeline currently supports and, when one matches,
//! builds and drains the iterator chain. Returns `Ok(None)` for
//! shapes that aren't yet covered so the caller can fall back to the
//! eager evaluator transparently.
//!
//! Currently supported:
//!
//! * `sum [by (label, ...)] (irate(metric{matchers}[range]))` —
//!   the dominant shape of rezolus dashboard queries.

use promql_parser::parser::{self, Expr};

use crate::promql::extract_filter_labels;
use crate::promql::streaming::{collect_to_matrix, irate_counters, sum_by};
use crate::promql::{QueryError, QueryResult};
use crate::tsdb::{Labels, Tsdb};

/// Try to evaluate `expr` via the streaming pipeline.
///
/// * `Ok(Some(result))` — the dispatcher matched a known shape and
///   produced a fully-materialised result.
/// * `Ok(None)` — shape not (yet) covered; caller must fall back.
/// * `Err(_)` — genuine evaluation error (e.g. metric not found in
///   a context where the eager path would also error).
pub fn try_streaming(
    tsdb: &Tsdb,
    expr: &Expr,
    start: f64,
    end: f64,
    step: f64,
) -> Result<Option<QueryResult>, QueryError> {
    // Shape 1: sum [by (..)] (irate(metric[range]))
    if let Some(result) = try_sum_irate(tsdb, expr, start, end, step)? {
        return Ok(Some(result));
    }
    Ok(None)
}

/// Match `sum [by (labels)] (irate(metric{matchers}[range]))` and
/// route through `irate_counters` → `sum_by` → `collect_to_matrix`.
fn try_sum_irate(
    tsdb: &Tsdb,
    expr: &Expr,
    start: f64,
    end: f64,
    step: f64,
) -> Result<Option<QueryResult>, QueryError> {
    let Expr::Aggregate(agg) = expr else {
        return Ok(None);
    };
    if agg.op.to_string() != "sum" {
        return Ok(None);
    }
    let by_labels: Vec<String> = match &agg.modifier {
        // Plain `sum(...)` — fold into a single group.
        None => Vec::new(),
        Some(parser::LabelModifier::Include(ls)) => ls.labels.clone(),
        // `sum without (..)` would need a "drop these labels, keep
        // the rest" group key; not handled by the streaming aggregator
        // yet, so let the eager path take it.
        Some(parser::LabelModifier::Exclude(_)) => return Ok(None),
    };

    let Expr::Call(call) = &*agg.expr else {
        return Ok(None);
    };
    if call.func.name != "irate" {
        return Ok(None);
    }

    let Some(first_arg) = call.args.args.first() else {
        return Ok(None);
    };
    let Expr::MatrixSelector(selector) = &**first_arg else {
        return Ok(None);
    };

    let metric_name = selector
        .vs
        .name
        .as_deref()
        .ok_or_else(|| QueryError::ParseError("Matrix selector missing name".to_string()))?;
    let filter_labels = extract_filter_labels(&selector.vs.matchers.matchers);
    let range_ns = selector.range.as_nanos() as u64;

    // The streaming `irate_counters` filters internally, but
    // `tsdb.counters()` already accepts a label filter — passing
    // `Labels::default()` keeps the full collection so the iterator
    // borrows stable references for the lifetime of this call.
    let Some(collection) = tsdb.counters(metric_name, Labels::default()) else {
        return Err(QueryError::MetricNotFound(metric_name.to_string()));
    };

    let start_ns = (start * 1e9) as u64;
    let end_ns = (end * 1e9) as u64;
    let step_ns = (step * 1e9) as u64;

    let stream = irate_counters(
        &collection,
        &filter_labels,
        start_ns,
        end_ns,
        step_ns,
        range_ns,
    );
    let summed = sum_by(stream, &by_labels);
    // Aggregated result: strip `__name__`, matching the eager
    // `handle_aggregate` behaviour (which drops every non-by label,
    // `__name__` included).
    let result = collect_to_matrix(summed, None);

    if result.is_empty() {
        return Err(QueryError::MetricNotFound(metric_name.to_string()));
    }

    Ok(Some(QueryResult::Matrix { result }))
}
