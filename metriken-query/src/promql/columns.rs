//! Resolve a PromQL query to the set of physical parquet columns it
//! touches — without reading any values. Walks the parsed AST the
//! same way `streaming::dispatch` does, but looks each selector up
//! in the unified column map instead of fetching series data.
//!
//! Metric names are unique across types in the exposition format, so
//! the resolver doesn't need to know which type table a selector
//! targets — the name alone identifies the column.

use std::collections::{HashMap, HashSet};

use promql_parser::label::Matcher;
use promql_parser::parser::{self, Expr};

use crate::labels::Labels;
use crate::promql::{
    extract_filter_labels, parse_histogram_call, parse_optional_stride, QueryEngine, QueryError,
};

impl QueryEngine {
    /// Resolve a PromQL query to the set of physical parquet column
    /// names in the underlying data source that it touches.
    ///
    /// Returns an empty set if the query parses cleanly but matches
    /// no series. Returns `QueryError::ParseError` on syntax error.
    pub fn columns(&self, query: &str) -> Result<HashSet<String>, QueryError> {
        let stripped = strip_rezolus_wrapper(query)?;
        let expr = parser::parse(stripped)
            .map_err(|e| QueryError::ParseError(format!("Failed to parse query: {e:?}")))?;
        let col_map = self.source.column_map();
        let mut out = HashSet::new();
        walk(&col_map, &expr, &mut out);
        Ok(out)
    }
}

/// The metric names a PromQL query references, without a data source.
///
/// `QueryEngine::columns` answers a related question — which *physical*
/// columns a query touches — but it needs the source's column map to expand a
/// selector into its labelled series, which means the source must already be
/// open. A caller routing a query *to* a source cannot have that yet.
///
/// This walks the same AST and collects the selector names only, so a reader
/// holding many tables can decide which of them a query could possibly touch
/// before opening any. Label matchers are deliberately ignored: routing needs
/// "could this table answer the query", and a name present with no matching
/// labels is a table that answers with an empty result, not one to skip.
///
/// Returns an empty set for a query that references no named metric (a bare
/// scalar, say). Returns `QueryError::ParseError` on syntax error, matching
/// `columns`.
pub fn referenced_metrics(query: &str) -> Result<HashSet<String>, QueryError> {
    let stripped = strip_rezolus_wrapper(query)?;
    let expr = parser::parse(stripped)
        .map_err(|e| QueryError::ParseError(format!("Failed to parse query: {e:?}")))?;
    let mut out = HashSet::new();
    walk_names(&expr, &mut out);
    Ok(out)
}

/// Each selector of a query: the metric names it can select (one, or none
/// for a selector with no name) and its label matchers as a filter
/// (`extract_filter_labels`). Like [`referenced_metrics`], needs no source;
/// a reader uses it to leave out tables whose fixed labels no selector can
/// match.
pub(crate) fn selectors(query: &str) -> Result<Vec<Selector>, QueryError> {
    let stripped = strip_rezolus_wrapper(query)?;
    let expr = parser::parse(stripped)
        .map_err(|e| QueryError::ParseError(format!("Failed to parse query: {e:?}")))?;
    let mut out = Vec::new();
    walk_selectors(&expr, &mut out);
    Ok(out)
}

/// A selector: the metric names it can select and its matchers as a filter.
pub(crate) type Selector = (HashSet<String>, Labels);

/// Mirror of [`walk_names`], collecting each selector whole.
fn walk_selectors(expr: &Expr, out: &mut Vec<(HashSet<String>, Labels)>) {
    match expr {
        Expr::Aggregate(agg) => walk_selectors(&agg.expr, out),
        Expr::Unary(u) => walk_selectors(&u.expr, out),
        Expr::Binary(b) => {
            walk_selectors(&b.lhs, out);
            walk_selectors(&b.rhs, out);
        }
        Expr::Paren(p) => walk_selectors(&p.expr, out),
        Expr::Call(call) => {
            for arg in &call.args.args {
                walk_selectors(arg, out);
            }
        }
        Expr::VectorSelector(sel) => out.push(selector(sel)),
        Expr::MatrixSelector(sel) => out.push(selector(&sel.vs)),
        Expr::Subquery(s) => walk_selectors(&s.expr, out),
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::Extension(_) => {}
    }
}

/// A label a selector of `query` matches more than once
/// (`{source!="a", source!="b"}`): the filter holds one matcher per label,
/// so all but one would be dropped silently.
pub(crate) fn repeated_matcher(query: &str) -> Result<Option<String>, QueryError> {
    let stripped = strip_rezolus_wrapper(query)?;
    let expr = parser::parse(stripped)
        .map_err(|e| QueryError::ParseError(format!("Failed to parse query: {e:?}")))?;
    let mut found = None;
    walk_vector_selectors(&expr, &mut |sel| {
        let mut seen = HashSet::new();
        for m in &sel.matchers.matchers {
            if m.name != "__name__" && !seen.insert(m.name.clone()) && found.is_none() {
                found = Some(m.name.clone());
            }
        }
    });
    Ok(found)
}

fn walk_vector_selectors(expr: &Expr, f: &mut dyn FnMut(&parser::VectorSelector)) {
    match expr {
        Expr::Aggregate(agg) => walk_vector_selectors(&agg.expr, f),
        Expr::Unary(u) => walk_vector_selectors(&u.expr, f),
        Expr::Binary(b) => {
            walk_vector_selectors(&b.lhs, f);
            walk_vector_selectors(&b.rhs, f);
        }
        Expr::Paren(p) => walk_vector_selectors(&p.expr, f),
        Expr::Call(call) => {
            for arg in &call.args.args {
                walk_vector_selectors(arg, f);
            }
        }
        Expr::VectorSelector(sel) => f(sel),
        Expr::MatrixSelector(sel) => f(&sel.vs),
        Expr::Subquery(s) => walk_vector_selectors(&s.expr, f),
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::Extension(_) => {}
    }
}

fn selector(sel: &parser::VectorSelector) -> (HashSet<String>, Labels) {
    let mut names = HashSet::new();
    collect_selector_names(sel, &mut names);
    (names, extract_filter_labels(&sel.matchers.matchers))
}

/// Mirror of [`walk`] that collects selector names instead of columns. Kept
/// beside it deliberately: the two must stay in step as the expression grammar
/// grows, and a missed arm here silently narrows routing rather than failing
/// loudly.
fn walk_names(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Aggregate(agg) => walk_names(&agg.expr, out),
        Expr::Unary(u) => walk_names(&u.expr, out),
        Expr::Binary(b) => {
            walk_names(&b.lhs, out);
            walk_names(&b.rhs, out);
        }
        Expr::Paren(p) => walk_names(&p.expr, out),
        Expr::Call(call) => {
            for arg in &call.args.args {
                walk_names(arg, out);
            }
        }
        Expr::VectorSelector(sel) => collect_selector_names(sel, out),
        Expr::MatrixSelector(sel) => collect_selector_names(&sel.vs, out),
        Expr::Subquery(s) => walk_names(&s.expr, out),
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::Extension(_) => {}
    }
}

/// A selector names a metric either as `foo` or as `{__name__="foo"}`; both
/// forms route the same, so both are collected.
fn collect_selector_names(sel: &parser::VectorSelector, out: &mut HashSet<String>) {
    if let Some(n) = sel.name.as_deref() {
        out.insert(n.to_string());
    }
    for m in sel
        .matchers
        .matchers
        .iter()
        .filter(|m| m.name == "__name__")
    {
        if !m.value.is_empty() {
            out.insert(m.value.clone());
        }
    }
}

fn strip_rezolus_wrapper(query: &str) -> Result<&str, QueryError> {
    if let Some(inner) = query
        .strip_prefix("histogram_quantiles(")
        .or_else(|| query.strip_prefix("histogram_percentiles("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let array_end = inner.find(']').ok_or_else(|| {
            QueryError::ParseError("Missing closing bracket in quantiles array".to_string())
        })?;
        let remaining = inner[array_end + 1..]
            .trim_start()
            .strip_prefix(',')
            .map(str::trim)
            .ok_or_else(|| {
                QueryError::ParseError(
                    "histogram_quantiles requires a metric name as second argument".to_string(),
                )
            })?;
        let (selector, _stride) = parse_optional_stride(remaining)?;
        return Ok(selector);
    }
    if let Some(inner) = query
        .strip_prefix("histogram_heatmap(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let (selector, _stride) = parse_optional_stride(inner.trim())?;
        return Ok(selector);
    }
    for func in [
        "histogram_mean",
        "histogram_count",
        "histogram_sum",
        "histogram_irate",
    ] {
        if let Some((inner, _group_by)) = parse_histogram_call(func, query)? {
            let (selector, _stride) = parse_optional_stride(inner.trim())?;
            return Ok(selector);
        }
    }
    Ok(query)
}

fn walk(
    col_map: &HashMap<String, HashMap<Labels, String>>,
    expr: &Expr,
    out: &mut HashSet<String>,
) {
    match expr {
        Expr::Paren(p) => walk(col_map, &p.expr, out),
        Expr::Unary(u) => walk(col_map, &u.expr, out),
        Expr::Aggregate(agg) => walk(col_map, &agg.expr, out),
        Expr::Binary(b) => {
            walk(col_map, &b.lhs, out);
            walk(col_map, &b.rhs, out);
        }
        Expr::Call(call) => {
            for arg in &call.args.args {
                walk(col_map, arg, out);
            }
        }
        Expr::VectorSelector(sel) => collect_selector(col_map, sel, out),
        Expr::MatrixSelector(sel) => collect_selector(col_map, &sel.vs, out),
        Expr::Subquery(s) => walk(col_map, &s.expr, out),
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::Extension(_) => {}
    }
}

fn collect_selector(
    col_map: &HashMap<String, HashMap<Labels, String>>,
    sel: &parser::VectorSelector,
    out: &mut HashSet<String>,
) {
    let label_filter = extract_filter_labels(&sel.matchers.matchers);
    let name_matchers: Vec<&Matcher> = sel
        .matchers
        .matchers
        .iter()
        .filter(|m| m.name == "__name__")
        .collect();

    if let Some(n) = sel.name.as_deref() {
        let Some(labels_map) = col_map.get(n) else {
            return;
        };
        if !name_matchers.iter().all(|m| m.is_match(n)) {
            return;
        }
        for (labels, col) in labels_map {
            if labels.matches(&label_filter) {
                out.insert(col.clone());
            }
        }
        return;
    }

    for (metric_name, labels_map) in col_map {
        if !name_matchers.iter().all(|m| m.is_match(metric_name)) {
            continue;
        }
        for (labels, col) in labels_map {
            if labels.matches(&label_filter) {
                out.insert(col.clone());
            }
        }
    }
}
