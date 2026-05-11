//! Resolve a PromQL query to the set of physical parquet columns it
//! touches — without reading any values. The walker mirrors
//! `streaming::dispatch::build`'s recursion shape so the column set
//! tracks what the evaluator would load.

use std::collections::{HashMap, HashSet};
use std::ops::Deref;

use promql_parser::label::Matcher;
use promql_parser::parser::{self, Expr};

use crate::promql::{extract_filter_labels, QueryEngine, QueryError};
use crate::tsdb::{Labels, Tsdb};

/// Which TSDB collection a vector-selector should resolve against,
/// determined by the enclosing call. `GaugeOrCounter` is the
/// `deriv(metric[d])` case — gauge-first, counter-fallback, matching
/// the runtime dispatch in `streaming::dispatch`.
#[derive(Copy, Clone)]
enum Kind {
    Gauge,
    Counter,
    Histogram,
    GaugeOrCounter,
}

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
        walk(&self.tsdb, &expr, Kind::Gauge, &mut out);
        Ok(out)
    }
}

/// `default_kind` is the collection a bare vector-selector resolves
/// against — set by the enclosing call, otherwise `Gauge`.
fn walk(tsdb: &Tsdb, expr: &Expr, default_kind: Kind, out: &mut HashSet<String>) {
    match expr {
        Expr::Paren(p) => walk(tsdb, &p.expr, default_kind, out),
        Expr::Unary(u) => walk(tsdb, &u.expr, default_kind, out),
        Expr::Aggregate(agg) => walk(tsdb, &agg.expr, default_kind, out),
        Expr::Binary(b) => {
            walk(tsdb, &b.lhs, default_kind, out);
            walk(tsdb, &b.rhs, default_kind, out);
        }
        Expr::Call(call) => walk_call(tsdb, call, out),
        Expr::VectorSelector(sel) => collect_selector(tsdb, sel, default_kind, out),
        Expr::MatrixSelector(sel) => collect_selector(tsdb, &sel.vs, default_kind, out),
        Expr::Subquery(s) => walk(tsdb, &s.expr, default_kind, out),
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::Extension(_) => {}
    }
}

fn walk_call(tsdb: &Tsdb, call: &parser::Call, out: &mut HashSet<String>) {
    // `histogram_quantile(q, v)` takes a bare vector selector for `v`;
    // the quantile literal `q` touches no columns.
    if call.func.name == "histogram_quantile" {
        for arg in &call.args.args {
            if let Expr::VectorSelector(sel) = arg.as_ref() {
                collect_selector(tsdb, sel, Kind::Histogram, out);
            }
        }
        return;
    }

    let inner_kind = match call.func.name {
        "rate" | "irate" => Kind::Counter,
        "avg_over_time" | "idelta" => Kind::Gauge,
        "deriv" => Kind::GaugeOrCounter,
        _ => Kind::Gauge,
    };

    for arg in &call.args.args {
        walk(tsdb, arg, inner_kind, out);
    }
}

fn collect_selector(
    tsdb: &Tsdb,
    sel: &parser::VectorSelector,
    kind: Kind,
    out: &mut HashSet<String>,
) {
    let label_filter = extract_filter_labels(&sel.matchers.matchers);
    let name_matchers: Vec<&Matcher> = sel
        .matchers
        .matchers
        .iter()
        .filter(|m| m.name == "__name__")
        .collect();
    let name = sel.name.as_deref();

    match kind {
        Kind::Gauge => collect_from(
            tsdb.gauge_columns_ref(),
            name,
            &name_matchers,
            &label_filter,
            out,
        ),
        Kind::Counter => collect_from(
            tsdb.counter_columns_ref(),
            name,
            &name_matchers,
            &label_filter,
            out,
        ),
        Kind::Histogram => collect_from(
            tsdb.histogram_columns_ref(),
            name,
            &name_matchers,
            &label_filter,
            out,
        ),
        Kind::GaugeOrCounter => {
            // `deriv` dispatcher: prefer gauges, fall back to counters
            // only when the gauge side contributes nothing.
            let before = out.len();
            collect_from(
                tsdb.gauge_columns_ref(),
                name,
                &name_matchers,
                &label_filter,
                out,
            );
            if out.len() == before {
                collect_from(
                    tsdb.counter_columns_ref(),
                    name,
                    &name_matchers,
                    &label_filter,
                    out,
                );
            }
        }
    }
}

fn collect_from(
    map: &HashMap<String, HashMap<Labels, String>>,
    exact_name: Option<&str>,
    name_matchers: &[&Matcher],
    label_filter: &Labels,
    out: &mut HashSet<String>,
) {
    if let Some(n) = exact_name {
        let Some(labels_map) = map.get(n) else {
            return;
        };
        if !name_matchers.iter().all(|m| m.is_match(n)) {
            return;
        }
        for (labels, col) in labels_map {
            if labels.matches(label_filter) {
                out.insert(col.clone());
            }
        }
        return;
    }

    // Regex / negated `__name__`: full scan, since the name is no
    // longer a single bucket key.
    for (metric_name, labels_map) in map {
        if !name_matchers.iter().all(|m| m.is_match(metric_name)) {
            continue;
        }
        for (labels, col) in labels_map {
            if labels.matches(label_filter) {
                out.insert(col.clone());
            }
        }
    }
}

/// Same column set as `histogram_quantile(_, metric)` — both consume
/// every bucket column of the named histogram. The quantile array is
/// not validated here; that's the runtime's job.
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
    collect_histogram(tsdb, &name, &labels, out);
    Ok(())
}

fn columns_histogram_heatmap(tsdb: &Tsdb, inner: &str, out: &mut HashSet<String>) {
    let (metric_selector, _stride) = strip_trailing_stride(inner.trim());
    let (name, labels) = parse_selector(metric_selector);
    collect_histogram(tsdb, &name, &labels, out);
}

fn collect_histogram(tsdb: &Tsdb, name: &str, labels: &Labels, out: &mut HashSet<String>) {
    let Some(labels_map) = tsdb.histogram_columns_ref().get(name) else {
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
