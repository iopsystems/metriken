//! A subscription to a producer's `/metrics/stream` over HTTP (feature
//! `http`).

use std::time::Duration;

use dendro::replicate::Frame;
use metriken_model::wire::{Layout, WireNames};
use reqwest::Client;
use url::Url;

use crate::frames::{frame_kind, Applied, FrameDecoder, Handshake, StreamSubscriber};
use crate::StreamAnswer;

/// A subscription that did not open: what the route answered, and a line
/// saying so with the URL and the detail.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Refusal {
    pub answer: StreamAnswer,
    pub message: String,
}

impl Refusal {
    /// A refusal: what the route answered, and the line that says so. A
    /// subscription builds these; a caller builds one to test code that
    /// consumes [`StreamEvent::Refused`].
    pub fn new(answer: StreamAnswer, message: impl Into<String>) -> Self {
        Self {
            answer,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Why reading the connection failed: the transport, which retrying can
/// change, or bytes that do not decode, which it cannot.
enum FillError {
    Transport(String),
    Decode(String),
}

impl FillError {
    fn into_message(self) -> String {
        match self {
            FillError::Transport(e) | FillError::Decode(e) => e,
        }
    }
}

/// One live subscription to a producer: the connection, the decoder and the
/// subscriber, handing back one interval at a time.
pub struct Subscription {
    response: reqwest::Response,
    decoder: FrameDecoder,
    subscriber: StreamSubscriber,
    /// Frames decoded but not yet part of a complete interval.
    pending: Vec<Frame>,
    update_floor: Option<Duration>,
    layout: Layout,
}

impl Subscription {
    /// Open a subscription to `base`'s `/metrics/stream` asking for
    /// `interval` and `layout`, and wait for its handshake.
    ///
    /// The interval is a request: the producer's update floor (its
    /// update-floor header) bounds how often a frame can carry anything new,
    /// and a shorter interval gets frames, most of them empty.
    ///
    /// `names` lists the wire names accepted. The response's `Content-Type`
    /// picks the entry, and that entry's header names are the ones read; a
    /// `Content-Type` no entry has is refused.
    ///
    /// Returns once the handshake has been applied, so
    /// [`handshake`](Self::handshake) is `Some` on every open subscription.
    /// Not bounded in time: the caller wraps it in its own timeout, which is
    /// [`StreamAnswer::Timeout`].
    pub async fn connect(
        client: &Client,
        base: &Url,
        interval: Duration,
        layout: Layout,
        names: &[WireNames],
    ) -> Result<Self, Refusal> {
        let mut url = base.clone();
        url.set_path("/metrics/stream");
        let interval = humantime::format_duration(interval);
        url.set_query(Some(&match layout {
            Layout::Wide => format!("interval={interval}"),
            Layout::Long => format!("interval={interval}&layout={}", layout.as_str()),
        }));

        let response = client.get(url.clone()).send().await.map_err(|e| Refusal {
            answer: StreamAnswer::Unreachable,
            message: format!("failed to subscribe to {url}: {e}"),
        })?;

        if !response.status().is_success() {
            let code = response.status().as_u16();
            let message = match code {
                409 => format!(
                    "{url} cannot serve a replication stream: the producer reports no \
                     acquisition groups, which a producer of V2 snapshots never has"
                ),
                // Not "predates the stream": a current producer behind a
                // proxy that does not route this path answers the same way.
                _ => format!("{url} returned HTTP {code}"),
            };
            return Err(Refusal {
                answer: StreamAnswer::Status(code),
                message,
            });
        }

        // Checked before any bytes, so a body of another kind is named as
        // that rather than as a stream of malformed frames.
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let Some(served_names) = names.iter().find(|n| n.content_type == content_type) else {
            let accepted: Vec<String> = names
                .iter()
                .map(|n| format!("`{}`", n.content_type))
                .collect();
            return Err(Refusal {
                message: format!(
                    "{url} serves `{content_type}`, not a replication stream this recorder \
                     reads ({})",
                    accepted.join(" or ")
                ),
                answer: StreamAnswer::ContentType(content_type),
            });
        };

        let update_floor = response
            .headers()
            .get(served_names.update_floor)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| humantime::parse_duration(v).ok());
        let served = match response
            .headers()
            .get(served_names.layout)
            .and_then(|v| v.to_str().ok())
        {
            Some("long") => Layout::Long,
            _ => Layout::Wide,
        };

        let mut sub = Self {
            response,
            decoder: FrameDecoder::new(),
            subscriber: StreamSubscriber::new(),
            pending: Vec::new(),
            update_floor,
            layout: served,
        };

        // The handshake is the first frame by protocol. Anything else first,
        // and bytes that do not decode, are refused: retrying gets the same
        // bytes. A connection that ends before or during the handshake is a
        // transport failure like any other.
        while sub.pending.is_empty() {
            let filled = sub.fill().await.map_err(|e| match e {
                FillError::Transport(e) => Refusal {
                    answer: StreamAnswer::Unreachable,
                    message: e,
                },
                FillError::Decode(e) => Refusal {
                    message: format!("{url} sent a malformed handshake: {e}"),
                    answer: StreamAnswer::Malformed(e),
                },
            })?;
            if !filled {
                return Err(Refusal {
                    answer: StreamAnswer::Unreachable,
                    message: format!("{url} closed the stream before sending a handshake"),
                });
            }
        }
        match sub.pending.first() {
            Some(Frame::Handshake { .. }) => {
                let handshake = sub.pending.remove(0);
                sub.subscriber.apply(vec![handshake]).map_err(|e| Refusal {
                    message: e.clone(),
                    answer: StreamAnswer::Malformed(e),
                })?;
            }
            Some(other) => {
                let kind = frame_kind(other);
                return Err(Refusal {
                    message: format!(
                        "{url} opened its stream with a {kind} frame instead of a handshake"
                    ),
                    answer: StreamAnswer::Malformed(format!("a {kind} frame came first")),
                });
            }
            None => unreachable!("the loop above exits with a frame pending"),
        }

        Ok(sub)
    }

    /// The next complete interval, or `None` when the producer closed the
    /// stream. An interval is every frame up to and including a `Rows`,
    /// which the producer emits last.
    pub async fn next_interval(&mut self) -> Result<Option<Applied>, String> {
        loop {
            if let Some(at) = self
                .pending
                .iter()
                .position(|f| matches!(f, Frame::Rows { .. }))
            {
                let batch: Vec<Frame> = self.pending.drain(..=at).collect();
                return self.subscriber.apply(batch).map(Some);
            }
            if !self.fill().await.map_err(FillError::into_message)? {
                return Ok(None);
            }
        }
    }

    /// Read one chunk and decode what it completes. `Ok(false)` means the
    /// producer closed the stream cleanly.
    async fn fill(&mut self) -> Result<bool, FillError> {
        let chunk = self
            .response
            .chunk()
            .await
            .map_err(|e| FillError::Transport(format!("replication stream failed: {e}")))?;
        let Some(chunk) = chunk else {
            if self.decoder.pending() > 0 {
                return Err(FillError::Transport(format!(
                    "the producer closed the stream mid-frame, with {} byte(s) unread",
                    self.decoder.pending()
                )));
            }
            return Ok(false);
        };
        self.pending
            .extend(self.decoder.push(&chunk).map_err(FillError::Decode)?);
        Ok(true)
    }

    /// How often this producer can have anything new to send, as its
    /// update-floor header said; `None` when it did not say.
    pub fn update_floor(&self) -> Option<Duration> {
        self.update_floor
    }

    /// The handshake, applied before [`connect`](Self::connect) returned.
    pub fn handshake(&self) -> &Handshake {
        self.subscriber
            .handshake()
            .expect("connect returns only once the handshake has been applied")
    }

    /// The layout the producer serves this subscription: its layout header,
    /// or wide when it sent none.
    pub fn layout(&self) -> Layout {
        self.layout
    }
}

/// What a [`pump`] reports.
///
/// Not `#[non_exhaustive]`, for the reason [`Detected`] is not: a caller
/// driving a pump acts on each event (stage the interval, mark the source
/// down, adopt the handshake, stop), and a new one should fail its match
/// rather than fall through a wildcard.
///
/// [`Detected`]: crate::Detected
#[derive(Debug)]
pub enum StreamEvent {
    /// One interval, applied.
    Interval(Applied),
    /// The connection ended (the producer closed it, it failed, or it went
    /// silent past the timeout) and the pump is reconnecting. Rows between
    /// here and the next `Connected` are lost.
    Dropped(String),
    /// A fresh connection is up, with its handshake. A different uuid is a
    /// restarted producer.
    Connected(Handshake),
    /// The producer answered a reconnect and cannot serve the stream.
    /// Retrying will not change that, so the pump has stopped.
    Refused(Refusal),
}

/// How a [`pump`] subscribes and reconnects. Built with
/// [`PumpConfig::new`]; every field can be set after.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PumpConfig {
    pub interval: Duration,
    pub layout: Layout,
    /// Bounds every reconnect; a connection silent for `interval + timeout`
    /// is dead.
    pub timeout: Duration,
    pub wire_names: Vec<WireNames>,
}

impl PumpConfig {
    /// The long layout under metriken's wire names.
    pub fn new(interval: Duration, timeout: Duration) -> Self {
        Self {
            interval,
            layout: Layout::Long,
            timeout,
            wire_names: vec![metriken_model::wire::WIRE_NAMES],
        }
    }
}

/// Drive one subscription until the receiver goes away or the producer
/// refuses a reconnect, sending each interval as it completes on `tx`,
/// tagged with `idx`. The channel is bounded, so a receiver that stops
/// draining pushes back on the socket rather than on memory.
///
/// A dropped connection is reconnected after max(interval, 1 s). A
/// reconnect whose answer is transient ([`StreamAnswer::is_transient`]) is
/// retried; any other answer ends the pump with [`StreamEvent::Refused`].
///
/// A producer sends a frame every interval, empty when nothing is new, so a
/// connection that produces nothing for `interval + timeout` is dead even
/// when the kernel has not noticed (a peer gone without a RST). The interval
/// is part of the bound because a frame is only due once per interval: with
/// the timeout alone, an interval longer than the timeout would drop every
/// connection between two frames.
pub async fn pump(
    idx: usize,
    mut sub: Subscription,
    client: Client,
    base: Url,
    cfg: PumpConfig,
    tx: tokio::sync::mpsc::Sender<(usize, StreamEvent)>,
) {
    let retry = cfg.interval.max(Duration::from_secs(1));
    let silence = cfg.interval + cfg.timeout;
    loop {
        let outcome = match tokio::time::timeout(silence, sub.next_interval()).await {
            Ok(Ok(Some(applied))) => {
                if tx
                    .send((idx, StreamEvent::Interval(applied)))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            Ok(Ok(None)) => "the producer closed the stream".to_string(),
            Ok(Err(e)) => e,
            Err(_) => format!(
                "no frame arrived within {}; the connection is dead",
                humantime::format_duration(silence)
            ),
        };
        if tx.send((idx, StreamEvent::Dropped(outcome))).await.is_err() {
            return;
        }
        sub = loop {
            tokio::time::sleep(retry).await;
            if tx.is_closed() {
                return;
            }
            let connected = tokio::time::timeout(
                cfg.timeout,
                Subscription::connect(&client, &base, cfg.interval, cfg.layout, &cfg.wire_names),
            )
            .await;
            match connected {
                Ok(Ok(sub)) => break sub,
                Err(_) => continue,
                Ok(Err(refusal)) if refusal.answer.is_transient() => continue,
                Ok(Err(refusal)) => {
                    let _ = tx.send((idx, StreamEvent::Refused(refusal))).await;
                    return;
                }
            }
        };
        let handshake = sub.handshake().clone();
        if tx
            .send((idx, StreamEvent::Connected(handshake)))
            .await
            .is_err()
        {
            return;
        }
    }
}
