use std::fmt;

/// What a stream route answered when it did not open a subscription.
///
/// Structured so that what decides the next step (the [`Probe`] hook, the
/// retry rule) reads the answer rather than parsing an error message.
///
/// [`Probe`]: crate::Probe
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamAnswer {
    /// An HTTP status other than success.
    Status(u16),
    /// A success whose `Content-Type` is none of the accepted wire names.
    ContentType(String),
    /// A success in an accepted content type whose first frame does not
    /// decode, is not a handshake, or does not apply.
    Malformed(String),
    /// No handshake within the caller's timeout.
    Timeout,
    /// No answer: the connection failed, or closed before the handshake.
    Unreachable,
}

impl StreamAnswer {
    /// Whether asking again could get a different answer: a 5xx, 408 or 429
    /// (the path between here and the producer; a proxy answering while the
    /// producer behind it restarts is the common case), a timeout, or a
    /// connection failure. Every other answer is the producer saying what it
    /// is.
    pub fn is_transient(&self) -> bool {
        match self {
            StreamAnswer::Status(code) => matches!(code, 408 | 429 | 500..=599),
            StreamAnswer::Timeout | StreamAnswer::Unreachable => true,
            StreamAnswer::ContentType(_) | StreamAnswer::Malformed(_) => false,
        }
    }
}

impl fmt::Display for StreamAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamAnswer::Status(code) => write!(f, "HTTP {code}"),
            StreamAnswer::ContentType(ct) if ct.is_empty() => {
                f.write_str("a response with no content type")
            }
            StreamAnswer::ContentType(ct) => write!(f, "content type `{ct}`"),
            StreamAnswer::Malformed(e) => write!(f, "a malformed handshake ({e})"),
            StreamAnswer::Timeout => f.write_str("no handshake within the timeout"),
            StreamAnswer::Unreachable => f.write_str("no answer"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_answers_are_the_ones_a_retry_can_change() {
        for code in [408, 429, 500, 502, 503, 599] {
            assert!(StreamAnswer::Status(code).is_transient(), "{code}");
        }
        for code in [400, 403, 404, 409, 410] {
            assert!(!StreamAnswer::Status(code).is_transient(), "{code}");
        }
        assert!(StreamAnswer::Timeout.is_transient());
        assert!(StreamAnswer::Unreachable.is_transient());
        assert!(!StreamAnswer::ContentType("text/html".into()).is_transient());
        assert!(!StreamAnswer::Malformed("x".into()).is_transient());
    }
}
