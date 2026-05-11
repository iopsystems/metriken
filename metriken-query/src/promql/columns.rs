//! Resolve a PromQL query to the set of physical parquet columns it
//! touches — without reading any values. Walks the parsed AST the
//! same way `streaming::dispatch` does, but looks each selector up
//! in the unified column map instead of fetching series data.
//!
//! Metric names are unique across types in the exposition format,
//! so the resolver doesn't need to know which type table a selector
//! targets — the name alone identifies the column.

use std::collections::HashSet;
use std::ops::Deref;

use promql_parser::label::Matcher;
use promql_parser::parser::{self, Expr};

use crate::promql::{extract_filter_labels, parse_optional_stride, QueryEngine, QueryError};
use crate::tsdb::Tsdb;

impl<T: Deref<Target = Tsdb>> QueryEngine<T> {
    /// Resolve a PromQL query to the set of physical parquet column
    /// names in the underlying TSDB that it touches during evaluation.
    ///
    /// Returns an empty set if the query parses cleanly but matches
    /// no series.  Returns `QueryError::ParseError` on syntax error,
    /// so callers can distinguish "query is broken" from "query
    /// touches nothing in this TSDB."
    ///
    /// Column identifiers are returned as-stored by the TSDB — for
    /// parquet-backed TSDBs that's the parquet column name (e.g.
    /// `"0::42"`, `"agent-4241::105"`, `"0::26x0:buckets"`). The
    /// caller is expected to map these back to the parquet schema and
    /// always-keep `timestamp` + `duration` separately.
    pub fn columns(&self, query: &str) -> Result<HashSet<String>, QueryError> {
        let stripped = strip_rezolus_wrapper(query)?;
        let expr = parser::parse(stripped)
            .map_err(|e| QueryError::ParseError(format!("Failed to parse query: {e:?}")))?;
        let mut out = HashSet::new();
        walk(&self.tsdb, &expr, &mut out);
        Ok(out)
    }
}

/// Reduce a rezolus-specific wrapper to its inner metric selector
/// so the standard PromQL parser can handle it. The wrappers
/// (`histogram_quantiles([qs], m, [stride])` and
/// `histogram_heatmap(m, [stride])`) use array-literal / multi-arg
/// syntax the parser rejects, but their *column set* is just the
/// inner selector's. Non-rezolus queries pass through unchanged.
fn strip_rezolus_wrapper(query: &str) -> Result<&str, QueryError> {
    if let Some(inner) = query
        .strip_prefix("histogram_quantiles(")
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
    Ok(query)
}

fn walk(tsdb: &Tsdb, expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Paren(p) => walk(tsdb, &p.expr, out),
        Expr::Unary(u) => walk(tsdb, &u.expr, out),
        Expr::Aggregate(agg) => walk(tsdb, &agg.expr, out),
        Expr::Binary(b) => {
            walk(tsdb, &b.lhs, out);
            walk(tsdb, &b.rhs, out);
        }
        Expr::Call(call) => {
            for arg in &call.args.args {
                walk(tsdb, arg, out);
            }
        }
        Expr::VectorSelector(sel) => collect_selector(tsdb, sel, out),
        Expr::MatrixSelector(sel) => collect_selector(tsdb, &sel.vs, out),
        Expr::Subquery(s) => walk(tsdb, &s.expr, out),
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::Extension(_) => {}
    }
}

/// Resolve one `VectorSelector` against the unified column map.
/// Mirrors `streaming::dispatch::build_vector_selector`'s
/// label-filter logic, but iterates the column-name map instead of
/// the typed series collection.
fn collect_selector(tsdb: &Tsdb, sel: &parser::VectorSelector, out: &mut HashSet<String>) {
    let label_filter = extract_filter_labels(&sel.matchers.matchers);
    let name_matchers: Vec<&Matcher> = sel
        .matchers
        .matchers
        .iter()
        .filter(|m| m.name == "__name__")
        .collect();

    if let Some(n) = sel.name.as_deref() {
        let Some(labels_map) = tsdb.columns_ref().get(n) else {
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

    // Regex / negated `__name__`: full scan, since the name is no
    // longer a single bucket key.
    for (metric_name, labels_map) in tsdb.columns_ref() {
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
