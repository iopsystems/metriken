//! [`Recorder`]: the sources, the tick, and the archive.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use metriken_model::wire::{Layout, WireNames, WIRE_NAMES};
use metriken_model::Snapshot;
use metriken_storage::writer::Staged;
use metriken_storage::{ArchiveWriter, SourceRecorder, WriterConfig};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::keys;

#[cfg(feature = "http")]
use crate::identity::Identity;
#[cfg(feature = "http")]
use crate::restart::{self, Restart, FETCH_ATTEMPTS};
#[cfg(feature = "http")]
use metriken_storage::StreamDecoder;
#[cfg(feature = "http")]
use tracing::info;

#[cfg(feature = "http")]
use crate::detect::{self, Detected, EndpointSpec, Outcome};
#[cfg(feature = "http")]
use crate::frames::Handshake;
#[cfg(feature = "http")]
use crate::probe::{self, At, Probe, Verdict};
#[cfg(feature = "http")]
use crate::prometheus::PrometheusConverter;
#[cfg(feature = "http")]
use crate::subscription::{self, PumpConfig, StreamEvent, Subscription};
#[cfg(feature = "http")]
use reqwest::Client;
#[cfg(feature = "http")]
use url::Url;

/// When a pass read its values: a timestamp on its source's timeline and the
/// wall clock's disagreement with it, both in nanoseconds.
///
/// Not `#[non_exhaustive]`: a [`LocalSource`] builds one for every pass, and
/// the two fields are the whole of a reading's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// `anchor + monotonic elapsed` on the source's timeline.
    pub ts: u64,
    /// Wall clock minus `ts` at the reading.
    pub wall_offset: i64,
}

impl Stamp {
    /// The wall clock this stamp names, `ts + wall_offset`, clamped to the
    /// Unix epoch.
    pub fn wall_ns(&self) -> u64 {
        (self.ts as i64).saturating_add(self.wall_offset).max(0) as u64
    }
}

/// What a local source is recorded as.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct SourceIdentity {
    /// The recording's labels; `source` names it.
    pub labels: BTreeMap<String, String>,
    /// The recording's metadata. These keys are the caller's.
    pub metadata: BTreeMap<String, String>,
    /// The wall clock the source's timeline is anchored to: its stamps'
    /// `ts` are `clock_anchor_wall_ns + monotonic elapsed`.
    pub clock_anchor_wall_ns: u64,
    /// The process's producer epoch, written as `producer_epoch`.
    pub epoch: Option<String>,
}

impl SourceIdentity {
    /// A source recorded with `labels` (a `source` names it) and
    /// `metadata`, on a timeline anchored at `clock_anchor_wall_ns`, with no
    /// producer epoch.
    pub fn new(
        labels: BTreeMap<String, String>,
        metadata: BTreeMap<String, String>,
        clock_anchor_wall_ns: u64,
    ) -> Self {
        Self {
            labels,
            metadata,
            clock_anchor_wall_ns,
            epoch: None,
        }
    }

    /// With the process's producer epoch.
    pub fn with_epoch(mut self, epoch: impl Into<String>) -> Self {
        self.epoch = Some(epoch.into());
        self
    }
}

/// A source the caller takes passes of itself, in process: a producer
/// recording its own registry. Its snapshots are staged as they are, so
/// histogram groups stay native.
///
/// `pass` is called on each tick on a blocking thread
/// (`tokio::task::spawn_blocking`), every local source's in one call, so a
/// snapshot that takes a while does not hold a runtime worker that other
/// tasks need. The tick still waits for it: a pass slower than the
/// interval delays that tick's commit and the passes after it.
pub trait LocalSource: Send + 'static {
    /// The source's labels, metadata and clock anchor, read once when the
    /// recorder starts.
    fn identity(&self) -> SourceIdentity;
    /// One pass, on each tick, or `None` to skip this tick. A pass whose
    /// `ts` is not after the last one staged is skipped.
    fn pass(&mut self) -> Option<(Snapshot, Stamp)>;
}

/// A source of a recording, numbered in the order [`Recorder::start`] was
/// given them: detected endpoints first, then local sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u32);

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "source {}", self.0)
    }
}

/// How an active source is recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActiveTransport {
    Stream,
    Prometheus,
    Local,
}

/// Where a source stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceState {
    /// Not detected yet; detected again on each tick.
    Pending,
    /// Streamed, and waiting for the probe hook's `enrich`, which runs off
    /// the tick; added to the archive on the first tick after it returns.
    Activating,
    /// In the archive and recording.
    Active,
    /// Answered and cannot be recorded. Never retried. A source refused on
    /// a reconnect keeps the rows it had.
    Refused,
}

/// How [`Recorder::finish`] ends the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stop {
    /// A wrapped command exited at `wall_ns`. One more pass of every local
    /// and scraped source, unless the exit landed during the pass just
    /// taken, then a wait of up to `bound` for each streamed source's frame
    /// stamped at or after `wall_ns`.
    AfterExit { wall_ns: u64, bound: Duration },
    /// Wait `duration` for frames in flight, with no extra pass.
    Grace(Duration),
    /// One more pass of each local source, and no wait.
    Now,
}

/// What a [`Recorder`] reports while it records.
///
/// The recorder never waits for the caller to read. `Frame`, `Gap` and
/// `Committed` are dropped when the channel is full and counted, and a
/// `Lagged` is delivered before the next event after a drop. Every other
/// event is queued inside the recorder and sent as the channel has room; a
/// caller that reads late gets them late, in order. The queue is bounded
/// only for `Dropped` and `Reconnected`, which a flapping producer can
/// emit without limit: past 1,024 queued events those two are counted in
/// `Lagged` instead. The rest number at most a few per source. Events still
/// queued when the recorder stops are sent from a task of their own until
/// the receiver is dropped. A caller that ignores events drops the
/// receiver.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// The source is in the archive. `name` is its `source`, and `inferred`
    /// says that nobody named it and `name` was made from its URL.
    Active {
        source: SourceId,
        transport: ActiveTransport,
        name: String,
        inferred: bool,
    },
    /// The source could not be detected at startup and is detected again on
    /// each tick.
    Pending { source: SourceId, reason: String },
    /// A streamed source's connection ended; it is being reconnected.
    Dropped { source: SourceId, reason: String },
    /// A streamed source reconnected. `restarted` says its handshake named a
    /// new producer epoch: every cumulative counter reset.
    Reconnected {
        source: SourceId,
        epoch: Option<String>,
        restarted: bool,
    },
    /// The producer's `seq` jumped to `seq`: intervals produced no frame.
    Gap { source: SourceId, seq: u64 },
    /// The source cannot be recorded; `reason` is the line that says why.
    Refused { source: SourceId, reason: String },
    /// A producer restart is now written into the recording's metadata
    /// (`producer_epochs`), with the new process's `version` when it
    /// reported one.
    Restarted {
        source: SourceId,
        epoch: String,
        version: Option<String>,
    },
    /// The archive failed. The recorder has stopped ticking; what is on disk
    /// holds every tick committed before the failure. [`Recorder::finish`]
    /// returns this error.
    Failed { error: String },
    /// A streamed frame was staged. `seq` counts this source's frames,
    /// across reconnects; `stamp` is its newest pass, `None` for an empty
    /// frame.
    Frame {
        source: SourceId,
        seq: u64,
        stamp: Option<Stamp>,
    },
    /// A commit included passes of this source. Cumulative, so a dropped
    /// one loses nothing: `passes_total` passes since the start, the first
    /// at `first_ts`, the newest at `ts` (wall clock `wall_ns`).
    Committed {
        source: SourceId,
        passes_total: u64,
        first_ts: u64,
        ts: u64,
        wall_ns: u64,
    },
    /// `dropped` events were dropped because the channel was full (or, for
    /// `Dropped` and `Reconnected`, the queue was).
    Lagged { dropped: u64 },
}

/// One source at the end of a recording.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SourceSummary {
    pub id: SourceId,
    /// The endpoint's URL; `None` for a local source.
    pub endpoint: Option<String>,
    /// The recording's `source`, once it was added; before, the caller's
    /// `source` label if it gave one.
    pub name: Option<String>,
    pub state: SourceState,
    pub transport: Option<ActiveTransport>,
    pub passes_total: u64,
    /// The wall clock of this host when the first frame, scrape or pass
    /// arrived, and the last.
    pub first_wall_ns: Option<u64>,
    pub last_wall_ns: Option<u64>,
}

/// How a recording ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Summary {
    pub sources: Vec<SourceSummary>,
    /// No source delivered a frame, a scrape or a pass, so the archive was
    /// removed rather than finalized.
    pub discarded: bool,
}

/// What a recording is written with. Built with [`RecorderConfig::new`];
/// every field can be set after.
#[non_exhaustive]
pub struct RecorderConfig {
    /// The `.dendro` archive to create; it must not exist.
    pub path: PathBuf,
    pub writer: WriterConfig,
    /// The tick, on multiples of which since the Unix epoch the recorder
    /// scrapes, takes local passes and commits.
    pub interval: Duration,
    /// Bounds each scrape, connect and probe call; a stream silent for
    /// `interval + timeout` is reconnected.
    pub timeout: Duration,
    /// The layout streams are asked for.
    pub layout: Layout,
    /// The wire names a stream is accepted under.
    pub wire_names: Vec<WireNames>,
    /// Keep only this much of each source, measured back from its newest
    /// row; `None` keeps everything.
    pub retain: Option<Duration>,
    /// The event channel's capacity.
    pub events: usize,
    /// Stage and commit each streamed frame as it arrives, with a seal check
    /// and retention after it, rather than on the next tick. A buffer that
    /// is copied while it records (hindsight's dumps) then holds every
    /// frame received; at the cost of one commit per frame per source
    /// rather than one per tick.
    pub commit_streams_on_arrival: bool,
}

impl RecorderConfig {
    /// A recording at `path` ticking every `interval`, with the writer's
    /// defaults, the long layout, metriken's wire names, no retention, and a
    /// timeout of [`default_timeout`].
    pub fn new(path: impl Into<PathBuf>, interval: Duration) -> Self {
        Self {
            path: path.into(),
            writer: WriterConfig::default(),
            interval,
            timeout: default_timeout(interval),
            layout: Layout::Long,
            wire_names: vec![WIRE_NAMES],
            retain: None,
            events: 1024,
            commit_streams_on_arrival: false,
        }
    }
}

/// Twice the interval, between 2 s and 10 s: long enough that a slow
/// endpoint is sampled at its own pace rather than timed out every tick,
/// short enough that a hung one does not hold a stop past a container's
/// grace period.
pub fn default_timeout(interval: Duration) -> Duration {
    (interval * 2).clamp(Duration::from_secs(2), Duration::from_secs(10))
}

/// Why a recording failed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The archive failed mid-run ([`Event::Failed`]), or at finalize.
    Archive(String),
    /// No such source, or one that was refused before it was recorded.
    Source(String),
    /// A metadata key the recorder writes itself.
    Reserved(String),
    /// The recorder is gone.
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Archive(e) | Error::Source(e) | Error::Reserved(e) => f.write_str(e),
            Error::Closed => f.write_str("the recorder has stopped"),
        }
    }
}

impl std::error::Error for Error {}

/// Why a recording did not start. Nothing is left on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StartError {
    /// The archive could not be created.
    Create(String),
    /// A source could not be added to the new archive.
    AddSource(String),
    /// The caller set a key the recorder writes itself.
    Reserved(String),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::Create(e) | StartError::AddSource(e) | StartError::Reserved(e) => {
                f.write_str(e)
            }
        }
    }
}

impl std::error::Error for StartError {}

/// Records sources into one `.dendro` archive, on its own task.
///
/// The recorder never exits the process and installs no signal handler: it
/// reports [`Event`]s, and [`finish`](Self::finish) returns a [`Summary`]
/// that says how each source ended. Dropping a `Recorder` stops its tasks
/// and leaves the archive on disk unfinalized.
pub struct Recorder {
    commands: mpsc::Sender<Command>,
    task: Option<JoinHandle<()>>,
}

enum Command {
    Patch {
        source: SourceId,
        patch: BTreeMap<String, String>,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    Finish {
        stop: Stop,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<Summary, Error>>,
    },
    Discard {
        reply: oneshot::Sender<()>,
    },
}

impl Recorder {
    /// Start recording `detected` endpoints and `local` sources into the
    /// archive at `cfg.path`.
    ///
    /// Every streamed endpoint's first [`Probe::enrich`] runs at once, each
    /// bounded by `cfg.timeout`, before the archive opens, since a
    /// recording's labels are fixed when it is added. One that does not
    /// answer in time is added with the caller's and the handshake's labels,
    /// with a warning. A pending endpoint is detected on each tick; a
    /// refused one is reported in the [`Summary`] and never recorded.
    ///
    /// Returns the recorder, the id of each detected endpoint and then each
    /// local source, in the order given, and the event channel.
    #[cfg(feature = "http")]
    pub async fn start(
        cfg: RecorderConfig,
        client: Client,
        probe: std::sync::Arc<dyn Probe>,
        detected: Vec<Detected>,
        local: Vec<Box<dyn LocalSource>>,
    ) -> Result<(Self, Vec<SourceId>, mpsc::Receiver<Event>), StartError> {
        Self::launch(cfg, Some((client, probe)), detected, local).await
    }

    /// Start recording `local` sources only, with no HTTP client.
    pub async fn start_local(
        cfg: RecorderConfig,
        local: Vec<Box<dyn LocalSource>>,
    ) -> Result<(Self, Vec<SourceId>, mpsc::Receiver<Event>), StartError> {
        #[cfg(feature = "http")]
        return Self::launch(cfg, None, Vec::new(), local).await;
        #[cfg(not(feature = "http"))]
        return Self::launch(cfg, local).await;
    }

    async fn launch(
        cfg: RecorderConfig,
        #[cfg(feature = "http")] remote: Option<(Client, std::sync::Arc<dyn Probe>)>,
        #[cfg(feature = "http")] detected: Vec<Detected>,
        local: Vec<Box<dyn LocalSource>>,
    ) -> Result<(Self, Vec<SourceId>, mpsc::Receiver<Event>), StartError> {
        let (event_tx, event_rx) = mpsc::channel(cfg.events.max(1));
        let (anchor_wall_ns, anchor_mono) = (wall_now_ns(), Instant::now());
        let mut slots: Vec<Slot> = Vec::new();

        #[cfg(feature = "http")]
        let endpoint_count = detected.len();
        #[cfg(not(feature = "http"))]
        let endpoint_count = 0usize;
        let (stream_tx, stream_rx) = mpsc::channel(16 * endpoint_count.max(1));

        #[cfg(feature = "http")]
        for d in detected {
            reserved(&d.spec().metadata)?;
            let id = SourceId(slots.len() as u32);
            let (spec, mode, state, opened) = match d {
                Detected::Streamed { spec, subscription } => (
                    spec,
                    Mode::Streamed,
                    SourceState::Active,
                    Some(subscription),
                ),
                Detected::Scraped { spec, url } => (
                    spec,
                    Mode::Scraped {
                        url,
                        converter: PrometheusConverter::new(),
                    },
                    SourceState::Active,
                    None,
                ),
                Detected::Pending { spec, reason } => (
                    spec,
                    Mode::Undetected(Some(reason)),
                    SourceState::Pending,
                    None,
                ),
                Detected::Refused { spec, reason: _ } => {
                    (spec, Mode::Undetected(None), SourceState::Refused, None)
                }
            };
            let mut endpoint = Endpoint::new(spec, mode);
            if let Some(sub) = &opened {
                endpoint.begin_stream(sub, cfg.interval);
            }
            endpoint.opened = opened.map(|s| *s);
            let mut slot = Slot::new(id, Kind::Endpoint(Box::new(endpoint)));
            slot.state = state;
            slots.push(slot);
        }
        let mut ids: Vec<SourceId> = slots.iter().map(|s| s.id).collect();
        for source in local {
            let identity = source.identity();
            reserved(&identity.metadata)?;
            let id = SourceId(slots.len() as u32);
            let mut slot = Slot::new(
                id,
                Kind::Local {
                    source: Some(source),
                    identity,
                },
            );
            slot.state = SourceState::Active;
            slots.push(slot);
            ids.push(id);
        }

        let mut driver = Driver {
            settings: Settings {
                interval: cfg.interval,
                timeout: cfg.timeout,
                layout: cfg.layout,
                wire_names: cfg.wire_names.clone(),
                retain: cfg.retain,
            },
            #[cfg(feature = "http")]
            remote,
            slots,
            archive: None,
            path: cfg.path.clone(),
            emitter: Emitter::new(event_tx),
            commit_streams_on_arrival: cfg.commit_streams_on_arrival,
            stream_tx,
            stream_rx,
            anchor_wall_ns,
            anchor_mono,
            last_pass: None,
            failed: None,
            seen_labels: BTreeMap::new(),
        };

        // The probe hook's first `enrich` of every streamed endpoint, at
        // once, each bounded by the timeout.
        #[cfg(feature = "http")]
        driver.first_enrich().await;

        let path = cfg.path.clone();
        let writer = tokio::task::spawn_blocking(move || ArchiveWriter::create(&path, cfg.writer))
            .await
            .map_err(|e| StartError::Create(format!("the writer did not start: {e}")))?
            .map_err(|e| {
                StartError::Create(format!("failed to create {}: {e}", cfg.path.display()))
            })?;
        driver.archive = Some(Archive {
            recs: BTreeMap::new(),
            staged: Vec::new(),
            writer,
        });

        let initial: Vec<usize> = (0..driver.slots.len())
            .filter(|&i| driver.slots[i].state == SourceState::Active)
            .collect();
        for i in initial {
            if let Err(e) = driver.add_source(i).await {
                driver.remove_archive().await;
                return Err(StartError::AddSource(e));
            }
        }
        #[cfg(feature = "http")]
        for i in 0..driver.slots.len() {
            driver.spawn_pump(i);
        }

        let (commands, command_rx) = mpsc::channel(8);
        let task = tokio::spawn(driver.run(command_rx));
        Ok((
            Self {
                commands,
                task: Some(task),
            },
            ids,
            event_rx,
        ))
    }

    /// Replace the keys in `patch` in one source's metadata, keeping every
    /// other key, and return once the writer has applied it. The keys become
    /// the caller's: a producer restart or a scrape's HELP text never
    /// replaces them. A source not added yet gets the patch when it is.
    pub async fn patch_metadata(
        &self,
        source: SourceId,
        patch: BTreeMap<String, String>,
    ) -> Result<(), Error> {
        let (reply, rx) = oneshot::channel();
        self.commands
            .send(Command::Patch {
                source,
                patch,
                reply,
            })
            .await
            .map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }

    /// Stop the recording as `stop` says, stage what arrived, write pending
    /// restarts, finalize each source at its last stamp and join the writer.
    /// The tick stops first, so no pass is taken while this waits.
    /// Cancelling `cancel` ends the `AfterExit` wait or the `Grace` sleep
    /// and goes on to finalize.
    ///
    /// When no source delivered a frame, a scrape or a pass, the archive is
    /// removed instead and [`Summary::discarded`] says so.
    pub async fn finish(mut self, stop: Stop, cancel: CancellationToken) -> Result<Summary, Error> {
        let (reply, rx) = oneshot::channel();
        self.commands
            .send(Command::Finish {
                stop,
                cancel,
                reply,
            })
            .await
            .map_err(|_| Error::Closed)?;
        let summary = rx.await.map_err(|_| Error::Closed)?;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        summary
    }

    /// Stop recording and remove the archive.
    pub async fn discard(mut self) {
        let (reply, rx) = oneshot::channel();
        if self.commands.send(Command::Discard { reply }).await.is_ok() {
            let _ = rx.await;
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// Refuse the keys the recorder writes from a producer's handshake and
/// rewrites on every restart: a caller's value would seed the restart
/// history with an epoch no process had.
fn reserved(metadata: &BTreeMap<String, String>) -> Result<(), StartError> {
    reserved_key(metadata).map_err(StartError::Reserved)
}

fn reserved_key(metadata: &BTreeMap<String, String>) -> Result<(), String> {
    for key in [keys::PRODUCER_EPOCH, keys::PRODUCER_EPOCHS] {
        if metadata.contains_key(key) {
            return Err(format!(
                "{key} is written by the recorder from the producer's handshake and rewritten \
                 when the producer restarts, so it cannot be set"
            ));
        }
    }
    Ok(())
}

fn wall_now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// The wait from `wall_ns` to the next multiple of `interval` since the Unix
/// epoch. A `wall_ns` on a boundary waits a whole interval.
fn align_wait(wall_ns: u64, interval: Duration) -> Duration {
    let step = interval.as_nanos().max(1);
    let rem = u128::from(wall_ns) % step;
    Duration::from_nanos((step - rem) as u64)
}

struct Settings {
    interval: Duration,
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    timeout: Duration,
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    layout: Layout,
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    wire_names: Vec<WireNames>,
    retain: Option<Duration>,
}

/// The archive and one recorder per added source. The recorders are
/// declared before the writer so they drop first: dropping the writer joins
/// its thread, and a recorder's final seals must reach it.
struct Archive {
    recs: BTreeMap<usize, SourceRecorder>,
    staged: Vec<Staged>,
    writer: ArchiveWriter,
}

/// Past this many queued events, `Dropped` and `Reconnected` are counted
/// in `Lagged` instead of queued.
const QUEUE_CAP: usize = 1024;

/// Sends events without waiting for the caller: `try_send` for every
/// event, a queue for the ones that are never dropped, and a `Lagged`
/// before the next delivery after a drop. See [`Event`].
struct Emitter {
    tx: mpsc::Sender<Event>,
    queue: VecDeque<Event>,
    dropped: u64,
}

impl Emitter {
    fn new(tx: mpsc::Sender<Event>) -> Self {
        Self {
            tx,
            queue: VecDeque::new(),
            dropped: 0,
        }
    }

    /// An event that is never dropped (but see [`QUEUE_CAP`]).
    fn must(&mut self, event: Event) {
        let flapping = matches!(event, Event::Dropped { .. } | Event::Reconnected { .. });
        if flapping && self.queue.len() >= QUEUE_CAP {
            self.dropped += 1;
        } else {
            self.queue.push_back(event);
        }
        self.flush();
    }

    /// An event dropped when it cannot be sent at once. One that would
    /// overtake a queued event is dropped too, so events stay in order.
    fn lossy(&mut self, event: Event) {
        use mpsc::error::TrySendError;
        self.flush();
        if self.tx.is_closed() {
            return;
        }
        if !self.queue.is_empty() || self.dropped > 0 {
            self.dropped += 1;
            return;
        }
        if let Err(TrySendError::Full(_)) = self.tx.try_send(event) {
            self.dropped += 1;
        }
    }

    /// Send what the channel has room for: the `Lagged` count, then the
    /// queue in order.
    fn flush(&mut self) {
        use mpsc::error::TrySendError;
        if self.tx.is_closed() {
            self.queue.clear();
            self.dropped = 0;
            return;
        }
        if self.dropped > 0 {
            match self.tx.try_send(Event::Lagged {
                dropped: self.dropped,
            }) {
                Ok(()) => self.dropped = 0,
                Err(TrySendError::Full(_)) => return,
                Err(TrySendError::Closed(_)) => return self.queue.clear(),
            }
        }
        while let Some(event) = self.queue.pop_front() {
            match self.tx.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    self.queue.push_front(event);
                    return;
                }
                Err(TrySendError::Closed(_)) => return self.queue.clear(),
            }
        }
    }

    /// The recorder has stopped: send what is still queued from a task of
    /// its own, which ends when the queue is empty or the receiver is
    /// dropped.
    fn hand_off(mut self) {
        self.flush();
        if self.queue.is_empty() && self.dropped == 0 {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            if self.dropped > 0
                && self
                    .tx
                    .send(Event::Lagged {
                        dropped: self.dropped,
                    })
                    .await
                    .is_err()
            {
                return;
            }
            for event in self.queue {
                if self.tx.send(event).await.is_err() {
                    return;
                }
            }
        });
    }
}

enum Kind {
    Local {
        /// `None` while its pass runs on a blocking thread.
        source: Option<Box<dyn LocalSource>>,
        identity: SourceIdentity,
    },
    #[cfg(feature = "http")]
    Endpoint(Box<Endpoint>),
}

/// How an endpoint is being recorded.
#[cfg(feature = "http")]
enum Mode {
    /// Not detected yet, with the last reason; or refused.
    Undetected(Option<String>),
    Scraped {
        url: Url,
        converter: PrometheusConverter,
    },
    Streamed,
}

#[cfg(feature = "http")]
struct Endpoint {
    spec: EndpointSpec,
    /// The name the source is recorded under when nobody names it.
    inferred: String,
    mode: Mode,
    /// A subscription opened by detection, read by the pump once the source
    /// is added.
    opened: Option<Subscription>,
    detecting: Option<JoinHandle<Outcome>>,
    enriching: Option<JoinHandle<Result<probe::Enriched, String>>>,
    pump: Option<JoinHandle<()>>,
    hook_labels: BTreeMap<String, String>,
    hook_metadata: BTreeMap<String, String>,
    /// The first connection's: a recording's labels are fixed when it is
    /// added.
    handshake_labels: BTreeMap<String, String>,
    /// The latest connection's: a restarted producer's replaces it.
    handshake_metadata: BTreeMap<String, String>,
    epoch: Option<String>,
    anchor: Option<u64>,
    restarts: Vec<Restart>,
    restart_fetches: u32,
    decoder: StreamDecoder,
    frames: u64,
    /// The newest frame: its newest row's wall clock (`None` for an empty
    /// frame) and this host's wall clock when it was staged.
    last_frame: Option<(Option<u64>, u64)>,
}

#[cfg(feature = "http")]
impl Endpoint {
    fn new(spec: EndpointSpec, mode: Mode) -> Self {
        Self {
            inferred: detect::inferred_name(&spec.url),
            spec,
            mode,
            opened: None,
            detecting: None,
            enriching: None,
            pump: None,
            hook_labels: BTreeMap::new(),
            hook_metadata: BTreeMap::new(),
            handshake_labels: BTreeMap::new(),
            handshake_metadata: BTreeMap::new(),
            epoch: None,
            anchor: None,
            restarts: Vec::new(),
            restart_fetches: 0,
            decoder: StreamDecoder::new(),
            frames: 0,
            last_frame: None,
        }
    }

    /// Take the handshake's word on the timeline, epoch, labels and
    /// metadata of a subscription detection opened.
    fn begin_stream(&mut self, sub: &Subscription, interval: Duration) {
        let url = &self.spec.url;
        if let Some(floor) = sub.update_floor().filter(|floor| interval < *floor) {
            warn!(
                "{url}: --interval {} is shorter than the producer's update floor of {}; a \
                 frame can only carry new readings every {}, and the rest will be empty",
                humantime::format_duration(interval),
                humantime::format_duration(floor),
                humantime::format_duration(floor)
            );
        }
        if sub.layout() != Layout::Long {
            info!(
                "{url}: the producer serves only the wide stream layout; groups of slots carry \
                 their whole member list on each change"
            );
        }
        let handshake = sub.handshake();
        self.handshake_labels = handshake.labels.clone();
        self.mode = Mode::Streamed;
        self.adopt(handshake, false);
    }

    /// Take a handshake's anchor, metadata and epoch. Returns the new epoch
    /// when it differs from the one on record: a restarted producer.
    fn adopt(&mut self, handshake: &Handshake, rows_staged: bool) -> Option<String> {
        // Zero is what no producer anchors at (1970), so it is not an anchor.
        if handshake.clock_anchor_wall_ns != 0 {
            self.anchor = Some(handshake.clock_anchor_wall_ns);
        }
        self.handshake_metadata = handshake.metadata.clone();
        let seen = handshake.epoch()?;
        match self.epoch.as_deref() {
            None => {
                self.epoch = Some(seen.to_string());
                None
            }
            Some(known) if known == seen => None,
            Some(known) => {
                warn!(
                    "{}: the producer restarted mid-recording (producer epoch {known} -> \
                     {seen}); every cumulative counter reset at this point",
                    self.spec.url
                );
                self.epoch = Some(seen.to_string());
                self.restarts.push(Restart {
                    epoch: seen.to_string(),
                    from_ts: None,
                    rows_before: rows_staged,
                });
                self.restart_fetches = 0;
                Some(seen.to_string())
            }
        }
    }

    fn abort_tasks(&mut self) {
        for task in [self.detecting.take().map(|t| t.abort_handle())]
            .into_iter()
            .flatten()
            .chain(self.enriching.take().map(|t| t.abort_handle()))
            .chain(self.pump.take().map(|t| t.abort_handle()))
        {
            task.abort();
        }
    }
}

struct Slot {
    id: SourceId,
    kind: Kind,
    state: SourceState,
    transport: Option<ActiveTransport>,
    name: Option<String>,
    /// Whether the archive has a recorder for this source.
    added: bool,
    /// The recording's metadata as last written.
    metadata: BTreeMap<String, String>,
    /// The caller's keys.
    pinned: BTreeSet<String>,
    /// Patches for a source not added yet.
    pending_patch: BTreeMap<String, String>,
    passes_total: u64,
    first_ts: Option<u64>,
    last_stamp: Option<(u64, i64)>,
    first_wall_ns: Option<u64>,
    last_wall_ns: Option<u64>,
    committed_this_tick: bool,
}

impl Slot {
    fn new(id: SourceId, kind: Kind) -> Self {
        let pinned = match &kind {
            Kind::Local { identity, .. } => identity.metadata.keys().cloned().collect(),
            #[cfg(feature = "http")]
            Kind::Endpoint(ep) => ep.spec.metadata.keys().cloned().collect(),
        };
        Self {
            id,
            kind,
            state: SourceState::Pending,
            transport: None,
            name: None,
            added: false,
            metadata: BTreeMap::new(),
            pinned,
            pending_patch: BTreeMap::new(),
            passes_total: 0,
            first_ts: None,
            last_stamp: None,
            first_wall_ns: None,
            last_wall_ns: None,
            committed_this_tick: false,
        }
    }

    /// The endpoint's URL, or the source's id for a local source.
    fn label(&self) -> String {
        match &self.kind {
            Kind::Local { .. } => self.id.to_string(),
            #[cfg(feature = "http")]
            Kind::Endpoint(ep) => match &self.name {
                Some(name) => format!("{name} ({})", ep.spec.url),
                None => ep.spec.url.to_string(),
            },
        }
    }

    fn delivered(&mut self, wall_ns: u64) {
        self.first_wall_ns.get_or_insert(wall_ns);
        self.last_wall_ns = Some(wall_ns);
    }

    fn staged(&mut self, ts: u64, wall_offset: i64) {
        self.first_ts.get_or_insert(ts);
        self.last_stamp = Some((ts, wall_offset));
        self.passes_total += 1;
        self.committed_this_tick = true;
        #[cfg(feature = "http")]
        if let Kind::Endpoint(ep) = &mut self.kind {
            for restart in &mut ep.restarts {
                restart.from_ts.get_or_insert(ts);
            }
        }
    }

    fn summary(&self) -> SourceSummary {
        let (endpoint, name) = match &self.kind {
            Kind::Local { identity, .. } => (
                None,
                self.name
                    .clone()
                    .or_else(|| identity.labels.get(keys::SOURCE).cloned()),
            ),
            #[cfg(feature = "http")]
            Kind::Endpoint(ep) => (
                Some(ep.spec.url.to_string()),
                self.name
                    .clone()
                    .or_else(|| ep.spec.labels.get(keys::SOURCE).cloned()),
            ),
        };
        SourceSummary {
            id: self.id,
            endpoint,
            name,
            state: self.state,
            transport: self.transport,
            passes_total: self.passes_total,
            first_wall_ns: self.first_wall_ns,
            last_wall_ns: self.last_wall_ns,
        }
    }
}

struct Driver {
    settings: Settings,
    #[cfg(feature = "http")]
    remote: Option<(Client, std::sync::Arc<dyn Probe>)>,
    slots: Vec<Slot>,
    archive: Option<Archive>,
    path: PathBuf,
    emitter: Emitter,
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    stream_tx: mpsc::Sender<(usize, StreamEventOf)>,
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    stream_rx: mpsc::Receiver<(usize, StreamEventOf)>,
    anchor_wall_ns: u64,
    anchor_mono: Instant,
    /// When the last pass started and ended, on this host's wall clock.
    last_pass: Option<(u64, u64)>,
    failed: Option<String>,
    /// Each added recording's label set, for the identical-labels warning.
    seen_labels: BTreeMap<BTreeMap<String, String>, String>,
    commit_streams_on_arrival: bool,
}

impl Drop for Driver {
    fn drop(&mut self) {
        let (closed, _) = mpsc::channel(1);
        std::mem::replace(&mut self.emitter, Emitter::new(closed)).hand_off();
    }
}

/// `rx.recv()` when `on`, otherwise a future that never resolves.
async fn recv_if<T>(rx: &mut mpsc::Receiver<T>, on: bool) -> Option<T> {
    if on {
        rx.recv().await
    } else {
        std::future::pending().await
    }
}

#[cfg(feature = "http")]
type StreamEventOf = StreamEvent;
#[cfg(not(feature = "http"))]
type StreamEventOf = std::convert::Infallible;

impl Driver {
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        for i in 0..self.slots.len() {
            let event = {
                let slot = &self.slots[i];
                match slot.state {
                    SourceState::Active => Some(Event::Active {
                        source: slot.id,
                        transport: slot.transport.unwrap_or(ActiveTransport::Local),
                        name: slot.name.clone().unwrap_or_default(),
                        inferred: self.inferred(i),
                    }),
                    #[cfg(feature = "http")]
                    SourceState::Pending => match &slot.kind {
                        Kind::Endpoint(ep) => match &ep.mode {
                            Mode::Undetected(Some(reason)) => Some(Event::Pending {
                                source: slot.id,
                                reason: reason.clone(),
                            }),
                            _ => None,
                        },
                        _ => None,
                    },
                    _ => None,
                }
            };
            if let Some(event) = event {
                self.emitter.must(event);
            }
        }

        let interval = self.settings.interval;
        let start = Instant::now() + align_wait(wall_now_ns(), interval);
        let mut ticker = tokio::time::interval_at(start.into(), interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // The tick outranks a command, so a stop that lands on a tick
            // still takes that tick's pass. A tick that overran its interval
            // is ready again at once and would win every time, so commands
            // that arrived during it are taken after it.
            let command = tokio::select! {
                biased;
                _ = ticker.tick(), if self.failed.is_none() => {
                    self.tick().await;
                    match commands.try_recv() {
                        Ok(command) => Some(command),
                        Err(mpsc::error::TryRecvError::Empty) => continue,
                        Err(mpsc::error::TryRecvError::Disconnected) => None,
                    }
                }
                command = commands.recv() => command,
                received = recv_if(
                    &mut self.stream_rx,
                    self.commit_streams_on_arrival && self.failed.is_none(),
                ) => {
                    #[cfg(feature = "http")]
                    if let Some((i, event)) = received {
                        match self.on_stream_event(i, event).await {
                            Ok(()) => self.commit(true).await,
                            Err(e) => self.fail(e).await,
                        }
                    }
                    #[cfg(not(feature = "http"))]
                    let _ = received;
                    continue;
                }
            };
            match command {
                Some(Command::Patch {
                    source,
                    patch,
                    reply,
                }) => {
                    let _ = reply.send(self.patch(source, patch).await);
                }
                Some(Command::Finish {
                    stop,
                    cancel,
                    reply,
                }) => {
                    let summary = self.finish(stop, cancel).await;
                    let _ = reply.send(summary);
                    return;
                }
                Some(Command::Discard { reply }) => {
                    self.abort_tasks();
                    self.remove_archive().await;
                    let _ = reply.send(());
                    return;
                }
                None => {
                    // The `Recorder` was dropped: stop, and leave the archive
                    // unfinalized on disk.
                    self.abort_tasks();
                    self.drop_archive().await;
                    return;
                }
            }
        }
    }

    fn inferred(&self, i: usize) -> bool {
        match &self.slots[i].kind {
            Kind::Local { identity, .. } => !identity.labels.contains_key(keys::SOURCE),
            #[cfg(feature = "http")]
            Kind::Endpoint(ep) => self.identity_of(ep).source().1,
        }
    }

    fn stamp_now(&self) -> (u64, u64, i64) {
        let wall = wall_now_ns();
        let ts = self
            .anchor_wall_ns
            .saturating_add(self.anchor_mono.elapsed().as_nanos() as u64);
        (wall, ts, wall as i64 - ts as i64)
    }

    async fn tick(&mut self) {
        self.emitter.flush();
        let (wall, ts, wall_offset) = self.stamp_now();
        let mut failure: Option<String> = None;
        #[cfg(feature = "http")]
        if let Err(e) = self.scrape_all(wall, ts, wall_offset).await {
            failure.get_or_insert(e);
        }
        #[cfg(not(feature = "http"))]
        let _ = (ts, wall_offset);
        if let Err(e) = self.local_passes(wall).await {
            failure.get_or_insert(e);
        }
        #[cfg(feature = "http")]
        {
            if let Err(e) = self.drain_streams().await {
                failure.get_or_insert(e);
            }
            self.apply_restarts(false).await;
            if let Err(e) = self.activate().await {
                failure.get_or_insert(e);
            }
        }
        match failure {
            Some(e) => self.fail(e).await,
            None => self.commit(true).await,
        }
        self.last_pass = Some((wall, wall_now_ns()));
    }

    /// The archive failed: report it, stop ticking, and leave what is on
    /// disk as it is.
    async fn fail(&mut self, error: String) {
        warn!("the recording failed: {error}");
        self.failed = Some(error.clone());
        self.abort_tasks();
        self.drop_archive().await;
        self.emitter.must(Event::Failed { error });
    }

    /// Run `f` on the archive on a blocking thread: commits, seals and
    /// metadata updates are blocking sends to dendro's writer thread.
    async fn blocking<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut Archive) -> T + Send + 'static,
    ) -> Option<T> {
        let mut archive = self.archive.take()?;
        match tokio::task::spawn_blocking(move || {
            let out = f(&mut archive);
            (archive, out)
        })
        .await
        {
            Ok((archive, out)) => {
                self.archive = Some(archive);
                Some(out)
            }
            Err(e) => {
                self.failed
                    .get_or_insert(format!("the writer task failed: {e}"));
                None
            }
        }
    }

    /// Commit the tick's staged rows as one transaction, run every source's
    /// seal check, apply retention, and report each source that committed.
    async fn commit(&mut self, maintain: bool) {
        let retain = self.settings.retain;
        let cutoffs: Vec<(usize, u64)> = match retain {
            Some(retain) if maintain => self
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    let newest = s.last_stamp?.0;
                    let back = retain.as_nanos().min(u64::MAX as u128) as u64;
                    Some((i, newest.saturating_sub(back)))
                })
                .collect(),
            _ => Vec::new(),
        };
        let result = self
            .blocking(move |a| -> Result<(), String> {
                a.writer
                    .commit(std::mem::take(&mut a.staged))
                    .map_err(|e| e.to_string())?;
                if maintain {
                    let mut first_err = None;
                    for rec in a.recs.values_mut() {
                        if let Err(e) = rec.maybe_seal() {
                            first_err.get_or_insert(e.to_string());
                        }
                    }
                    if let Some(e) = first_err {
                        return Err(e);
                    }
                    for (i, cutoff) in cutoffs {
                        if let Some(rec) = a.recs.get_mut(&i) {
                            rec.evict_before(cutoff).map_err(|e| e.to_string())?;
                        }
                    }
                }
                Ok(())
            })
            .await;
        match result {
            Some(Ok(())) => {}
            Some(Err(e)) => return self.fail(e).await,
            None => {
                if let Some(e) = self.failed.clone() {
                    self.failed = None;
                    return self.fail(e).await;
                }
                return;
            }
        }
        for slot in &mut self.slots {
            if !std::mem::take(&mut slot.committed_this_tick) {
                continue;
            }
            let (Some(first_ts), Some((ts, wall_offset))) = (slot.first_ts, slot.last_stamp) else {
                continue;
            };
            self.emitter.lossy(Event::Committed {
                source: slot.id,
                passes_total: slot.passes_total,
                first_ts,
                ts,
                wall_ns: Stamp { ts, wall_offset }.wall_ns(),
            });
        }
    }

    /// Stage `snapshot` for slot `i`.
    fn stage(
        &mut self,
        i: usize,
        snapshot: &Snapshot,
        ts: u64,
        wall_offset: i64,
    ) -> Result<(), String> {
        let archive = self
            .archive
            .as_mut()
            .ok_or_else(|| "the archive is closed".to_string())?;
        let rec = archive.recs.get_mut(&i).ok_or_else(|| {
            format!(
                "{} delivered a pass with no recording open for it",
                self.slots[i].label()
            )
        })?;
        archive.staged.push(
            rec.stage(snapshot, ts, wall_offset)
                .map_err(|e| e.to_string())?,
        );
        self.slots[i].staged(ts, wall_offset);
        Ok(())
    }

    /// One pass of every active local source, on a blocking thread (see
    /// [`LocalSource`]), staged in order.
    // Without `http`, `Kind::Local` is the only kind.
    #[cfg_attr(not(feature = "http"), allow(irrefutable_let_patterns))]
    async fn local_passes(&mut self, wall: u64) -> Result<(), String> {
        let mut taken: Vec<(usize, Box<dyn LocalSource>)> = Vec::new();
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.state != SourceState::Active || !slot.added {
                continue;
            }
            if let Kind::Local { source, .. } = &mut slot.kind {
                if let Some(source) = source.take() {
                    taken.push((i, source));
                }
            }
        }
        if taken.is_empty() {
            return Ok(());
        }
        let passes = tokio::task::spawn_blocking(move || {
            taken
                .into_iter()
                .map(|(i, mut source)| {
                    let pass = source.pass();
                    (i, source, pass)
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| format!("a local source's pass failed: {e}"))?;
        for (i, returned, pass) in passes {
            if let Kind::Local { source, .. } = &mut self.slots[i].kind {
                *source = Some(returned);
            }
            let Some((snapshot, stamp)) = pass else {
                continue;
            };
            if self.slots[i]
                .last_stamp
                .is_some_and(|(last, _)| stamp.ts <= last)
            {
                debug!(
                    "{}: a pass stamped {} is not after the last one staged; skipped",
                    self.slots[i].label(),
                    stamp.ts
                );
                continue;
            }
            self.slots[i].delivered(wall);
            self.stage(i, &snapshot, stamp.ts, stamp.wall_offset)?;
        }
        Ok(())
    }

    /// Add slot `i`'s recording to the archive.
    async fn add_source(&mut self, i: usize) -> Result<(), String> {
        let interval_ms = self.settings.interval.as_millis().to_string();
        let (labels, mut metadata, anchor, transport, name) = match &self.slots[i].kind {
            Kind::Local { identity, .. } => {
                let mut metadata = identity.metadata.clone();
                if let Some(epoch) = &identity.epoch {
                    metadata
                        .entry(keys::PRODUCER_EPOCH.to_string())
                        .or_insert_with(|| epoch.clone());
                }
                metadata
                    .entry(keys::SAMPLING_INTERVAL_MS.to_string())
                    .or_insert(interval_ms);
                let name = identity
                    .labels
                    .get(keys::SOURCE)
                    .cloned()
                    .unwrap_or_else(|| "local".to_string());
                let mut labels = identity.labels.clone();
                labels.insert(keys::SOURCE.to_string(), name.clone());
                metadata
                    .entry(keys::SOURCE.to_string())
                    .or_insert_with(|| name.clone());
                (
                    labels,
                    metadata,
                    identity.clock_anchor_wall_ns,
                    ActiveTransport::Local,
                    name,
                )
            }
            #[cfg(feature = "http")]
            Kind::Endpoint(ep) => {
                let mut recorder = BTreeMap::new();
                recorder.insert(keys::SAMPLING_INTERVAL_MS.to_string(), interval_ms);
                if let Some(epoch) = &ep.epoch {
                    recorder.insert(keys::PRODUCER_EPOCH.to_string(), epoch.clone());
                }
                let identity = Identity {
                    recorder_metadata: recorder,
                    ..self.identity_of(ep)
                };
                let transport = match ep.mode {
                    Mode::Streamed => ActiveTransport::Stream,
                    _ => ActiveTransport::Prometheus,
                };
                // A streamed recording's rows carry the producer's stamps,
                // so it is anchored on the producer's clock; a scraped one
                // keeps the recorder's, whose stamps its rows carry.
                let anchor = match transport {
                    ActiveTransport::Stream => ep.anchor.unwrap_or(self.anchor_wall_ns),
                    _ => self.anchor_wall_ns,
                };
                (
                    identity.labels(),
                    identity.metadata(),
                    anchor,
                    transport,
                    identity.source().0,
                )
            }
        };
        metadata.extend(std::mem::take(&mut self.slots[i].pending_patch));
        let label = self.slots[i].label();
        if let Some(warning) = label_collision(&self.seen_labels, &labels, &label) {
            warn!("{warning}");
        }
        self.seen_labels.insert(labels.clone(), label);

        let seed = metadata.clone();
        let rec = self
            .blocking(move |a| {
                a.writer
                    .add_source(labels, seed, anchor)
                    .map_err(|e| e.to_string())
            })
            .await
            .ok_or_else(|| "the archive is closed".to_string())??;
        if let Some(archive) = self.archive.as_mut() {
            archive.recs.insert(i, rec);
        }
        let slot = &mut self.slots[i];
        slot.metadata = metadata;
        slot.added = true;
        slot.state = SourceState::Active;
        slot.transport = Some(transport);
        slot.name = Some(name);
        Ok(())
    }

    async fn patch(
        &mut self,
        source: SourceId,
        patch: BTreeMap<String, String>,
    ) -> Result<(), Error> {
        if let Some(e) = &self.failed {
            return Err(Error::Archive(e.clone()));
        }
        reserved_key(&patch).map_err(Error::Reserved)?;
        let i = source.0 as usize;
        let slot = self
            .slots
            .get_mut(i)
            .ok_or_else(|| Error::Source(format!("no {source}")))?;
        if slot.state == SourceState::Refused && !slot.added {
            return Err(Error::Source(format!(
                "{} was refused and has no recording to patch",
                slot.label()
            )));
        }
        slot.pinned.extend(patch.keys().cloned());
        if !slot.added {
            slot.pending_patch.extend(patch);
            return Ok(());
        }
        slot.metadata.extend(patch.clone());
        let result = self
            .blocking(move |a| -> Result<(), String> {
                let rec = a
                    .recs
                    .get_mut(&i)
                    .ok_or_else(|| format!("no recording is open for {source}"))?;
                rec.update_metadata(patch).map_err(|e| e.to_string())?;
                rec.sync().map_err(|e| e.to_string())
            })
            .await
            .ok_or(Error::Closed)?;
        result.map_err(Error::Archive)
    }

    /// Replace keys of slot `i`'s metadata without pinning them, for the
    /// recorder's own writes.
    #[cfg(feature = "http")]
    async fn write_metadata(
        &mut self,
        i: usize,
        patch: BTreeMap<String, String>,
    ) -> Result<(), String> {
        self.slots[i].metadata.extend(patch.clone());
        self.blocking(move |a| match a.recs.get_mut(&i) {
            Some(rec) => rec.update_metadata(patch).map_err(|e| e.to_string()),
            None => Err("no recording is open".to_string()),
        })
        .await
        .unwrap_or_else(|| Err("the archive is closed".to_string()))
    }

    async fn finish(&mut self, stop: Stop, cancel: CancellationToken) -> Result<Summary, Error> {
        if let Some(e) = &self.failed {
            return Err(Error::Archive(e.clone()));
        }
        let mut failure: Option<String> = None;
        let (wall, ts, wall_offset) = self.stamp_now();
        match stop {
            Stop::AfterExit { wall_ns, bound } => {
                let sampled = self.last_pass.is_some_and(|(_, end)| wall_ns <= end);
                if !sampled {
                    #[cfg(feature = "http")]
                    if let Err(e) = self.scrape_all(wall, ts, wall_offset).await {
                        failure.get_or_insert(e);
                    }
                    if let Err(e) = self.local_passes(wall).await {
                        failure.get_or_insert(e);
                    }
                }
                #[cfg(feature = "http")]
                if failure.is_none() {
                    if let Err(e) = self.await_frames(wall_ns, bound, &cancel).await {
                        failure.get_or_insert(e);
                    }
                }
                #[cfg(not(feature = "http"))]
                let _ = bound;
            }
            Stop::Grace(duration) => {
                if !duration.is_zero() {
                    tokio::select! {
                        _ = cancel.cancelled() => {}
                        _ = tokio::time::sleep(duration) => {}
                    }
                }
            }
            Stop::Now => {
                if let Err(e) = self.local_passes(wall).await {
                    failure.get_or_insert(e);
                }
            }
        }
        #[cfg(not(feature = "http"))]
        let _ = (ts, wall_offset);
        #[cfg(feature = "http")]
        {
            if failure.is_none() {
                if let Err(e) = self.drain_streams().await {
                    failure.get_or_insert(e);
                }
            }
            self.apply_restarts(true).await;
        }
        self.abort_tasks();
        if let Some(e) = failure {
            self.fail(e.clone()).await;
            return Err(Error::Archive(e));
        }
        self.commit(false).await;
        if let Some(e) = &self.failed {
            return Err(Error::Archive(e.clone()));
        }

        let sources: Vec<SourceSummary> = self.slots.iter().map(Slot::summary).collect();
        if self.slots.iter().all(|s| s.first_wall_ns.is_none()) {
            self.remove_archive().await;
            return Ok(Summary {
                sources,
                discarded: true,
            });
        }

        let fallback = (ts, wall_offset);
        let lasts: BTreeMap<usize, (u64, i64)> = self
            .slots
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.last_stamp.unwrap_or(fallback)))
            .collect();
        let finalized = self
            .blocking(move |a| -> Result<(), String> {
                let mut first_err = None;
                for (i, rec) in std::mem::take(&mut a.recs) {
                    let last = lasts.get(&i).copied().unwrap_or(fallback);
                    if let Err(e) = rec.finalize(last) {
                        first_err.get_or_insert(e.to_string());
                    }
                }
                let joined = a.writer.join().map_err(|e| e.to_string());
                first_err.map_or(joined, Err)
            })
            .await;
        self.drop_archive().await;
        match finalized {
            Some(Ok(())) => Ok(Summary {
                sources,
                discarded: false,
            }),
            Some(Err(e)) => Err(Error::Archive(e)),
            None => Err(Error::Archive(
                self.failed
                    .clone()
                    .unwrap_or_else(|| "the archive is closed".to_string()),
            )),
        }
    }

    fn abort_tasks(&mut self) {
        #[cfg(feature = "http")]
        for slot in &mut self.slots {
            if let Kind::Endpoint(ep) = &mut slot.kind {
                ep.abort_tasks();
            }
        }
    }

    /// Drop the archive on a blocking thread: dropping the writer joins its
    /// thread.
    async fn drop_archive(&mut self) {
        if let Some(archive) = self.archive.take() {
            let _ = tokio::task::spawn_blocking(move || drop(archive)).await;
        }
    }

    /// Drop the archive and remove its file and SQLite sidecars. The writer
    /// created the path exclusively during this run, so it is ours.
    async fn remove_archive(&mut self) {
        self.drop_archive().await;
        remove_files(&self.path);
    }
}

/// What is wrong with `labels` against the label sets already added, or
/// `None` when nothing is: identical label sets, which no reader can tell
/// apart, or label sets equal but for ASCII case. dendro staggers each
/// source's seals by its label key, and two keys that differ only in case
/// fall in the same stagger bucket, so those recordings seal in lockstep.
fn label_collision(
    seen: &BTreeMap<BTreeMap<String, String>, String>,
    labels: &BTreeMap<String, String>,
    label: &str,
) -> Option<String> {
    if let Some(other) = seen.get(labels) {
        return Some(format!(
            "{other} and {label} carry identical labels ({labels:?}), so nothing downstream \
             can tell their recordings apart; give each its own source"
        ));
    }
    let folded = |m: &BTreeMap<String, String>| -> Vec<(String, String)> {
        m.iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_ascii_lowercase()))
            .collect()
    };
    let mine = folded(labels);
    seen.iter()
        .find(|(other, _)| folded(other) == mine)
        .map(|(other_labels, other)| {
            format!(
                "{other} ({other_labels:?}) and {label} ({labels:?}) differ only in case; dendro \
                 staggers seals by label key and cannot tell them apart, so their recordings \
                 will seal in lockstep. Give them labels that differ by more than case"
            )
        })
}

fn remove_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(sidecar));
    }
}

#[cfg(feature = "http")]
impl Driver {
    fn client(&self) -> Option<(Client, std::sync::Arc<dyn Probe>)> {
        self.remote.clone()
    }

    fn identity_of<'a>(&self, ep: &'a Endpoint) -> Identity<'a> {
        Identity {
            user_labels: &ep.spec.labels,
            user_metadata: &ep.spec.metadata,
            recorder_metadata: BTreeMap::new(),
            hook_labels: &ep.hook_labels,
            hook_metadata: &ep.hook_metadata,
            handshake_labels: &ep.handshake_labels,
            handshake_metadata: &ep.handshake_metadata,
            inferred: &ep.inferred,
        }
    }

    async fn first_enrich(&mut self) {
        let Some((_, probe)) = self.client() else {
            return;
        };
        let timeout = self.settings.timeout;
        let asks: Vec<(usize, Url, Option<String>)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match &s.kind {
                Kind::Endpoint(ep) if ep.opened.is_some() => {
                    Some((i, ep.spec.url.clone(), ep.epoch.clone()))
                }
                _ => None,
            })
            .collect();
        let replies = futures_util::future::join_all(asks.into_iter().map(|(i, url, epoch)| {
            let probe = probe.clone();
            async move {
                let reply =
                    probe::enrich_within(probe.as_ref(), &url, epoch.as_deref(), timeout).await;
                (i, reply)
            }
        }))
        .await;
        for (i, reply) in replies {
            if let Kind::Endpoint(ep) = &mut self.slots[i].kind {
                adopt_enrich(ep, reply);
            }
        }
    }

    fn pump_config(&self) -> PumpConfig {
        PumpConfig {
            interval: self.settings.interval,
            layout: self.settings.layout,
            timeout: self.settings.timeout,
            wire_names: self.settings.wire_names.clone(),
        }
    }

    /// Start slot `i`'s pump over the subscription detection opened, once
    /// the source is in the archive.
    fn spawn_pump(&mut self, i: usize) {
        let Some((client, _)) = self.client() else {
            return;
        };
        let cfg = self.pump_config();
        let tx = self.stream_tx.clone();
        let slot = &mut self.slots[i];
        if !slot.added {
            return;
        }
        let Kind::Endpoint(ep) = &mut slot.kind else {
            return;
        };
        let Some(sub) = ep.opened.take() else {
            return;
        };
        ep.pump = Some(tokio::spawn(subscription::pump(
            i,
            sub,
            client,
            ep.spec.url.clone(),
            cfg,
            tx,
        )));
    }

    /// Scrape every active scraped endpoint at once and stage what came
    /// back.
    async fn scrape_all(&mut self, wall: u64, ts: u64, wall_offset: i64) -> Result<(), String> {
        let Some((client, _)) = self.client() else {
            return Ok(());
        };
        let targets: Vec<(usize, Url)> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.state == SourceState::Active && s.added)
            .filter_map(|(i, s)| match &s.kind {
                Kind::Endpoint(ep) => match &ep.mode {
                    Mode::Scraped { url, .. } => Some((i, url.clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let timeout = self.settings.timeout;
        let (anchor_wall_ns, anchor_mono) = (self.anchor_wall_ns, self.anchor_mono);
        let results = futures_util::future::join_all(targets.into_iter().map(|(i, url)| {
            let client = client.clone();
            async move {
                let result = match tokio::time::timeout(
                    timeout,
                    scrape_one(&client, &url, anchor_wall_ns, anchor_mono),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(format!(
                        "timed out after {}",
                        humantime::format_duration(timeout)
                    )),
                };
                (i, result)
            }
        }))
        .await;

        for (i, result) in results {
            let (body, request_ns, response_ns) = match result {
                Ok(scraped) => scraped,
                Err(e) => {
                    warn!("scrape failed for {}: {e}", self.slots[i].label());
                    continue;
                }
            };
            self.slots[i].delivered(wall);
            let pinned = self.slots[i].pinned.contains(keys::DESCRIPTIONS);
            let (snapshot, descriptions) = match &mut self.slots[i].kind {
                Kind::Endpoint(ep) => match &mut ep.mode {
                    Mode::Scraped { converter, .. } => {
                        let text = String::from_utf8_lossy(&body);
                        let snapshot = converter.convert(&text, request_ns, response_ns);
                        let pending = converter.pending_descriptions();
                        if pending.is_some() && pinned {
                            converter.descriptions_stored();
                        }
                        (snapshot, pending.filter(|_| !pinned))
                    }
                    _ => continue,
                },
                _ => continue,
            };
            self.stage(i, &snapshot, ts, wall_offset)?;
            // The scrape's HELP text, when it named a metric not yet stored.
            // A failed hand-off is retried after the next scrape.
            if let Some(json) = descriptions {
                let patch = BTreeMap::from([(keys::DESCRIPTIONS.to_string(), json)]);
                match self.write_metadata(i, patch).await {
                    Ok(()) => {
                        if let Kind::Endpoint(ep) = &mut self.slots[i].kind {
                            if let Mode::Scraped { converter, .. } = &mut ep.mode {
                                converter.descriptions_stored();
                            }
                        }
                    }
                    Err(e) => warn!(
                        "not storing the HELP text of {} yet: {e}",
                        self.slots[i].label()
                    ),
                }
            }
        }
        Ok(())
    }

    /// Stage everything the pumps delivered so far.
    async fn drain_streams(&mut self) -> Result<(), String> {
        let mut failure = None;
        while let Ok((i, event)) = self.stream_rx.try_recv() {
            if let Err(e) = self.on_stream_event(i, event).await {
                failure.get_or_insert(e);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    async fn on_stream_event(&mut self, i: usize, event: StreamEvent) -> Result<(), String> {
        let wall = wall_now_ns();
        let id = self.slots[i].id;
        let label = self.slots[i].label();
        match event {
            StreamEvent::Interval(applied) => {
                if self.slots[i].state != SourceState::Active {
                    return Ok(());
                }
                self.slots[i].delivered(wall);
                let newest = applied.newest_wall_ns();
                let (seq, gap) = (applied.seq, applied.gap);
                let passes = applied.passes()?;
                let frames = {
                    let Kind::Endpoint(ep) = &mut self.slots[i].kind else {
                        return Ok(());
                    };
                    ep.frames += 1;
                    ep.last_frame = Some((newest, wall));
                    ep.frames
                };
                if gap {
                    warn!(
                        "{label}: the stream jumped to interval {seq}; the intervals before it \
                         produced no frame (the subscription was starved, or frames were lost)"
                    );
                    self.emitter.lossy(Event::Gap { source: id, seq });
                }
                let mut stamp = None;
                for pass in passes {
                    let groups = {
                        let Kind::Endpoint(ep) = &mut self.slots[i].kind else {
                            return Ok(());
                        };
                        let before = ep.decoder.unresolved;
                        let groups = ep.decoder.decode(pass.rows)?;
                        if ep.decoder.unresolved > before {
                            warn!(
                                "{label}: {} streamed rows named a schema this connection had \
                                 not sent; skipped",
                                ep.decoder.unresolved - before
                            );
                        }
                        groups
                    };
                    let archive = self
                        .archive
                        .as_mut()
                        .ok_or_else(|| "the archive is closed".to_string())?;
                    let rec = archive.recs.get_mut(&i).ok_or_else(|| {
                        format!("{label} streamed an interval with no recording open for it")
                    })?;
                    archive.staged.push(
                        rec.stage_streamed(groups, pass.ts, pass.wall_offset)
                            .map_err(|e| e.to_string())?,
                    );
                    self.slots[i].staged(pass.ts, pass.wall_offset);
                    stamp = Some(Stamp {
                        ts: pass.ts,
                        wall_offset: pass.wall_offset,
                    });
                }
                self.emitter.lossy(Event::Frame {
                    source: id,
                    seq: frames,
                    stamp,
                });
            }
            StreamEvent::Dropped(reason) => {
                warn!("{label}: the stream ended ({reason}); reconnecting");
                self.emitter.must(Event::Dropped { source: id, reason });
            }
            StreamEvent::Connected(handshake) => {
                info!("{label}: stream reconnected");
                let rows = self.slots[i].first_ts.is_some();
                let restarted = match &mut self.slots[i].kind {
                    Kind::Endpoint(ep) => ep.adopt(&handshake, rows).is_some(),
                    _ => false,
                };
                self.emitter.must(Event::Reconnected {
                    source: id,
                    epoch: handshake.epoch().map(str::to_string),
                    restarted,
                });
            }
            StreamEvent::Refused(refusal) => {
                // Asked for the wording only: a streamed source is never
                // switched to a scrape, and a verdict other than a refusal
                // gives the route's answer as the reason.
                let reason = match self.client() {
                    Some((_, probe)) => {
                        let url = match &self.slots[i].kind {
                            Kind::Endpoint(ep) => ep.spec.url.clone(),
                            _ => return Ok(()),
                        };
                        let verdict = tokio::time::timeout(
                            self.settings.timeout,
                            probe.not_streamable(&url, At::Reconnect, &refusal.answer),
                        )
                        .await
                        .unwrap_or(Verdict::Pending);
                        match verdict {
                            Verdict::Refuse { reason } => reason,
                            _ => format!("{refusal}. {RECONNECT_REFUSED}"),
                        }
                    }
                    None => format!("{refusal}. {RECONNECT_REFUSED}"),
                };
                self.refuse(i, reason).await;
            }
        }
        Ok(())
    }

    async fn refuse(&mut self, i: usize, reason: String) {
        let slot = &mut self.slots[i];
        slot.state = SourceState::Refused;
        if let Kind::Endpoint(ep) = &mut slot.kind {
            ep.abort_tasks();
        }
        let source = slot.id;
        self.emitter.must(Event::Refused { source, reason });
    }

    /// Write each restart into its recording's metadata once a row from the
    /// new process has been staged. The probe hook's reply is asked for on
    /// up to [`FETCH_ATTEMPTS`] ticks; on the `last` call there is no next
    /// tick, so the restart is written at once.
    async fn apply_restarts(&mut self, last: bool) {
        let Some((_, probe)) = self.client() else {
            return;
        };
        let timeout = self.settings.timeout;
        for i in 0..self.slots.len() {
            if !self.slots[i].added {
                continue;
            }
            let (url, newest) = match &self.slots[i].kind {
                Kind::Endpoint(ep) => match ep.restarts.last() {
                    Some(newest) if ep.restarts.iter().all(|r| r.from_ts.is_some()) => {
                        (ep.spec.url.clone(), newest.epoch.clone())
                    }
                    _ => continue,
                },
                _ => continue,
            };
            let label = self.slots[i].label();
            let hook =
                match probe::enrich_within(probe.as_ref(), &url, Some(&newest), timeout).await {
                    Ok((_, metadata)) => metadata,
                    Err(why) => {
                        let Kind::Endpoint(ep) = &mut self.slots[i].kind else {
                            continue;
                        };
                        if !last && ep.restart_fetches + 1 < FETCH_ATTEMPTS {
                            ep.restart_fetches += 1;
                            continue;
                        }
                        warn!(
                        "{label}: could not read the restarted producer's metadata ({why}); the \
                         recording's metadata records the restart without it"
                    );
                        BTreeMap::new()
                    }
                };
            let slot = &mut self.slots[i];
            let Kind::Endpoint(ep) = &mut slot.kind else {
                continue;
            };
            ep.restart_fetches = 0;
            let mut reported = ep.handshake_metadata.clone();
            reported.extend(hook);
            let restarts = std::mem::take(&mut ep.restarts);
            let merge = |key: &str, old: &str, new: &str| probe.merge(key, old, new);
            let patch = restart::metadata_patch(
                &slot.metadata,
                slot.first_ts,
                &restarts,
                &reported,
                &slot.pinned,
                &merge,
            );
            let version = patch.get(keys::VERSION).cloned().or_else(|| {
                (!slot.pinned.contains(keys::VERSION))
                    .then(|| reported.get(keys::VERSION).cloned())
                    .flatten()
            });
            let source = slot.id;
            match self.write_metadata(i, patch).await {
                Ok(()) => self.emitter.must(Event::Restarted {
                    source,
                    epoch: newest,
                    version,
                }),
                Err(e) => warn!(
                    "{label}: the producer restart could not be written into the recording's \
                     metadata: {e}"
                ),
            }
        }
    }

    /// Late activation, in steps so that no tick waits on detection or on
    /// the probe hook: a pending endpoint is detected on its own task, one
    /// in flight per endpoint, and the tick that consumes a `Pending` starts
    /// the next. A streamed endpoint's `enrich` runs on its own task, and it
    /// is added on the first tick after that returns or times out.
    async fn activate(&mut self) -> Result<(), String> {
        let Some((client, probe)) = self.client() else {
            return Ok(());
        };
        let detect_cfg = detect::DetectConfig {
            interval: self.settings.interval,
            timeout: self.settings.timeout,
            layout: self.settings.layout,
            wire_names: self.settings.wire_names.clone(),
        };
        let timeout = self.settings.timeout;
        let mut added: Vec<usize> = Vec::new();

        for i in 0..self.slots.len() {
            match self.slots[i].state {
                SourceState::Activating => {
                    let Kind::Endpoint(ep) = &mut self.slots[i].kind else {
                        continue;
                    };
                    if !ep.enriching.as_ref().is_some_and(|t| t.is_finished()) {
                        continue;
                    }
                    let task = ep.enriching.take().expect("checked above");
                    let reply = task
                        .await
                        .unwrap_or_else(|e| Err(format!("its metadata read failed: {e}")));
                    adopt_enrich(ep, reply);
                    added.push(i);
                }
                SourceState::Pending => {
                    let Kind::Endpoint(ep) = &mut self.slots[i].kind else {
                        continue;
                    };
                    let url = ep.spec.url.clone();
                    let transport = ep.spec.transport;
                    let spawn = || {
                        let (client, cfg, probe, url) = (
                            client.clone(),
                            detect_cfg.clone(),
                            probe.clone(),
                            url.clone(),
                        );
                        tokio::spawn(async move {
                            detect::detect_one(
                                &client,
                                &cfg,
                                &url,
                                transport,
                                probe.as_ref(),
                                At::Activation,
                            )
                            .await
                        })
                    };
                    let task = match ep.detecting.take() {
                        None => {
                            ep.detecting = Some(spawn());
                            continue;
                        }
                        Some(task) if !task.is_finished() => {
                            ep.detecting = Some(task);
                            continue;
                        }
                        Some(task) => task,
                    };
                    let outcome = task.await.unwrap_or_else(|e| {
                        Outcome::Pending(format!("endpoint {url}: detection failed: {e}"))
                    });
                    match outcome {
                        Outcome::Pending(why) => {
                            debug!("{why}");
                            ep.detecting = Some(spawn());
                        }
                        Outcome::Refused(reason) => {
                            self.refuse(i, reason).await;
                        }
                        Outcome::Streamed(sub) => {
                            info!(
                                "endpoint {url} now reachable, subscribed to its replication \
                                 stream; reading its metadata"
                            );
                            ep.begin_stream(&sub, self.settings.interval);
                            let epoch = ep.epoch.clone();
                            ep.opened = Some(*sub);
                            let probe = probe.clone();
                            let url = url.clone();
                            ep.enriching = Some(tokio::spawn(async move {
                                probe::enrich_within(
                                    probe.as_ref(),
                                    &url,
                                    epoch.as_deref(),
                                    timeout,
                                )
                                .await
                            }));
                            self.slots[i].state = SourceState::Activating;
                        }
                        Outcome::Scraped(scrape) => {
                            info!(
                                "endpoint {url} now reachable, scraped as Prometheus at {scrape}"
                            );
                            ep.mode = Mode::Scraped {
                                url: scrape,
                                converter: PrometheusConverter::new(),
                            };
                            added.push(i);
                        }
                    }
                }
                _ => {}
            }
        }

        for i in added {
            self.add_source(i).await.map_err(|e| {
                format!(
                    "failed to open a recording for {}: {e}",
                    self.slots[i].label()
                )
            })?;
            self.spawn_pump(i);
            let event = Event::Active {
                source: self.slots[i].id,
                transport: self.slots[i]
                    .transport
                    .unwrap_or(ActiveTransport::Prometheus),
                name: self.slots[i].name.clone().unwrap_or_default(),
                inferred: self.inferred(i),
            };
            self.emitter.must(event);
        }
        Ok(())
    }

    /// Wait, up to `bound`, for each active streamed source's frame stamped
    /// at or after `wall_ns`. An empty frame counts when it arrived after
    /// `wall_ns`.
    async fn await_frames(
        &mut self,
        wall_ns: u64,
        bound: Duration,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + bound;
        loop {
            let waiting: Vec<usize> = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.state == SourceState::Active
                        && s.added
                        && s.transport == Some(ActiveTransport::Stream)
                })
                .filter(|(_, s)| match &s.kind {
                    Kind::Endpoint(ep) => !match ep.last_frame {
                        Some((Some(stamp), _)) => stamp >= wall_ns,
                        Some((None, received)) => received >= wall_ns,
                        None => false,
                    },
                    _ => false,
                })
                .map(|(i, _)| i)
                .collect();
            if waiting.is_empty() {
                return Ok(());
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                event = self.stream_rx.recv() => {
                    let Some((i, event)) = event else { return Ok(()) };
                    self.on_stream_event(i, event).await?;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    for i in waiting {
                        warn!(
                            "{}: no stream frame stamped at or after the stop arrived within \
                             {}; the recording ends before it",
                            self.slots[i].label(),
                            humantime::format_duration(bound)
                        );
                    }
                    return Ok(());
                }
            }
        }
    }
}

/// What happens to a source refused on a reconnect, when the probe hook does
/// not word the refusal itself.
#[cfg(feature = "http")]
pub(crate) const RECONNECT_REFUSED: &str =
    "This endpoint's recording stops here, keeping the rows it had, and is not retried; the \
     other endpoints keep recording";

#[cfg(feature = "http")]
fn adopt_enrich(ep: &mut Endpoint, reply: Result<probe::Enriched, String>) {
    match reply {
        Ok((labels, metadata)) => {
            ep.hook_labels = labels;
            ep.hook_metadata = metadata;
        }
        Err(why) => warn!(
            "{}: {why}; its recording is added with the labels and metadata of its stream \
             handshake only",
            ep.spec.url
        ),
    }
}

/// One scrape's body, bracketed by when the request went out and when the
/// response was fully read, on the recorder's anchored timeline. The
/// bracket is the scrape's acquisition window.
#[cfg(feature = "http")]
async fn scrape_one(
    client: &Client,
    url: &Url,
    anchor_wall_ns: u64,
    anchor_mono: Instant,
) -> Result<(Vec<u8>, u64, u64), String> {
    let at = || anchor_wall_ns.saturating_add(anchor_mono.elapsed().as_nanos() as u64);
    let request_ns = at();
    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| format!("{e}"))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let body = response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("{e}"))?;
    // After the body: the values are in it.
    Ok((body, request_ns, at()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tick_waits_to_the_next_multiple_of_the_interval() {
        let s = Duration::from_secs(1);
        assert_eq!(
            align_wait(10_000_000_000, s),
            s,
            "on a boundary: a whole one"
        );
        assert_eq!(align_wait(10_250_000_000, s), Duration::from_millis(750));
        assert_eq!(
            align_wait(10_250_000_000, Duration::from_millis(200)),
            Duration::from_millis(150)
        );
    }

    #[test]
    fn the_default_timeout_is_twice_the_interval_within_bounds() {
        assert_eq!(
            default_timeout(Duration::from_millis(100)),
            Duration::from_secs(2)
        );
        assert_eq!(
            default_timeout(Duration::from_secs(3)),
            Duration::from_secs(6)
        );
        assert_eq!(
            default_timeout(Duration::from_secs(60)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn labels_that_differ_only_in_case_are_warned_about() {
        let labels = |v: &str| BTreeMap::from([("source".to_string(), v.to_string())]);
        let mut seen = BTreeMap::new();
        seen.insert(labels("Web"), "a".to_string());
        let same = label_collision(&seen, &labels("Web"), "b").unwrap();
        assert!(same.contains("identical labels"), "{same}");
        let case = label_collision(&seen, &labels("web"), "b").unwrap();
        assert!(case.contains("differ only in case"), "{case}");
        assert_eq!(label_collision(&seen, &labels("api"), "b"), None);
    }

    /// A must-deliver event into a full channel is queued, not awaited, and
    /// is delivered in order once the caller reads.
    #[tokio::test]
    async fn a_must_deliver_event_never_waits_for_the_caller() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut emitter = Emitter::new(tx);
        let refused = |n| Event::Refused {
            source: SourceId(n),
            reason: String::new(),
        };
        emitter.must(refused(0));
        emitter.must(refused(1));
        emitter.must(refused(2));
        assert_eq!(emitter.queue.len(), 2, "queued, not awaited");
        assert_eq!(rx.recv().await, Some(refused(0)));
        emitter.flush();
        assert_eq!(rx.recv().await, Some(refused(1)));
        emitter.hand_off();
        assert_eq!(rx.recv().await, Some(refused(2)));
    }

    #[tokio::test]
    async fn a_full_channel_drops_lossy_events_and_says_how_many() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut emitter = Emitter::new(tx);
        let gap = |seq| Event::Gap {
            source: SourceId(0),
            seq,
        };
        emitter.lossy(gap(1));
        emitter.lossy(gap(2));
        emitter.lossy(gap(3));
        emitter.lossy(gap(4));
        assert_eq!(emitter.dropped, 2);
        assert_eq!(rx.recv().await, Some(gap(1)));
        assert_eq!(rx.recv().await, Some(gap(2)));
        emitter.must(Event::Failed { error: "x".into() });
        assert_eq!(rx.recv().await, Some(Event::Lagged { dropped: 2 }));
        assert_eq!(rx.recv().await, Some(Event::Failed { error: "x".into() }));
    }
}
