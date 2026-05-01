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
//!   that don't find a match are dropped, matching the eager
//!   engine's behaviour modulo the single-right fallback (see
//!   below).
//!
//! What's NOT covered here and falls back to eager:
//!
//! * `group_left` / `group_right` (one-to-many matching) — needs an
//!   iterator-tee mechanism that defeats streaming.
//! * Single-right fallback (`metric / scalar_aggregate` without an
//!   `on()`/`ignoring()` modifier) — would require buffering one
//!   side. The eager path keeps this working transparently; the
//!   streaming dispatcher returns `None` so the eager evaluator
//!   takes over.
//! * Comparison operators (`==`, `!=`, `<`, `>`, `<=`, `>=`) and
//!   the `bool` modifier — not implemented in the eager engine
//!   today either.

use std::collections::{BTreeMap, HashMap};

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

/// `left OP right` over two series sets, joining by `spec`-derived
/// match key. Output preserves left's labels (matching the eager
/// path, which copies `left_sample.metric.clone()` to the result).
///
/// Returns `None` instead of an empty `SeriesSet` when the dispatcher
/// would need the eager engine's single-right fallback (left has at
/// least one series with no match on the right, AND `spec` is
/// `Default`, AND right is single-series). Caller falls back to eager.
pub fn matrix_matrix_op<'a>(
    left_set: SeriesSet<'a>,
    right_set: SeriesSet<'a>,
    op: BinOp,
    spec: MatchSpec<'_>,
) -> Option<SeriesSet<'a>> {
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
    let mut any_unmatched = false;
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
            None => {
                // Drop this left series — it doesn't match anything
                // on the right.  Track for the fallback decision
                // below.
                any_unmatched = true;
                drop(left);
            }
        }
    }

    // Single-right fallback: when no explicit matcher is given AND
    // left had unmatched series AND right was single-series, the
    // eager engine pairs every left with that one right. We can't
    // tee a streaming iterator without buffering, so signal "fall
    // back to eager" rather than silently dropping series.
    let needs_fallback = any_unmatched
        && matches!(spec, MatchSpec::Default)
        && right_by_key.len() == 1
        && out.is_empty();
    if needs_fallback {
        return None;
    }

    Some(out)
}
