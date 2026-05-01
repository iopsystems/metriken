//! `sum/avg/min/max/count [by | without (..)]` as streaming aggregators.
//!
//! Children are bucketed by their reduced label set up front; for each
//! group we build a [`MergeReduce`] iterator that pulls one point at a
//! time from every child belonging to that group, then reduces the
//! values sharing a timestamp into a single output via [`AggOp`].
//!
//! Aggregation is the model's main barrier: a group's emitted point at
//! time `t` requires having peeked all children at `t`. State is one
//! [`std::iter::Peekable`] per child — typically a single buffered
//! `Point` (16 bytes) per series — so an aggregate over `S` children
//! costs `O(S)` resident bytes regardless of stream length.
//!
//! The merge tolerates ragged inputs (children that skip timestamps);
//! the smallest peeked timestamp wins each tick, and only children
//! holding that exact timestamp contribute to the reduction. Children
//! with aligned grids (the common case for step-aligned PromQL queries)
//! degenerate to a straight-line reduce.

use std::collections::HashMap;

use crate::tsdb::Labels;

use super::{LabeledSeries, Point, SeriesSet};

/// Reduction operator. Mirrors the PromQL aggregate operations the
/// streaming dispatcher recognises.
#[derive(Copy, Clone, Debug)]
pub enum AggOp {
    Sum,
    Avg,
    Min,
    Max,
    Count,
}

/// How to derive a group key from each input series's labels.
///
/// `Include` keeps only the listed labels (the eager engine's
/// `LabelModifier::Include`, i.e. `by (..)`).  `Exclude` keeps every
/// label *except* the listed ones, and also drops the synthetic
/// `__name__` label — same shape as the eager engine's
/// `LabelModifier::Exclude` (`without (..)`).
#[derive(Copy, Clone, Debug)]
pub enum GroupBy<'a> {
    Include(&'a [String]),
    Exclude(&'a [String]),
}

/// Backwards-compatible thin wrapper. Older callers pass a list of
/// labels meaning "group by these"; the equivalent on the general
/// API is `aggregate(input, AggOp::Sum, GroupBy::Include(labels))`.
pub fn sum_by<'a>(input: SeriesSet<'a>, by_labels: &[String]) -> SeriesSet<'a> {
    aggregate(input, AggOp::Sum, GroupBy::Include(by_labels))
}

/// Group `input` by the reduced label set selected via `group_by`,
/// then emit one [`LabeledSeries`] per group whose iterator reduces
/// all member iterators per timestamp using `op`.
pub fn aggregate<'a>(input: SeriesSet<'a>, op: AggOp, group_by: GroupBy<'_>) -> SeriesSet<'a> {
    let mut groups: HashMap<Labels, Vec<Box<dyn Iterator<Item = Point> + 'a>>> = HashMap::new();
    for ls in input {
        let group_labels = derive_group_labels(&ls.labels, group_by);
        groups.entry(group_labels).or_default().push(ls.iter);
    }

    groups
        .into_iter()
        .map(|(labels, children)| LabeledSeries::new(labels, MergeReduce::new(children, op)))
        .collect()
}

fn derive_group_labels(labels: &Labels, group_by: GroupBy<'_>) -> Labels {
    let mut out = Labels::default();
    match group_by {
        GroupBy::Include(by) => {
            for k in by {
                if let Some(v) = labels.inner.get(k) {
                    out.inner.insert(k.clone(), v.clone());
                }
            }
        }
        GroupBy::Exclude(without) => {
            for (k, v) in &labels.inner {
                if k == "__name__" {
                    continue;
                }
                if without.iter().any(|x| x == k) {
                    continue;
                }
                out.inner.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

/// Per-group merge reducer. Pulls one point per child whose peeked
/// timestamp equals the smallest among children, applies `op`, emits
/// one output point per timestamp tick.
pub struct MergeReduce<'a> {
    children: Vec<std::iter::Peekable<Box<dyn Iterator<Item = Point> + 'a>>>,
    op: AggOp,
}

impl<'a> MergeReduce<'a> {
    pub fn new(children: Vec<Box<dyn Iterator<Item = Point> + 'a>>, op: AggOp) -> Self {
        Self {
            children: children.into_iter().map(Iterator::peekable).collect(),
            op,
        }
    }
}

impl<'a> Iterator for MergeReduce<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        let mut min_ts: Option<u64> = None;
        for c in self.children.iter_mut() {
            if let Some(&(t, _)) = c.peek() {
                min_ts = Some(min_ts.map_or(t, |m| m.min(t)));
            }
        }
        let t = min_ts?;

        let mut sum = 0.0;
        let mut count = 0u32;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;

        for c in self.children.iter_mut() {
            let take = matches!(c.peek(), Some(&(ts, _)) if ts == t);
            if take {
                let (_, v) = c.next().expect("peek returned Some, next must too");
                sum += v;
                count += 1;
                if v < min {
                    min = v;
                }
                if v > max {
                    max = v;
                }
            }
        }

        if count == 0 {
            return None;
        }

        let v = match self.op {
            AggOp::Sum => sum,
            AggOp::Avg => sum / count as f64,
            AggOp::Min => min,
            AggOp::Max => max,
            AggOp::Count => count as f64,
        };
        Some((t, v))
    }
}
