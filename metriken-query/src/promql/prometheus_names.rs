//! A selector naming a series by the name a Prometheus exporter served it
//! under, when it is stored under another
//! ([`metriken_storage::prometheus_units`]).
//!
//! When a selector's metric name is held by none of the series it can
//! reach, and a candidate stored name ([`fallback_candidates`]) has series
//! whose [`PROMETHEUS_NAME_LABEL`] is that name, the selector reads the
//! candidate with the matcher `__prometheus_name__="<name>"` added. A name
//! held exactly is always read as itself. The series are read as stored:
//! in the normalized unit, and named by the stored name in the result.
//!
//! What a selector can reach: over an archive, the recordings its scope and
//! its recording-label matchers leave (`archive::Plan`, which resolves each
//! selector and hands the result to the engine as a [`NameResolution`]);
//! over any other source, the whole source.

use std::collections::HashMap;

use metriken_storage::prometheus_units::fallback_candidates;
use metriken_storage::PROMETHEUS_NAME_LABEL;
use promql_parser::label::{MatchOp as PromMatchOp, Matcher};
use promql_parser::parser::{self, Expr};

use crate::labels::{Labels, MatchOp};
use crate::promql::extract_filter_labels;
use crate::DataSource;

/// The stored name each selector reads instead of the name it gives, keyed
/// by that name and the selector's filter (`extract_filter_labels` of its
/// matchers). A selector that is not a key reads its own name.
pub(crate) type NameResolution = HashMap<(String, Labels), String>;

/// Whether `source` holds `name` as any kind.
fn holds(source: &dyn DataSource, name: &str) -> bool {
    source.counter_names().iter().any(|n| n == name)
        || source.gauge_names().iter().any(|n| n == name)
        || source.histogram_names().iter().any(|n| n == name)
        || source.gauge_histogram_names().iter().any(|n| n == name)
}

/// Whether `source` has a series of `name`, of any kind, whose exporter
/// name is `original`.
fn holds_renamed(source: &dyn DataSource, name: &str, original: &str) -> bool {
    let renamed = |sets: Vec<std::collections::BTreeMap<String, String>>| {
        sets.iter().any(|set| {
            set.get(PROMETHEUS_NAME_LABEL)
                .is_some_and(|v| v == original)
        })
    };
    renamed(source.counter_labels(name))
        || renamed(source.gauge_labels(name))
        || renamed(source.histogram_labels(name))
        || renamed(source.gauge_histogram_labels(name))
}

/// The stored name a selector of `name` reads over the whole of `source`:
/// `None` when `source` holds `name`, or no candidate has a series renamed
/// from it.
pub(crate) fn stored_name(source: &dyn DataSource, name: &str) -> Option<String> {
    let candidates = fallback_candidates(name);
    if candidates.is_empty() || holds(source, name) {
        return None;
    }
    candidates
        .into_iter()
        .find(|c| holds(source, c) && holds_renamed(source, c, name))
}

/// `filter` with the matcher selecting the series renamed from `original`.
pub(crate) fn renamed_filter(mut filter: Labels, original: &str) -> Labels {
    filter.inner.insert(
        PROMETHEUS_NAME_LABEL.to_string(),
        MatchOp::Equal.encode(original),
    );
    filter
}

/// Point each named selector of `expr` that `resolve` resolves at the
/// stored name, adding the `__prometheus_name__` matcher. `resolve` takes
/// the selector's name and filter. A selector with `or` matchers is left
/// as it is.
pub(crate) fn resolve_selectors(
    expr: &mut Expr,
    resolve: &dyn Fn(&str, &Labels) -> Option<String>,
) {
    match expr {
        Expr::Aggregate(agg) => {
            resolve_selectors(&mut agg.expr, resolve);
            if let Some(param) = agg.param.as_mut() {
                resolve_selectors(param, resolve);
            }
        }
        Expr::Unary(u) => resolve_selectors(&mut u.expr, resolve),
        Expr::Binary(b) => {
            resolve_selectors(&mut b.lhs, resolve);
            resolve_selectors(&mut b.rhs, resolve);
        }
        Expr::Paren(p) => resolve_selectors(&mut p.expr, resolve),
        Expr::Call(call) => {
            for arg in call.args.args.iter_mut() {
                resolve_selectors(arg, resolve);
            }
        }
        Expr::VectorSelector(sel) => resolve_selector(sel, resolve),
        Expr::MatrixSelector(sel) => resolve_selector(&mut sel.vs, resolve),
        Expr::Subquery(s) => resolve_selectors(&mut s.expr, resolve),
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::Extension(_) => {}
    }
}

fn resolve_selector(
    sel: &mut parser::VectorSelector,
    resolve: &dyn Fn(&str, &Labels) -> Option<String>,
) {
    if !sel.matchers.or_matchers.is_empty() {
        return;
    }
    let Some(name) = sel.name.clone() else {
        return;
    };
    let filter = extract_filter_labels(&sel.matchers.matchers);
    let Some(stored) = resolve(&name, &filter) else {
        return;
    };
    sel.matchers
        .matchers
        .retain(|m| m.name != "__name__" && m.name != PROMETHEUS_NAME_LABEL);
    sel.matchers.matchers.push(Matcher::new(
        PromMatchOp::Equal,
        PROMETHEUS_NAME_LABEL,
        &name,
    ));
    sel.name = Some(stored);
}
