//! Acquisition window for a metric observation: the interval over which the
//! value was read. Both ends are recorded so "when did we read it" is a
//! read-time interpretation, not a write-time loss.

/// A measurement's acquisition window, in nanoseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Window {
    /// Start of the acquisition interval (ns since Unix epoch).
    pub begin_ns: u64,
    /// End of the acquisition interval (ns since Unix epoch).
    pub end_ns: u64,
}

impl Window {
    /// Construct a window from begin/end nanoseconds.
    pub const fn new(begin_ns: u64, end_ns: u64) -> Self {
        Self { begin_ns, end_ns }
    }

    /// Width of the window in nanoseconds (saturating; 0 if end precedes begin).
    pub const fn width_ns(&self) -> u64 {
        self.end_ns.saturating_sub(self.begin_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_is_end_minus_begin() {
        let w = Window::new(1_000, 3_500);
        assert_eq!(w.begin_ns, 1_000);
        assert_eq!(w.end_ns, 3_500);
        assert_eq!(w.width_ns(), 2_500);
    }

    #[test]
    fn width_saturates_when_reversed() {
        assert_eq!(Window::new(5, 1).width_ns(), 0);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_roundtrip() {
        let w = Window::new(10, 20);
        let json = serde_json::to_string(&w).unwrap();
        let back: Window = serde_json::from_str(&json).unwrap();
        assert_eq!(w, back);
    }
}
