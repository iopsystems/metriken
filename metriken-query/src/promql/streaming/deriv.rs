//! Streaming `deriv()` consumer.
//!
//! Generic least-squares slope over a forward-leaning window
//! `[t - 2*step, t + step]` of upstream points, evaluated at each
//! step tick. Mirrors the eager `calculate_deriv_for_*` semantics.
//!
//! Used in two places:
//!
//! * Direct gauge deriv via the random-access [`super::GaugeDeriv`]
//!   producer (cheaper because gauges live in a borrowed slice).
//! * Counter-rate deriv: chained on top of
//!   [`super::CounterPairwiseRate`] for the 2nd-derivative case.
//!   The pair-wise rate stream's timestamps come from the source
//!   counter pairs (not a step grid), so we can't random-access —
//!   hence the buffered sliding-window form here.

use std::collections::VecDeque;

use super::Point;

pub struct StreamingDeriv<I: Iterator<Item = Point>> {
    upstream: std::iter::Peekable<I>,
    cursor_ns: u64,
    end_ns: u64,
    step_ns: u64,
    /// Points currently within the sliding window
    /// `[cursor - 2*step, cursor + step]`. Front-trimmed as the
    /// cursor advances; back-extended by pulling upstream until the
    /// next peeked timestamp exceeds the window's right edge.
    buffer: VecDeque<Point>,
    done: bool,
}

impl<I: Iterator<Item = Point>> StreamingDeriv<I> {
    pub fn new(upstream: I, start_ns: u64, end_ns: u64, step_ns: u64) -> Self {
        Self {
            upstream: upstream.peekable(),
            cursor_ns: start_ns,
            end_ns,
            step_ns,
            buffer: VecDeque::new(),
            done: step_ns == 0,
        }
    }
}

impl<I: Iterator<Item = Point>> Iterator for StreamingDeriv<I> {
    type Item = Point;

    fn next(&mut self) -> Option<Point> {
        while !self.done && self.cursor_ns <= self.end_ns {
            let t = self.cursor_ns;
            let window_start = t.saturating_sub(self.step_ns.saturating_mul(2));
            let window_end = t.saturating_add(self.step_ns);

            // Pull upstream until next peeked ts is past window_end.
            while let Some(&(next_ts, _)) = self.upstream.peek() {
                if next_ts > window_end {
                    break;
                }
                self.buffer
                    .push_back(self.upstream.next().expect("peek matched"));
            }

            // Evict points before window_start.
            while let Some(&(ts, _)) = self.buffer.front() {
                if ts < window_start {
                    self.buffer.pop_front();
                } else {
                    break;
                }
            }

            // Advance cursor for the next call.
            match self.cursor_ns.checked_add(self.step_ns) {
                Some(next) => self.cursor_ns = next,
                None => self.done = true,
            }

            if self.buffer.len() < 2 {
                continue;
            }

            // Allocation-free least-squares slope. `x` in seconds.
            let n = self.buffer.len() as f64;
            let mut sum_x = 0.0_f64;
            let mut sum_y = 0.0_f64;
            let mut sum_xy = 0.0_f64;
            let mut sum_x2 = 0.0_f64;
            for &(ts, y) in self.buffer.iter() {
                let x = ts as f64 / 1e9;
                sum_x += x;
                sum_y += y;
                sum_xy += x * y;
                sum_x2 += x * x;
            }
            let denom = n * sum_x2 - sum_x * sum_x;
            if denom.abs() < 1e-10 {
                return Some((t, 0.0));
            }
            return Some((t, (n * sum_xy - sum_x * sum_y) / denom));
        }
        None
    }
}
