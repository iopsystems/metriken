//! Streaming binary operators (`+`, `-`, `*`, `/`).
//!
//! Three shapes are wired up:
//!
//! * **Matrix × Scalar / Scalar × Matrix** — [`ScalarBroadcast`]
//!   wraps an upstream point iterator and applies the binop with a
//!   constant on the other side. Pure pass-through state.
//! * **Matrix × Matrix** — [`ZipMergeBinary`] pairs one left-side
//!   iterator with one right-side iterator and emits the binop at
//!   timestamps where both have a sample.
//! * **Series-set joining** — [`matrix_matrix_op`] groups the right
//!   side by label-match key (per [`MatchSpec`]) and pairs each
//!   left-side series with its matching right-side series. Series
//!   that don't find a match are dropped.
//!
//! When no `on()`/`ignoring()` modifier is given AND exactly one
//! right-side series is left after the keyed join, that singleton is
//! broadcast against every unmatched left series via
//! [`RightLookupBinary`] (materialises the one right series's points
//! into a shared timestamp lookup). Mirrors the eager engine's
//! per-left-series fallback.
//!
//! What's NOT covered here:
//!
//! * `group_left` / `group_right` (one-to-many matching) — needs an
//!   iterator-tee mechanism that defeats streaming.
//! * Comparison operators (`==`, `!=`, `<`, `>`, `<=`, `>=`) and
//!   the `bool` modifier — not implemented in the eager engine
//!   today either.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use promql_parser::parser::token::TokenType;

use crate::labels::{is_matched_label, Labels};

use super::{Band, Held, Join, LabeledSeries, Point, RateEdges, SeriesSet};

/// Timestamp → `(value, optional band)` lookup for the right singleton in a
/// single-right broadcast. Aliased to keep the `Rc<HashMap<…>>` readable
/// (clears clippy's `type_complexity`).
/// Materialised right-hand series for the single-right broadcast: value,
/// band, and the acquisition edges the band came from. The edges are carried
/// for the same reason the zip path carries them — without them this path
/// could not tell a same-read combination from a cross-table one.
type RightLookup = HashMap<u64, (f64, Option<Band>, Option<RateEdges>, bool)>;

/// A series' points, not yet pulled.
type PointIter<'a> = Box<dyn Iterator<Item = Point> + 'a>;

/// Subset of PromQL binary operators the streaming pipeline
/// recognises. Maps directly onto the eager `apply_binary_op`
/// branches. `from_token` returns `None` for any token outside this
/// set so the dispatcher falls through to the eager path cleanly.
#[derive(Copy, Clone, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl BinOp {
    pub fn from_token(t: &TokenType) -> Option<Self> {
        match t.to_string().as_str() {
            "+" => Some(Self::Add),
            "-" => Some(Self::Sub),
            "*" => Some(Self::Mul),
            "/" => Some(Self::Div),
            _ => None,
        }
    }

    /// Apply the binop. Returns `None` for a division by zero so
    /// the caller can drop the point — matching the eager engine,
    /// which uses `continue` in the same case.
    pub fn apply(self, lhs: f64, rhs: f64) -> Option<f64> {
        match self {
            Self::Add => Some(lhs + rhs),
            Self::Sub => Some(lhs - rhs),
            Self::Mul => Some(lhs * rhs),
            Self::Div => {
                if rhs != 0.0 {
                    Some(lhs / rhs)
                } else {
                    None
                }
            }
        }
    }
}

/// Interval arithmetic for a binary op over two uncertainty bands. The four
/// corner combinations cover every sign case for `*` (and add/sub/div, which are
/// monotone in each operand, land on corners too); `/` returns `None` when the
/// denominator band spans zero (the quotient is unbounded). The nominal
/// `op(v_l, v_r)` always lies inside the returned band, since each operand's
/// value lies inside its own band.
pub(crate) fn interval_binop(op: BinOp, l: (f64, f64), r: (f64, f64)) -> Option<(f64, f64)> {
    let (a, b) = l;
    let (c, d) = r;
    let corners = match op {
        BinOp::Add => [a + c, a + d, b + c, b + d],
        BinOp::Sub => [a - c, a - d, b - c, b - d],
        BinOp::Mul => [a * c, a * d, b * c, b * d],
        BinOp::Div => {
            if c <= 0.0 && d >= 0.0 {
                return None; // denominator band contains 0 → unbounded
            }
            [a / c, a / d, b / c, b / d]
        }
    };
    if corners.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let lo = corners.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = corners.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Some((lo, hi))
}

/// Widen a band to the one it would have had over a LONGER admissible span.
///
/// A rate band is `increase / elapsed`, where the elapsed span runs between the
/// interpolated window edges of the range's endpoints — widest edges give the
/// slowest rate, narrowest the fastest. So stretching the edges scales the band
/// by a pure ratio, and that is what is applied here rather than recomputing
/// from a stored numerator.
///
/// Scaling rather than recomputing is what makes this composable. By the time a
/// band reaches a binary op it may have been through `sum` over 32 series, an
/// `avg`, or a scalar multiply; it is no longer any single `increase/elapsed`.
/// But every one of those operations is linear in the increase and shares one
/// elapsed span (they preserve edges only when every contributor came from the
/// same read), so the SAME ratio still applies exactly.
///
/// Never narrows: each endpoint keeps whichever of the two is more
/// conservative, so a degenerate or inverted span can only leave the band
/// alone.
fn widen_band(band: Band, own: &RateEdges, union: &RateEdges, nominal: f64) -> Band {
    let own_max = own.right.1 - own.left.0;
    let own_min = own.right.0 - own.left.1;
    let union_max = union.right.1 - union.left.0;
    let union_min = union.right.0 - union.left.1;
    if own_max <= 0.0 || own_min <= 0.0 || union_max <= 0.0 || union_min <= 0.0 {
        return band;
    }
    // lo = increase/elapsed_max, so a longer max span lowers it; hi is the
    // mirror image.
    let lo = band.0 * (own_max / union_max);
    let hi = band.1 * (own_min / union_min);
    (band.0.min(lo).min(nominal), band.1.max(hi).max(nominal))
}

/// Combine two operands' optional bands for `op`.
///
/// If either side carries a real band, propagate (the bandless side is exact,
/// `[v, v]`); if neither does, the result has no band.
///
/// **Cross-table operands are widened first.** Two points whose acquisition
/// edges differ were read at different instants, and combining them as if
/// simultaneous is an approximation their own bands do not account for. Each
/// side's band is therefore re-derived over the UNION of both sides' edges —
/// the span within which either observation could have happened — before the
/// two are combined. Identical edges mean the same read (an acquisition group
/// is one read with one window), so nothing is widened, which is the common
/// case and stays exactly as tight as before.
fn combine_bounds(
    op: BinOp,
    lv: f64,
    lb: Option<(f64, f64)>,
    le: Option<RateEdges>,
    rv: f64,
    rb: Option<(f64, f64)>,
    re: Option<RateEdges>,
) -> Option<(f64, f64)> {
    if lb.is_none() && rb.is_none() {
        return None;
    }
    let (mut lb, mut rb) = (lb, rb);
    if let (Some(l), Some(r)) = (le, re) {
        if l != r {
            let union = RateEdges {
                left: (l.left.0.min(r.left.0), l.left.1.max(r.left.1)),
                right: (l.right.0.min(r.right.0), l.right.1.max(r.right.1)),
            };
            lb = lb.map(|b| widen_band(b, &l, &union, lv));
            rb = rb.map(|b| widen_band(b, &r, &union, rv));
        }
    }
    interval_binop(op, lb.unwrap_or((lv, lv)), rb.unwrap_or((rv, rv)))
}

/// Wrap an upstream `(t, v)` iterator, applying `op` against a
/// constant scalar on every emitted point.  `scalar_first = true`
/// for `scalar OP matrix`, `false` for `matrix OP scalar` — the
/// distinction matters for non-commutative ops (`-`, `/`).
pub struct ScalarBroadcast<I> {
    upstream: I,
    op: BinOp,
    scalar: f64,
    scalar_first: bool,
}

impl<I: Iterator<Item = Point>> Iterator for ScalarBroadcast<I> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        let (op, scalar, scalar_first) = (self.op, self.scalar, self.scalar_first);
        let apply = |x: f64| -> Option<f64> {
            if scalar_first {
                op.apply(scalar, x)
            } else {
                op.apply(x, scalar)
            }
        };
        for p in self.upstream.by_ref() {
            if let Some(r) = apply(p.v) {
                // Propagate the uncertainty band through the scalar op via the
                // same interval arithmetic as series-op-series: treat the scalar
                // as an exact band `[scalar, scalar]`. This handles the tricky
                // `scalar / value` case where the value's band spans zero — the
                // reciprocal is non-monotonic and unbounded, so `interval_binop`
                // returns None rather than a bogus narrow interval that would
                // exclude the nominal. `rate(x[1m]) * 8` still carries a scaled
                // band. (Naively mapping the band endpoints was wrong here.)
                let sb = (self.scalar, self.scalar);
                let bounds = p.bounds.and_then(|b| {
                    if scalar_first {
                        interval_binop(op, sb, b)
                    } else {
                        interval_binop(op, b, sb)
                    }
                });
                return Some(Point {
                    t: p.t,
                    v: r,
                    bounds,
                    // Scaling by a constant does not move the observation: the
                    // point still belongs to the read it came from, so a later
                    // cross-table combination can still detect that.
                    edges: p.edges,
                    interpolated: p.interpolated,
                });
            }
            // Skip on division by zero, mirroring the eager path's
            // `continue` in the same situation.
        }
        None
    }
}

/// One point through `op` against `scalar`, as [`matrix_scalar_op`] applies
/// it; `None` where the op drops the point.
pub(crate) fn scalar_point(p: Point, op: BinOp, scalar: f64, scalar_first: bool) -> Option<Point> {
    ScalarBroadcast {
        upstream: std::iter::once(p),
        op,
        scalar,
        scalar_first,
    }
    .next()
}

/// `series OP scalar` (or `scalar OP series` if `scalar_first`)
/// applied across every input series.
pub fn matrix_scalar_op<'a>(
    series: SeriesSet<'a>,
    op: BinOp,
    scalar: f64,
    scalar_first: bool,
) -> SeriesSet<'a> {
    series
        .into_iter()
        .map(|ls| {
            let iter = ScalarBroadcast {
                upstream: ls.iter,
                op,
                scalar,
                scalar_first,
            };
            LabeledSeries::new(ls.labels, iter)
        })
        .collect()
}

/// How to derive the label-match key for two-sided binary ops.
///
/// Mirrors the eager `match_key()`, plus PromQL's default rule that
/// the synthetic `__name__` label never participates in matching.
#[derive(Copy, Clone, Debug)]
pub enum MatchSpec<'a> {
    /// No `on()`/`ignoring()` modifier: match on the full label set
    /// minus `__name__`.
    Default,
    /// `on(labels)`: match only on the listed labels.
    Include(&'a [String]),
    /// `ignoring(labels)`: match on every label except the listed
    /// ones (and `__name__`).
    Exclude(&'a [String]),
}

fn match_key(labels: &Labels, spec: MatchSpec<'_>) -> BTreeMap<String, String> {
    let mut k = BTreeMap::new();
    match spec {
        MatchSpec::Default => {
            // Default matching is on the visible label set: internal labels
            // (`__name__`, `__run__`, a reader's `__incarnation__`) are
            // identity within one side, not a key to join the two sides on.
            // `__recording__` is the exception: it is what tells apart two
            // recordings with identical labels (`is_matched_label`).
            for (key, val) in &labels.inner {
                if is_matched_label(key) {
                    k.insert(key.clone(), val.clone());
                }
            }
        }
        MatchSpec::Include(list) => {
            for label_name in list {
                if let Some(v) = labels.inner.get(label_name) {
                    k.insert(label_name.clone(), v.clone());
                }
            }
        }
        MatchSpec::Exclude(list) => {
            for (key, val) in &labels.inner {
                if !is_matched_label(key) || list.iter().any(|x| x == key) {
                    continue;
                }
                k.insert(key.clone(), val.clone());
            }
        }
    }
    k
}

/// `op` over one left and one right point at the left's time; `None` where
/// the op drops the point (division by zero).
fn combine_points(op: BinOp, left: Point, right: Point) -> Option<Point> {
    let v = op.apply(left.v, right.v)?;
    let bounds = combine_bounds(
        op,
        left.v,
        left.bounds,
        left.edges,
        right.v,
        right.bounds,
        right.edges,
    );
    // The result is a single read only when both operands were: otherwise it
    // belongs to no one window and must not claim one downstream.
    let edges = if left.edges == right.edges {
        left.edges
    } else {
        None
    };
    // Either side spanning an unobserved stretch makes the combination one
    // too — and drops its band with it. `combine_bounds` will happily derive
    // one from the operand that HAS a band, treating the other as exact; for
    // an operand nobody observed that is a band the combination has not
    // earned. The producer's invariant is that an interpolated point carries
    // no band, and it has to survive the operators or it means nothing.
    let interpolated = left.interpolated || right.interpolated;
    Some(Point {
        t: left.t,
        v,
        bounds: if interpolated { None } else { bounds },
        edges,
        interpolated,
    })
}

/// Pairs two point streams that emit at the same step grid (or
/// otherwise have aligned timestamps) and applies a binop pointwise.
/// Timestamps that appear on only one side are dropped.
pub struct ZipMergeBinary<'a> {
    left: std::iter::Peekable<Box<dyn Iterator<Item = Point> + 'a>>,
    right: std::iter::Peekable<Box<dyn Iterator<Item = Point> + 'a>>,
    op: BinOp,
}

impl<'a> Iterator for ZipMergeBinary<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let lt = self.left.peek().map(|p| p.t)?;
            let rt = self.right.peek().map(|p| p.t)?;
            match lt.cmp(&rt) {
                std::cmp::Ordering::Less => {
                    self.left.next();
                }
                std::cmp::Ordering::Greater => {
                    self.right.next();
                }
                std::cmp::Ordering::Equal => {
                    let left = self.left.next().expect("peek matched");
                    let right = self.right.next().expect("peek matched");
                    if let Some(p) = combine_points(self.op, left, right) {
                        return Some(p);
                    }
                }
            }
        }
    }
}

/// [`Join::Latest`]'s pairing: a point at every timestamp either side has,
/// from each side's latest point while it is held ([`Held`]).
pub struct LatestBinary<'a> {
    left: Held<'a>,
    right: Held<'a>,
    op: BinOp,
}

impl<'a> Iterator for LatestBinary<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let t = match (self.left.peek_t(), self.right.peek_t()) {
                (None, None) => return None,
                (l, r) => l.into_iter().chain(r).min()?,
            };
            self.left.advance_to(t);
            self.right.advance_to(t);
            let (Some(mut left), Some(right)) = (self.left.at(t), self.right.at(t)) else {
                continue;
            };
            left.t = t;
            if let Some(p) = combine_points(self.op, left, right) {
                return Some(p);
            }
        }
    }
}

/// A shared, materialised series replayed from the start.
struct Replay {
    points: Rc<[Point]>,
    next: usize,
}

impl Iterator for Replay {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        let p = *self.points.get(self.next)?;
        self.next += 1;
        Some(p)
    }
}

/// `left OP right` for one pair, under `join`.
fn pair<'a>(
    left: Box<dyn Iterator<Item = Point> + 'a>,
    right: Box<dyn Iterator<Item = Point> + 'a>,
    op: BinOp,
    join: Join,
) -> Box<dyn Iterator<Item = Point> + 'a> {
    match join {
        Join::Exact => Box::new(ZipMergeBinary {
            left: left.peekable(),
            right: right.peekable(),
            op,
        }),
        Join::Latest => Box::new(LatestBinary {
            left: Held::new(left),
            right: Held::new(right),
            op,
        }),
    }
}

/// Wrap a left-side `(t, v)` iterator and apply `op` against a
/// pre-materialised right-side timestamp→value lookup.  Used by the
/// single-right broadcast fallback in [`matrix_matrix_op`] — the same
/// `Rc<HashMap<...>>` is shared across every left series, so the right
/// singleton is decoded once regardless of left fan-out.
pub struct RightLookupBinary<'a> {
    upstream: Box<dyn Iterator<Item = Point> + 'a>,
    op: BinOp,
    rhs: Rc<RightLookup>,
}

impl<'a> Iterator for RightLookupBinary<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        for p in self.upstream.by_ref() {
            if let Some(&(rv, rb, re, r_interp)) = self.rhs.get(&p.t) {
                let right = Point {
                    t: p.t,
                    v: rv,
                    bounds: rb,
                    edges: re,
                    interpolated: r_interp,
                };
                if let Some(out) = combine_points(self.op, p, right) {
                    return Some(out);
                }
            }
        }
        None
    }
}

/// `left OP right` over two series sets, joining by `spec`-derived
/// match key. Output preserves left's labels (matching the eager path,
/// which copies `left_sample.metric.clone()` to the result).
///
/// When `spec` is `Default`, an unmatched left series is paired with the one
/// unmatched right series of its recording, when there is exactly one —
/// the common case is `sum(rate(x[..])) / y`, where `sum(...)` strips labels
/// and `y` carries some. Two series are of different recordings when they
/// carry one of `recording_keys` with different values
/// ([`DataSource::recording_label_keys`](crate::DataSource::recording_label_keys)).
/// With no recording keys, every series is of one recording, so a single
/// unmatched right series pairs with every unmatched left one.
pub fn matrix_matrix_op<'a>(
    left_set: SeriesSet<'a>,
    right_set: SeriesSet<'a>,
    op: BinOp,
    spec: MatchSpec<'_>,
    recording_keys: &[String],
    join: Join,
) -> SeriesSet<'a> {
    // Index right side by match key.  If two right-side series share
    // a key, the later one wins (the eager engine's HashMap-insert
    // has the same shape).
    let mut right_by_key: HashMap<BTreeMap<String, String>, LabeledSeries<'a>> =
        HashMap::with_capacity(right_set.len());
    for ls in right_set {
        let key = match_key(&ls.labels, spec);
        right_by_key.insert(key, ls);
    }

    let mut out: SeriesSet<'a> = Vec::new();
    let mut unmatched_left: Vec<LabeledSeries<'a>> = Vec::new();
    for left in left_set {
        let lk = match_key(&left.labels, spec);
        match right_by_key.remove(&lk) {
            Some(right) => {
                let iter = pair(left.iter, right.iter, op, join);
                out.push(LabeledSeries::new(left.labels, iter));
            }
            None => unmatched_left.push(left),
        }
    }

    if unmatched_left.is_empty() || !matches!(spec, MatchSpec::Default) || right_by_key.is_empty() {
        return out;
    }
    // Broadcast: each unmatched left series against the one unmatched right
    // series of its recording. A right series is materialised the first time
    // a left series pairs with it, and decoded once whatever the left
    // fan-out.
    let mut rights: Vec<(Labels, Option<PointIter<'a>>)> = right_by_key
        .into_values()
        .map(|ls| (ls.labels, Some(ls.iter)))
        .collect();
    let mut shared: Vec<Option<Rc<[Point]>>> = vec![None; rights.len()];
    let mut lookups: Vec<Option<Rc<RightLookup>>> = vec![None; rights.len()];
    for left in unmatched_left {
        let mut candidates = rights
            .iter()
            .enumerate()
            .filter(|(_, (labels, _))| same_recording(&left.labels, labels, recording_keys));
        let (Some((index, _)), None) = (candidates.next(), candidates.next()) else {
            continue;
        };
        let points = shared[index].get_or_insert_with(|| {
            let iter = rights[index].1.take().expect("materialised once");
            iter.collect()
        });
        let iter: Box<dyn Iterator<Item = Point> + 'a> = match join {
            // At the left's timestamps, through a timestamp lookup.
            Join::Exact => Box::new(RightLookupBinary {
                upstream: left.iter,
                op,
                rhs: Rc::clone(lookups[index].get_or_insert_with(|| {
                    Rc::new(
                        points
                            .iter()
                            .map(|p| (p.t, (p.v, p.bounds, p.edges, p.interpolated)))
                            .collect(),
                    )
                })),
            }),
            Join::Latest => pair(
                left.iter,
                Box::new(Replay {
                    points: Rc::clone(points),
                    next: 0,
                }),
                op,
                join,
            ),
        };
        out.push(LabeledSeries::new(left.labels, iter));
    }
    out
}

/// Whether two series can be of one recording: neither carries a recording
/// key with a value the other contradicts. A key one of them lacks (an
/// aggregation dropped it) contradicts nothing.
fn same_recording(a: &Labels, b: &Labels, recording_keys: &[String]) -> bool {
    recording_keys
        .iter()
        .all(|k| match (a.inner.get(k), b.inner.get(k)) {
            (Some(x), Some(y)) => x == y,
            _ => true,
        })
}

#[cfg(test)]
mod match_key_tests {
    use super::*;

    /// Default matching joins the two sides on their visible labels. An
    /// internal label is identity within one side, and a key that must be
    /// equal on both sides would make `a / b` between a series and its own
    /// aggregate fail whenever the reader had split the series by
    /// incarnation.
    #[test]
    fn default_matching_ignores_internal_labels() {
        let left = Labels::from([("__name__", "a"), ("__incarnation__", "x"), ("cpu", "1")]);
        let right = Labels::from([("__name__", "b"), ("cpu", "1")]);
        assert_eq!(
            match_key(&left, MatchSpec::Default),
            match_key(&right, MatchSpec::Default)
        );
        let excl = ["mode".to_string()];
        assert_eq!(
            match_key(&left, MatchSpec::Exclude(&excl)),
            match_key(&right, MatchSpec::Exclude(&excl))
        );
        // `__recording__` is matched on: it is what tells apart two
        // recordings with identical labels.
        let twin = Labels::from([("__recording__", "1"), ("cpu", "1")]);
        let other = Labels::from([("__recording__", "2"), ("cpu", "1")]);
        assert_ne!(
            match_key(&twin, MatchSpec::Default),
            match_key(&other, MatchSpec::Default)
        );
        assert_ne!(
            match_key(&twin, MatchSpec::Exclude(&excl)),
            match_key(&other, MatchSpec::Exclude(&excl))
        );
        // `on(__incarnation__)` still names it explicitly.
        let on = ["__incarnation__".to_string()];
        assert_ne!(
            match_key(&left, MatchSpec::Include(&on)),
            match_key(&right, MatchSpec::Include(&on))
        );
    }
}

#[cfg(test)]
mod interval_tests {
    use super::*;

    #[test]
    fn interval_binop_div_positive_bounds() {
        // [80,120] / [8,12] = [80/12, 120/8]; nominal 100/10=10 stays inside.
        let (lo, hi) = interval_binop(BinOp::Div, (80.0, 120.0), (8.0, 12.0)).unwrap();
        assert!((lo - 80.0 / 12.0).abs() < 1e-9, "lo {lo}");
        assert!((hi - 120.0 / 8.0).abs() < 1e-9, "hi {hi}");
        assert!(lo <= 10.0 && 10.0 <= hi);
    }

    #[test]
    fn interval_binop_div_denominator_spanning_zero_is_none() {
        assert!(interval_binop(BinOp::Div, (1.0, 2.0), (-1.0, 1.0)).is_none());
    }

    /// Two operands read at DIFFERENT instants must produce a wider band than
    /// the same two read together.
    ///
    /// This is the whole point of carrying acquisition edges. A group is one
    /// read with one window, so operands from the same group carry identical
    /// edges and combine exactly as before — no widening, no regression in
    /// tightness. Operands from different tables were sampled at different
    /// instants, and treating them as simultaneous is an approximation their
    /// own bands do not contain; each is re-derived over the union of both
    /// sides' edges first.
    ///
    /// Without the widening the cross-table band equals the same-table one,
    /// which is the failure this guards: a band that is too NARROW is worse
    /// than no band, because it claims a precision the data cannot support.
    #[test]
    fn cross_table_operands_widen_the_band_and_same_table_ones_do_not() {
        // Two reads 10 ms apart, each a 1 ms window, over a 1 s range.
        const S: f64 = 1e9;
        let a = RateEdges {
            left: (0.0, 1e6),
            right: (S, S + 1e6),
        };
        // Same read: byte-identical edges.
        let same = a;
        // Different table: shifted 10 ms.
        let other = RateEdges {
            left: (10e6, 11e6),
            right: (S + 10e6, S + 11e6),
        };

        let band_of = |l: RateEdges, r: RateEdges| {
            // Nominals chosen to sit inside their own bands.
            combine_bounds(
                BinOp::Div,
                100.0,
                Some((99.0, 101.0)),
                Some(l),
                50.0,
                Some((49.0, 51.0)),
                Some(r),
            )
            .expect("a band is produced")
        };

        let (same_lo, same_hi) = band_of(a, same);
        let (cross_lo, cross_hi) = band_of(a, other);

        assert_eq!(
            (same_lo, same_hi),
            (99.0 / 51.0, 101.0 / 49.0),
            "identical edges mean one read: the band must be exactly the \
             interval-arithmetic combination, untouched"
        );
        assert!(
            cross_hi - cross_lo > same_hi - same_lo,
            "operands read 10 ms apart must yield a WIDER band than the same \
             two read together: same={:?}, cross={:?}",
            (same_lo, same_hi),
            (cross_lo, cross_hi)
        );
        assert!(
            cross_lo <= 100.0 / 50.0 && cross_hi >= 100.0 / 50.0,
            "the widened band must still contain the nominal quotient"
        );
    }

    /// A missing side's edges must not silently disable the widening for the
    /// other — but with only one side known there is no union to take, so the
    /// combination stays as it was rather than inventing a span.
    #[test]
    fn one_sided_edges_leave_the_band_unchanged() {
        let e = RateEdges {
            left: (0.0, 1e6),
            right: (1e9, 1e9 + 1e6),
        };
        let with_one = combine_bounds(
            BinOp::Div,
            100.0,
            Some((99.0, 101.0)),
            Some(e),
            50.0,
            Some((49.0, 51.0)),
            None,
        );
        let with_none = combine_bounds(
            BinOp::Div,
            100.0,
            Some((99.0, 101.0)),
            None,
            50.0,
            Some((49.0, 51.0)),
            None,
        );
        assert_eq!(with_one, with_none);
    }

    #[test]
    fn combine_bounds_none_when_both_exact() {
        assert!(combine_bounds(BinOp::Div, 100.0, None, None, 10.0, None, None).is_none());
        // one-sided: exact numerator, banded denominator still propagates
        assert!(
            combine_bounds(BinOp::Div, 100.0, None, None, 10.0, Some((8.0, 12.0)), None).is_some()
        );
    }

    #[test]
    fn zip_merge_propagates_division_band() {
        let l: Box<dyn Iterator<Item = Point>> = Box::new(std::iter::once(Point {
            t: 1,
            v: 100.0,
            bounds: Some((80.0, 120.0)),
            edges: None,
            interpolated: false,
        }));
        let r: Box<dyn Iterator<Item = Point>> = Box::new(std::iter::once(Point {
            t: 1,
            v: 10.0,
            bounds: Some((8.0, 12.0)),
            edges: None,
            interpolated: false,
        }));
        let mut z = ZipMergeBinary {
            left: l.peekable(),
            right: r.peekable(),
            op: BinOp::Div,
        };
        let p = z.next().unwrap();
        assert!((p.v - 10.0).abs() < 1e-9);
        let (lo, hi) = p
            .bounds
            .expect("division of two banded series carries a band");
        assert!(lo <= p.v && p.v <= hi);
        assert!((lo - 80.0 / 12.0).abs() < 1e-9 && (hi - 120.0 / 8.0).abs() < 1e-9);
    }

    #[test]
    fn scalar_over_series_div_spanning_zero_drops_band() {
        // `10 / value` where the value's band strictly spans zero: x ↦ 10/x is
        // non-monotonic and unbounded near 0, so applying the op to the band
        // endpoints yields a narrow finite interval that EXCLUDES the nominal.
        // The correct answer is "no finite band" (None). Regression for the
        // ScalarBroadcast reciprocal bug.
        let up: Box<dyn Iterator<Item = Point>> = Box::new(std::iter::once(Point {
            t: 1,
            v: 0.1,
            bounds: Some((-4.0, 4.0)),
            edges: None,
            interpolated: false,
        }));
        let mut sb = ScalarBroadcast {
            upstream: up,
            op: BinOp::Div,
            scalar: 10.0,
            scalar_first: true,
        };
        let p = sb.next().unwrap();
        assert!((p.v - 100.0).abs() < 1e-9, "nominal 10/0.1 = 100");
        assert!(
            p.bounds.is_none(),
            "spanning-zero denominator ⇒ unbounded ⇒ no band, got {:?}",
            p.bounds
        );
    }

    #[test]
    fn scalar_over_series_div_positive_band_contains_nominal() {
        // Guard the fix doesn't over-drop: `10 / [8,12]`, nominal 10/10 = 1.0,
        // band [10/12, 10/8] still propagates and contains the nominal.
        let up: Box<dyn Iterator<Item = Point>> = Box::new(std::iter::once(Point {
            t: 1,
            v: 10.0,
            bounds: Some((8.0, 12.0)),
            edges: None,
            interpolated: false,
        }));
        let mut sb = ScalarBroadcast {
            upstream: up,
            op: BinOp::Div,
            scalar: 10.0,
            scalar_first: true,
        };
        let p = sb.next().unwrap();
        let (lo, hi) = p.bounds.expect("positive denominator band propagates");
        assert!(lo <= p.v && p.v <= hi, "nominal {} in [{lo}, {hi}]", p.v);
    }
}

#[cfg(test)]
mod broadcast_tests {
    use super::*;

    fn series<'a>(pairs: &[(&str, &str)], v: f64) -> LabeledSeries<'a> {
        LabeledSeries::new(
            Labels::from(pairs),
            (1..=3u64).map(move |t| Point::at(t, v)),
        )
    }

    fn run(left: SeriesSet<'_>, right: SeriesSet<'_>, keys: &[String]) -> Vec<(Labels, f64)> {
        let mut out: Vec<(Labels, f64)> = matrix_matrix_op(
            left,
            right,
            BinOp::Div,
            MatchSpec::Default,
            keys,
            Join::Exact,
        )
        .into_iter()
        .map(|ls| {
            let v = ls.iter.last().map(|p| p.v).unwrap_or(f64::NAN);
            (ls.labels, v)
        })
        .collect();
        out.sort_by(|a, b| a.0.inner.cmp(&b.0.inner));
        out
    }

    /// With recording keys, an unmatched left series is paired with the
    /// unmatched right series of its own recording: `x / cores` on two
    /// hosts, each with its own `cores`.
    #[test]
    fn a_broadcast_pairs_within_each_recording() {
        let keys = ["source".to_string()];
        let left = vec![
            series(&[("cpu", "0"), ("source", "a")], 8.0),
            series(&[("cpu", "0"), ("source", "b")], 8.0),
        ];
        let right = vec![
            series(&[("source", "a")], 4.0),
            series(&[("source", "b")], 2.0),
        ];
        let out = run(left, right, &keys);
        assert_eq!(
            out,
            vec![
                (Labels::from([("cpu", "0"), ("source", "a")]), 2.0),
                (Labels::from([("cpu", "0"), ("source", "b")]), 4.0),
            ]
        );
    }

    /// A single right series of another recording is not paired: with `x`
    /// only in recording a and `y` only in b, `x / y` is empty, as in
    /// Prometheus. Without recording keys a single right series pairs with
    /// every left one, so the result has one series.
    #[test]
    fn a_broadcast_never_crosses_recordings() {
        let keys = ["source".to_string()];
        let out = run(
            vec![series(&[("source", "a")], 8.0)],
            vec![series(&[("source", "b")], 2.0)],
            &keys,
        );
        assert!(out.is_empty(), "{out:?}");
        let out = run(
            vec![series(&[("source", "a")], 8.0)],
            vec![series(&[("source", "b")], 2.0)],
            &[],
        );
        assert_eq!(out.len(), 1);
    }

    /// A left series that lost the recording keys (an aggregation dropped
    /// them) is of either recording, so with two candidates it pairs with
    /// neither.
    #[test]
    fn a_series_without_recording_labels_is_ambiguous_between_two() {
        let keys = ["source".to_string()];
        let out = run(
            vec![series(&[], 8.0)],
            vec![
                series(&[("source", "a")], 4.0),
                series(&[("source", "b")], 2.0),
            ],
            &keys,
        );
        assert!(out.is_empty(), "{out:?}");
    }
}
