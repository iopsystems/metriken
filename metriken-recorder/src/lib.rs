//! Records metriken producers into one `.dendro` archive.
//!
//! A [`Recorder`] owns the archive and its sources and records them on an
//! aligned tick, one commit per tick (or per streamed frame with
//! `commit_streams_on_arrival`), through metriken-storage's
//! `ArchiveWriter`. A source is one of:
//!
//! - **Streamed**: a producer's `/metrics/stream`, read by a [`pump`] per
//!   source and staged as it arrives, on the producer's own timeline.
//! - **Scraped**: Prometheus text at a URL, one acquisition group
//!   `prometheus/scrape` per scrape, windowed by the round trip.
//! - **Local**: a [`LocalSource`] the caller takes passes of in process
//!   ([`RegistrySource`] with feature `registry`).
//!
//! A source's labels and metadata are its identity layers
//! (metriken-storage's `layers`): the recorder's entries (`instance`,
//! `transport`, `sampling_interval_ms`, the probe hook's reply, and one
//! entry per producer epoch), the producer's (its handshake, or a local
//! source's [`SourceIdentity`], with a producer identity written as
//! `producer_name`, `version`, `build` and the `host` label), and the
//! caller's (an endpoint's labels and metadata, a local source's labels,
//! [`Recorder::patch_metadata`]), merged in that order, the last winning.
//! The recorder writes no `source` of its own. It refuses a caller's
//! `metriken.` keys, the flat epoch keys, and dendro's `encoder` and
//! `writer_sessions`; a caller's `events` is stored as given, outside the
//! layers.
//!
//! [`detect`] decides how each endpoint is recorded before the archive
//! opens: by its stream first, then by the caller's [`Probe`] hook, then as
//! Prometheus. The library never exits the process and installs no signal
//! handler; it reports [`Event`]s and returns a [`Summary`], and the caller
//! decides what each means.
//!
//! # Features
//!
//! - `http` (default): [`detect`], [`Subscription`], [`pump`], scraping and
//!   [`Recorder::start`], over a `reqwest::Client` the caller builds, so TLS,
//!   headers and the crypto provider are the caller's. Off, the frame
//!   decoder ([`FrameDecoder`], [`StreamSubscriber`]) and the Prometheus
//!   converter take bytes, and [`Recorder::start_local`] records local
//!   sources.
//! - `registry`: [`RegistrySource`], the process's own metriken registry as
//!   a local source. It is the only feature that links the registry.
//!
//! # Runtime
//!
//! A `Recorder` runs on the caller's tokio runtime, current-thread or
//! multi-thread. The runtime needs its time driver (`enable_time`, or
//! `enable_all`), and with `http` its IO driver; without them the tick and
//! the HTTP client panic. Commits, seals, metadata updates and local
//! passes run on the runtime's blocking pool, since each is a blocking send
//! to dendro's writer thread or a call into the caller's code.
//!
//! The design is metriken's `docs/journal/2026-10-12-metriken-recorder.md`.
//!
//! [`pump`]: crate::pump
//! [`detect`]: crate::detect
//! [`Probe`]: crate::Probe
//! [`Subscription`]: crate::Subscription
//! [`RegistrySource`]: crate::RegistrySource

// The crate docs link to items behind both features; docs.rs builds with
// every feature.
#![cfg_attr(
    not(all(feature = "http", feature = "registry")),
    allow(rustdoc::broken_intra_doc_links)
)]

mod answer;
#[cfg(feature = "http")]
mod detect;
mod exposition;
mod frames;
mod identity;
mod layered;
#[cfg(feature = "registry")]
mod local;
#[cfg(feature = "http")]
mod probe;
mod prometheus;
mod recorder;
#[cfg(feature = "http")]
mod restart;
#[cfg(feature = "http")]
mod subscription;

pub use answer::StreamAnswer;
#[cfg(feature = "http")]
pub use detect::{detect, DetectConfig, Detected, EndpointSpec, Transport};
// dendro's frame and row types appear in `FrameDecoder::push`,
// `StreamSubscriber::apply` and `Applied::rows`; re-exported so a caller
// names the same types without depending on dendro.
pub use dendro::archive::WalRow;
pub use dendro::replicate::Frame;
pub use frames::{Applied, FrameDecoder, Handshake, StreamPass, StreamSubscriber};
pub use identity::infer_source_name;
#[cfg(feature = "registry")]
pub use local::RegistrySource;
pub use metriken_model::identity::ProducerIdentity;
pub use metriken_model::wire::{Layout, WireNames, WIRE_NAMES};
pub use metriken_storage::WriterConfig;
#[cfg(feature = "http")]
pub use probe::{accept, enrich_within, At, Enrich, Enriched, NoProbe, Probe, Verdict};
pub use prometheus::{PrometheusConverter, PROMETHEUS_GROUP};
pub use recorder::{
    default_timeout, ActiveTransport, Error, Event, LocalSource, Recorder, RecorderConfig,
    SourceId, SourceIdentity, SourceState, SourceSummary, Stamp, StartError, Stop, Summary,
};
#[cfg(feature = "http")]
pub use subscription::{pump, PumpConfig, Refusal, StreamEvent, Subscription};
pub use tokio_util::sync::CancellationToken;

/// The label and metadata keys the recorder reads or writes.
pub mod keys {
    /// The label a caller, a producer or the probe hook names a recording
    /// with. The recorder writes none of its own.
    pub const SOURCE: &str = "source";
    /// The recorder's tick, in milliseconds: a recorder-layer key.
    pub const SAMPLING_INTERVAL_MS: &str = "sampling_interval_ms";
    /// The flat key for a source's current epoch (dendro's
    /// `keys::PRODUCER_EPOCH`), which recordings made before the identity
    /// layers carry. The recorder writes the recorder layer's epoch entries
    /// instead, and refuses this key from a caller.
    pub const PRODUCER_EPOCH: &str = "producer_epoch";
    /// The flat key for every epoch a source observed (dendro's
    /// `keys::PRODUCER_EPOCHS`), which recordings made before the identity
    /// layers carry. Refused from a caller, as `PRODUCER_EPOCH` is.
    pub const PRODUCER_EPOCHS: &str = "producer_epochs";
    /// The producer's version, as its handshake (the producer layer) or the
    /// probe hook (the recorder layer) reports it.
    pub const VERSION: &str = "version";
    /// A JSON object of metric name to help text; a scraped source's `#
    /// HELP` lines.
    pub const DESCRIPTIONS: &str = "descriptions";
}
