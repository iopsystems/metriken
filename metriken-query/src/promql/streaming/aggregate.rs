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
//!
//! Several groups advance together ([`Lockstep`]): reading any group's
//! next point computes every group's points up to that timestamp and
//! buffers those not yet read. A consumer that reads one group to its end
//! before the next still pulls every child in timestamp order, and buffers
//! every other live group's points meanwhile: 88 bytes each, plus the
//! buffers' spare capacity. A group whose reader is dropped is no longer
//! advanced, and an exhausted group's reader gets `None` without advancing
//! the others.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use crate::labels::{is_matched_label, Labels};

use super::{Held, Join, LabeledSeries, Point, SeriesSet};

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
    aggregate(input, AggOp::Sum, GroupBy::Include(by_labels), Join::Exact)
}

/// Group `input` by the reduced label set selected via `group_by`,
/// then emit one [`LabeledSeries`] per group whose iterator reduces
/// all member iterators per timestamp using `op`.
pub fn aggregate<'a>(
    input: SeriesSet<'a>,
    op: AggOp,
    group_by: GroupBy<'_>,
    join: Join,
) -> SeriesSet<'a> {
    let mut groups: HashMap<Labels, Vec<Box<dyn Iterator<Item = Point> + 'a>>> = HashMap::new();
    for ls in input {
        let group_labels = derive_group_labels(&ls.labels, group_by);
        groups.entry(group_labels).or_default().push(ls.iter);
    }

    if groups.len() < 2 {
        return groups
            .into_iter()
            .map(|(labels, children)| {
                LabeledSeries::new(labels, MergeReduce::joined(children, op, join))
            })
            .collect();
    }
    let (labels, merges): (Vec<Labels>, Vec<MergeReduce<'a>>) = groups
        .into_iter()
        .map(|(labels, children)| (labels, MergeReduce::joined(children, op, join)))
        .unzip();
    let shared = Rc::new(RefCell::new(Lockstep {
        buffers: merges.iter().map(|_| VecDeque::new()).collect(),
        heads: vec![None; merges.len()],
        live: vec![true; merges.len()],
        started: false,
        merges,
    }));
    labels
        .into_iter()
        .enumerate()
        .map(|(index, labels)| {
            LabeledSeries::new(
                labels,
                LockstepGroup {
                    shared: Rc::clone(&shared),
                    index,
                },
            )
        })
        .collect()
}

/// Several groups' merges, advanced together through time.
struct Lockstep<'a> {
    merges: Vec<MergeReduce<'a>>,
    /// Points computed and not yet read, by group.
    buffers: Vec<VecDeque<Point>>,
    /// Each merge's next timestamp; `None` once it is exhausted.
    heads: Vec<Option<u64>>,
    /// Whether the group's reader still exists.
    live: Vec<bool>,
    /// Whether `heads` has been filled.
    started: bool,
}

impl Lockstep<'_> {
    /// The next point of group `index`, advancing every live group whose
    /// next timestamp is the smallest until that group has one.
    fn next(&mut self, index: usize) -> Option<Point> {
        if !self.started {
            for g in 0..self.merges.len() {
                if self.live[g] {
                    self.heads[g] = self.merges[g].next_ts();
                }
            }
            self.started = true;
        }
        if self.buffers[index].is_empty() && self.heads[index].is_none() {
            return None;
        }
        while self.buffers[index].is_empty() {
            let t = self
                .heads
                .iter()
                .zip(&self.live)
                .filter_map(|(h, live)| h.filter(|_| *live))
                .min()?;
            for g in 0..self.merges.len() {
                if self.live[g] && self.heads[g] == Some(t) {
                    self.buffers[g].extend(self.merges[g].next());
                    self.heads[g] = self.merges[g].next_ts();
                }
            }
        }
        self.buffers[index].pop_front()
    }
}

/// One group of a [`Lockstep`].
struct LockstepGroup<'a> {
    shared: Rc<RefCell<Lockstep<'a>>>,
    index: usize,
}

impl Iterator for LockstepGroup<'_> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        self.shared.borrow_mut().next(self.index)
    }
}

impl Drop for LockstepGroup<'_> {
    fn drop(&mut self) {
        let Ok(mut shared) = self.shared.try_borrow_mut() else {
            return;
        };
        shared.live[self.index] = false;
        shared.buffers[self.index] = VecDeque::new();
        // Its children are never read again.
        shared.merges[self.index].clear();
        shared.heads[self.index] = None;
    }
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
            // Internal labels go with the named ones: an aggregate is over a
            // set of series, and which incarnation or run each input was is
            // meaningless for the sum. `__name__` was always dropped here;
            // the rule is now the prefix, not the one name. `__recording__`
            // stays, as it does in matching (`is_matched_label`): two
            // recordings with identical labels are two sets of series.
            for (k, v) in &labels.inner {
                if !is_matched_label(k) {
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

/// Per-group merge reducer. Emits one output point per timestamp: under
/// [`Join::Exact`] it pulls one point per child whose peeked timestamp equals
/// the smallest among children; under [`Join::Latest`] every child
/// contributes its latest point while held ([`Held`]).
pub struct MergeReduce<'a> {
    children: Children<'a>,
    op: AggOp,
}

enum Children<'a> {
    Exact(Vec<std::iter::Peekable<Box<dyn Iterator<Item = Point> + 'a>>>),
    Latest(Vec<Held<'a>>),
}

impl<'a> MergeReduce<'a> {
    #[cfg(test)]
    pub fn new(children: Vec<Box<dyn Iterator<Item = Point> + 'a>>, op: AggOp) -> Self {
        Self::joined(children, op, Join::Exact)
    }

    pub(crate) fn joined(
        children: Vec<Box<dyn Iterator<Item = Point> + 'a>>,
        op: AggOp,
        join: Join,
    ) -> Self {
        let children = match join {
            Join::Exact => Children::Exact(children.into_iter().map(Iterator::peekable).collect()),
            Join::Latest => Children::Latest(children.into_iter().map(Held::new).collect()),
        };
        Self { children, op }
    }

    /// The timestamp of the next point, without emitting it.
    fn next_ts(&mut self) -> Option<u64> {
        match &mut self.children {
            Children::Exact(cs) => cs.iter_mut().filter_map(|c| c.peek().map(|p| p.t)).min(),
            Children::Latest(cs) => cs.iter_mut().filter_map(|c| c.peek_t()).min(),
        }
    }

    /// Drop every child: the group is read no more.
    fn clear(&mut self) {
        match &mut self.children {
            Children::Exact(cs) => cs.clear(),
            Children::Latest(cs) => cs.iter_mut().for_each(Held::clear),
        }
    }
}

impl<'a> Iterator for MergeReduce<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        let t = self.next_ts()?;
        let mut acc = Reduction::default();
        match &mut self.children {
            Children::Exact(cs) => {
                for c in cs.iter_mut() {
                    if matches!(c.peek(), Some(&p) if p.t == t) {
                        acc.add(c.next().expect("peek returned Some, next must too"));
                    }
                }
            }
            Children::Latest(cs) => {
                for c in cs.iter_mut() {
                    c.advance_to(t);
                    if let Some(p) = c.at(t) {
                        acc.add(p);
                    }
                }
            }
        }
        acc.finish(self.op, t)
    }
}

/// The points one output point reduces.
struct Reduction {
    sum: f64,
    count: u32,
    min: f64,
    max: f64,
    // Interval arithmetic for sum/avg: a windowless child contributes its
    // point value as a degenerate band [v, v].
    any_bounded: bool,
    any_interpolated: bool,
    lo_sum: f64,
    hi_sum: f64,
    // Unanimity tracking for the acquisition edges: `Some(e)` while every
    // contributor so far carried exactly `e`, `None` the moment one differs
    // or lacks them.
    shared_edges: Option<super::RateEdges>,
    edges_unanimous: bool,
    seen_any: bool,
}

impl Default for Reduction {
    fn default() -> Self {
        Self {
            sum: 0.0,
            count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            any_bounded: false,
            any_interpolated: false,
            lo_sum: 0.0,
            hi_sum: 0.0,
            shared_edges: None,
            edges_unanimous: true,
            seen_any: false,
        }
    }
}

impl Reduction {
    fn add(&mut self, p: Point) {
        let v = p.v;
        self.sum += v;
        self.count += 1;
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
        }
        let (lo, hi) = p.bounds.unwrap_or((v, v));
        if p.bounds.is_some() {
            self.any_bounded = true;
        }
        // Any contributor spanning an unobserved stretch makes the
        // aggregate one too — same rule `bounds` follows.
        self.any_interpolated |= p.interpolated;
        if !self.seen_any {
            self.shared_edges = p.edges;
            self.seen_any = true;
        } else if self.shared_edges != p.edges {
            self.edges_unanimous = false;
        }
        self.lo_sum += lo;
        self.hi_sum += hi;
    }

    fn finish(self, op: AggOp, t: u64) -> Option<Point> {
        let count = self.count;
        if count == 0 {
            return None;
        }

        let v = match op {
            AggOp::Sum => self.sum,
            AggOp::Avg => self.sum / count as f64,
            AggOp::Min => self.min,
            AggOp::Max => self.max,
            AggOp::Count => count as f64,
        };
        // sum/avg propagate the band by interval arithmetic ([Σlo, Σhi], /n),
        // and the nominal stays inside it (each child band contains its own
        // nominal). min/max are declined: which series is the extremum is
        // uncertain, so the nominal can fall outside the true interval. count is
        // exact.
        //
        // An aggregate over ANY interpolated contributor is itself
        // interpolated, and drops its band with it: summing the members that do
        // have bands would produce one covering only part of what went in,
        // while presenting as if it covered all of it. The producer's invariant
        // — an interpolated point carries no band — has to survive the
        // operators or it means nothing downstream.
        let bounds = if self.any_bounded && !self.any_interpolated {
            match op {
                AggOp::Sum => Some((self.lo_sum, self.hi_sum)),
                AggOp::Avg => Some((self.lo_sum / count as f64, self.hi_sum / count as f64)),
                AggOp::Min | AggOp::Max | AggOp::Count => None,
            }
        } else {
            None
        };
        let edges = if self.edges_unanimous {
            self.shared_edges
        } else {
            None
        };
        // An aggregate keeps its edges only when every contributing point
        // came from the SAME read. `sum(irate(cpu_usage[5m]))` over 32 CPUs is
        // one acquisition group with one window, so it does — and keeping them
        // is what lets a later `/ cpu_cores` see that it is crossing a table.
        // Aggregating across groups yields no single read, hence no edges.
        Some(Point {
            t,
            v,
            bounds,
            edges,
            interpolated: self.any_interpolated,
        })
    }
}

#[cfg(test)]
mod group_label_tests {
    use super::*;

    /// `without` drops every internal label, not only `__name__`: an
    /// aggregate is over a set of series, and which run or incarnation each
    /// input was is meaningless for the result. Two series that differ only
    /// in an internal label must land in one group.
    #[test]
    fn without_drops_internal_labels_as_it_drops_name() {
        let a = Labels::from([
            ("__name__", "cpu"),
            ("__run__", "0"),
            ("__incarnation__", "a"),
            ("cpu", "1"),
            ("mode", "user"),
        ]);
        let b = Labels::from([("__incarnation__", "b"), ("cpu", "1"), ("mode", "user")]);
        let without = ["mode".to_string()];
        let ga = derive_group_labels(&a, GroupBy::Exclude(&without));
        let gb = derive_group_labels(&b, GroupBy::Exclude(&without));
        assert_eq!(ga, Labels::from([("cpu", "1")]), "{ga:?}");
        assert_eq!(ga, gb, "the two incarnations aggregate as one");
    }

    /// `without` keeps `__recording__`: two recordings with identical
    /// labels are two sets of series, and `sum without (cpu)` over them is a
    /// sum per recording.
    #[test]
    fn without_keeps_the_recording_label() {
        let a = Labels::from([("__recording__", "0"), ("cpu", "1"), ("host", "h")]);
        let b = Labels::from([("__recording__", "1"), ("cpu", "1"), ("host", "h")]);
        let without = ["cpu".to_string()];
        let ga = derive_group_labels(&a, GroupBy::Exclude(&without));
        let gb = derive_group_labels(&b, GroupBy::Exclude(&without));
        assert_eq!(
            ga,
            Labels::from([("__recording__", "0"), ("host", "h")]),
            "{ga:?}"
        );
        assert_ne!(ga, gb);
    }

    /// `by` is unchanged: it keeps exactly what it names, and naming an
    /// internal label is how a caller asks to keep the split.
    #[test]
    fn by_keeps_an_internal_label_when_asked() {
        let a = Labels::from([("__run__", "1"), ("cpu", "1")]);
        let by = ["__run__".to_string()];
        assert_eq!(
            derive_group_labels(&a, GroupBy::Include(&by)),
            Labels::from([("__run__", "1")])
        );
    }
}

#[cfg(test)]
mod interval_tests {
    use super::*;

    fn one(t: u64, v: f64, b: Option<(f64, f64)>) -> Box<dyn Iterator<Item = Point>> {
        Box::new(std::iter::once(Point {
            t,
            v,
            bounds: b,
            edges: None,
            interpolated: false,
        }))
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

#[cfg(test)]
mod lockstep_tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A series at timestamps 1..=3 that records `(series, t)` as it is
    /// pulled.
    fn recorded<'a>(
        id: &'static str,
        group: &'static str,
        log: &Rc<RefCell<Vec<(&'static str, u64)>>>,
    ) -> LabeledSeries<'a> {
        let log = Rc::clone(log);
        LabeledSeries::new(
            Labels::from([("id", id), ("group", group)]),
            (1..=3u64).map(move |t| {
                log.borrow_mut().push((id, t));
                Point {
                    t,
                    v: 1.0,
                    bounds: None,
                    edges: None,
                    interpolated: false,
                }
            }),
        )
    }

    /// Reading one group to its end pulls every group's children through
    /// time together, and each group still gets all of its points.
    #[test]
    fn groups_advance_together() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let input = vec![
            recorded("a", "x", &log),
            recorded("b", "x", &log),
            recorded("c", "y", &log),
        ];
        let by = ["group".to_string()];
        let mut groups = aggregate(input, AggOp::Sum, GroupBy::Include(&by), Join::Exact);
        groups.sort_by(|a, b| a.labels.inner.cmp(&b.labels.inner));
        let mut drained = Vec::new();
        for g in groups {
            drained.push(g.iter.map(|p| (p.t, p.v)).collect::<Vec<_>>());
        }
        assert_eq!(drained[0], vec![(1, 2.0), (2, 2.0), (3, 2.0)]);
        assert_eq!(drained[1], vec![(1, 1.0), (2, 1.0), (3, 1.0)]);
        // The second group's child is pulled at t=1 before the first group's
        // reach t=3, though the first group was read to its end first.
        let pulls = log.borrow();
        let at = |p: (&str, u64)| pulls.iter().position(|x| *x == p).unwrap();
        assert!(at(("c", 1)) < at(("a", 3)), "{pulls:?}");
    }

    /// A group whose reader is dropped is no longer advanced: its child is
    /// pulled no further than the first point a merge peeks at.
    #[test]
    fn a_dropped_group_is_not_advanced() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let input = vec![recorded("a", "x", &log), recorded("c", "y", &log)];
        let by = ["group".to_string()];
        let mut groups = aggregate(input, AggOp::Sum, GroupBy::Include(&by), Join::Exact);
        groups.sort_by(|a, b| a.labels.inner.cmp(&b.labels.inner));
        let y = groups.pop().unwrap();
        drop(y);
        let x = groups.pop().unwrap();
        assert_eq!(x.iter.count(), 3);
        let pulls = log.borrow();
        let c = pulls.iter().filter(|(id, _)| *id == "c").count();
        assert!(c <= 1, "{pulls:?}");
    }

    /// Asking an exhausted group for its next point does not run the other
    /// groups to their ends.
    #[test]
    fn an_exhausted_group_does_not_advance_the_others() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let short = {
            let log = Rc::clone(&log);
            LabeledSeries::new(
                Labels::from([("id", "a"), ("group", "x")]),
                (1..=2u64).map(move |t| {
                    log.borrow_mut().push(("a", t));
                    Point {
                        t,
                        v: 1.0,
                        bounds: None,
                        edges: None,
                        interpolated: false,
                    }
                }),
            )
        };
        let long = {
            let log = Rc::clone(&log);
            LabeledSeries::new(
                Labels::from([("id", "b"), ("group", "y")]),
                (1..=1000u64).map(move |t| {
                    log.borrow_mut().push(("b", t));
                    Point {
                        t,
                        v: 1.0,
                        bounds: None,
                        edges: None,
                        interpolated: false,
                    }
                }),
            )
        };
        let by = ["group".to_string()];
        let mut groups = aggregate(
            vec![short, long],
            AggOp::Sum,
            GroupBy::Include(&by),
            Join::Exact,
        );
        groups.sort_by(|a, b| a.labels.inner.cmp(&b.labels.inner));
        let mut x = groups.remove(0).iter;
        assert_eq!(x.by_ref().count(), 2);
        assert!(x.next().is_none());
        let b = log.borrow().iter().filter(|(id, _)| *id == "b").count();
        assert!(b <= 4, "b pulled {b} times");
    }
}
