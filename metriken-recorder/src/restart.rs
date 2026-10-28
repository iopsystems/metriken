//! A producer that restarts mid-recording.
//!
//! When a streamed producer restarts, every cumulative counter starts again
//! from zero and the new process may be a different build. The reconnect's
//! handshake names a new epoch, which the recorder writes into the
//! recording's identity layers once a row from the new process is staged: a
//! recorder entry and a producer entry (its handshake) for each new epoch,
//! the newest recorder entry with what the probe hook reports about its
//! process.

use std::collections::BTreeMap;

/// A restart seen on a source, waiting for its first row and then for the
/// new process's metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Restart {
    /// The new process's producer epoch.
    pub epoch: String,
    /// The timestamp of the first row staged from the new process; `None`
    /// until one is.
    pub from_ts: Option<u64>,
    /// The new process's handshake labels and metadata, kept per restart so
    /// that one replaced by a later restart before it was written still
    /// gets its producer entry.
    pub labels: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
}

/// How many times the probe hook is asked for the restarted producer's
/// metadata, one tick apart, before the restart is written without it.
pub(crate) const FETCH_ATTEMPTS: u32 = 3;

#[cfg(test)]
mod tests {
    use crate::keys;

    #[test]
    fn the_keys_match_dendro() {
        assert_eq!(keys::PRODUCER_EPOCH, dendro::keys::PRODUCER_EPOCH);
        assert_eq!(keys::PRODUCER_EPOCHS, dendro::keys::PRODUCER_EPOCHS);
    }
}
