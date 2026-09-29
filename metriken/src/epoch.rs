//! The process's **producer epoch**: an opaque id that changes exactly when
//! every cumulative counter the process exposes restarts from zero together,
//! and the clock anchor its timestamps are relative to.
//!
//! A metriken process's counters start at zero when it starts and count up
//! until it exits, so a restart resets all of them at once, and nothing short
//! of a restart does. The epoch names one run.
//!
//! # Why a consumer needs it
//!
//! From the values alone, a counter that was reset and one that wrapped are
//! identical: both went down, and their arithmetic is not (`cur` versus
//! `cur + (2^w - prev)`). A counter reset to zero that counts past its
//! previous value before the next observation shows no drop at all. An epoch
//! change is visible either way. It settles the restart case for every
//! counter at once; a single counter that wraps, or that its producer zeroes
//! on read, needs a generation per counter, which this is not.
//!
//! # Why random rather than derived
//!
//! A restart counter would have to be persisted and would be wrong after any
//! state loss; host identity plus boot time needs a source per platform and
//! collides for two processes started in the same second. A random id needs
//! no state and collides with negligible probability.
//!
//! The key name and semantics follow dendro's `keys::PRODUCER_EPOCH`.

use std::sync::OnceLock;
use std::time::Instant;

/// One run of this process: what its counters are relative to, and what its
/// timestamps are relative to.
///
/// The two are minted together because they describe the same thing and a
/// consumer must never see them disagree. An epoch says "these counters all
/// started here"; the anchor says "these timestamps all started here". They
/// begin and end at the same moment — process start to process exit — so
/// deriving them from one `OnceLock` makes a second timeline for one epoch
/// unrepresentable rather than merely discouraged.
struct Source {
    epoch: String,
    /// The wall clock when this process's timeline was anchored.
    anchor_wall_ns: i64,
    /// The monotonic instant that reading was taken at. Elapsed time from here
    /// is what advances the timeline, so an NTP step moves `wall_offset` and
    /// never a timestamp.
    anchor: Instant,
}

static SOURCE: OnceLock<Source> = OnceLock::new();

fn source() -> &'static Source {
    SOURCE.get_or_init(|| Source {
        epoch: mint(),
        anchor_wall_ns: wall_now_ns(),
        anchor: Instant::now(),
    })
}

fn wall_now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// This process's epoch, minted on first call and stable thereafter.
///
/// Callers must not cache it across a restart, which is the whole point: the
/// value is the identity of one run of one process.
pub fn producer_epoch() -> &'static str {
    &source().epoch
}

/// The wall clock this process's timeline is anchored to, in nanoseconds since
/// the Unix epoch.
///
/// One value per process, not per connection. A subscriber is told this in the
/// handshake and every row it receives is `anchor + elapsed` from it, so two
/// subscribers to one process must be told the same number: the source they name
/// is the same source, and two anchors for it would put its rows on two
/// timelines that differ by however far the wall clock moved between the
/// connections.
pub fn clock_anchor_wall_ns() -> i64 {
    source().anchor_wall_ns
}

/// This moment on the source's timeline: `anchor + monotonic elapsed`.
///
/// Derived from the monotonic clock rather than read from the wall clock, so
/// it cannot go backwards. A sealed segment holding a decreasing timestamp
/// would feed `rate()` a `dt <= 0`, which is not a value a consumer can
/// interpret — where a wall-clock step is something it can see and account
/// for, through `wall_offset`.
pub fn anchored_ts(now: Instant) -> i64 {
    let src = source();
    src.anchor_wall_ns + now.saturating_duration_since(src.anchor).as_nanos() as i64
}

/// Anchor this process's timeline now, if nothing has yet.
///
/// Called at startup so the anchor precedes every sample. Without it the
/// timeline is anchored by whichever consumer asks first, and any `Instant`
/// taken before that saturates to the anchor — several samples could share one
/// timestamp, and the process would report a start time later than its own.
pub fn anchor_now() {
    let _ = source();
}

/// This moment as `(anchored ts, wall_offset)`.
///
/// `wall_offset` is the wall clock's disagreement with the timeline at this
/// moment: `ts + wall_offset` is the wall clock. It is a per-observation value
/// rather than a property of the source precisely because it moves — that is
/// what locates a clock step to the moment it happened, instead of absorbing
/// it into every timestamp after.
pub fn anchored_now() -> (i64, i64) {
    let now = Instant::now();
    let wall = wall_now_ns();
    let ts = anchored_ts(now);
    (ts, wall - ts)
}

/// A random RFC 4122 version-4 UUID, formatted canonically.
///
/// The same shape dendro mints for a source, so the two are comparable by eye
/// in a manifest even though neither generates the other's.
pub fn mint() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        // The OS refused to give us 16 random bytes, which on a running system
        // means something is badly wrong. An epoch is still better than none:
        // a consumer needs a value that CHANGES across restarts, and the clock
        // plus the pid does that. It is not a v4 UUID and is not formatted as
        // one, so it cannot be mistaken for a real mint.
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        return format!("fallback-{ns:x}-{}", std::process::id());
    }
    // Version 4, variant 1, per RFC 4122 §4.4.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The anchor is stable for the same reason the epoch is, and they are one
    /// value: a consumer is told both and uses them together to place a
    /// reading, so an anchor that moved while the epoch did not would put two
    /// readings of one source on two timelines.
    #[test]
    fn the_anchor_is_stable_within_a_process() {
        assert_eq!(clock_anchor_wall_ns(), clock_anchor_wall_ns());
    }

    /// The timeline has wall-clock magnitude: a consumer adds `wall_offset`
    /// and gets a real Unix timestamp, so `ts` cannot be a bare monotonic
    /// reading counting from boot.
    #[test]
    fn the_timeline_reads_as_wall_time() {
        let ts = anchored_ts(Instant::now());
        let wall = wall_now_ns();
        assert!(
            (ts - wall).abs() < 1_000_000_000,
            "the timeline is {} ns from the wall clock, which is not a clock \
             offset but a different kind of number",
            ts - wall
        );
    }

    /// What the anchored construction is FOR: a wall-clock step cannot make a
    /// timestamp go backwards. A sealed segment holding a decreasing timestamp
    /// feeds `rate()` a `dt <= 0`, which is not something a consumer can
    /// interpret.
    ///
    /// Asserted over the monotonic clock rather than by stepping the machine's
    /// wall clock, which a test cannot do: `anchored_ts` reads the wall clock
    /// exactly once, at the anchor, so there is no later read for a step to
    /// affect. That is the property, stated the only way it can be observed
    /// from inside the process.
    #[test]
    fn the_timeline_never_goes_backwards() {
        let a = Instant::now();
        let b = a + std::time::Duration::from_secs(1);
        assert!(anchored_ts(b) > anchored_ts(a));
        // An instant BEFORE the anchor saturates rather than running the
        // timeline backwards past its own origin.
        assert_eq!(
            anchored_ts(source().anchor - std::time::Duration::from_secs(60)),
            clock_anchor_wall_ns()
        );
    }

    /// Stable within a process: two reads of the same run must agree, or a
    /// consumer would see an epoch change where no counter restarted.
    #[test]
    fn the_epoch_is_stable_within_a_process() {
        assert_eq!(producer_epoch(), producer_epoch());
    }

    /// Canonical v4 shape, so it is recognizable as a UUID wherever it lands.
    #[test]
    fn a_minted_epoch_is_a_canonical_v4_uuid() {
        let id = mint();
        assert_eq!(id.len(), 36, "{id}");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{id}"
        );
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "{id}"
        );
        assert_eq!(parts[2].as_bytes()[0], b'4', "version nibble: {id}");
        assert!(
            matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
            "variant nibble: {id}"
        );
    }

    /// Two mints differ. The id exists to distinguish runs, so a generator
    /// that repeated itself would defeat the whole mechanism silently.
    #[test]
    fn two_mints_differ() {
        assert_ne!(mint(), mint());
    }
}
