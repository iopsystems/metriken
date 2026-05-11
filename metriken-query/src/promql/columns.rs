//! Resolve a PromQL query to the set of physical parquet columns it
//! touches — without reading any values. Walks the parsed AST, finds
//! every vector/matrix selector, and looks each one up in the TSDB's
//! unified column map.
//!
//! Metric names are unique across types in the exposition format
//! (a name lives in exactly one of counters/gauges/histograms), so
//! the resolver doesn't need to know which type table a selector
//! targets — the name alone identifies the column.

use std::collections::HashSet;
use std::ops::Deref;

use promql_parser::label::Matcher;
use promql_parser::parser::{self, Expr};

use crate::promql::{extract_filter_labels, QueryEngine, QueryError};
use crate::tsdb::{Labels, Tsdb};

impl<T: Deref<Target = Tsdb>> QueryEngine<T> {
    /// Resolve a PromQL query to the set of physical parquet column
    /// names in the underlying TSDB that it touches during evaluation.
    ///
    /// Walks the parsed AST, finds every vector selector, and resolves
    /// each through the same matcher logic that `query`/`query_range`
    /// would use — but stops short of actually reading values.
    /// Returns an empty set if the query parses cleanly but matches no
    /// series.  Returns `QueryError::ParseError` on syntax error so
    /// callers can distinguish "query is broken" from "query touches
    /// nothing in this TSDB."
    ///
    /// Column identifiers are returned as-stored by the TSDB — for
    /// parquet-backed TSDBs that's the parquet column name (e.g.
    /// `"0::42"`, `"agent-4241::105"`, `"0::26x0:buckets"`). The
    /// caller is expected to map these back to the parquet schema and
    /// always-keep `timestamp` + `duration` separately.
    pub fn columns(&self, query: &str) -> Result<HashSet<String>, QueryError> {
        let mut out = HashSet::new();

        // Rezolus-specific forms use array-literal / multi-output
        // syntax the standard PromQL parser rejects; intercept them
        // before parsing, matching `query_range`.
        if let Some(inner) = query
            .strip_prefix("histogram_quantiles(")
            .and_then(|s| s.strip_suffix(')'))
        {
            columns_histogram_quantiles(&self.tsdb, inner, &mut out)?;
            return Ok(out);
        }
        if let Some(inner) = query
            .strip_prefix("histogram_heatmap(")
            .and_then(|s| s.strip_suffix(')'))
        {
            columns_histogram_heatmap(&self.tsdb, inner, &mut out);
            return Ok(out);
        }

        let expr = parser::parse(query)
            .map_err(|e| QueryError::ParseError(format!("Failed to parse query: {e:?}")))?;
        walk(&self.tsdb, &expr, &mut out);
        Ok(out)
    }
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

fn columns_histogram_quantiles(
    tsdb: &Tsdb,
    inner: &str,
    out: &mut HashSet<String>,
) -> Result<(), QueryError> {
    let array_end = inner.find(']').ok_or_else(|| {
        QueryError::ParseError("Missing closing bracket in quantiles array".to_string())
    })?;
    let after_array = inner[array_end + 1..].trim_start();
    let remaining = after_array
        .strip_prefix(',')
        .map(str::trim)
        .ok_or_else(|| {
            QueryError::ParseError(
                "histogram_quantiles requires a metric name as second argument".to_string(),
            )
        })?;
    let (metric_selector, _stride) = strip_trailing_stride(remaining);
    let (name, labels) = parse_selector(metric_selector);
    collect_by_name(tsdb, &name, &labels, out);
    Ok(())
}

fn columns_histogram_heatmap(tsdb: &Tsdb, inner: &str, out: &mut HashSet<String>) {
    let (metric_selector, _stride) = strip_trailing_stride(inner.trim());
    let (name, labels) = parse_selector(metric_selector);
    collect_by_name(tsdb, &name, &labels, out);
}

fn collect_by_name(tsdb: &Tsdb, name: &str, labels: &Labels, out: &mut HashSet<String>) {
    let Some(labels_map) = tsdb.columns_ref().get(name) else {
        return;
    };
    for (series_labels, col) in labels_map {
        if series_labels.matches(labels) {
            out.insert(col.clone());
        }
    }
}

/// Strip an optional trailing `, <seconds>` stride argument. Mirrors
/// `parse_optional_stride` in `promql/mod.rs`, but brace-aware so
/// labels with commas stay intact and tolerant of a non-numeric tail
/// (the caller's selector may have a top-level comma we shouldn't
/// split on).
fn strip_trailing_stride(s: &str) -> (&str, Option<&str>) {
    let mut brace_depth = 0i32;
    let mut last_comma = None;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' => brace_depth += 1,
            '}' => brace_depth -= 1,
            ',' if brace_depth == 0 => last_comma = Some(i),
            _ => {}
        }
    }
    if let Some(i) = last_comma {
        let tail = s[i + 1..].trim();
        if tail.parse::<f64>().is_ok() {
            return (s[..i].trim(), Some(tail));
        }
    }
    (s, None)
}

/// Raw-string metric selector parser for the rezolus-specific
/// `histogram_*` forms (which bypass the PromQL parser). Empty input
/// yields `("", empty)` so the caller stays on the no-match-is-empty
/// path instead of erroring.
fn parse_selector(selector: &str) -> (String, Labels) {
    let Some(brace_pos) = selector.find('{') else {
        return (selector.trim().to_string(), Labels::default());
    };
    let name = selector[..brace_pos].trim().to_string();
    let end = selector.rfind('}').unwrap_or(selector.len());
    let body = &selector[brace_pos + 1..end];

    let mut labels = Labels::default();
    for part in body.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, value, prefix) = if let Some(pos) = part.find("!=") {
            (&part[..pos], part[pos + 2..].trim().trim_matches('"'), "!")
        } else if let Some(pos) = part.find("!~") {
            (&part[..pos], part[pos + 2..].trim().trim_matches('"'), "!")
        } else if let Some(pos) = part.find("=~") {
            (&part[..pos], part[pos + 2..].trim().trim_matches('"'), "~")
        } else if let Some(pos) = part.find('=') {
            (&part[..pos], part[pos + 1..].trim().trim_matches('"'), "")
        } else {
            continue;
        };
        let key = key.trim().to_string();
        labels.inner.insert(key, format!("{prefix}{value}"));
    }
    (name, labels)
}
