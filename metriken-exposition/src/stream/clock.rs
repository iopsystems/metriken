//! A subscription's timer and `seq`: boundaries aligned to the Unix epoch,
//! and the index of the interval an instant falls in.
//!
//! Aligning to absolute time rather than to when a subscription opened makes
//! two producers with the same interval read at the same instants, so their
//! readings line up. Each subscription aligns to its own interval, so an
//! interval need not divide or relate to any other subscription's.

use std::time::Duration;

/// How long from `now_ns` (nanoseconds since the Unix epoch) to the next
/// whole multiple of `interval`. A `now_ns` exactly on a boundary waits a
/// whole interval, so one instant never fires twice. A zero interval waits
/// zero.
pub(crate) fn align_wait(now_ns: u128, interval: Duration) -> Duration {
    let period = interval.as_nanos();
    if period == 0 {
        return interval;
    }
    let wait = period - (now_ns % period);
    Duration::from_nanos(wait.min(u64::MAX as u128) as u64)
}

/// The index of the interval `wall_ns` falls in, counted from the Unix
/// epoch: the `seq` a rows frame carries.
pub(crate) fn interval_index(wall_ns: u64, interval: Duration) -> u64 {
    let period = (interval.as_nanos() as u64).max(1);
    wall_ns / period
}

/// [`interval_index`], held above `last`.
///
/// The index comes from the wall clock, which can step backwards (an NTP
/// correction). A consumer checks `seq` for `last + 1` and has no rule for a
/// lower value, so an index at or below `last` becomes `last + 1`. Each frame
/// carries its pass's own stamp, so nothing that needs absolute time reads
/// `seq`.
pub(crate) fn monotonic_interval_index(wall_ns: u64, interval: Duration, last: Option<u64>) -> u64 {
    let measured = interval_index(wall_ns, interval);
    match last {
        Some(previous) if measured <= previous => previous + 1,
        _ => measured,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four seconds forward, thirty back, then forward again: the indices
    /// stay contiguous.
    #[test]
    fn a_backwards_clock_step_cannot_lower_the_interval_index() {
        let interval = Duration::from_secs(1);
        let base = 1_700_000_000_000_000_000u64;

        let mut last: Option<u64> = None;
        let mut emitted = Vec::new();
        for now in [
            base,
            base + 1_000_000_000,
            base + 2_000_000_000,
            base + 3_000_000_000,
            base - 30_000_000_000,
            base - 29_000_000_000,
            base + 4_000_000_000,
        ] {
            let index = monotonic_interval_index(now, interval, last);
            last = Some(index);
            emitted.push(index);
        }

        for pair in emitted.windows(2) {
            assert_eq!(pair[1], pair[0] + 1, "{emitted:?}");
        }
    }

    #[test]
    fn alignment_lands_on_wall_clock_multiples() {
        let offsets = [0u128, 1, 999, 1_000_000, 123_456_789, 999_999_999];
        for millis in [1u64, 10, 50, 100, 1_000, 5_000, 97_000] {
            let interval = Duration::from_millis(millis);
            let period = interval.as_nanos();
            for base in offsets {
                let now = 1_700_000_000_000_000_000u128 + base;
                let wait = align_wait(now, interval).as_nanos();
                assert!(
                    wait > 0 && wait <= period,
                    "interval {millis}ms at +{base}ns produced a {wait}ns wait"
                );
                assert_eq!(
                    (now + wait) % period,
                    0,
                    "interval {millis}ms at +{base}ns did not land on a boundary"
                );
            }
        }
    }

    /// A 97 s interval fires on its own boundaries, 97 s apart, whenever the
    /// subscription opened.
    #[test]
    fn a_prime_interval_fires_exactly_on_its_own_boundaries() {
        let interval = Duration::from_secs(97);
        let period = interval.as_nanos();

        for start in [1_700_000_000_123_456_789u128, 1_700_000_055_987_654_321] {
            let mut at = start + align_wait(start, interval).as_nanos();
            let mut fired = Vec::new();
            for _ in 0..5 {
                assert_eq!(at % period, 0, "fired off a 97s boundary");
                fired.push(at);
                at += align_wait(at, interval).as_nanos();
            }
            for pair in fired.windows(2) {
                assert_eq!(pair[1] - pair[0], period);
            }
        }
    }

    #[test]
    fn a_zero_interval_does_not_divide_by_zero() {
        assert_eq!(align_wait(12_345, Duration::ZERO), Duration::ZERO);
        assert_eq!(interval_index(12_345, Duration::ZERO), 12_345);
    }
}
