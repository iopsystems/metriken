//! How an endpoint is recorded (feature `http`).
//!
//! An endpoint whose path is `/` or `/metrics/stream` is asked for its stream
//! first. A handshake in an accepted content type means it is streamed.
//! Anything else (a 404, a 409, a foreign content type, a malformed
//! handshake, a 5xx, a timeout, a connection error) goes to
//! [`Probe::not_streamable`] with the route's [`StreamAnswer`], whose
//! [`Verdict`] decides: scrape it as Prometheus at `/metrics` (once
//! `/metrics` answers with a success), refuse it, or try again next tick. An
//! endpoint with any other path is scraped at its URL, once the URL answers
//! with a success. [`Transport`] overrides the path: `Stream` is only ever
//! streamed, `Prometheus` only ever scraped.

use std::collections::BTreeMap;
use std::time::Duration;

use metriken_model::wire::{Layout, WireNames, WIRE_NAMES};
use reqwest::Client;
use url::Url;

use crate::probe::{At, Probe, Verdict};
use crate::subscription::Subscription;
use crate::StreamAnswer;

/// How an endpoint is declared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    /// Detected: see the module docs.
    #[default]
    Auto,
    /// Recorded from its stream only, never scraped.
    Stream,
    /// Scraped as Prometheus only, at `/metrics` for a URL whose path is `/`
    /// and at the URL otherwise.
    Prometheus,
}

/// An endpoint to record, and what the caller says about it.
#[derive(Clone, Debug)]
pub struct EndpointSpec {
    pub url: Url,
    pub transport: Transport,
    /// The highest-ranked labels; a `source` here names the recording.
    pub labels: BTreeMap<String, String>,
    /// The highest-ranked metadata. These keys are the caller's: a producer
    /// restart, or a scrape's HELP text, never replaces them.
    pub metadata: BTreeMap<String, String>,
}

impl EndpointSpec {
    /// An endpoint with automatic detection and no labels or metadata.
    pub fn new(url: Url) -> Self {
        Self {
            url,
            transport: Transport::Auto,
            labels: BTreeMap::new(),
            metadata: BTreeMap::new(),
        }
    }
}

/// The subscription settings detection opens a stream with, so the stream it
/// hands on is the one the recording reads.
#[derive(Clone, Debug)]
pub struct DetectConfig {
    pub interval: Duration,
    /// Bounds the stream connect, the hook's call, and the check of a
    /// scraped URL, each on its own.
    pub timeout: Duration,
    pub layout: Layout,
    /// The wire names a stream is accepted under; the default is metriken's
    /// alone.
    pub wire_names: Vec<WireNames>,
}

impl DetectConfig {
    /// The long layout under metriken's wire names.
    pub fn new(interval: Duration, timeout: Duration) -> Self {
        Self {
            interval,
            timeout,
            layout: Layout::Long,
            wire_names: vec![WIRE_NAMES],
        }
    }
}

/// How one endpoint is to be recorded, for [`Recorder::start`].
///
/// [`Recorder::start`]: crate::Recorder::start
pub enum Detected {
    /// Streamed over this open subscription, whose handshake has been
    /// applied ([`Subscription::handshake`]).
    Streamed {
        spec: EndpointSpec,
        subscription: Box<Subscription>,
    },
    /// Scraped as Prometheus at `url`.
    Scraped { spec: EndpointSpec, url: Url },
    /// Cannot be recorded; `reason` is the line that says why.
    Refused { spec: EndpointSpec, reason: String },
    /// Not known yet; detected again on each tick of a recording. `reason`
    /// is the line that says why.
    Pending { spec: EndpointSpec, reason: String },
}

impl Detected {
    pub fn spec(&self) -> &EndpointSpec {
        match self {
            Detected::Streamed { spec, .. }
            | Detected::Scraped { spec, .. }
            | Detected::Refused { spec, .. }
            | Detected::Pending { spec, .. } => spec,
        }
    }
}

/// Detect every endpoint at once, at startup ([`At::Startup`]), in the order
/// given.
pub async fn detect(
    client: &Client,
    cfg: &DetectConfig,
    endpoints: Vec<EndpointSpec>,
    probe: &dyn Probe,
) -> Vec<Detected> {
    let answers = futures_util::future::join_all(
        endpoints
            .iter()
            .map(|spec| detect_one(client, cfg, &spec.url, spec.transport, probe, At::Startup)),
    )
    .await;
    endpoints
        .into_iter()
        .zip(answers)
        .map(|(spec, outcome)| match outcome {
            Outcome::Streamed(subscription) => Detected::Streamed { spec, subscription },
            Outcome::Scraped(url) => Detected::Scraped { spec, url },
            Outcome::Refused(reason) => Detected::Refused { spec, reason },
            Outcome::Pending(reason) => Detected::Pending { spec, reason },
        })
        .collect()
}

/// [`Detected`] without its spec.
pub(crate) enum Outcome {
    Streamed(Box<Subscription>),
    Scraped(Url),
    Refused(String),
    Pending(String),
}

/// Whether `url` is asked for its stream first under [`Transport::Auto`].
fn streams_first(url: &Url) -> bool {
    matches!(url.path(), "/" | "/metrics/stream")
}

fn pending_stream(url: &Url, message: &str) -> Outcome {
    Outcome::Pending(format!(
        "endpoint {url}: its replication stream did not open ({message}); will retry each tick"
    ))
}

/// Detect one endpoint. See the module docs.
pub(crate) async fn detect_one(
    client: &Client,
    cfg: &DetectConfig,
    url: &Url,
    transport: Transport,
    probe: &dyn Probe,
    at: At,
) -> Outcome {
    match transport {
        Transport::Prometheus => {
            let mut scrape = url.clone();
            if scrape.path() == "/" {
                scrape.set_path("/metrics");
            }
            check_scrape(client, cfg, scrape).await
        }
        Transport::Auto if !streams_first(url) => check_scrape(client, cfg, url.clone()).await,
        Transport::Auto | Transport::Stream => {
            let (answer, message) = match tokio::time::timeout(
                cfg.timeout,
                Subscription::connect(client, url, cfg.interval, cfg.layout, &cfg.wire_names),
            )
            .await
            {
                Ok(Ok(sub)) => return Outcome::Streamed(Box::new(sub)),
                Ok(Err(refusal)) => (refusal.answer, refusal.message),
                Err(_) => (
                    StreamAnswer::Timeout,
                    format!(
                        "{url} did not complete a handshake within {}",
                        humantime::format_duration(cfg.timeout)
                    ),
                ),
            };
            let verdict = tokio::time::timeout(cfg.timeout, probe.not_streamable(url, at, &answer))
                .await
                .unwrap_or(Verdict::Pending);
            match verdict {
                Verdict::Refuse { reason } => Outcome::Refused(reason),
                Verdict::Pending => pending_stream(url, &message),
                Verdict::Scrape if transport == Transport::Auto => {
                    let mut scrape = url.clone();
                    scrape.set_path("/metrics");
                    scrape.set_query(None);
                    check_scrape(client, cfg, scrape).await
                }
                // Declared a stream: never scraped instead. An answer that
                // may change is waited out; any other refuses it.
                Verdict::Scrape if answer.is_transient() => pending_stream(url, &message),
                Verdict::Scrape => Outcome::Refused(format!(
                    "{message}. {url} is declared protocol=stream, so it is not scraped \
                     instead; declare protocol=prometheus to scrape it"
                )),
            }
        }
    }
}

/// A scraped endpoint is active once its URL answers with a success; until
/// then it is pending.
async fn check_scrape(client: &Client, cfg: &DetectConfig, url: Url) -> Outcome {
    match tokio::time::timeout(cfg.timeout, client.get(url.clone()).send()).await {
        Ok(Ok(resp)) if resp.status().is_success() => Outcome::Scraped(url),
        Ok(Ok(resp)) => Outcome::Pending(format!(
            "endpoint {url} answered HTTP {}, will retry each tick",
            resp.status().as_u16()
        )),
        Ok(Err(_)) | Err(_) => Outcome::Pending(format!(
            "endpoint {url} not reachable, will retry each tick"
        )),
    }
}

/// The name a source is recorded under when nobody named it.
pub(crate) fn inferred_name(url: &Url) -> String {
    crate::identity::infer_source_name(url.host_str(), url.port(), url.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_root_and_the_stream_route_are_asked_for_a_stream() {
        for (url, first) in [
            ("http://h:1", true),
            ("http://h:1/", true),
            ("http://h:1/metrics/stream", true),
            ("http://h:1/metrics", false),
            ("http://h:1/federate", false),
        ] {
            let url: Url = url.parse().unwrap();
            assert_eq!(streams_first(&url), first, "{url}");
        }
    }
}
