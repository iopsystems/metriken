//! Serving `/metrics/stream`: the request, the response headers, and one
//! subscription's frames, independent of any HTTP framework or runtime.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use metriken_model::wire::{Layout, WIRE_NAMES};
use tracing::warn;

use super::clock;
use super::frames::{EncodedGroup, EncodedStreamGroup, FrameProducer, SchemaCache, StreamRow};
use crate::group_builder::{ExtraGroup, GroupBuilder, MemberNames, Positional, Router, Stamp};

/// What a pass is stamped with: the arguments [`FrameProducer::interval`]
/// takes, and the wall time the empty-frame rule compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassStamp {
    /// The wall clock when the pass read its values, nanoseconds since the
    /// Unix epoch. Two passes with equal `wall_ns` are the same pass.
    pub wall_ns: u64,
    /// The pass's timestamp on the producer's timeline ([`metriken::epoch`]).
    pub ts: i64,
    /// Wall clock minus `ts` at the pass.
    pub wall_offset: i64,
}

/// One pass of a producer's groups, in one layout.
pub trait Pass: Send + Sync + 'static {
    type Row: StreamRow;
    fn stamp(&self) -> PassStamp;
    fn rows(&self) -> &[Self::Row];
}

impl<R: StreamRow + Send + Sync + 'static> Pass for (PassStamp, Vec<R>) {
    type Row = R;

    fn stamp(&self) -> PassStamp {
        self.0
    }

    fn rows(&self) -> &[R] {
        &self.1
    }
}

impl<P: Pass> Pass for Arc<P> {
    type Row = P::Row;

    fn stamp(&self) -> PassStamp {
        (**self).stamp()
    }

    fn rows(&self) -> &[P::Row] {
        (**self).rows()
    }
}

/// A stream request the producer refuses: the HTTP status to answer with
/// and the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}

impl Refusal {
    fn bad_request(message: String) -> Self {
        Self {
            status: 400,
            message,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.status, self.message)
    }
}

impl std::error::Error for Refusal {}

/// `interval` and `layout` from a stream request's query string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamRequest {
    /// How often a frame is sent. Defaults to one second.
    pub interval: Duration,
    pub layout: Layout,
}

impl Default for StreamRequest {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            layout: Layout::Wide,
        }
    }
}

impl StreamRequest {
    /// Parses `interval` (a humantime duration such as `500ms`) and
    /// `layout` (`wide` or `long`) from a query string, with or without its
    /// leading `?`. Other parameters are ignored.
    ///
    /// Refuses with 400 a zero or malformed interval, an unknown layout, and
    /// either parameter given twice. An interval below the producer's update
    /// floor is accepted; most of its frames are empty.
    pub fn parse(query: &str) -> Result<Self, Refusal> {
        let query = query.strip_prefix('?').unwrap_or(query);
        let mut interval = None;
        let mut layout = None;
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let slot = match percent_decode(key).as_str() {
                "interval" => &mut interval,
                "layout" => &mut layout,
                _ => continue,
            };
            if slot.is_some() {
                return Err(Refusal::bad_request(format!(
                    "duplicate parameter {:?}",
                    percent_decode(key)
                )));
            }
            *slot = Some(percent_decode(value));
        }

        let interval = match interval {
            Some(s) => match s.parse::<humantime::Duration>() {
                Ok(d) if d.is_zero() => {
                    return Err(Refusal::bad_request(
                        "interval must be greater than zero".to_string(),
                    ))
                }
                Ok(d) => *d,
                Err(e) => return Err(Refusal::bad_request(format!("bad interval {s:?}: {e}"))),
            },
            None => Duration::from_secs(1),
        };
        let layout = match layout.as_deref() {
            None | Some("wide") => Layout::Wide,
            Some("long") => Layout::Long,
            Some(other) => {
                return Err(Refusal::bad_request(format!(
                    "bad layout {other:?}: expected \"wide\" or \"long\""
                )))
            }
        };
        Ok(Self { interval, layout })
    }
}

/// `%XX` escapes decoded and `+` as a space. An escape that is not two hex
/// digits, or bytes that are not UTF-8, are kept as they are.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// One subscription to a producer's stream: what it has been sent, and the
/// rules for each interval's frame.
///
/// The caller owns the timer and fetches each pass; this turns a pass (or
/// its absence) and the wall time the subscription woke at into the bytes to
/// send. Synchronous and free of I/O. With feature `stream-tokio`,
/// `into_stream` runs the timer and the fetch.
pub struct StreamSubscription {
    request: StreamRequest,
    update_floor: Duration,
    producer: FrameProducer,
    /// Per stream, the end of the last window sent. A pass carries every
    /// group the producer has, including ones not read again since the last
    /// pass, which keep their old window; sending one again would report a
    /// reading that did not happen.
    last_window: HashMap<String, u64>,
    /// The `wall_ns` of the last pass a frame was built from.
    last_sent_wall: Option<u64>,
    /// The `seq` of the last rows frame.
    last_index: Option<u64>,
}

impl StreamSubscription {
    /// A subscription to this process: the source uuid and clock anchor are
    /// [`metriken::epoch`]'s (see [`FrameProducer::new`]). `update_floor` is
    /// how often a pass can be new, reported in the update-floor header.
    pub fn new(
        request: StreamRequest,
        update_floor: Duration,
        labels: BTreeMap<String, String>,
        metadata: BTreeMap<String, String>,
    ) -> Self {
        Self::with_producer(request, update_floor, FrameProducer::new(labels, metadata))
    }

    /// A subscription whose handshake names `uuid` and `clock_anchor_wall_ns`
    /// and carries `labels` and `metadata` as given, for tests that compare
    /// bytes.
    #[doc(hidden)]
    pub fn for_source(
        request: StreamRequest,
        update_floor: Duration,
        uuid: String,
        clock_anchor_wall_ns: i64,
        labels: BTreeMap<String, String>,
        metadata: BTreeMap<String, String>,
    ) -> Self {
        Self::with_producer(
            request,
            update_floor,
            FrameProducer::for_source(uuid, clock_anchor_wall_ns, labels, metadata),
        )
    }

    fn with_producer(
        request: StreamRequest,
        update_floor: Duration,
        producer: FrameProducer,
    ) -> Self {
        Self {
            request,
            update_floor,
            producer,
            last_window: HashMap::new(),
            last_sent_wall: None,
            last_index: None,
        }
    }

    /// The request this subscription serves.
    pub fn request(&self) -> StreamRequest {
        self.request
    }

    /// `Content-Type` and the [`WIRE_NAMES`] headers, to send before the
    /// body. Durations are humantime (`1s`, `500ms`).
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("content-type", WIRE_NAMES.content_type.to_string()),
            (
                WIRE_NAMES.frame_interval,
                humantime::format_duration(self.request.interval).to_string(),
            ),
            (
                WIRE_NAMES.update_floor,
                humantime::format_duration(self.update_floor).to_string(),
            ),
            (WIRE_NAMES.layout, self.request.layout.as_str().to_string()),
        ]
    }

    /// The preamble and the handshake frame: dendro's preamble first, so a
    /// consumer can reject a stream it cannot read before decoding a frame.
    pub fn opening(&mut self) -> std::io::Result<Bytes> {
        let opening = self.producer.opening().map_err(std::io::Error::other)?;
        Ok(Bytes::from(opening))
    }

    /// The next multiple of the interval after `now`, counted from the Unix
    /// epoch. A `now` exactly on a boundary gives the next one. A `now`
    /// before the epoch gives `now` plus the interval.
    pub fn next_deadline(&self, now: SystemTime) -> SystemTime {
        let interval = self.request.interval;
        match now.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(since) => now + clock::align_wait(since.as_nanos(), interval),
            Err(_) => now + interval,
        }
    }

    /// The frame for one interval.
    ///
    /// `now` is the wall clock read when the subscription woke, before
    /// `pass` was fetched; `seq` is the index of the interval it falls in,
    /// held above the last `seq` sent when the wall clock stepped back, and
    /// a skipped index is logged. The frame carries the rows of `pass` whose
    /// window end differs from the last one sent on their stream, and every
    /// row with no window, stamped with the pass's own stamp. It is empty
    /// when `pass` has the same `wall_ns` as the last pass sent, or `pass`
    /// is `None`.
    pub fn frame_at<P: Pass>(
        &mut self,
        pass: Option<&P>,
        now: SystemTime,
    ) -> std::io::Result<Bytes> {
        let interval = self.request.interval;
        let woke_ns = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or_default();

        // The index of the boundary woken for, not of the pass's stamp: a
        // pass served from cache can predate the boundary, and `seq` has to
        // advance once per interval regardless.
        let measured = clock::interval_index(woke_ns, interval);
        let index = clock::monotonic_interval_index(woke_ns, interval, self.last_index);
        if index != measured {
            warn!(
                "wall clock moved backwards under a stream subscriber at {interval:?}; \
                 holding the interval index monotonic ({measured} -> {index})"
            );
        }
        if let Some(previous) = self.last_index {
            if index > previous + 1 {
                // Every interval gets a frame, so this is a lost reading
                // rather than a quiet one.
                warn!(
                    "stream subscriber at {interval:?} missed {} interval(s); \
                     the subscription was starved past its boundary",
                    index - previous - 1
                );
            }
        }
        self.last_index = Some(index);

        let frame = match pass {
            Some(pass) => {
                let stamp = pass.stamp();
                // The same pass as last time, reached when the interval is
                // shorter than the update floor: nothing in it can be new.
                let advanced = self.last_sent_wall != Some(stamp.wall_ns);
                self.last_sent_wall = Some(stamp.wall_ns);

                // Filtered before the producer sees the rows: it records a
                // schema as sent when it builds a row carrying it, so a row
                // dropped afterwards would leave its group referencing a
                // schema this subscriber never received.
                let last_window = &mut self.last_window;
                let rows = pass.rows().iter().filter(|row| {
                    // A windowless row carries no evidence of being new, so
                    // it is sent only with a pass that advanced.
                    if !advanced {
                        return false;
                    }
                    if let Some(end) = row.window().map(|w| w.end_ns) {
                        if last_window.get(row.stream()) == Some(&end) {
                            return false;
                        }
                        last_window.insert(row.stream().to_string(), end);
                    }
                    true
                });
                self.producer
                    .interval(rows, stamp.ts, stamp.wall_offset, index)
            }
            // The interval elapsed with no pass (none yet, or one that
            // failed): it still gets a frame, or the subscriber would read
            // the `seq` gap as a lost interval.
            None => {
                warn!(
                    "stream subscriber at {interval:?}: no snapshot to send for \
                     interval {index}; sending an empty frame"
                );
                self.producer.empty_interval(index)
            }
        };

        let mut body = Vec::new();
        dendro::replicate::wire::encode_frame(&frame, &mut body).map_err(std::io::Error::other)?;
        Ok(Bytes::from(body))
    }

    /// [`frame_at`](Self::frame_at) with no pass.
    pub fn empty_frame_at(&mut self, now: SystemTime) -> std::io::Result<Bytes> {
        self.frame_at::<(PassStamp, Vec<EncodedGroup>)>(None, now)
    }
}

/// A pass built by [`RegistryPasses`].
pub type RegistryPass = Arc<(PassStamp, Vec<EncodedStreamGroup>)>;

/// The extra groups [`RegistryPasses`] adds to each pass.
type ExtraFn = dyn Fn() -> Vec<ExtraGroup> + Send + Sync;

/// Passes over a [`GroupBuilder`], for a producer whose metrics are a
/// metriken registry.
///
/// A pass is built and encoded under a mutex and shared by every
/// subscription that asks within the freshness floor of it; one pass is kept
/// per layout. The wide layout is [`GroupBuilder::build_groups`], the long
/// [`GroupBuilder::build_stream`], each with the groups the extra callback
/// returns. A pass is stamped when it is built ([`Stamp::now`]).
pub struct RegistryPasses<R: Router, N: MemberNames = Positional> {
    state: Mutex<PassState<R, N>>,
    floor: Duration,
    extra: Box<ExtraFn>,
}

struct PassState<R: Router, N: MemberNames> {
    builder: GroupBuilder<R, N>,
    /// Per layout, the schema conversions and the last pass with the `now`
    /// it was built for.
    wide: LayoutState,
    long: LayoutState,
}

#[derive(Default)]
struct LayoutState {
    schemas: SchemaCache,
    last: Option<(SystemTime, RegistryPass)>,
}

impl<R: Router, N: MemberNames> RegistryPasses<R, N> {
    /// Passes over `builder`, each reused for `floor` after it is built.
    /// `floor` is what a subscription reports as its update floor.
    ///
    /// Choose a floor well below the shortest interval served (rezolus uses
    /// 10 ms under 1 s intervals). A pass is reused while a caller's clock is
    /// within the floor of the build, so with an interval equal to the floor,
    /// wake-time jitter can hand a subscriber the previous boundary's pass and
    /// it sends an empty frame for that interval.
    pub fn new(builder: GroupBuilder<R, N>, floor: Duration) -> Self {
        Self {
            state: Mutex::new(PassState {
                builder,
                wide: LayoutState::default(),
                long: LayoutState::default(),
            }),
            floor,
            extra: Box::new(Vec::new),
        }
    }

    /// Adds the groups `extra` returns to every pass, called once per build.
    /// It is called with the pass lock held, so it must not call `pass`.
    pub fn with_extra(
        mut self,
        extra: impl Fn() -> Vec<ExtraGroup> + Send + Sync + 'static,
    ) -> Self {
        self.extra = Box::new(extra);
        self
    }

    /// The freshness floor.
    pub fn floor(&self) -> Duration {
        self.floor
    }

    /// The pass for `layout` at `now`: the last one when `now` is within the
    /// floor of the `now` it was built for, on either side, otherwise a new
    /// one. Subscribers that wake at one boundary read slightly different
    /// clocks, so a `now` just before the cached one shares the pass. `None`
    /// when a group fails to encode, which is logged.
    pub fn pass(&self, layout: Layout, now: SystemTime) -> Option<RegistryPass> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let PassState {
            builder,
            wide,
            long,
        } = &mut *state;
        let cached = match layout {
            Layout::Wide => wide,
            Layout::Long => long,
        };
        if let Some((built, pass)) = &cached.last {
            let fresh = match now.duration_since(*built) {
                Ok(age) => age < self.floor,
                Err(e) => e.duration() < self.floor,
            };
            if fresh {
                return Some(Arc::clone(pass));
            }
        }

        let stamp = Stamp::now();
        let extra = (self.extra)();
        let encoded: Result<Vec<EncodedStreamGroup>, String> = match layout {
            Layout::Wide => builder
                .build_groups(extra)
                .iter()
                .map(|g| EncodedGroup::encode(g, &mut cached.schemas).map(EncodedStreamGroup::Wide))
                .collect(),
            Layout::Long => builder
                .build_stream(extra)
                .iter()
                .map(|g| EncodedStreamGroup::encode(g, &mut cached.schemas))
                .collect(),
        };
        let rows = match encoded {
            Ok(rows) => rows,
            Err(e) => {
                warn!("stream pass ({layout}) failed to encode: {e}");
                cached.last = None;
                return None;
            }
        };
        let names: HashSet<&str> = rows.iter().map(StreamRow::stream).collect();
        cached.schemas.retain(|name| names.contains(name));
        let wall_ns = (stamp.ts + stamp.wall_offset).max(0) as u64;
        let pass = Arc::new((
            PassStamp {
                wall_ns,
                ts: stamp.ts,
                wall_offset: stamp.wall_offset,
            },
            rows,
        ));
        cached.last = Some((now, Arc::clone(&pass)));
        Some(pass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_defaults_to_one_second_wide() {
        assert_eq!(StreamRequest::parse("").unwrap(), StreamRequest::default());
        assert_eq!(StreamRequest::parse("?").unwrap(), StreamRequest::default());
        assert_eq!(
            StreamRequest::parse("?other=1").unwrap(),
            StreamRequest::default()
        );
    }

    #[test]
    fn a_request_reads_interval_and_layout() {
        assert_eq!(
            StreamRequest::parse("?interval=500ms&layout=long").unwrap(),
            StreamRequest {
                interval: Duration::from_millis(500),
                layout: Layout::Long,
            }
        );
        assert_eq!(
            StreamRequest::parse("layout=wide&interval=1s+500ms").unwrap(),
            StreamRequest {
                interval: Duration::from_millis(1500),
                layout: Layout::Wide,
            }
        );
        assert_eq!(
            StreamRequest::parse("interval=2%20s").unwrap().interval,
            Duration::from_secs(2)
        );
    }

    #[test]
    fn a_request_refuses_what_it_cannot_serve() {
        for query in [
            "interval=0s",
            "interval=soon",
            "layout=diagonal",
            "interval=1s&interval=2s",
        ] {
            let refusal = StreamRequest::parse(query).unwrap_err();
            assert_eq!(refusal.status, 400, "{query}");
        }
        assert_eq!(
            StreamRequest::parse("interval=0s").unwrap_err().message,
            "interval must be greater than zero"
        );
        assert_eq!(
            StreamRequest::parse("layout=diagonal").unwrap_err().message,
            "bad layout \"diagonal\": expected \"wide\" or \"long\""
        );
    }

    #[test]
    fn headers_name_the_interval_floor_and_layout() {
        let sub = StreamSubscription::new(
            StreamRequest {
                interval: Duration::from_millis(500),
                layout: Layout::Long,
            },
            Duration::from_secs(1),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        assert_eq!(
            sub.headers(),
            vec![
                (
                    "content-type",
                    "application/vnd.metriken.replication.v1+dendro".to_string()
                ),
                ("x-metriken-frame-interval", "500ms".to_string()),
                ("x-metriken-update-floor", "1s".to_string()),
                ("x-metriken-layout", "long".to_string()),
            ]
        );
    }

    #[test]
    fn the_next_deadline_is_the_next_boundary() {
        let sub = StreamSubscription::new(
            StreamRequest::default(),
            Duration::from_secs(1),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        let at = |ns: u64| SystemTime::UNIX_EPOCH + Duration::from_nanos(ns);
        let base = 1_700_000_000_000_000_000u64;
        assert_eq!(sub.next_deadline(at(base + 1)), at(base + 1_000_000_000));
        assert_eq!(sub.next_deadline(at(base)), at(base + 1_000_000_000));
    }
}
