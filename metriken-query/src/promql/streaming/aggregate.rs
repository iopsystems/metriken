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

use crate::labels::Labels;

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

/// Thin wrapper used by tests; the general API is
/// `aggregate(input, AggOp::Sum, GroupBy::Include(labels))`.
#[cfg(test)]
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

pub(crate) fn derive_group_labels(labels: &Labels, group_by: GroupBy<'_>) -> Labels {
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
            if let Some(&p) = c.peek() {
                let t = p.t;
                min_ts = Some(min_ts.map_or(t, |m| m.min(t)));
            }
        }
        let t = min_ts?;

        let mut sum = 0.0;
        let mut count = 0u32;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        // Interval arithmetic for sum/avg: a windowless child contributes its
        // point value as a degenerate band [v, v].
        let mut any_bounded = false;
        let (mut lo_sum, mut hi_sum) = (0.0f64, 0.0f64);

        for c in self.children.iter_mut() {
            let take = matches!(c.peek(), Some(&p) if p.t == t);
            if take {
                let p = c.next().expect("peek returned Some, next must too");
                let v = p.v;
                sum += v;
                count += 1;
                if v < min {
                    min = v;
                }
                if v > max {
                    max = v;
                }
                let (lo, hi) = p.bounds.unwrap_or((v, v));
                if p.bounds.is_some() {
                    any_bounded = true;
                }
                lo_sum += lo;
                hi_sum += hi;
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
        // sum/avg propagate the band by interval arithmetic ([Σlo, Σhi], /n),
        // and the nominal stays inside it (each child band contains its own
        // nominal). min/max are declined: which series is the extremum is
        // uncertain, so the nominal can fall outside the true interval. count is
        // exact.
        let bounds = if any_bounded {
            match self.op {
                AggOp::Sum => Some((lo_sum, hi_sum)),
                AggOp::Avg => Some((lo_sum / count as f64, hi_sum / count as f64)),
                AggOp::Min | AggOp::Max | AggOp::Count => None,
            }
        } else {
            None
        };
        Some(Point { t, v, bounds })
    }
}

#[cfg(test)]
mod interval_tests {
    use super::*;

    fn one(t: u64, v: f64, b: Option<(f64, f64)>) -> Box<dyn Iterator<Item = Point>> {
        Box::new(std::iter::once(Point { t, v, bounds: b }))
    }

    #[test]
    fn sum_propagates_interval_arithmetic() {
        // sum([9,11] + [18,22]) = [27, 33]; nominal 30 stays inside.
        let mut mr = MergeReduce::new(
            vec![
                one(1, 10.0, Some((9.0, 11.0))),
                one(1, 20.0, Some((18.0, 22.0))),
            ],
            AggOp::Sum,
        );
        let p = mr.next().unwrap();
        assert_eq!(p.v, 30.0);
        assert_eq!(p.bounds, Some((27.0, 33.0)));
        assert!(mr.next().is_none());
    }

    #[test]
    fn avg_propagates_scaled_interval() {
        let mut mr = MergeReduce::new(
            vec![
                one(1, 10.0, Some((9.0, 11.0))),
                one(1, 20.0, Some((18.0, 22.0))),
            ],
            AggOp::Avg,
        );
        let p = mr.next().unwrap();
        assert_eq!(p.v, 15.0);
        assert_eq!(p.bounds, Some((13.5, 16.5))); // [27/2, 33/2]
    }

    #[test]
    fn min_declines_bounds() {
        // Nominal min = 5 (series A), but B could dip to 1: the honest true-min
        // interval [1,3] excludes the nominal, so min declines a band.
        let mut mr = MergeReduce::new(
            vec![
                one(1, 5.0, Some((4.0, 100.0))),
                one(1, 10.0, Some((1.0, 3.0))),
            ],
            AggOp::Min,
        );
        let p = mr.next().unwrap();
        assert_eq!(p.v, 5.0);
        assert!(p.bounds.is_none());
    }

    #[test]
    fn sum_windowless_children_no_band() {
        let mut mr = MergeReduce::new(vec![one(1, 10.0, None), one(1, 20.0, None)], AggOp::Sum);
        let p = mr.next().unwrap();
        assert_eq!(p.v, 30.0);
        assert!(p.bounds.is_none());
    }
}
