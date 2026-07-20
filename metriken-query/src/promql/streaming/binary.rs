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

use crate::labels::Labels;

use super::{Band, LabeledSeries, Point, SeriesSet};

/// Timestamp → `(value, optional band)` lookup for the right singleton in a
/// single-right broadcast. Aliased to keep the `Rc<HashMap<…>>` readable
/// (clears clippy's `type_complexity`).
type RightLookup = HashMap<u64, (f64, Option<Band>)>;

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

/// Combine two operands' optional bands for `op`. If either side carries a real
/// band, propagate (the bandless side is exact, `[v, v]`); if neither does, the
/// result has no band.
fn combine_bounds(
    op: BinOp,
    lv: f64,
    lb: Option<(f64, f64)>,
    rv: f64,
    rb: Option<(f64, f64)>,
) -> Option<(f64, f64)> {
    if lb.is_none() && rb.is_none() {
        return None;
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
                });
            }
            // Skip on division by zero, mirroring the eager path's
            // `continue` in the same situation.
        }
        None
    }
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
            for (key, val) in &labels.inner {
                if key != "__name__" {
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
                if key == "__name__" || list.iter().any(|x| x == key) {
                    continue;
                }
                k.insert(key.clone(), val.clone());
            }
        }
    }
    k
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
                    if let Some(v) = self.op.apply(left.v, right.v) {
                        let bounds =
                            combine_bounds(self.op, left.v, left.bounds, right.v, right.bounds);
                        return Some(Point {
                            t: left.t,
                            v,
                            bounds,
                        });
                    }
                }
            }
        }
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
            if let Some(&(rv, rb)) = self.rhs.get(&p.t) {
                if let Some(v) = self.op.apply(p.v, rv) {
                    let bounds = combine_bounds(self.op, p.v, p.bounds, rv, rb);
                    return Some(Point { t: p.t, v, bounds });
                }
            }
        }
        None
    }
}

/// `left OP right` over two series sets, joining by `spec`-derived
/// match key. Output preserves left's labels (matching the eager
/// path, which copies `left_sample.metric.clone()` to the result).
///
/// When `spec` is `Default` and exactly one right-side series remains
/// unmatched after the keyed join, that singleton is broadcast against
/// every unmatched left series — common case is `sum(rate(x[..])) / y`
/// where `sum(...)` strips labels and `y` carries some.
pub fn matrix_matrix_op<'a>(
    left_set: SeriesSet<'a>,
    right_set: SeriesSet<'a>,
    op: BinOp,
    spec: MatchSpec<'_>,
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
                let iter = ZipMergeBinary {
                    left: left.iter.peekable(),
                    right: right.iter.peekable(),
                    op,
                };
                out.push(LabeledSeries::new(left.labels, iter));
            }
            None => unmatched_left.push(left),
        }
    }

    // Single-right broadcast: with no explicit matcher and exactly one
    // unmatched right series, pair every unmatched left with that
    // singleton (timestamps via a shared lookup).  Mirrors the eager
    // engine's per-left fallback for `aggregated / scalar_metric`.
    if !unmatched_left.is_empty() && matches!(spec, MatchSpec::Default) && right_by_key.len() == 1 {
        let (_, right_singleton) = right_by_key.into_iter().next().unwrap();
        let rhs: Rc<RightLookup> = Rc::new(
            right_singleton
                .iter
                .map(|p| (p.t, (p.v, p.bounds)))
                .collect(),
        );
        for left in unmatched_left {
            let iter = RightLookupBinary {
                upstream: left.iter,
                op,
                rhs: Rc::clone(&rhs),
            };
            out.push(LabeledSeries::new(left.labels, iter));
        }
    }

    out
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

    #[test]
    fn combine_bounds_none_when_both_exact() {
        assert!(combine_bounds(BinOp::Div, 100.0, None, 10.0, None).is_none());
        // one-sided: exact numerator, banded denominator still propagates
        assert!(combine_bounds(BinOp::Div, 100.0, None, 10.0, Some((8.0, 12.0))).is_some());
    }

    #[test]
    fn zip_merge_propagates_division_band() {
        let l: Box<dyn Iterator<Item = Point>> = Box::new(std::iter::once(Point {
            t: 1,
            v: 100.0,
            bounds: Some((80.0, 120.0)),
        }));
        let r: Box<dyn Iterator<Item = Point>> = Box::new(std::iter::once(Point {
            t: 1,
            v: 10.0,
            bounds: Some((8.0, 12.0)),
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
