//! What a recorder asks of its caller about a producer.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use url::Url;

use crate::StreamAnswer;

/// What to do with an endpoint whose stream route did not open.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verdict {
    /// Not a producer that streams: scrape it as Prometheus.
    Scrape,
    /// A producer that should stream and cannot. `reason` is the whole line
    /// the user is shown.
    Refuse { reason: String },
    /// Not known yet; ask again next tick.
    Pending,
}

/// When the hook is asked, which decides how a refusal is worded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum At {
    /// Before the archive exists: a refusal ends the run.
    Startup,
    /// After the archive opened, on an endpoint's first activation: a
    /// refusal leaves this endpoint out.
    Activation,
    /// On a reconnect, after the stream had been open: the endpoint's
    /// recording stops with the rows it had.
    Reconnect,
}

/// What a producer says about itself beyond its handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Enrich {
    Metadata {
        /// The producer's epoch as the hook read it. A reply whose epoch
        /// differs from the handshake's uuid is about another process and
        /// is discarded.
        epoch: Option<String>,
        labels: BTreeMap<String, String>,
        metadata: BTreeMap<String, String>,
    },
    /// Nothing answered; ask again later.
    Retry,
}

/// What a recorder asks of the caller about a producer it cannot classify
/// from the stream route alone, and about what the producer's handshake
/// does not carry.
#[async_trait]
pub trait Probe: Send + Sync {
    /// The stream route at `url` answered `answer` rather than a handshake.
    /// Called at detection ([`At::Startup`], [`At::Activation`]) for the
    /// verdict, and on a reconnect ([`At::Reconnect`]) for the reason text
    /// only: a streamed source is never switched to a scrape, and a verdict
    /// other than `Refuse` there gives the route's answer as the reason.
    async fn not_streamable(&self, url: &Url, at: At, answer: &StreamAnswer) -> Verdict;

    /// Labels and metadata for the streamed producer at `url`, whose
    /// handshake named `epoch`. Runs after the handshake, so it never sets
    /// the epoch or the clock anchor.
    async fn enrich(&self, url: &Url, epoch: Option<&str>) -> Enrich;

    /// Not called by the recorder. A restarted producer's metadata is written
    /// as new entries in the recording's identity layers, whose fold takes a
    /// key's newest value in each layer, the higher layer winning, and merges
    /// `descriptions` key by key.
    fn merge(&self, key: &str, old: &str, new: &str) -> String {
        let _ = (key, old);
        new.to_string()
    }
}

/// The hook for a caller with nothing to add: a producer recording itself,
/// or one recording producers that put their identity in their handshake.
///
/// `not_streamable` returns [`Verdict::Pending`] for an answer that asking
/// again could change (a 5xx, 408, 429, a timeout, a connection error) and
/// [`Verdict::Scrape`] otherwise. `enrich` adds nothing.
pub struct NoProbe;

#[async_trait]
impl Probe for NoProbe {
    async fn not_streamable(&self, _url: &Url, _at: At, answer: &StreamAnswer) -> Verdict {
        if answer.is_transient() {
            Verdict::Pending
        } else {
            Verdict::Scrape
        }
    }

    async fn enrich(&self, _url: &Url, _epoch: Option<&str>) -> Enrich {
        Enrich::Metadata {
            epoch: None,
            labels: BTreeMap::new(),
            metadata: BTreeMap::new(),
        }
    }
}

/// Labels and metadata from the probe hook.
pub type Enriched = (BTreeMap<String, String>, BTreeMap<String, String>);

/// An [`Enrich`] reply as labels and metadata, or `None` when there is none
/// to take: `Retry`, or a reply about a process other than `epoch`'s (the
/// producer restarted between the handshake and the hook's reads). A reply
/// with no epoch cannot be checked and is taken.
pub fn accept(reply: Enrich, epoch: Option<&str>) -> Option<Enriched> {
    match reply {
        Enrich::Retry => None,
        Enrich::Metadata {
            epoch: Some(seen), ..
        } if epoch.is_some_and(|e| e != seen) => None,
        Enrich::Metadata {
            labels, metadata, ..
        } => Some((labels, metadata)),
    }
}

/// [`Probe::enrich`] bounded by `timeout`, taken through [`accept`]. `Err`
/// says why there is nothing to take.
pub async fn enrich_within(
    probe: &dyn Probe,
    url: &Url,
    epoch: Option<&str>,
    timeout: Duration,
) -> Result<Enriched, String> {
    match tokio::time::timeout(timeout, probe.enrich(url, epoch)).await {
        Err(_) => Err(format!(
            "its metadata did not arrive within {}",
            humantime::format_duration(timeout)
        )),
        Ok(Enrich::Retry) => Err("its metadata routes did not answer".to_string()),
        Ok(reply) => accept(reply, epoch)
            .ok_or_else(|| "its metadata describes another process (it restarted)".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_reply_about_another_process_is_discarded() {
        let reply = |epoch: Option<&str>| Enrich::Metadata {
            epoch: epoch.map(str::to_string),
            labels: BTreeMap::new(),
            metadata: map(&[("version", "1")]),
        };
        assert!(accept(reply(Some("a")), Some("a")).is_some());
        assert!(accept(reply(Some("b")), Some("a")).is_none());
        assert!(
            accept(reply(None), Some("a")).is_some(),
            "unchecked is taken"
        );
        assert!(accept(reply(Some("b")), None).is_some());
        assert!(accept(Enrich::Retry, Some("a")).is_none());
    }

    /// `NoProbe` waits out an answer that may change and scrapes on any
    /// other: the producer said what it is.
    #[tokio::test]
    async fn no_probe_is_pending_on_a_transient_answer_and_scrapes_otherwise() {
        let url: Url = "http://h:1/".parse().unwrap();
        for (answer, verdict) in [
            (StreamAnswer::Status(503), Verdict::Pending),
            (StreamAnswer::Status(429), Verdict::Pending),
            (StreamAnswer::Timeout, Verdict::Pending),
            (StreamAnswer::Unreachable, Verdict::Pending),
            (StreamAnswer::Status(404), Verdict::Scrape),
            (
                StreamAnswer::ContentType("text/plain".into()),
                Verdict::Scrape,
            ),
        ] {
            assert_eq!(
                NoProbe.not_streamable(&url, At::Startup, &answer).await,
                verdict,
                "{answer}"
            );
        }
    }
}
