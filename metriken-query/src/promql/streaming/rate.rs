//! Counter rate producers.
//!
//! * [`CounterGridRate`] — `rate`/`irate` in this engine: a fixed-phase
//!   evaluation grid whose value is the reset-adjusted cumulative counter
//!   interpolated across each step interval (see the struct docs).
//! * [`CounterPairwiseRate`] — one point per consecutive sample pair at the
//!   real sample timestamp; used by `RateMode::Raw` and by `deriv`.

use std::collections::VecDeque;

use super::{Point, RateEdges};
use crate::types::CounterSample;

/// A boxed sample stream a producer consumes; see [`crate::CounterStream`].
pub type Samples<'a> = Box<dyn Iterator<Item = CounterSample> + 'a>;

/// Turn owned sample vectors into a stream, for a caller that has the whole
/// series in hand (tests, and a source without a streaming read).
fn stream_of(
    timestamps: Vec<u64>,
    values: Vec<u64>,
    windows: Option<Vec<(u64, u64)>>,
) -> Samples<'static> {
    let n = timestamps.len();
    let windows = windows.unwrap_or_default();
    Box::new((0..n).map(move |i| CounterSample {
        ts: timestamps[i],
        value: values[i],
        window: windows.get(i).copied(),
    }))
}

/// Pair-wise rate producer over a counter sample stream. Emits one point per
/// consecutive sample pair, stamped at the later sample, for pairs whose stamp
/// falls in `[start_ns, end_ns]`. The stream is fetched with lookback (for
/// context), so the `start_ns` bound is what keeps a windowed/zoomed query
/// from spilling points before the requested window — mirroring how the grid
/// producer's cursor starts at `start_ns`.
///
/// Pulls one sample at a time and remembers the previous: a producer is the
/// series' iterator for the rest of the pipeline, and it holds only what the
/// next point needs. The dispatcher used to run every producer to completion
/// into a `Vec` first, and on a wide table those vectors — and the whole
/// series they were computed from — were most of a query's memory.
pub struct CounterPairwiseRate<'a> {
    source: Samples<'a>,
    prev: Option<(u64, u64)>,
    start_ns: u64,
    end_ns: u64,
}

impl<'a> CounterPairwiseRate<'a> {
    pub fn new(timestamps: Vec<u64>, values: Vec<u64>, start_ns: u64, end_ns: u64) -> Self {
        Self::from_stream(stream_of(timestamps, values, None), start_ns, end_ns)
    }

    pub fn from_stream(source: Samples<'a>, start_ns: u64, end_ns: u64) -> Self {
        Self {
            source,
            prev: None,
            start_ns,
            end_ns,
        }
    }
}

impl<'a> Iterator for CounterPairwiseRate<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        loop {
            let cur = self.source.next()?;
            let Some((ts_prev, v_prev)) = self.prev.replace((cur.ts, cur.value)) else {
                continue;
            };
            let (ts_cur, v_cur) = (cur.ts, cur.value);
            if ts_cur > self.end_ns {
                return None;
            }
            // Skip pairs before the requested window; the stream carries
            // lookback context whose stamps precede start_ns.
            if ts_cur < self.start_ns {
                continue;
            }
            let delta = if v_cur >= v_prev {
                (v_cur - v_prev) as f64
            } else {
                v_cur as f64
            };
            let dur_s = (ts_cur - ts_prev) as f64 / 1e9;
            if dur_s <= 0.0 {
                continue;
            }
            return Some(Point::at(ts_cur, delta / dur_s));
        }
    }
}

/// One sample as the grid producer keeps it: its stamp, the reset-adjusted
/// cumulative value at it, and its acquisition window.
#[derive(Copy, Clone, Debug)]
struct Kept {
    ts: u64,
    cum: f64,
    window: Option<(u64, u64)>,
}

/// Grid-aligned rate producer (`RateMode::Grid`).
///
/// At each grid tick `t = start + k·step`, linearly interpolate the
/// reset-adjusted cumulative counter to `t` and to `t − step`, then emit
/// `(V(t) − V(t − step)) / step`. Unlike a whole-window average rate, the value
/// is attributable to the grid interval `[t − step, t]` regardless of sample
/// phase, so two recordings on a shared grid are directly comparable. The
/// query's `[range]` window is intentionally ignored — the step is the
/// interval. A grid point is emitted only when both interval edges fall
/// within the observed sample range `[first_ts, last_ts]`; no extrapolation
/// beyond observed data (leading/trailing partial intervals are dropped).
///
/// Consumes its samples as a stream. The grid advances monotonically, so at
/// any point only the samples bracketing the current interval are needed:
/// those are kept, the ones before the interval's left edge are let go, and
/// the ones after its right edge have not been pulled yet. A series is held
/// one interval's worth at a time rather than whole — see
/// [`CounterPairwiseRate`] for what that used to cost.
pub struct CounterGridRate<'a> {
    source: Samples<'a>,
    exhausted: bool,
    /// The samples bracketing the current interval, oldest first.
    kept: VecDeque<Kept>,
    /// The first and the latest sample stamps pulled: the observed range,
    /// outside which nothing is extrapolated.
    first_ts: Option<u64>,
    last_ts: Option<u64>,
    /// Samples pulled so far.
    pulled: usize,
    /// The previous raw value and the running reset-adjusted cumulative.
    prev_value: Option<u64>,
    acc: f64,
    /// The gaps between the first samples, for the typical spacing that
    /// tells a hole from jitter — see [`Self::sampled_window`].
    spacings: Vec<u64>,
    /// Their median, fixed once the probe is full. Computed per point before,
    /// which was a clone and a sort per emitted point.
    typical: u64,
    /// Whether the samples carry acquisition windows.
    windowed: bool,
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    /// Averaging window per emitted point. Equal to `step_ns` for the classic
    /// behaviour; wider smooths each value without moving the points.
    span_ns: u64,
    /// Explicit evaluation timestamps, when the caller supplied them. Each
    /// value is then the increase across the gap from the PRECEDING timestamp,
    /// which makes the uniform grid the special case where every gap is
    /// `step_ns`.
    ///
    /// This exists because a slow source's readings are not evenly spaced —
    /// measured 30 s then 60 s apart on a real recording — so no uniform grid
    /// can land on them. Evaluating where the data actually is keeps a
    /// combined value simultaneous with its slow operand instead of
    /// interpolating that operand across the gap.
    points: Option<(std::sync::Arc<[u64]>, usize)>,
    done: bool,
}

/// How many leading samples the typical spacing is taken from.
const SPACING_PROBE: usize = 9;

impl<'a> CounterGridRate<'a> {
    /// From whole vectors; see [`from_stream`](Self::from_stream).
    #[cfg(test)]
    pub fn new(
        timestamps: Vec<u64>,
        values: &[u64],
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        // Averaging window per point; see the field of the same name.
        span_ns: u64,
        windows: Option<Vec<(u64, u64)>>,
    ) -> Self {
        let windowed = windows.is_some();
        Self::from_stream(
            stream_of(timestamps, values.to_vec(), windows),
            windowed,
            start_ns,
            end_ns,
            step_ns,
            span_ns,
        )
    }

    /// Over a sample stream. `windowed` says whether the samples carry
    /// acquisition windows, which decides between a band and no band before
    /// the first sample arrives.
    pub fn from_stream(
        source: Samples<'a>,
        windowed: bool,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        span_ns: u64,
    ) -> Self {
        let mut this = Self {
            source,
            exhausted: false,
            kept: VecDeque::new(),
            first_ts: None,
            last_ts: None,
            pulled: 0,
            prev_value: None,
            acc: 0.0,
            spacings: Vec::new(),
            typical: 1,
            windowed,
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            span_ns: span_ns.max(1),
            points: None,
            done: step_ns == 0,
        };
        // The typical spacing is taken from the first samples, so they are
        // pulled up front; that is also where a series too short to bracket
        // any interval is found out.
        while this.pulled < SPACING_PROBE && this.pull() {}
        if this.pulled < 2 {
            this.done = true;
        }
        let mut spacings = this.spacings.clone();
        spacings.sort_unstable();
        this.typical = spacings
            .get(spacings.len() / 2)
            .copied()
            .unwrap_or(1)
            .max(1);
        this
    }

    /// Pull one more sample into `kept`. False once the stream is exhausted.
    fn pull(&mut self) -> bool {
        if self.exhausted {
            return false;
        }
        let Some(sample) = self.source.next() else {
            self.exhausted = true;
            return false;
        };
        // Reset-adjusted cumulative: same convention as CounterRate's
        // total_increase (a decrease is treated as a fresh counter start,
        // contributing its own value as the increment).
        if let Some(prev) = self.prev_value {
            self.acc += if sample.value >= prev {
                (sample.value - prev) as f64
            } else {
                sample.value as f64
            };
        }
        self.prev_value = Some(sample.value);
        if let Some(last) = self.last_ts {
            if self.spacings.len() < SPACING_PROBE - 1 {
                self.spacings.push(sample.ts - last);
            }
        }
        self.first_ts.get_or_insert(sample.ts);
        self.last_ts = Some(sample.ts);
        self.pulled += 1;
        self.kept.push_back(Kept {
            ts: sample.ts,
            cum: self.acc,
            window: sample.window,
        });
        true
    }

    /// Pull until a sample at or past `edge` is kept, or the stream ends.
    fn reach(&mut self, edge: u64) {
        while self.kept.back().is_none_or(|k| k.ts < edge) && self.pull() {}
    }

    /// Let go of the samples before `left` that no interval will need
    /// again: everything but the last sample at or before it, which the
    /// interpolation at `left` brackets against.
    fn trim(&mut self, left: u64) {
        while self.kept.len() >= 2 && self.kept[1].ts <= left {
            self.kept.pop_front();
        }
    }

    /// Position in `kept` of the first sample at or after `edge`, or
    /// `kept.len()` when none.
    fn hi(&self, edge: u64) -> usize {
        self.kept.partition_point(|k| k.ts < edge)
    }

    /// Whether `edge` lies inside the observed sample range.
    fn observed(&self, edge: u64) -> bool {
        match (self.first_ts, self.last_ts) {
            (Some(first), Some(last)) => edge >= first && edge <= last,
            _ => false,
        }
    }

    /// Linearly interpolate the reset-adjusted cumulative value at `edge`.
    /// Returns `None` when `edge` is outside the observed sample range (no
    /// extrapolation). The caller has pulled through `edge` and not trimmed
    /// past it.
    fn interp(&self, edge: u64) -> Option<f64> {
        if !self.observed(edge) {
            return None;
        }
        let hi = self.hi(edge);
        let k_hi = self.kept.get(hi)?;
        if k_hi.ts == edge {
            return Some(k_hi.cum);
        }
        // edge is strictly between hi-1 and hi (hi >= 1 since edge > first).
        let k_lo = self.kept.get(hi.checked_sub(1)?)?;
        let span = (k_hi.ts - k_lo.ts) as f64;
        let frac = (edge - k_lo.ts) as f64 / span;
        Some(k_lo.cum + frac * (k_hi.cum - k_lo.cum))
    }

    /// The acquisition window `(begin, end)` of the read bracketing `edge`,
    /// but only when `edge` is close enough to a real sample for that window to
    /// describe it.
    ///
    /// "Close enough" is the sample spacing either side of `edge`: a grid edge
    /// falling between two adjacent reads is legitimately described by an
    /// interpolation between their windows, which is what
    /// [`Self::interp_window`] does. A grid edge falling inside a HOLE — where
    /// the neighbouring reads are many steps apart because the series was null
    /// between them — is not described by any acquisition at all, and
    /// interpolating one fabricates a read. Returns `None` there, which is what
    /// suppresses the band and raises `Point::interpolated`.
    ///
    /// The threshold is the median sample spacing, doubled: adjacent reads pass
    /// even when the cadence jitters, while a hole spanning several missing
    /// samples does not.
    fn sampled_window(&self, edge: u64) -> Option<(f64, f64)> {
        if self.pulled < 2 {
            return self.interp_window(edge);
        }
        let hi = self.hi(edge);
        // Exactly on a read: that read's own window, no question of holes.
        if self.kept.get(hi).is_some_and(|k| k.ts == edge) {
            return self.interp_window(edge);
        }
        if hi == 0 || hi >= self.kept.len() {
            return None;
        }
        let gap = self.kept[hi].ts - self.kept[hi - 1].ts;
        // Typical spacing, from the first few gaps — enough to characterize a
        // regular cadence without walking the whole series.
        if gap > self.typical.saturating_mul(2) {
            return None;
        }
        self.interp_window(edge)
    }

    /// Interpolate the acquisition-window `(begin, end)` at `edge`, in the
    /// same way as [`Self::interp`] does the value, so the uncertainty band
    /// is attributable to the grid edge rather than the nearest raw sample.
    /// `None` when there are no windows or `edge` is outside the sample range.
    fn interp_window(&self, edge: u64) -> Option<(f64, f64)> {
        if !self.windowed || !self.observed(edge) {
            return None;
        }
        let hi = self.hi(edge);
        let k_hi = self.kept.get(hi)?;
        let (b_hi, e_hi) = k_hi.window?;
        if k_hi.ts == edge {
            return Some((b_hi as f64, e_hi as f64));
        }
        let k_lo = self.kept.get(hi.checked_sub(1)?)?;
        let (b_lo, e_lo) = k_lo.window?;
        let span = (k_hi.ts - k_lo.ts) as f64;
        let frac = (edge - k_lo.ts) as f64 / span;
        let b = b_lo as f64 + frac * (b_hi as f64 - b_lo as f64);
        let e = e_lo as f64 + frac * (e_hi as f64 - e_lo as f64);
        Some((b, e))
    }

    /// Evaluate at `points` rather than on the uniform grid. Each emitted
    /// value covers the gap from the previous point, so the caller's choice of
    /// timestamps sets both placement AND averaging window.
    pub(crate) fn at_points(mut self, points: std::sync::Arc<[u64]>) -> Self {
        self.points = Some((points, 0));
        self
    }
}

impl<'a> Iterator for CounterGridRate<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while !self.done {
            // Two placements, one body. Explicit points take their window from
            // the preceding point; the uniform grid takes it from `span_ns`.
            let (t, left) = match &mut self.points {
                Some((points, idx)) => {
                    // Each value spans one gap, so N points yield N-1 values:
                    // the first has no predecessor to measure across, the same
                    // rule the grid follows at `start`, which needs a step of
                    // lookback before it can emit.
                    let i = *idx;
                    *idx += 1;
                    let (&prev, &t) = (points.get(i)?, points.get(i + 1)?);
                    if t > self.end_ns || t <= prev {
                        continue;
                    }
                    (t, prev)
                }
                None => {
                    if self.cursor_ns > self.end_ns {
                        return None;
                    }
                    let t = self.cursor_ns;
                    match self.cursor_ns.checked_add(self.step_ns) {
                        Some(next) => self.cursor_ns = next,
                        None => self.done = true,
                    }
                    // The averaging window, which may be wider than the point
                    // spacing — see `span_ns`.
                    let Some(left) = t.checked_sub(self.span_ns) else {
                        continue;
                    };
                    (t, left)
                }
            };
            // Bring the kept samples to this interval: through its right edge,
            // and no further back than the read before its left edge.
            self.reach(t);
            self.trim(left);
            // Past the observed range on the uniform grid, every later tick
            // is too; on explicit points a later one might not be, so only
            // the grid stops here.
            if self.exhausted && self.last_ts.is_some_and(|last| t > last) {
                self.points.as_ref()?;
                continue;
            }
            let (Some(v_hi), Some(v_lo)) = (self.interp(t), self.interp(left)) else {
                continue;
            };
            let step_s = (t - left) as f64 / 1e9;
            if step_s <= 0.0 {
                continue;
            }
            let increase = v_hi - v_lo;
            let v = increase / step_s;
            // Band from the window edges: the elapsed span between the left
            // edge's begin and the right edge's end (widest → slowest) and
            // between the right edge's begin and the left edge's end
            // (narrowest → fastest). Widen to always contain the nominal, which
            // divides by the exact step rather than the window-derived span.
            //
            // Both edges must land on a REAL acquisition. `interp_window` will
            // happily synthesize a window at any timestamp inside the sample
            // range, which is the right reading when the grid edge merely falls
            // between two adjacent reads — but across a hole (a counter that
            // was null for a stretch) it invents an acquisition at a time when
            // the producer did not read, and the band then claims a precision
            // nobody measured. Where that would happen, the point is emitted
            // with a value and no band, flagged `interpolated`; see `Point`.
            let window_pair = self.sampled_window(left).zip(self.sampled_window(t));
            let interpolated = window_pair.is_none() && self.windowed;
            let bounds = window_pair
                .and_then(|((b_left, e_left), (b_hi, e_hi))| {
                    let elapsed_max = (e_hi - b_left) / 1e9;
                    let elapsed_min = (b_hi - e_left) / 1e9;
                    if elapsed_min > 0.0 && elapsed_max > 0.0 {
                        Some((increase / elapsed_max, increase / elapsed_min))
                    } else {
                        None
                    }
                })
                .map(|(lo, hi)| (lo.min(v), hi.max(v)));
            // Carried so a binary op against a DIFFERENT table can re-derive
            // this band over the union of both operands' edges. Only meaningful
            // alongside a band, so they travel together.
            let edges = bounds.and_then(|_| {
                window_pair.map(|(left_w, right_w)| RateEdges {
                    left: left_w,
                    right: right_w,
                })
            });
            return Some(Point {
                t,
                v,
                bounds,
                edges,
                interpolated,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Grid producer (RateMode::Grid) ----

    #[test]
    fn grid_rate_constant_counter_yields_constant_rate() {
        // Counter climbing 100/s, samples aligned on the grid.
        let ts = [0u64, 1_000_000_000, 2_000_000_000, 3_000_000_000];
        let vals = [0u64, 100, 200, 300];
        let pts: Vec<Point> = CounterGridRate::new(
            ts.to_vec(),
            &vals,
            0,             // start_ns (already snapped by the caller)
            3_000_000_000, // end_ns
            1_000_000_000, // step_ns
            1_000_000_000, // span_ns (classic: one step per point)
            None,          // windows
        )
        .collect();
        // Grid point t needs both edges (t-step, t) inside the sample range
        // [0, 3s]; t=0 has no left edge, so emit at t=1,2,3s.
        let times: Vec<u64> = pts.iter().map(|p| p.t).collect();
        assert_eq!(times, vec![1_000_000_000, 2_000_000_000, 3_000_000_000]);
        for p in &pts {
            assert!((p.v - 100.0).abs() < 1e-6, "t={} v={}", p.t, p.v);
        }
    }

    /// A hole in the samples: the rate still spans it, but says so.
    ///
    /// Reads at 3s, 4s, 5s, then nothing until 8s and 9s — a counter that went
    /// null for a stretch, which is what a device appearing at runtime or a
    /// partially-populated group produces. The 400 that accrued across the hole
    /// is real and the average over it is knowable, so points are still
    /// emitted; but nobody observed anything at 6s or 7s, so those points carry
    /// no band and are flagged.
    #[test]
    fn grid_rate_across_a_hole_declines_a_band_and_flags_the_points() {
        const S: u64 = 1_000_000_000;
        let ts = [3 * S, 4 * S, 5 * S, 8 * S, 9 * S];
        let vals = [100u64, 200, 300, 700, 800];
        // 50ms acquisition window ending at each sample.
        let windows: Vec<(u64, u64)> = ts.iter().map(|&t| (t - 50_000_000, t)).collect();

        let pts: Vec<Point> = CounterGridRate::new(
            ts.to_vec(),
            &vals,
            3 * S,
            9 * S,
            S,
            S,
            Some(windows.clone()),
        )
        .collect();

        let observed: Vec<(u64, bool, bool)> = pts
            .iter()
            .map(|p| (p.t / S, p.bounds.is_some(), p.interpolated))
            .collect();

        assert_eq!(
            observed,
            vec![
                // Adjacent reads either side: a band, no flag.
                (4, true, false),
                (5, true, false),
                // Inside the hole: value only.
                (6, false, true),
                (7, false, true),
                // t=8 is a real read, but its interval reaches back to 7s,
                // which nobody observed — so it is interpolated too.
                (8, false, true),
                // Both edges real again.
                (9, true, false),
            ],
        );

        // The values are unchanged by any of this: the hole-spanning points
        // still report the true average across it, 400 over 3s.
        let gap: Vec<f64> = pts.iter().filter(|p| p.interpolated).map(|p| p.v).collect();
        for v in gap {
            assert!((v - 400.0 / 3.0).abs() < 1e-6, "v={v}");
        }
    }

    /// A gapped series keeps its bands on the points that have them.
    ///
    /// `intervals` is all-or-nothing by construction and cannot express a
    /// partial set, so it goes `None` here — its documented, unchanged
    /// behaviour. `bands` is the lossless view the renderer reads, and
    /// `interpolated` says which points lost their band and why.
    #[test]
    fn a_hole_does_not_strip_bands_from_the_points_that_have_them() {
        use crate::promql::streaming::collect_to_matrix;
        use crate::promql::streaming::LabeledSeries;

        const S: u64 = 1_000_000_000;
        let ts = [3 * S, 4 * S, 5 * S, 8 * S, 9 * S];
        let vals = [100u64, 200, 300, 700, 800];
        let windows: Vec<(u64, u64)> = ts.iter().map(|&t| (t - 50_000_000, t)).collect();
        let pts: Vec<Point> = CounterGridRate::new(
            ts.to_vec(),
            &vals,
            3 * S,
            9 * S,
            S,
            S,
            Some(windows.clone()),
        )
        .collect();

        let series = vec![LabeledSeries::new(Default::default(), pts.into_iter())];
        let out = collect_to_matrix(series, Some("probe"));
        let m = &out[0];

        assert!(
            m.intervals.is_none(),
            "the legacy all-or-nothing field cannot carry a partial set"
        );
        let bands = m
            .bands
            .as_ref()
            .expect("bands present when any point has one");
        assert_eq!(bands.len(), m.values.len(), "parallel to values");
        let present: Vec<bool> = bands.iter().map(|b| b.is_some()).collect();
        assert_eq!(
            present,
            vec![true, true, false, false, false, true],
            "bands survive on the observed points"
        );
        assert_eq!(
            m.interpolated.as_deref(),
            Some(&[false, false, true, true, true, false][..]),
            "and the gap points say why theirs are missing"
        );
    }

    /// The no-band invariant has to survive the operators.
    ///
    /// Found by rendering a real chart: `sum(irate(a)) / sum(irate(b))` over a
    /// series with a hole drew an uncertainty band straight across it, because
    /// `combine_bounds` derived one from the operand that still had a band and
    /// treated the unobserved one as exact. The flag alone is not enough — a
    /// consumer that trusts `bounds` would have shown a confident band over an
    /// interval nobody watched.
    #[test]
    fn combining_with_an_interpolated_operand_drops_the_band() {
        use crate::promql::streaming::{matrix_matrix_op, BinOp, LabeledSeries, MatchSpec};

        let banded = Point {
            t: 1,
            v: 10.0,
            bounds: Some((9.0, 11.0)),
            edges: None,
            interpolated: false,
        };
        let hole = Point {
            t: 1,
            v: 2.0,
            bounds: None,
            edges: None,
            interpolated: true,
        };

        let left: Vec<LabeledSeries<'_>> = vec![LabeledSeries::new(
            Default::default(),
            std::iter::once(banded),
        )];
        let right: Vec<LabeledSeries<'_>> = vec![LabeledSeries::new(
            Default::default(),
            std::iter::once(hole),
        )];

        let out = matrix_matrix_op(left, right, BinOp::Div, MatchSpec::Default);
        let pts: Vec<Point> = out.into_iter().next().unwrap().iter.collect();

        assert_eq!(pts.len(), 1);
        assert!(pts[0].interpolated, "the hole taints the combination");
        assert_eq!(
            pts[0].bounds, None,
            "and takes the band with it — a band derived from only the observed \
             operand claims a precision the result does not have"
        );
    }

    /// The strict rule must not disturb the ordinary offset-grid case.
    ///
    /// A grid edge landing between two ADJACENT reads is legitimately described
    /// by an interpolation between their windows — that is not a hole, and
    /// stripping its band would remove uncertainty bands from most queries.
    #[test]
    fn grid_rate_offset_from_the_samples_still_carries_a_band() {
        const S: u64 = 1_000_000_000;
        let ts = [500_000_000u64, 1_500_000_000, 2_500_000_000];
        let vals = [0u64, 100, 200];
        let windows: Vec<(u64, u64)> = ts.iter().map(|&t| (t - 50_000_000, t)).collect();

        let pts: Vec<Point> =
            CounterGridRate::new(ts.to_vec(), &vals, 0, 3 * S, S, S, Some(windows.clone()))
                .collect();

        assert_eq!(pts.len(), 1);
        assert!(
            pts[0].bounds.is_some(),
            "an edge between adjacent reads is not a hole"
        );
        assert!(!pts[0].interpolated);
    }

    #[test]
    fn grid_rate_interpolates_between_offset_samples() {
        // Samples offset 0.5s from the grid; the process is a constant 100/s.
        // Only t=2s has both edges (1s, 2s) inside the sample range [0.5, 2.5],
        // and interpolation must recover 100/s there — the case that
        // distinguishes Grid from a naive per-window sum.
        let ts = [500_000_000u64, 1_500_000_000, 2_500_000_000];
        let vals = [0u64, 100, 200];
        let pts: Vec<Point> = CounterGridRate::new(
            ts.to_vec(),
            &vals,
            0,
            3_000_000_000,
            1_000_000_000,
            1_000_000_000,
            None,
        )
        .collect();
        assert_eq!(pts.len(), 1, "only the interior grid point is emitted");
        assert_eq!(pts[0].t, 2_000_000_000);
        // V(2s)=150 (interp 1.5→2.5), V(1s)=50 (interp 0.5→1.5) → 100/s.
        assert!((pts[0].v - 100.0).abs() < 1e-6, "v={}", pts[0].v);
    }

    #[test]
    fn grid_rate_handles_counter_reset() {
        // Reset between idx 1 and 2 (100 → 50): reset-adjusted increments are
        // 100, 50, 100 over 1s each.
        let ts = [0u64, 1_000_000_000, 2_000_000_000, 3_000_000_000];
        let vals = [0u64, 100, 50, 150];
        let pts: Vec<Point> = CounterGridRate::new(
            ts.to_vec(),
            &vals,
            0,
            3_000_000_000,
            1_000_000_000,
            1_000_000_000,
            None,
        )
        .collect();
        let vs: Vec<f64> = pts.iter().map(|p| p.v).collect();
        assert_eq!(vs.len(), 3);
        assert!((vs[0] - 100.0).abs() < 1e-6, "{vs:?}");
        assert!((vs[1] - 50.0).abs() < 1e-6, "{vs:?}");
        assert!((vs[2] - 100.0).abs() < 1e-6, "{vs:?}");
    }

    /// A span wider than the step smooths each value WITHOUT moving the
    /// points.
    ///
    /// That separation is the whole reason the span exists. Coarsening the
    /// step to smooth also relocates the evaluation grid, and when a query
    /// combines a fast source with a slow one the grid then lands where the
    /// slow source has no reading — its window gets interpolated across the
    /// gap and the combined uncertainty band explodes. Widening the span
    /// leaves the grid, and therefore the points where both sources really
    /// have data, exactly where they were.
    /// The first evaluation point may coincide exactly with the first sample,
    /// and the point that measures across from it must still be emitted.
    ///
    /// This is the normal case when the points come from a series' own rows,
    /// so an interpolation that demanded a strictly-earlier sample would drop
    /// the first value of every such query.
    #[test]
    fn a_first_point_on_the_first_sample_still_yields_the_next_rate() {
        const S: u64 = 1_000_000_000;
        let ts = [1_500_000_000u64, 4_500_000_000, 10_500_000_000];
        let vals = [1u64, 7, 19];
        let points: std::sync::Arc<[u64]> =
            vec![1_500_000_000u64, 4_500_000_000, 10_500_000_000].into();
        let pts: Vec<Point> = CounterGridRate::new(ts.to_vec(), &vals, 0, 14 * S, S, S, None)
            .at_points(points)
            .collect();
        let times: Vec<u64> = pts.iter().map(|p| p.t).collect();
        assert_eq!(times, vec![4_500_000_000, 10_500_000_000], "got {times:?}");
    }

    /// Explicit evaluation points land exactly where asked, even when they
    /// are IRREGULARLY spaced, and each value covers the gap it follows.
    ///
    /// Irregularity is the reason this mode exists. A slow sampler's readings
    /// are not evenly spaced — measured 30 s apart and then 60 s apart on a
    /// real recording — so no uniform grid can sit on them at any step or
    /// phase. Evaluating on the grid instead forces the slow operand to be
    /// held or interpolated between real readings, and whatever it is combined
    /// with inherits that as uncertainty.
    #[test]
    fn explicit_points_are_honoured_including_irregular_spacing() {
        const S: u64 = 1_000_000_000;
        // A counter climbing by 100/s, sampled every second.
        let ts: Vec<u64> = (0..=10).map(|i| i * S).collect();
        let vals: Vec<u64> = (0..=10).map(|i| i * 100).collect();

        // 30 s / 60 s in miniature: gaps of 2 s then 4 s.
        let points: std::sync::Arc<[u64]> = vec![2 * S, 4 * S, 8 * S].into();
        let pts: Vec<Point> = CounterGridRate::new(ts.to_vec(), &vals, 0, 10 * S, S, S, None)
            .at_points(points)
            .collect();

        let times: Vec<u64> = pts.iter().map(|p| p.t).collect();
        assert_eq!(
            times,
            vec![4 * S, 8 * S],
            "values land on the supplied points; the first has no predecessor \
             to measure a rate from, exactly as the grid's start does"
        );

        // The rate is constant, so an uneven gap must not distort it — that is
        // what proves the divisor is the ACTUAL gap and not a fixed span.
        for p in &pts {
            assert!(
                (p.v - 100.0).abs() < 1e-6,
                "a steady 100/s counter must read 100/s over any gap, got {}",
                p.v
            );
        }
    }

    #[test]
    fn a_wider_span_smooths_without_moving_the_points() {
        // A deliberately jagged counter: +0, +200, +0, +200 …
        let ts: Vec<u64> = (0..=6).map(|i| i * 1_000_000_000).collect();
        let vals: [u64; 7] = [0, 0, 200, 200, 400, 400, 600];

        let at = |span: u64| -> Vec<Point> {
            CounterGridRate::new(
                ts.to_vec(),
                &vals,
                0,
                6_000_000_000,
                1_000_000_000,
                span,
                None,
            )
            .collect()
        };

        let narrow = at(1_000_000_000);
        let wide = at(2_000_000_000);

        // Points are on the same grid either way — only their VALUES differ.
        let narrow_t: Vec<u64> = narrow.iter().map(|p| p.t).collect();
        let wide_t: Vec<u64> = wide.iter().map(|p| p.t).collect();
        assert_eq!(
            narrow_t.iter().filter(|t| wide_t.contains(t)).count(),
            wide_t.len(),
            "a wider span must not relocate the grid: narrow={narrow_t:?} \
             wide={wide_t:?}"
        );

        let spread = |pts: &[Point]| -> f64 {
            let v: Vec<f64> = pts.iter().map(|p| p.v).collect();
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len() as f64).sqrt()
        };

        assert!(
            spread(&wide) < spread(&narrow),
            "a wider span must smooth: narrow spread {:.1}, wide spread {:.1}",
            spread(&narrow),
            spread(&wide)
        );
    }

    #[test]
    fn grid_rate_derives_bounds_from_interpolated_windows() {
        let ts = [1_000_000_000u64, 2_000_000_000, 3_000_000_000];
        let vals = [0u64, 100, 200];
        // Per-sample acquisition windows (begin, end).
        let windows = [
            (1_000_000_000u64, 1_020_000_000u64),
            (1_980_000_000u64, 2_000_000_000u64),
            (2_980_000_000u64, 3_000_000_000u64),
        ];
        let pts: Vec<Point> = CounterGridRate::new(
            ts.to_vec(),
            &vals,
            0,
            3_000_000_000,
            1_000_000_000,
            1_000_000_000,
            Some(windows.to_vec()),
        )
        .collect();
        // t=1s dropped (left edge 0 precedes first sample); emit t=2s, t=3s.
        assert_eq!(
            pts.iter().map(|p| p.t).collect::<Vec<_>>(),
            vec![2_000_000_000, 3_000_000_000]
        );
        let p = pts[0]; // t=2s, interval [1s, 2s]
        assert!((p.v - 100.0).abs() < 1e-6, "nominal {}", p.v);
        let (lo, hi) = p.bounds.expect("grid bounds present");
        // increase=100; elapsed_max=(2.00-1.00)=1.0s → 100;
        // elapsed_min=(1.98-1.02)=0.96s → 104.17. Band widened to contain 100.
        assert!((lo - 100.0).abs() < 0.05, "lo {lo}");
        assert!((hi - 104.1667).abs() < 0.05, "hi {hi}");
        assert!(lo <= p.v && p.v <= hi);
    }

    // Note: the interpolated-window bounds convention (Grid) supersedes the old
    // whole-window `CounterRate` bounds tests, which were retired with that
    // producer. `grid_rate_derives_bounds_from_interpolated_windows` above is
    // the replacement coverage.
}
