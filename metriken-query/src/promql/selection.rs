//! The series a selector's label matchers select, including a selector
//! with `or` branches (`x{job="a" or job="b"}`).
//!
//! promql-parser puts every matcher of a selector with `or` into
//! `Matchers::or_matchers`, one list per branch, and leaves
//! `Matchers::matchers` empty. A source reads with one filter
//! ([`Labels::matches`]), which holds one matcher per label and cannot
//! express a union. A [`Selection`] therefore reads with the matchers every
//! branch shares, and keeps a series read with them when it satisfies every
//! matcher of at least one branch. A series that two branches select is
//! read once, so it is kept once.

use std::collections::BTreeMap;

use metriken_storage::PROMETHEUS_NAME_LABEL;
use promql_parser::label::{MatchOp as PromMatchOp, Matcher, Matchers};

use crate::histogram_stream::{HistogramStream, HistogramStreamMeta};
use crate::labels::{Labels, MatchOp};
use crate::promql::extract_filter_labels;

/// A selector's label matchers, as a read applies them.
#[derive(Clone, Debug)]
pub(crate) struct Selection {
    /// The filter a read takes. For a selector without `or`, its matchers
    /// other than `__name__` ([`extract_filter_labels`]). For a selector
    /// with `or`, the matchers every branch has, less any non-negated
    /// matcher that matches the empty string (`l=""`, `l=~".*"`): PromQL
    /// passes a series without `l` for those, and [`Labels::matches`] does
    /// not.
    pub(crate) filter: Labels,
    /// For a selector with `or`, its branches without `__name__` matchers;
    /// a series read with `filter` is selected when it satisfies every
    /// matcher of one. Each matcher is applied as Prometheus applies it, a
    /// label a series does not carry taken as the empty string. Empty when
    /// no branch can select the selector's name.
    /// `None` for a selector without `or`, whose series are those `filter`
    /// reads.
    branches: Option<Vec<Vec<Matcher>>>,
}

impl Selection {
    /// The selection of a selector of metric `name` with `matchers`. A
    /// branch whose `__name__` matchers do not match `name` selects
    /// nothing; with no `name`, `__name__` matchers are not consulted.
    pub(crate) fn new(name: Option<&str>, matchers: &Matchers) -> Self {
        if matchers.or_matchers.is_empty() {
            return Self {
                filter: extract_filter_labels(&matchers.matchers),
                branches: None,
            };
        }
        // promql-parser leaves `matchers` empty when there are branches;
        // a matcher there would apply to every branch.
        let branches: Vec<Vec<Matcher>> = matchers
            .or_matchers
            .iter()
            .filter(|branch| {
                name.is_none_or(|name| {
                    matchers
                        .matchers
                        .iter()
                        .chain(branch.iter())
                        .filter(|m| m.name == "__name__")
                        .all(|m| m.is_match(name))
                })
            })
            .map(|branch| {
                matchers
                    .matchers
                    .iter()
                    .chain(branch.iter())
                    .filter(|m| m.name != "__name__")
                    .cloned()
                    .collect()
            })
            .collect();
        let shared: Vec<Matcher> = match branches.split_first() {
            Some((first, rest)) => first
                .iter()
                .filter(|m| rest.iter().all(|branch| branch.contains(m)))
                .filter(|m| filters_alike(m))
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        Self {
            filter: extract_filter_labels(&shared),
            branches: Some(branches),
        }
    }

    /// The selection of a selector whose matchers are `filter`, without
    /// `or`.
    pub(crate) fn from_filter(filter: Labels) -> Self {
        Self {
            filter,
            branches: None,
        }
    }

    /// Whether the selector has `or` branches, which a read with
    /// [`filter`](Self::filter) alone does not apply.
    pub(crate) fn has_branches(&self) -> bool {
        self.branches.is_some()
    }

    /// A filter for each branch, of its matchers that
    /// [`Labels::matches`] applies as PromQL does or more loosely (as
    /// [`filter`](Self::filter) is chosen): a series a branch selects
    /// passes that branch's filter. For a selector without `or`, `filter`
    /// alone. Empty when no branch can select the selector's name.
    #[cfg_attr(not(feature = "archive"), allow(dead_code))]
    pub(crate) fn branch_filters(&self) -> Vec<Labels> {
        match &self.branches {
            None => vec![self.filter.clone()],
            Some(branches) => branches
                .iter()
                .map(|branch| {
                    let alike: Vec<Matcher> = branch
                        .iter()
                        .filter(|m| filters_alike(m))
                        .cloned()
                        .collect();
                    extract_filter_labels(&alike)
                })
                .collect(),
        }
    }

    /// Whether a series with `labels`, read with [`filter`](Self::filter),
    /// is selected: always for a selector without `or`, else when it
    /// satisfies every matcher of a branch.
    pub(crate) fn keeps(&self, labels: &Labels) -> bool {
        self.keeps_set(&labels.inner)
    }

    /// [`keeps`](Self::keeps) for a label set held as a map.
    pub(crate) fn keeps_set(&self, labels: &BTreeMap<String, String>) -> bool {
        let Some(branches) = &self.branches else {
            return true;
        };
        branches.iter().any(|branch| {
            branch.iter().all(|m| {
                let value = labels.get(&m.name).map_or("", String::as_str);
                m.is_match(value)
            })
        })
    }

    /// Whether a series with `labels`, before any read, is selected: it
    /// passes [`filter`](Self::filter) and [`keeps`](Self::keeps).
    pub(crate) fn selects(&self, labels: &Labels) -> bool {
        labels.matches(&self.filter) && self.keeps(labels)
    }

    /// Keep the series of `series` this selection [keeps](Self::keeps),
    /// each with the labels `labels` gives.
    pub(crate) fn retain<T>(&self, series: &mut Vec<T>, labels: impl Fn(&T) -> &Labels) {
        if self.branches.is_some() {
            series.retain(|s| self.keeps(labels(s)));
        }
    }

    /// `stream` with only the series this selection [keeps](Self::keeps),
    /// and their rows.
    pub(crate) fn histogram(&self, stream: HistogramStream) -> HistogramStream {
        if self.branches.is_none() {
            return stream;
        }
        // Each series' position in the kept series, `None` for one left out.
        let mut kept = Vec::new();
        let at: Vec<Option<usize>> = stream
            .meta
            .series
            .into_iter()
            .map(|labels| {
                self.keeps(&labels).then(|| {
                    kept.push(labels);
                    kept.len() - 1
                })
            })
            .collect();
        HistogramStream {
            meta: HistogramStreamMeta {
                config: stream.meta.config,
                series: kept,
            },
            rows: Box::new(stream.rows.filter_map(move |mut row| {
                row.series_idx = at.get(row.series_idx).copied().flatten()?;
                Some(row)
            })),
        }
    }

    /// This selection reading the series an exporter served as `original`
    /// under a stored name: `__prometheus_name__="<original>"` in the
    /// filter, in place of any `__prometheus_name__` matcher a branch has.
    pub(crate) fn renamed(mut self, original: &str) -> Self {
        self.filter.inner.insert(
            PROMETHEUS_NAME_LABEL.to_string(),
            MatchOp::Equal.encode(original),
        );
        if let Some(branches) = &mut self.branches {
            for branch in branches {
                branch.retain(|m| m.name != PROMETHEUS_NAME_LABEL);
            }
        }
        self
    }
}

/// Whether [`Labels::matches`] passes every series `m` passes in PromQL:
/// false for a non-negated matcher that matches the empty string, which
/// PromQL passes for a series without the label and `Labels::matches`
/// does not.
fn filters_alike(m: &Matcher) -> bool {
    matches!(m.op, PromMatchOp::NotEqual | PromMatchOp::NotRe(_)) || !m.is_match("")
}
