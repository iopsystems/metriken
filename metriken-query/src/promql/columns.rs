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

/// Which labels of the series it combines a combination keeps apart: an
/// aggregation's grouping, or a binary operator's matching.
#[derive(Clone)]
pub(crate) enum Keeps {
    /// Every label: a binary operator with no `on`/`ignoring`.
    All,
    /// Only these: `by (...)`, `on (...)`.
    Only(HashSet<String>),
    /// All but these: `without (...)`, `ignoring (...)`.
    AllBut(HashSet<String>),
}

impl Keeps {
    /// Whether series that differ in `key` stay apart. `without` and
    /// `ignoring` drop internal labels (`__recording__`) along with the
    /// listed ones, as the engine does (`streaming/aggregate.rs`,
    /// `streaming/binary.rs`).
    pub(crate) fn keeps(&self, key: &str) -> bool {
        match self {
            Keeps::All => true,
            Keeps::Only(keys) => keys.contains(key),
            Keeps::AllBut(keys) => !keys.contains(key) && !crate::labels::is_internal_label(key),
        }
    }

    fn of(modifier: Option<&parser::LabelModifier>, default: Keeps) -> Keeps {
        let set = |l: &promql_parser::label::Labels| l.labels.iter().cloned().collect();
        match modifier {
            None => default,
            Some(parser::LabelModifier::Include(l)) => Keeps::Only(set(l)),
            Some(parser::LabelModifier::Exclude(l)) => Keeps::AllBut(set(l)),
        }
    }
}

/// A place where a query combines series.
pub(crate) struct Site {
    /// The selectors of an aggregation's operand, or of an operator's left
    /// side.
    pub(crate) lhs: Vec<Selector>,
    /// The selectors of an operator's right side; `None` for an aggregation.
    pub(crate) rhs: Option<Vec<Selector>>,
    /// The labels the site keeps apart.
    pub(crate) keeps: Keeps,
    /// For an operator: the groupings of every aggregation on either side.
    /// A side's series carry a label only if each of its aggregations keeps
    /// it.
    pub(crate) side_groupings: Vec<Keeps>,
}

impl Site {
    /// Whether an operator's sides still carry `key` where they meet, and
    /// its matching keeps it.
    pub(crate) fn keeps_across(&self, key: &str) -> bool {
        self.keeps.keeps(key) && self.side_groupings.iter().all(|g| g.keeps(key))
    }
}

fn groupings(expr: &Expr, out: &mut Vec<Keeps>) {
    match expr {
        Expr::Aggregate(agg) => {
            out.push(Keeps::of(
                agg.modifier.as_ref(),
                Keeps::Only(HashSet::new()),
            ));
            groupings(&agg.expr, out);
        }
        Expr::Binary(b) => {
            groupings(&b.lhs, out);
            groupings(&b.rhs, out);
        }
        Expr::Unary(u) => groupings(&u.expr, out),
        Expr::Paren(p) => groupings(&p.expr, out),
        Expr::Call(call) => {
            for a in &call.args.args {
                groupings(a, out);
            }
        }
        Expr::Subquery(s) => groupings(&s.expr, out),
        Expr::VectorSelector(_)
        | Expr::MatrixSelector(_)
        | Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::Extension(_) => {}
    }
}

/// Every place `query` combines series: an aggregation and a binary
/// operator with a selector on both sides, each with the labels it keeps
/// apart. For a reader that evaluates a query once per recording and must
/// know where the result would merge series of different recordings.
pub(crate) fn combination_sites(query: &str) -> Result<Vec<Site>, QueryError> {
    let stripped = strip_rezolus_wrapper(query)?;
    let expr = parser::parse(stripped)
        .map_err(|e| QueryError::ParseError(format!("Failed to parse query: {e:?}")))?;
    let mut out = Vec::new();
    walk_combinations(&expr, &mut out);
    Ok(out)
}

fn selectors_of(expr: &Expr) -> Vec<Selector> {
    let mut out = Vec::new();
    walk_selectors(expr, &mut out);
    out
}

fn walk_combinations(expr: &Expr, out: &mut Vec<Site>) {
    match expr {
        Expr::Aggregate(agg) => {
            // No grouping keeps nothing apart: `sum(x)` is one series.
            let keeps = Keeps::of(agg.modifier.as_ref(), Keeps::Only(HashSet::new()));
            out.push(Site {
                lhs: selectors_of(&agg.expr),
                rhs: None,
                keeps,
                side_groupings: Vec::new(),
            });
            walk_combinations(&agg.expr, out);
        }
        Expr::Binary(b) => {
            let (lhs, rhs) = (selectors_of(&b.lhs), selectors_of(&b.rhs));
            if !lhs.is_empty() && !rhs.is_empty() {
                let matching = b.modifier.as_ref().and_then(|m| m.matching.as_ref());
                out.push(Site {
                    lhs,
                    rhs: Some(rhs),
                    keeps: Keeps::of(matching, Keeps::All),
                    side_groupings: {
                        let mut g = Vec::new();
                        groupings(&b.lhs, &mut g);
                        groupings(&b.rhs, &mut g);
                        g
                    },
                });
            }
            walk_combinations(&b.lhs, out);
            walk_combinations(&b.rhs, out);
        }
        Expr::Unary(u) => walk_combinations(&u.expr, out),
        Expr::Paren(p) => walk_combinations(&p.expr, out),
        Expr::Call(call) => {
            for arg in &call.args.args {
                walk_combinations(arg, out);
            }
        }
        Expr::Subquery(s) => walk_combinations(&s.expr, out),
        Expr::VectorSelector(_)
        | Expr::MatrixSelector(_)
        | Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::Extension(_) => {}
    }
}

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
