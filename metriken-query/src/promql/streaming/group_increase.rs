//! `increase(x[r])` over a process group of several members (S9).
//!
//! `rate` over a group takes each point from one member: the rate over a
//! member's part of the window is the rate. An increase is not, so a point
//! here is the sum over the window's pieces: `[t - r, t]` is cut at the
//! edges of every member's runs of rows (S3), each piece is answered by the
//! first member in rank order (finest first, then the tie rule) whose rows
//! are on both sides of its middle, or else the first whose run holds it
//! (up to an interval past its last row), and each adds that member's
//! reset-adjusted increase across it, interpolated between its rows, from
//! its run's first row to the member's last row. Where a later piece starts
//! after the earlier one ends, the counter's increase between them is added
//! where the counter runs on: within one member (across a hole in its rows,
//! from its reset-adjusted values) and between members of one producer
//! epoch (copies of one process) as the forward difference: a lower value
//! there adds nothing, so a disagreement between two reads is not taken
//! for a reset, and a reset that falls between two members' rows (a
//! counter that wraps, or a sampler that zeroes on read) is not counted
//! either. Nothing is added at
//! the instant one member hands over to another, nor across a restart
//! (members of two epochs): the counter started again, and the counts
//! between the last row before the restart and the first after it were
//! read by neither.

use std::collections::HashMap;

use crate::labels::Labels;
use crate::types::CounterSample;

use super::per_source::runs;
use super::rate::counter_increase;
use super::Point;

/// One member's samples of each series it holds, in rank order among the
/// group's members.
pub(crate) struct MemberRows {
    /// The member's producer epoch, `None` when its source has none.
    pub epoch: Option<String>,
    /// The member's interval for the counter.
    pub interval_ns: u64,
    /// Each series' samples, in time order, within the member's span.
    pub series: Vec<(Labels, Vec<CounterSample>)>,
}

/// One member's rows of one series.
struct Track {
    /// `(time, raw value, reset-adjusted cumulative increase)` per row.
    rows: Vec<(u64, f64, f64)>,
    runs: Vec<(u64, u64)>,
    interval_ns: u64,
}

impl Track {
    fn new(samples: Vec<CounterSample>, interval_ns: u64) -> Self {
        let mut rows: Vec<(u64, f64, f64)> = Vec::with_capacity(samples.len());
        let mut prev: Option<CounterSample> = None;
        let mut cum = 0.0;
        for s in samples {
            if prev.is_some_and(|p| s.ts <= p.ts) {
                continue;
            }
            if let Some(p) = prev {
                cum += counter_increase(p.value, s.value);
            }
            rows.push((s.ts, s.value.as_f64(), cum));
            prev = Some(s);
        }
        let ts: Vec<u64> = rows.iter().map(|r| r.0).collect();
        let runs = if ts.is_empty() { Vec::new() } else { runs(&ts) };
        Self {
            rows,
            runs,
            interval_ns,
        }
    }

    /// The run holding `t`: from its first row to one interval past its
    /// last (`per_source::Coverage::run_at`).
    fn run_at(&self, t: u64) -> Option<(u64, u64)> {
        let i = self.runs.partition_point(|r| r.0 <= t);
        let run = *self.runs.get(i.checked_sub(1)?)?;
        (t <= run.1.saturating_add(self.interval_ns)).then_some(run)
    }

    /// The run whose rows are on both sides of `t`, or at it.
    fn run_within(&self, t: u64) -> Option<(u64, u64)> {
        self.run_at(t).filter(|run| t <= run.1)
    }

    /// `t`, from `run`'s first row to the member's last, and the raw value
    /// and cumulative increase there, interpolated between the rows either
    /// side; and whether `t` is past `run`'s last row, interpolated across
    /// the hole after it, as a `rate` interpolates across one.
    fn at(&self, run: (u64, u64), t: u64) -> (u64, f64, f64, bool) {
        let last = self.rows.last().map_or(run.1, |r| r.0);
        let t = t.clamp(run.0, last.max(run.0));
        let i = self.rows.partition_point(|r| r.0 <= t);
        let (t0, raw0, cum0) = self.rows[i - 1];
        if t0 == t || i == self.rows.len() {
            return (t, raw0, cum0, false);
        }
        let (t1, _, cum1) = self.rows[i];
        let frac = (t - t0) as f64 / (t1 - t0) as f64;
        let added = (cum1 - cum0) * frac;
        (t, raw0 + added, cum0 + added, t > run.1)
    }
}

impl Track {
    /// The cumulative increase at `t` interpolated between the member's
    /// rows either side of it, across a hole too, when one is at or before
    /// `t` no more than a reach (two of the interval) before it, as a
    /// `rate`'s window starts (`range_start`).
    fn reached(&self, t: u64) -> Option<f64> {
        let i = self.rows.partition_point(|r| r.0 <= t);
        let (t0, _, cum0) = *self.rows.get(i.checked_sub(1)?)?;
        if t - t0 > self.interval_ns.saturating_mul(2) {
            return None;
        }
        let Some(&(t1, _, cum1)) = self.rows.get(i) else {
            return (t0 == t).then_some(cum0);
        };
        Some(cum0 + (cum1 - cum0) * (t - t0) as f64 / (t1 - t0) as f64)
    }
}

/// `increase(x[range_ns])` at each of `times` over `members` (in rank
/// order), per series, in the order the series first appear. A series has a
/// point at `t` where a member's run holds `t` and the member has a row at
/// or after it, as a member's `rate` has a point. A point that crosses a
/// stretch no member read is interpolated; no point carries a band.
pub(crate) fn increase(
    members: Vec<MemberRows>,
    times: &[u64],
    range_ns: u64,
) -> Vec<(Labels, Vec<Point>)> {
    let n = members.len();
    let epochs: Vec<Option<String>> = members.iter().map(|m| m.epoch.clone()).collect();
    let mut order: Vec<Labels> = Vec::new();
    let mut by_series: HashMap<Labels, Vec<Option<Track>>> = HashMap::new();
    for (i, m) in members.into_iter().enumerate() {
        for (labels, samples) in m.series {
            let tracks = by_series.entry(labels.clone()).or_insert_with(|| {
                order.push(labels.clone());
                (0..n).map(|_| None).collect()
            });
            tracks[i] = Some(Track::new(samples, m.interval_ns));
        }
    }
    // Whether the counter runs on from member `a`'s rows to member `b`'s.
    let continues = |a: usize, b: usize| a == b || (epochs[a].is_some() && epochs[a] == epochs[b]);
    order
        .into_iter()
        .map(|labels| {
            let tracks = by_series.remove(&labels).expect("listed");
            let holder = |t: u64| {
                (0..n).find_map(|i| {
                    let track = tracks[i].as_ref()?;
                    track.run_at(t).map(|run| (i, run))
                })
            };
            // A piece's member: one whose rows are on both sides of it
            // first, so a member that holds it only in the interval after
            // its last row hands it to one that read it.
            let answerer = |t: u64| {
                (0..n)
                    .find_map(|i| {
                        let track = tracks[i].as_ref()?;
                        track.run_within(t).map(|run| (i, run))
                    })
                    .or_else(|| holder(t))
            };
            // The increase from where the last piece ended to where the
            // next starts, `(member, time, raw, cumulative)` each.
            let bridge = |(lm, lt, lraw, lcum): (usize, u64, f64, f64),
                          (m, p, raw_p, cum_p): (usize, u64, f64, f64)|
             -> Option<f64> {
                if !continues(lm, m) || p <= lt {
                    return None;
                }
                Some(if lm == m {
                    cum_p - lcum
                } else {
                    (raw_p - lraw).max(0.0)
                })
            };
            let mut points = Vec::new();
            for &t in times {
                let held = tracks.iter().flatten().any(|track| {
                    track.run_at(t).is_some() && track.rows.last().is_some_and(|r| r.0 >= t)
                });
                if !held {
                    continue;
                }
                let from = t.saturating_sub(range_ns);
                let mut cuts = vec![from, t];
                for track in tracks.iter().flatten() {
                    // The runs touching the window: starting at or before `t`
                    // and held past `from`.
                    let upto = track.runs.partition_point(|r| r.0 <= t);
                    for &(first, last) in track.runs[..upto]
                        .iter()
                        .rev()
                        .take_while(|r| r.1.saturating_add(track.interval_ns) >= from)
                    {
                        cuts.push(first);
                        cuts.push(last);
                        cuts.push(last.saturating_add(track.interval_ns));
                    }
                }
                cuts.retain(|c| (from..=t).contains(c));
                cuts.sort_unstable();
                cuts.dedup();
                let mut total = 0.0;
                let mut read = false;
                let mut interpolated = false;
                // The member, time, raw value and cumulative increase the
                // last piece ended at.
                let mut last: Option<(usize, u64, f64, f64)> = None;
                let mut gap = false;
                for w in cuts.windows(2) {
                    let (x, y) = (w[0], w[1]);
                    let Some((m, run)) = answerer(x + (y - x) / 2) else {
                        gap = true;
                        continue;
                    };
                    let track = tracks[m].as_ref().expect("holds");
                    let (p, raw_p, cum_p, across_p) = track.at(run, x);
                    let (q, raw_q, cum_q, across_q) = track.at(run, y);
                    interpolated |= across_p || across_q;
                    // The window starts in a stretch nobody answers: from
                    // the window's start where this member reaches it, as a
                    // `rate`'s window starts across a hole.
                    if last.is_none() && p > from {
                        if let Some(cum_from) = track.reached(from) {
                            total += cum_p - cum_from;
                            interpolated = true;
                            read = true;
                        }
                    }
                    if let Some(added) = last.and_then(|l| bridge(l, (m, p, raw_p, cum_p))) {
                        total += added;
                        interpolated |= gap;
                    }
                    if q > p {
                        total += cum_q - cum_p;
                        read = true;
                    }
                    // A piece a coarser member answers can end, clamped to
                    // its last row, before the previous piece did; the next
                    // bridge starts from the later end, so nothing is
                    // counted twice.
                    if last.is_none_or(|l| q >= l.1) {
                        last = Some((m, q, raw_q, cum_q));
                    }
                    gap = false;
                }
                // A window ending after a stretch no member read: the
                // counter's increase across it to the value at `t`.
                if gap {
                    if let (Some(l), Some((m, run))) = (last, answerer(t)) {
                        let track = tracks[m].as_ref().expect("holds");
                        let (p, raw_p, cum_p, _) = track.at(run, t);
                        if let Some(added) = bridge(l, (m, p, raw_p, cum_p)) {
                            total += added;
                            interpolated = true;
                            read = true;
                        }
                    }
                }
                if read {
                    points.push(Point {
                        t,
                        v: total,
                        bounds: None,
                        edges: None,
                        interpolated,
                        timing: false,
                    });
                }
            }
            (labels, points)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CounterValue;

    const S: u64 = 1_000_000_000;

    fn member(epoch: &str, interval_s: u64, rows: &[(u64, u64)]) -> MemberRows {
        MemberRows {
            epoch: Some(epoch.to_string()),
            interval_ns: interval_s * S,
            series: vec![(
                Labels::default(),
                rows.iter()
                    .map(|&(t, v)| CounterSample {
                        ts: t * S,
                        value: CounterValue::Int(v),
                        window: None,
                    })
                    .collect(),
            )],
        }
    }

    fn at(members: Vec<MemberRows>, t: u64, range_s: u64) -> Option<f64> {
        increase(members, &[t * S], range_s * S)
            .first()?
            .1
            .first()
            .map(|p| p.v)
    }

    /// A coarser member answering a piece after a finer one's run ends
    /// does not take the next bridge back over what the finer one counted:
    /// a counter equal to its second, read each second (with a hole from
    /// 31 s to 39 s) and every 10 s, counts 50 over `[0, 50]`, as the
    /// finer member alone does.
    #[test]
    fn a_coarser_members_piece_does_not_count_a_stretch_twice() {
        let fine: Vec<(u64, u64)> = (0..=30).chain(40..=50).map(|t| (t, t)).collect();
        let coarse = [(5, 5), (15, 15), (25, 25)];
        let both = vec![member("e", 1, &fine), member("e", 10, &coarse)];
        assert_eq!(at(both, 50, 50), Some(50.0));
        assert_eq!(at(vec![member("e", 1, &fine)], 50, 50), Some(50.0));
    }

    /// A reset that falls between two members' rows is not counted: 100 to
    /// 110 over the first member's rows, 5 to 15 over the second's, gives
    /// the 20 the two read, where one member's rows would give 25 (the reset
    /// adding the 5 after it).
    #[test]
    fn a_reset_between_two_members_is_not_counted() {
        let first: Vec<(u64, u64)> = (0..=10).map(|t| (t, 100 + t)).collect();
        let second: Vec<(u64, u64)> = (20..=30).map(|t| (t, t - 15)).collect();
        let both = vec![member("e", 1, &first), member("e", 1, &second)];
        assert_eq!(at(both, 30, 30), Some(20.0));
    }
}
