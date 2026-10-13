//! In-process producers for the recorder's tests: a metriken stream producer
//! served through `metriken-exposition`'s stream route, a Prometheus
//! exporter, and a stand-in for an agent too old to stream, each an axum
//! server on a free port whose behaviour a test changes while it runs.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use metriken_exposition::stream::{
    EncodedGroup, PassStamp, SchemaCache, StreamRequest, StreamSubscription,
};
use metriken_model::wire::{WireNames, WIRE_NAMES};
use metriken_model::{GroupSchema, GroupSnapshot, MetricDesc};
use metriken_recorder::{At, Enrich, Probe, StreamAnswer, Verdict};
use url::Url;

/// Names a pre-6.0 rezolus agent served its stream under.
pub const LEGACY_NAMES: WireNames = WireNames {
    content_type: "application/vnd.rezolus.replication.v1+dendro",
    frame_interval: "x-rezolus-frame-interval",
    update_floor: "x-rezolus-update-floor",
    layout: "x-rezolus-layout",
};

/// How the fake serves `/metrics/stream`.
#[derive(Clone, Debug)]
pub enum Streams {
    /// 404, as a producer without the route.
    Never,
    /// A stream on every subscription.
    Always,
    /// The first subscription gets this many frames and is closed; every
    /// later one gets this status.
    OnceThen(u64, u16),
    /// The first subscription gets this many frames and is closed; every
    /// later one streams under epoch `epoch-2` with `restarted_metadata`.
    Restarts(u64),
}

/// What a fake producer serves. Changed while it runs through
/// [`Fake::set`].
#[derive(Clone, Debug)]
pub struct Behaviour {
    pub streams: Streams,
    pub names: WireNames,
    /// The handshake's labels and metadata.
    pub labels: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
    /// The handshake's metadata after a restart.
    pub restarted_metadata: BTreeMap<String, String>,
    /// The body of `/`, e.g. `Rezolus 5.20.0 Agent`.
    pub banner: Option<String>,
    /// The body of `/metrics`.
    pub prometheus: Option<String>,
    /// 503 for everything.
    pub down: bool,
    /// How long `/metrics` takes to answer.
    pub delay: Duration,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            streams: Streams::Never,
            names: WIRE_NAMES,
            labels: BTreeMap::new(),
            metadata: BTreeMap::new(),
            restarted_metadata: BTreeMap::new(),
            banner: None,
            prometheus: None,
            down: false,
            delay: Duration::ZERO,
        }
    }
}

pub fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A producer serving `/metrics/stream` only, whose handshake carries
/// `labels` and `metadata`.
pub fn stream_only(labels: &[(&str, &str)], metadata: &[(&str, &str)]) -> Behaviour {
    Behaviour {
        streams: Streams::Always,
        labels: map(labels),
        metadata: map(metadata),
        ..Behaviour::default()
    }
}

/// A Prometheus exporter: `/metrics` and 404 elsewhere.
pub fn exporter(counter: &str) -> Behaviour {
    Behaviour {
        prometheus: Some(format!(
            "# HELP {counter} Requests served.\n# TYPE {counter} counter\n{counter} 42\n"
        )),
        ..Behaviour::default()
    }
}

/// An agent older than the stream: the `/` banner and 404 elsewhere.
pub fn old_agent(version: &str) -> Behaviour {
    Behaviour {
        banner: Some(format!("Rezolus {version} Agent\n")),
        ..Behaviour::default()
    }
}

pub struct Fake {
    pub url: Url,
    state: Arc<Mutex<Behaviour>>,
    /// Requests served, of any path.
    pub hits: Arc<AtomicU64>,
}

impl Fake {
    pub fn set(&self, behaviour: Behaviour) {
        *self.state.lock().unwrap() = behaviour;
    }
}

/// Serve `behaviour` on a free port.
pub async fn serve(behaviour: Behaviour) -> Fake {
    let state = Arc::new(Mutex::new(behaviour));
    let hits = Arc::new(AtomicU64::new(0));
    let subscriptions = Arc::new(AtomicU64::new(0));
    let router = {
        let (state, hits) = (state.clone(), hits.clone());
        axum::Router::new().fallback(move |uri: Uri| {
            let behaviour = state.lock().unwrap().clone();
            hits.fetch_add(1, Ordering::SeqCst);
            let subscriptions = subscriptions.clone();
            async move {
                if uri.path() == "/metrics" && !behaviour.delay.is_zero() {
                    tokio::time::sleep(behaviour.delay).await;
                }
                answer(behaviour, uri, &subscriptions)
            }
        })
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Fake {
        url: format!("http://{addr}/").parse().unwrap(),
        state,
        hits,
    }
}

/// A port nothing listens on.
pub fn unused_url() -> Url {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    format!("http://127.0.0.1:{port}/").parse().unwrap()
}

fn answer(b: Behaviour, uri: Uri, subscriptions: &AtomicU64) -> Response {
    if b.down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match uri.path() {
        "/metrics/stream" => {
            let n = subscriptions.fetch_add(1, Ordering::SeqCst);
            let (frames, epoch, metadata) = match &b.streams {
                Streams::Never => return StatusCode::NOT_FOUND.into_response(),
                Streams::Always => (None, "epoch-1", b.metadata.clone()),
                Streams::OnceThen(frames, _) if n == 0 => {
                    (Some(*frames), "epoch-1", b.metadata.clone())
                }
                Streams::OnceThen(_, status) => {
                    return StatusCode::from_u16(*status).unwrap().into_response()
                }
                Streams::Restarts(frames) if n == 0 => {
                    (Some(*frames), "epoch-1", b.metadata.clone())
                }
                Streams::Restarts(_) => (None, "epoch-2", b.restarted_metadata.clone()),
            };
            stream(&b, uri.query().unwrap_or(""), frames, epoch, metadata)
        }
        "/metrics" => match b.prometheus {
            Some(text) => {
                ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], text).into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        },
        "/" => match b.banner {
            Some(banner) => banner.into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn wall_ns() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// A subscription through the stream route: one counter in the `fake/ops`
/// group, read at each frame, under `b.names`.
fn stream(
    b: &Behaviour,
    query: &str,
    frames: Option<u64>,
    epoch: &str,
    metadata: BTreeMap<String, String>,
) -> Response {
    let request = match StreamRequest::parse(query) {
        Ok(r) => r,
        Err(refusal) => {
            return (
                StatusCode::from_u16(refusal.status).unwrap(),
                refusal.message,
            )
                .into_response()
        }
    };
    let subscription = StreamSubscription::for_source(
        request,
        Duration::from_millis(10),
        epoch.to_string(),
        wall_ns() as i64,
        b.labels.clone(),
        metadata,
    );
    // Rename the headers to the fake's wire names.
    let mut response = Response::builder().status(StatusCode::OK);
    for (name, value) in subscription.headers() {
        let name = match name {
            "content-type" => header::CONTENT_TYPE.as_str(),
            n if n == WIRE_NAMES.frame_interval => b.names.frame_interval,
            n if n == WIRE_NAMES.update_floor => b.names.update_floor,
            n if n == WIRE_NAMES.layout => b.names.layout,
            n => n,
        };
        let value = if name == header::CONTENT_TYPE.as_str() {
            b.names.content_type.to_string()
        } else {
            value
        };
        response = response.header(name, value);
    }

    let schema = Arc::new(GroupSchema {
        counters: vec![MetricDesc {
            name: "0x0".to_string(),
            metadata: [("metric".to_string(), "fake_ops".to_string())]
                .into_iter()
                .collect(),
        }],
        gauges: Vec::new(),
        histograms: Vec::new(),
    });
    let mut cache = SchemaCache::new();
    let mut count = 0u64;
    let fetch = move |now: SystemTime| {
        count += 1;
        let wall = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let group = GroupSnapshot {
            name: "fake/ops".to_string(),
            schema_hash: schema.hash(),
            schema: Some(schema.clone()),
            window: Some(metriken_types::Window::new(wall - 1_000_000, wall)),
            counters: vec![Some(count * 10)],
            gauges: Vec::new(),
            histograms: Vec::new(),
        };
        let encoded = EncodedGroup::encode(&group, &mut cache).unwrap();
        let pass = (
            PassStamp {
                wall_ns: wall,
                ts: wall as i64,
                wall_offset: 0,
            },
            vec![encoded],
        );
        async move { Some(pass) }
    };
    let body = subscription.into_stream(fetch, (), SystemTime::now);
    // The opening, then `frames` frames, then the connection closes.
    let body = match frames {
        Some(n) => body.take(n as usize + 1).boxed(),
        None => body.boxed(),
    };
    response.body(Body::from_stream(body)).unwrap()
}

/// A probe hook like rezolus's: a producer whose `/` is the banner
/// `Rezolus <version> Agent` is an agent, which is never scraped: a
/// transient answer is waited out and any other refuses it, naming its
/// version. Anything else that answered `/` is scraped; nothing answering is
/// pending. `enrich` names an agent `source=rezolus` with its `version`.
pub struct AgentProbe(pub reqwest::Client);

pub enum Identified {
    Agent(String),
    Other,
    Unanswered,
}

impl AgentProbe {
    pub async fn identify(&self, url: &Url) -> Identified {
        let mut root = url.clone();
        root.set_path("/");
        root.set_query(None);
        let Ok(resp) = self.0.get(root).send().await else {
            return Identified::Unanswered;
        };
        if StreamAnswer::Status(resp.status().as_u16()).is_transient() {
            return Identified::Unanswered;
        }
        let body = resp.text().await.unwrap_or_default();
        match body
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("Rezolus "))
            .and_then(|l| l.strip_suffix(" Agent"))
        {
            Some(version) => Identified::Agent(version.to_string()),
            None => Identified::Other,
        }
    }
}

#[async_trait::async_trait]
impl Probe for AgentProbe {
    async fn not_streamable(&self, url: &Url, at: At, answer: &StreamAnswer) -> Verdict {
        match self.identify(url).await {
            Identified::Agent(_) if answer.is_transient() => Verdict::Pending,
            Identified::Agent(version) => Verdict::Refuse {
                reason: format!(
                    "{url} is Rezolus {version}, which predates the replication stream: \
                     {answer} ({at:?}); record it with a 5.x recorder"
                ),
            },
            Identified::Other => Verdict::Scrape,
            Identified::Unanswered => Verdict::Pending,
        }
    }

    async fn enrich(&self, url: &Url, _epoch: Option<&str>) -> Enrich {
        match self.identify(url).await {
            Identified::Agent(version) => Enrich::Metadata {
                epoch: None,
                labels: map(&[("source", "rezolus")]),
                metadata: map(&[("version", &version)]),
            },
            Identified::Other => Enrich::Metadata {
                epoch: None,
                labels: BTreeMap::new(),
                metadata: BTreeMap::new(),
            },
            Identified::Unanswered => Enrich::Retry,
        }
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder().http1_only().build().unwrap()
}

/// One recording of an archive: labels, metadata, rows in `table`, and
/// whether it was finalized.
#[derive(Debug)]
pub struct Recording {
    pub labels: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
    pub rows: u64,
    pub complete: bool,
    pub first_ts: Option<u64>,
    pub last_ts: Option<u64>,
}

/// Every recording in the archive at `path`, keyed by `source`, with its
/// row count in `table`.
pub fn recordings(path: &Path, table: &str) -> BTreeMap<String, Recording> {
    use metriken_storage::Catalog;
    let catalog = metriken_storage::DendroCatalog::open(path).expect("the archive opens");
    catalog
        .sources()
        .expect("the catalog reads")
        .into_iter()
        .map(|s| {
            let sealed = catalog.segment_span(s.id, table).unwrap().1;
            let live = catalog.live_wal_span(s.id, table).unwrap();
            let first = [sealed.first_ts, live.first_ts]
                .into_iter()
                .filter(|_| sealed.rows + live.rows > 0)
                .flatten()
                .min();
            let last = sealed.last_ts.max(live.last_ts);
            let labels: BTreeMap<String, String> = s.labels.clone().into_iter().collect();
            let name = labels.get("source").cloned().unwrap_or_default();
            (
                name,
                Recording {
                    labels,
                    metadata: s.metadata.clone().into_iter().collect(),
                    rows: sealed.rows + live.rows,
                    complete: s.complete,
                    first_ts: first,
                    last_ts: last,
                },
            )
        })
        .collect()
}
