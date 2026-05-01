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

use crate::tsdb::Labels;

use super::{LabeledSeries, Point, SeriesSet};

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
        for (t, v) in self.upstream.by_ref() {
            let result = if self.scalar_first {
                self.op.apply(self.scalar, v)
            } else {
                self.op.apply(v, self.scalar)
            };
            if let Some(r) = result {
                return Some((t, r));
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
            let lt = self.left.peek().map(|&(t, _)| t)?;
            let rt = self.right.peek().map(|&(t, _)| t)?;
            match lt.cmp(&rt) {
                std::cmp::Ordering::Less => {
                    self.left.next();
                }
                std::cmp::Ordering::Greater => {
                    self.right.next();
                }
                std::cmp::Ordering::Equal => {
                    let (t, lv) = self.left.next().expect("peek matched");
                    let (_, rv) = self.right.next().expect("peek matched");
                    if let Some(v) = self.op.apply(lv, rv) {
                        return Some((t, v));
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
    rhs: Rc<HashMap<u64, f64>>,
}

impl<'a> Iterator for RightLookupBinary<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        for (t, lv) in self.upstream.by_ref() {
            if let Some(&rv) = self.rhs.get(&t) {
                if let Some(v) = self.op.apply(lv, rv) {
                    return Some((t, v));
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
        let rhs: Rc<HashMap<u64, f64>> = Rc::new(right_singleton.iter.collect());
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
