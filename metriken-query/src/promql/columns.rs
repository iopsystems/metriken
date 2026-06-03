//! Resolve a PromQL query to the set of physical parquet columns it
//! touches — without reading any values.

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
