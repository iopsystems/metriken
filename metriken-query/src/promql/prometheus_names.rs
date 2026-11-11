//! A selector naming a series by the name a Prometheus exporter served it
//! under, when it is stored under another
//! ([`metriken_storage::prometheus_units`]).
//!
//! When a selector's metric name is held by none of the series it can
//! reach, and a candidate stored name ([`fallback_candidates`]) has series
//! whose [`PROMETHEUS_NAME_LABEL`] is that name, the selector reads the
//! candidate with the matcher `__prometheus_name__="<name>"` added. A name
//! held exactly is always read as itself. The series are read as stored:
//! in the normalized unit, and named by the stored name in the result. When
//! more than one candidate has such series in reach (two endpoints whose
//! `# UNIT` lines differ for one name), the query is refused with an error
//! naming them, since they are in different units.
//!
//! What a selector can reach: over an archive, the recordings its scope and
//! its recording-label matchers leave (`archive::Plan`, which resolves each
//! selector and hands the result to the engine as a [`NameResolution`]);
//! over any other source, the whole source.

use std::collections::{HashMap, HashSet};

use metriken_storage::prometheus_units::fallback_candidates;
use metriken_storage::PROMETHEUS_NAME_LABEL;
use promql_parser::label::{MatchOp as PromMatchOp, Matcher};
use promql_parser::parser::{self, Expr};

use crate::labels::Labels;
use crate::promql::{QueryError, Selection};
use crate::DataSource;

/// The stored name each selector reads instead of the name it gives, keyed
/// by that name and the filter its read takes (`Selection::filter`). A
/// selector that is not a key reads its own name.
pub(crate) type NameResolution = HashMap<(String, Labels), String>;

/// Every name `source` holds, of any kind: read once per engine, so a
/// query lists the source's names once however many selectors it has. An
/// engine lives for one query for `MemoryStore` and archives, and for the
/// reader's life for `ParquetReader`, `SegmentedParquetReader` and
/// `UnionMetricsSource`, whose names are fixed at open.
pub(crate) fn held_names(source: &dyn DataSource) -> HashSet<String> {
    let mut held: HashSet<String> = source.counter_names().into_iter().collect();
    held.extend(source.gauge_names());
    held.extend(source.histogram_names());
    held.extend(source.gauge_histogram_names());
    held
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

/// The stored name a selector of `name` reads over the whole of `source`,
/// whose names are `held`: `None` when `source` holds `name`, or no
/// candidate has a series renamed from it; an error ([`ambiguous`]) when
/// more than one does. The candidates' label sets are read only when
/// `name` is not held.
pub(crate) fn stored_name(
    source: &dyn DataSource,
    held: &HashSet<String>,
    name: &str,
) -> Result<Option<String>, QueryError> {
    if held.contains(name) {
        return Ok(None);
    }
    let found: Vec<String> = fallback_candidates(name)
        .into_iter()
        .filter(|c| held.contains(c) && holds_renamed(source, c, name))
        .collect();
    one_of(name, found)
}

/// The one stored name in `found`, `None` for none, or the error for
/// several ([`ambiguous`], offering no recording-label matcher).
fn one_of(name: &str, mut found: Vec<String>) -> Result<Option<String>, QueryError> {
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(ambiguous(name, &found, None)),
    }
}

/// The error for exporter name `name` stored under each of `stored`. It
/// names the form that selects one on any source, a stored name with
/// `__prometheus_name__="<name>"`; with `recording`, a recording label and
/// a value of it, also that recording-label matcher, which selects
/// recordings only over an archive.
pub(crate) fn ambiguous(
    name: &str,
    stored: &[String],
    recording: Option<(&str, &str)>,
) -> QueryError {
    let first = &stored[0];
    let recording = match recording {
        Some((key, value)) => format!(
            ", or select one recording with a recording-label matcher \
             ({name}{{{key}={value:?}}})"
        ),
        None => String::new(),
    };
    QueryError::EvaluationError(format!(
        "no series this selector reaches is named {name}, and series an exporter served as \
         {name} are stored under {}, in different units; name one of them with \
         {first}{{{PROMETHEUS_NAME_LABEL}=\"{name}\"}}{recording}",
        stored.join(" and ")
    ))
}

/// The resolver [`resolve_selectors`] takes: a selector's name and the
/// series it selects to the stored name it reads, if any.
pub(crate) type Resolve<'a> = dyn Fn(&str, &Selection) -> Result<Option<String>, QueryError> + 'a;

/// Point each named selector of `expr` that `resolve` resolves at the
/// stored name, adding the `__prometheus_name__` matcher (to each branch of
/// a selector with `or`). `resolve` takes the selector's name and the
/// series it selects. The first error it returns is returned.
pub(crate) fn resolve_selectors(expr: &mut Expr, resolve: &Resolve) -> Result<(), QueryError> {
    let each = |e: &mut Expr| resolve_selectors(e, resolve);
    match expr {
        Expr::Aggregate(agg) => {
            each(&mut agg.expr)?;
            if let Some(param) = agg.param.as_mut() {
                each(param)?;
            }
            Ok(())
        }
        Expr::Unary(u) => each(&mut u.expr),
        Expr::Binary(b) => {
            each(&mut b.lhs)?;
            each(&mut b.rhs)
        }
        Expr::Paren(p) => each(&mut p.expr),
        Expr::Call(call) => {
            for arg in call.args.args.iter_mut() {
                each(arg)?;
            }
            Ok(())
        }
        Expr::VectorSelector(sel) => resolve_selector(sel, resolve),
        Expr::MatrixSelector(sel) => resolve_selector(&mut sel.vs, resolve),
        Expr::Subquery(s) => each(&mut s.expr),
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::Extension(_) => Ok(()),
    }
}

fn resolve_selector(sel: &mut parser::VectorSelector, resolve: &Resolve) -> Result<(), QueryError> {
    let Some(name) = sel.name.clone() else {
        return Ok(());
    };
    let Some(stored) = resolve(&name, &Selection::new(Some(&name), &sel.matchers))? else {
        return Ok(());
    };
    let renamed = |matchers: &mut Vec<Matcher>| {
        matchers.retain(|m| m.name != "__name__" && m.name != PROMETHEUS_NAME_LABEL);
        matchers.push(Matcher::new(
            PromMatchOp::Equal,
            PROMETHEUS_NAME_LABEL,
            &name,
        ));
    };
    if sel.matchers.or_matchers.is_empty() {
        renamed(&mut sel.matchers.matchers);
    } else {
        // A branch whose `__name__` matchers do not match the name selects
        // nothing. With none left the selector selects nothing, and is left
        // as written: emptied, `or_matchers` would select every series.
        let names = |branch: &Vec<Matcher>| {
            branch
                .iter()
                .filter(|m| m.name == "__name__")
                .all(|m| m.is_match(&name))
        };
        if !sel.matchers.or_matchers.iter().any(names) {
            return Ok(());
        }
        sel.matchers.or_matchers.retain(names);
        sel.matchers.or_matchers.iter_mut().for_each(renamed);
    }
    sel.name = Some(stored);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use metriken_storage::histogram_stream::HistogramStream;
    use metriken_storage::memory::Memory;
    use metriken_storage::types::{Counters, Gauges};

    use super::*;
    use crate::promql::QueryEngine;

    /// `inner`, counting the calls that list its names.
    struct Counting {
        inner: Memory,
        listings: AtomicUsize,
    }

    impl DataSource for Counting {
        fn counters(&self, name: &str, filter: &Labels, s: u64, e: u64) -> Option<Counters> {
            self.inner.counters(name, filter, s, e)
        }
        fn gauges(&self, name: &str, filter: &Labels, s: u64, e: u64) -> Option<Gauges> {
            self.inner.gauges(name, filter, s, e)
        }
        fn histogram_stream(
            &self,
            name: &str,
            filter: &Labels,
            s: u64,
            e: u64,
        ) -> Option<HistogramStream> {
            self.inner.histogram_stream(name, filter, s, e)
        }
        fn interval(&self) -> f64 {
            self.inner.interval()
        }
        fn time_range(&self) -> Option<(u64, u64)> {
            self.inner.time_range()
        }
        fn counter_names(&self) -> Vec<String> {
            self.listings.fetch_add(1, Ordering::Relaxed);
            self.inner.counter_names()
        }
        fn gauge_names(&self) -> Vec<String> {
            self.inner.gauge_names()
        }
        fn histogram_names(&self) -> Vec<String> {
            self.inner.histogram_names()
        }
        fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
            self.inner.counter_labels(name)
        }
        fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
            self.inner.gauge_labels(name)
        }
        fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
            self.inner.histogram_labels(name)
        }
        fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
            self.inner.column_map()
        }
    }

    /// Resolving a query's selectors lists the source's names once, however
    /// many selectors it has, held or not.
    #[test]
    fn a_query_lists_the_names_once() {
        let mut inner = Memory::new(1000);
        inner.upsert_float_counter_sample("a_total", Labels::default(), 1_000_000_000, 1.0);
        let renamed = Labels::from([(PROMETHEUS_NAME_LABEL, "b_seconds_total")]);
        inner.upsert_float_counter_sample("b_nanoseconds_total", renamed, 1_000_000_000, 1.0);
        let source = Arc::new(Counting {
            inner,
            listings: AtomicUsize::new(0),
        });
        let engine = QueryEngine::new(Arc::clone(&source) as Arc<dyn DataSource>);
        let mut expr = parser::parse(
            "a_total + a_total + b_seconds_total + b_seconds_total + c_seconds_total + a_total",
        )
        .unwrap();
        engine.resolve_names(&mut expr).unwrap();
        assert_eq!(source.listings.load(Ordering::Relaxed), 1);
        assert!(expr.to_string().contains("b_nanoseconds_total"), "{expr}");
    }

    /// A selector with `or` naming a series by its exporter name reads, of
    /// the stored name, the series renamed from it that a branch selects,
    /// and not a series the stored name holds as its own.
    #[test]
    fn a_selector_with_or_reads_the_renamed_series_its_branches_select() {
        let mut inner = Memory::new(1000);
        for (t, v) in [(1, 1.0), (2, 2.0), (3, 3.0)] {
            let ts = t * 1_000_000_000;
            for job in ["j", "k"] {
                let renamed =
                    Labels::from([(PROMETHEUS_NAME_LABEL, "b_seconds_total"), ("job", job)]);
                inner.upsert_float_counter_sample("b_nanoseconds_total", renamed, ts, v);
            }
            let own = Labels::from([("job", "k")]);
            inner.upsert_float_counter_sample("b_nanoseconds_total", own, ts, 100.0 * v);
        }
        let engine = QueryEngine::new(Arc::new(inner) as Arc<dyn DataSource>);
        let opts = crate::QueryOptions::default();
        for query in [
            r#"rate(b_seconds_total{job="k" or job="zzz"}[2s])"#,
            r#"irate(b_seconds_total{job="zzz" or job="k"}[2s])"#,
        ] {
            let result = engine
                .query_range_opts(query, 3.0, 3.0, 1.0, &opts)
                .unwrap();
            let crate::QueryResult::Matrix { result, .. } = result else {
                panic!("{query}: not a matrix")
            };
            assert_eq!(result.len(), 1, "{query}: {result:?}");
            assert_eq!(result[0].metric["job"], "k", "{query}: {result:?}");
            assert!(
                result[0].values.iter().all(|(_, v)| *v < 10.0),
                "{query}: {result:?}"
            );
        }
    }

    /// A query fills the engine's names, so an engine that lives for a
    /// reader's life lists them once, not once per query.
    #[test]
    fn a_query_keeps_the_names_for_the_next() {
        let mut inner = Memory::new(1000);
        inner.upsert_float_counter_sample("a_total", Labels::default(), 1_000_000_000, 1.0);
        let engine = QueryEngine::new(Arc::new(inner) as Arc<dyn DataSource>);
        assert!(engine.held.get().is_none());
        engine
            .query_range_opts(
                "rate(a_total[1s])",
                1.0,
                2.0,
                1.0,
                &crate::QueryOptions::default(),
            )
            .unwrap();
        assert!(engine.held.get().is_some(), "the names are kept");
    }
}
