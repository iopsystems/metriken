//! `sum by (labels)` as a streaming aggregator.
//!
//! Children are bucketed by their reduced label set up front; for each
//! group we build a [`SumMerge`] iterator that pulls one point at a
//! time from every child belonging to that group, summing values that
//! share a timestamp.
//!
//! Aggregation is the model's main barrier: a group's emitted point at
//! time `t` requires having peeked all children at `t`. State is one
//! [`std::iter::Peekable`] per child — typically a single buffered
//! `Point` (16 bytes) per series — so an aggregate over `S` children
//! costs `O(S)` resident bytes regardless of stream length.
//!
//! The merge tolerates ragged inputs (children that skip timestamps);
//! the smallest peeked timestamp wins each tick, and only children
//! holding that exact timestamp contribute to the sum. Children with
//! aligned grids (the common case for step-aligned PromQL queries)
//! degenerate to a straight-line sum.

use std::collections::HashMap;

use crate::tsdb::Labels;

use super::{LabeledSeries, Point, SeriesSet};

/// Group `input` by the reduced label set selected via `by_labels`,
/// then emit one [`LabeledSeries`] per group whose iterator sums all
/// member iterators per timestamp.
pub fn sum_by<'a>(input: SeriesSet<'a>, by_labels: &[String]) -> SeriesSet<'a> {
    let mut groups: HashMap<Labels, Vec<Box<dyn Iterator<Item = Point> + 'a>>> = HashMap::new();
    for ls in input {
        let mut group_labels = Labels::default();
        for k in by_labels {
            if let Some(v) = ls.labels.inner.get(k) {
                group_labels.inner.insert(k.clone(), v.clone());
            }
        }
        groups.entry(group_labels).or_default().push(ls.iter);
    }

    groups
        .into_iter()
        .map(|(labels, children)| LabeledSeries::new(labels, SumMerge::new(children)))
        .collect()
}

pub struct SumMerge<'a> {
    children: Vec<std::iter::Peekable<Box<dyn Iterator<Item = Point> + 'a>>>,
}

impl<'a> SumMerge<'a> {
    pub fn new(children: Vec<Box<dyn Iterator<Item = Point> + 'a>>) -> Self {
        Self {
            children: children.into_iter().map(Iterator::peekable).collect(),
        }
    }
}

impl<'a> Iterator for SumMerge<'a> {
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
        let mut any = false;
        for c in self.children.iter_mut() {
            let take = matches!(c.peek(), Some(&(ts, _)) if ts == t);
            if take {
                let (_, v) = c.next().expect("peek returned Some, next must too");
                sum += v;
                any = true;
            }
        }

        if any {
            Some((t, sum))
        } else {
            None
        }
    }
}
