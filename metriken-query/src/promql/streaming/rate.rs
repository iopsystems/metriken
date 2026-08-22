//! Counter rate producers.
//!
//! * [`CounterGridRate`] — `rate`/`irate` in this engine: a fixed-phase
//!   evaluation grid whose value is the reset-adjusted cumulative counter
//!   interpolated across each step interval (see the struct docs).
//! * [`CounterPairwiseRate`] — one point per consecutive sample pair at the
//!   real sample timestamp; used by `RateMode::Raw` and by `deriv`.

use super::{Point, RateEdges};

/// Pair-wise rate producer over a counter sample slice. Emits one point per
/// consecutive sample pair, stamped at the later sample, for pairs whose stamp
/// falls in `[start_ns, end_ns]`. The source slice is fetched with lookback
/// (for context), so the `start_ns` bound is what keeps a windowed/zoomed query
/// from spilling points before the requested window — mirroring how the grid
/// producer's cursor starts at `start_ns`.
pub struct CounterPairwiseRate<'a> {
    timestamps: &'a [u64],
    values: &'a [u64],
    cursor: usize,
    start_ns: u64,
    end_ns: u64,
}

impl<'a> CounterPairwiseRate<'a> {
    pub fn new(timestamps: &'a [u64], values: &'a [u64], start_ns: u64, end_ns: u64) -> Self {
        Self {
            timestamps,
            values,
            cursor: 0,
            start_ns,
            end_ns,
        }
    }
}

impl<'a> Iterator for CounterPairwiseRate<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while self.cursor + 1 < self.timestamps.len() {
            let i = self.cursor;
            self.cursor += 1;
            let ts_prev = self.timestamps[i];
            let v_prev = self.values[i];
            let ts_cur = self.timestamps[i + 1];
            let v_cur = self.values[i + 1];
            if ts_cur > self.end_ns {
                return None;
            }
            // Skip pairs before the requested window; the slice carries lookback
            // context whose stamps precede start_ns.
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
        None
    }
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
pub struct CounterGridRate<'a> {
    timestamps: &'a [u64],
    /// Reset-adjusted monotone cumulative counter, aligned with `timestamps`.
    cum: Vec<f64>,
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    windows: Option<&'a [(u64, u64)]>,
    done: bool,
}

impl<'a> CounterGridRate<'a> {
    pub fn new(
        timestamps: &'a [u64],
        values: &'a [u64],
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
        windows: Option<&'a [(u64, u64)]>,
    ) -> Self {
        // Reset-adjusted cumulative: same convention as CounterRate's
        // total_increase (a decrease is treated as a fresh counter start,
        // contributing its own value as the increment).
        let mut cum = Vec::with_capacity(values.len());
        let mut acc = 0.0;
        for (i, &v) in values.iter().enumerate() {
            if i == 0 {
                cum.push(0.0);
            } else {
                let prev = values[i - 1];
                acc += if v >= prev {
                    (v - prev) as f64
                } else {
                    v as f64
                };
                cum.push(acc);
            }
        }
        Self {
            timestamps,
            cum,
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            windows,
            done: step_ns == 0 || timestamps.len() < 2,
        }
    }

    /// Linearly interpolate the reset-adjusted cumulative value at `edge`.
    /// Returns `None` when `edge` is outside the observed sample range (no
    /// extrapolation).
    fn interp(&self, edge: u64) -> Option<f64> {
        let ts = self.timestamps;
        let last = *ts.last()?;
        if edge < ts[0] || edge > last {
            return None;
        }
        // First index with ts >= edge.
        let hi = ts.partition_point(|&t| t < edge);
        if ts[hi] == edge {
            return Some(self.cum[hi]);
        }
        // edge is strictly between hi-1 and hi (hi >= 1 since edge > ts[0]).
        let lo = hi - 1;
        let span = (ts[hi] - ts[lo]) as f64;
        let frac = (edge - ts[lo]) as f64 / span;
        Some(self.cum[lo] + frac * (self.cum[hi] - self.cum[lo]))
    }

    /// Interpolate the acquisition-window `(begin, end)` at `edge`, in the
    /// same way as [`Self::interp`] does the value, so the uncertainty band
    /// is attributable to the grid edge rather than the nearest raw sample.
    /// `None` when there are no windows or `edge` is outside the sample range.
    fn interp_window(&self, edge: u64) -> Option<(f64, f64)> {
        let w = self.windows?;
        let ts = self.timestamps;
        let last = *ts.last()?;
        if edge < ts[0] || edge > last {
            return None;
        }
        let hi = ts.partition_point(|&t| t < edge);
        let (b_hi, e_hi) = *w.get(hi)?;
        if ts[hi] == edge {
            return Some((b_hi as f64, e_hi as f64));
        }
        let lo = hi - 1;
        let (b_lo, e_lo) = *w.get(lo)?;
        let span = (ts[hi] - ts[lo]) as f64;
        let frac = (edge - ts[lo]) as f64 / span;
        let b = b_lo as f64 + frac * (b_hi as f64 - b_lo as f64);
        let e = e_lo as f64 + frac * (e_hi as f64 - e_lo as f64);
        Some((b, e))
    }
}

impl<'a> Iterator for CounterGridRate<'a> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while !self.done && self.cursor_ns <= self.end_ns {
            let t = self.cursor_ns;
            match self.cursor_ns.checked_add(self.step_ns) {
                Some(next) => self.cursor_ns = next,
                None => self.done = true,
            }

            let Some(left) = t.checked_sub(self.step_ns) else {
                continue;
            };
            let (Some(v_hi), Some(v_lo)) = (self.interp(t), self.interp(left)) else {
                continue;
            };
            let step_s = self.step_ns as f64 / 1e9;
            if step_s <= 0.0 {
                continue;
            }
            let increase = v_hi - v_lo;
            let v = increase / step_s;
            // Band from the interpolated window edges: the elapsed span between
            // the left edge's begin and the right edge's end (widest → slowest)
            // and between the right edge's begin and the left edge's end
            // (narrowest → fastest). Widen to always contain the nominal, which
            // divides by the exact step rather than the window-derived span.
            let window_pair = self.interp_window(left).zip(self.interp_window(t));
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
            &ts,
            &vals,
            0,             // start_ns (already snapped by the caller)
            3_000_000_000, // end_ns
            1_000_000_000, // step_ns
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

    #[test]
    fn grid_rate_interpolates_between_offset_samples() {
        // Samples offset 0.5s from the grid; the process is a constant 100/s.
        // Only t=2s has both edges (1s, 2s) inside the sample range [0.5, 2.5],
        // and interpolation must recover 100/s there — the case that
        // distinguishes Grid from a naive per-window sum.
        let ts = [500_000_000u64, 1_500_000_000, 2_500_000_000];
        let vals = [0u64, 100, 200];
        let pts: Vec<Point> =
            CounterGridRate::new(&ts, &vals, 0, 3_000_000_000, 1_000_000_000, None).collect();
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
        let pts: Vec<Point> =
            CounterGridRate::new(&ts, &vals, 0, 3_000_000_000, 1_000_000_000, None).collect();
        let vs: Vec<f64> = pts.iter().map(|p| p.v).collect();
        assert_eq!(vs.len(), 3);
        assert!((vs[0] - 100.0).abs() < 1e-6, "{vs:?}");
        assert!((vs[1] - 50.0).abs() < 1e-6, "{vs:?}");
        assert!((vs[2] - 100.0).abs() < 1e-6, "{vs:?}");
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
        let pts: Vec<Point> =
            CounterGridRate::new(&ts, &vals, 0, 3_000_000_000, 1_000_000_000, Some(&windows))
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
