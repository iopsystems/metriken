//! The recorder against in-process producers: detection, identity,
//! orchestration and the archive it writes.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use common::*;
use metriken_recorder::*;

const INTERVAL: Duration = Duration::from_millis(100);

fn detect_cfg() -> DetectConfig {
    DetectConfig::new(INTERVAL, Duration::from_secs(2))
}

fn spec(url: &url::Url) -> EndpointSpec {
    EndpointSpec::new(url.clone())
}

fn named(url: &url::Url, source: &str) -> EndpointSpec {
    let mut spec = spec(url);
    spec.labels.insert("source".into(), source.into());
    spec
}

async fn detect_one(spec: EndpointSpec, probe: &dyn Probe) -> Detected {
    detect(&client(), &detect_cfg(), vec![spec], probe)
        .await
        .remove(0)
}

fn describe(d: &Detected) -> String {
    match d {
        Detected::Streamed { .. } => "a stream".into(),
        Detected::Scraped { url, .. } => format!("a scrape at {url}"),
        Detected::Refused { reason, .. } => format!("a refusal: {reason}"),
        Detected::Pending { reason, .. } => format!("pending: {reason}"),
    }
}

/// A recording run to its end: what `finish` returned, every event, and
/// the archive's path (inside `_dir`).
struct Run {
    summary: Result<Summary, Error>,
    events: Vec<Event>,
    path: PathBuf,
    _dir: tempfile::TempDir,
}

async fn record(
    detected: Vec<Detected>,
    probe: Arc<dyn Probe>,
    local: Vec<Box<dyn LocalSource>>,
    configure: impl FnOnce(&mut RecorderConfig),
    run_for: Duration,
    stop: Stop,
) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.dendro");
    let mut cfg = RecorderConfig::new(&path, INTERVAL);
    configure(&mut cfg);
    let (recorder, _ids, mut rx) = Recorder::start(cfg, client(), probe, detected, local)
        .await
        .expect("the recording starts");
    let collect = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        events
    });
    tokio::time::sleep(run_for).await;
    let summary = recorder.finish(stop, CancellationToken::new()).await;
    let events = collect.await.unwrap();
    Run {
        summary,
        events,
        path,
        _dir: dir,
    }
}

// ── detection (GO check 3) ─────────────────────────────────────────────

/// A producer serving `/metrics/stream` and nothing else is streamed, with
/// its handshake's labels and metadata.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_only_producer_is_streamed() {
    let fake = serve(stream_only(
        &[("source", "cachecannon")],
        &[("version", "0.3.0")],
    ))
    .await;
    match detect_one(spec(&fake.url), &NoProbe).await {
        Detected::Streamed { subscription, .. } => {
            let handshake = subscription.handshake();
            assert_eq!(handshake.labels["source"], "cachecannon");
            assert_eq!(handshake.metadata["version"], "0.3.0");
            assert_eq!(handshake.epoch(), Some("epoch-1"));
        }
        other => panic!("expected a stream, got {}", describe(&other)),
    }
}

/// A Prometheus exporter given by its root URL is scraped at `/metrics`.
#[tokio::test(flavor = "multi_thread")]
async fn a_prometheus_endpoint_at_the_root_is_scraped_at_metrics() {
    let fake = serve(exporter("http_requests_total")).await;
    for probe in [&NoProbe as &dyn Probe, &AgentProbe(client())] {
        match detect_one(spec(&fake.url), probe).await {
            Detected::Scraped { url, .. } => assert_eq!(url.path(), "/metrics"),
            other => panic!("expected a scrape, got {}", describe(&other)),
        }
    }
}

/// An agent too old to stream is refused at startup by the probe hook,
/// never scraped. With `NoProbe` the same producer is only a 404 and its
/// `/metrics` 404s too, so it is pending, not scraped.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_without_a_stream_is_refused_at_startup() {
    let fake = serve(old_agent("5.20.0")).await;
    match detect_one(spec(&fake.url), &AgentProbe(client())).await {
        Detected::Refused { reason, .. } => {
            for needle in [
                "Rezolus 5.20.0",
                "predates",
                "HTTP 404",
                "Startup",
                "5.x recorder",
            ] {
                assert!(reason.contains(needle), "{needle:?} in {reason}");
            }
        }
        other => panic!("expected a refusal, got {}", describe(&other)),
    }
    match detect_one(spec(&fake.url), &NoProbe).await {
        Detected::Pending { reason, .. } => {
            assert!(reason.contains("/metrics answered HTTP 404"), "{reason}")
        }
        other => panic!("expected pending, got {}", describe(&other)),
    }
}

/// 503 to everything is pending (a transient answer), whatever the hook.
#[tokio::test(flavor = "multi_thread")]
async fn an_endpoint_answering_5xx_is_pending() {
    let fake = serve(Behaviour {
        down: true,
        ..stream_only(&[], &[])
    })
    .await;
    for probe in [&NoProbe as &dyn Probe, &AgentProbe(client())] {
        match detect_one(spec(&fake.url), probe).await {
            Detected::Pending { reason, .. } => {
                assert!(reason.contains("HTTP 503"), "{reason}");
                assert!(reason.contains("will retry each tick"), "{reason}");
            }
            other => panic!("expected pending, got {}", describe(&other)),
        }
    }
    let unreachable = unused_url();
    assert!(matches!(
        detect_one(spec(&unreachable), &NoProbe).await,
        Detected::Pending { .. }
    ));
}

/// `Transport::Stream` is never scraped: a Prometheus exporter declared a
/// stream is refused. `Transport::Prometheus` is never asked for a stream:
/// a stream-only producer declared Prometheus is pending on its 404.
#[tokio::test(flavor = "multi_thread")]
async fn a_declared_transport_overrides_detection() {
    let prom = serve(exporter("up")).await;
    let mut declared = spec(&prom.url);
    declared.transport = Transport::Stream;
    match detect_one(declared, &NoProbe).await {
        Detected::Refused { reason, .. } => assert!(reason.contains("protocol=stream"), "{reason}"),
        other => panic!("expected a refusal, got {}", describe(&other)),
    }

    let producer = serve(stream_only(&[], &[])).await;
    let mut declared = spec(&producer.url);
    declared.transport = Transport::Prometheus;
    match detect_one(declared, &NoProbe).await {
        Detected::Pending { reason, .. } => assert!(reason.contains("HTTP 404"), "{reason}"),
        other => panic!("expected pending, got {}", describe(&other)),
    }

    // Declared a stream, answering 503: waited out, not refused.
    let down = serve(Behaviour {
        down: true,
        ..Behaviour::default()
    })
    .await;
    let mut declared = spec(&down.url);
    declared.transport = Transport::Stream;
    assert!(matches!(
        detect_one(declared, &NoProbe).await,
        Detected::Pending { .. }
    ));
}

/// A stream served under names the caller did not accept is a foreign
/// content type; accepted, it is streamed.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_under_legacy_names_is_read_when_the_caller_accepts_them() {
    let fake = serve(Behaviour {
        names: LEGACY_NAMES,
        ..stream_only(&[("source", "rezolus")], &[])
    })
    .await;
    // A foreign content type is the producer saying what it is: `NoProbe`
    // scrapes it, and its `/metrics` 404s.
    match detect_one(spec(&fake.url), &NoProbe).await {
        Detected::Pending { reason, .. } => {
            assert!(reason.contains("/metrics answered HTTP 404"), "{reason}")
        }
        other => panic!("expected pending, got {}", describe(&other)),
    }
    // Declared a stream, the same answer refuses it, quoting the type.
    let mut declared = spec(&fake.url);
    declared.transport = Transport::Stream;
    match detect_one(declared, &NoProbe).await {
        Detected::Refused { reason, .. } => assert!(
            reason.contains("not a replication stream this recorder reads"),
            "{reason}"
        ),
        other => panic!("expected a refusal, got {}", describe(&other)),
    }
    let mut cfg = detect_cfg();
    cfg.wire_names = vec![WIRE_NAMES, LEGACY_NAMES];
    let detected = detect(&client(), &cfg, vec![spec(&fake.url)], &NoProbe).await;
    assert!(
        matches!(detected[0], Detected::Streamed { .. }),
        "{}",
        describe(&detected[0])
    );
}

// ── recording ──────────────────────────────────────────────────────────

/// A producer that does not negotiate serves the offer: the served
/// interval is the offer, its handshake carries no stream keys, and the
/// recording's interval is the offer.
#[tokio::test(flavor = "multi_thread")]
async fn a_producer_that_does_not_negotiate_serves_the_offer() {
    let producer = serve(Behaviour {
        update_floor: Duration::from_millis(300),
        negotiate: false,
        ..stream_only(&[("source", "producer")], &[])
    })
    .await;
    let sub = Subscription::connect(
        &client(),
        &producer.url,
        INTERVAL,
        Layout::Long,
        &[WIRE_NAMES],
    )
    .await
    .expect("subscribed");
    assert_eq!(sub.served_interval(), Some(INTERVAL));
    assert!(!sub
        .handshake()
        .metadata
        .keys()
        .any(|k| k.starts_with("metriken.")));
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![spec(&producer.url)],
        &NoProbe,
    )
    .await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(1000),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    let r = &recordings(&run.path, "fake/ops")["producer"];
    assert_eq!(r.metadata["sampling_interval_ms"], "100");
}

/// Without the frame-interval header (a proxy dropped it), the served
/// interval comes from the handshake.
#[tokio::test(flavor = "multi_thread")]
async fn the_served_interval_is_read_from_the_handshake_without_the_header() {
    let producer = serve(Behaviour {
        update_floor: Duration::from_millis(300),
        drop_frame_interval: true,
        ..stream_only(&[("source", "producer")], &[])
    })
    .await;
    let sub = Subscription::connect(
        &client(),
        &producer.url,
        Duration::from_millis(100),
        Layout::Long,
        &[WIRE_NAMES],
    )
    .await
    .expect("subscribed");
    assert_eq!(sub.served_interval(), Some(Duration::from_millis(300)));
    assert_eq!(
        sub.handshake().served_interval,
        Some(Duration::from_millis(300))
    );
}

/// The wait for a frame after a wrapped command exits lasts at least the
/// interval the producer serves plus the timeout, as the producer said on
/// its latest connection: after a reconnect at a 2.5 s floor, a 200 ms bound
/// still gets the frame stamped after the exit.
#[tokio::test(flavor = "multi_thread")]
async fn the_stop_waits_for_a_frame_at_the_served_interval() {
    let producer = serve(Behaviour {
        streams: Streams::Restarts(2),
        update_floor: Duration::from_millis(100),
        ..stream_only(&[("source", "producer")], &[])
    })
    .await;
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![spec(&producer.url)],
        &NoProbe,
    )
    .await;
    // The first connection closes after two frames; the reconnect is served
    // at 2.5 s.
    producer.set(Behaviour {
        streams: Streams::Restarts(2),
        update_floor: Duration::from_millis(2500),
        ..stream_only(&[("source", "producer")], &[])
    });
    // The exit falls 100 ms after a 2.5 s boundary, so the next frame is
    // 2.4 s away: further than the first connection's 100 ms interval plus
    // the 1 s timeout.
    let now_ns = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let step = 2_500_000_000u64;
    let exit_ns = (now_ns + 4_000_000_000).div_ceil(step) * step + 100_000_000;
    let run_for = Duration::from_nanos(exit_ns - now_ns);
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |cfg| cfg.timeout = Duration::from_millis(1000),
        run_for,
        Stop::AfterExit {
            wall_ns: exit_ns,
            bound: Duration::from_millis(200),
        },
    )
    .await;
    run.summary.expect("finished");
    let queries = producer.stream_queries.lock().unwrap().clone();
    assert!(queries.len() >= 2, "{queries:?}");
    let r = &recordings(&run.path, "fake/ops")["producer"];
    assert!(
        r.last_ts.unwrap() >= exit_ns,
        "the last row ({:?}) is before the exit ({exit_ns})",
        r.last_ts
    );
}

/// A stream is dead only after the interval its producer serves plus the
/// timeout: a 2.5 s floor against a 100 ms offer and a 2 s timeout keeps one
/// connection, where the offer's 2.1 s would drop it between every frame.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_is_held_for_its_served_interval() {
    let producer = serve(Behaviour {
        update_floor: Duration::from_millis(2500),
        ..stream_only(&[("source", "producer")], &[])
    })
    .await;
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![spec(&producer.url)],
        &NoProbe,
    )
    .await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(7000),
        Stop::Grace(Duration::from_millis(100)),
    )
    .await;
    run.summary.expect("finished");
    let queries = producer.stream_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 1, "reconnected: {queries:?}");
}

/// A producer serves at least its update floor: an offer of 100 ms to a
/// producer with a 300 ms floor is served at 300 ms, the recording's
/// interval is 300 ms, and a reconnect offers the floor. The stream's own
/// handshake keys stay out of the recording's metadata.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_is_recorded_at_the_interval_its_producer_serves() {
    let producer = serve(Behaviour {
        streams: Streams::Restarts(3),
        update_floor: Duration::from_millis(300),
        ..stream_only(&[("source", "producer")], &[("version", "1.0")])
    })
    .await;
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![spec(&producer.url)],
        &NoProbe,
    )
    .await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(3000),
        Stop::Grace(Duration::from_millis(400)),
    )
    .await;
    run.summary.expect("finished");
    let queries = producer.stream_queries.lock().unwrap().clone();
    assert!(queries.len() >= 2, "{queries:?}");
    assert!(queries[0].contains("interval=100ms"), "{queries:?}");
    assert!(
        queries[1..].iter().all(|q| q.contains("interval=300ms")),
        "a reconnect offers the floor: {queries:?}"
    );
    let r = &recordings(&run.path, "fake/ops")["producer"];
    assert_eq!(r.metadata["sampling_interval_ms"], "300");
    assert_eq!(r.metadata["version"], "1.0");
    assert!(
        !r.metadata
            .keys()
            .any(|k| k.starts_with("metriken.") && !k.starts_with("metriken.layer.")),
        "the stream's own keys are not stored: {:?}",
        r.metadata
    );
}

/// A streamed producer and a Prometheus exporter in one archive: one
/// recording each, rows in each, both finalized, and a `Committed` event
/// for each.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_and_a_scrape_are_recorded_into_one_archive() {
    let producer = serve(stream_only(&[("source", "producer")], &[])).await;
    let exporter = serve(exporter("http_requests_total")).await;
    let mut prom = spec(&exporter.url);
    prom.url.set_path("/metrics");
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![spec(&producer.url), named(&prom.url, "svc")],
        &NoProbe,
    )
    .await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(1200),
        Stop::Grace(INTERVAL),
    )
    .await;
    let summary = run.summary.expect("finished");
    assert!(!summary.discarded);
    assert!(summary
        .sources
        .iter()
        .all(|s| s.state == SourceState::Active));

    let streamed = recordings(&run.path, "fake/ops");
    let scraped = recordings(&run.path, PROMETHEUS_GROUP);
    assert_eq!(streamed.len(), 2, "{streamed:?}");
    assert!(streamed["producer"].rows >= 5, "{:?}", streamed["producer"]);
    assert!(scraped["svc"].rows >= 5, "{:?}", scraped["svc"]);
    assert!(streamed.values().all(|r| r.complete), "finalized");
    // The scrape's HELP text is the recording's descriptions.
    let descriptions: BTreeMap<String, String> =
        serde_json::from_str(&scraped["svc"].metadata["descriptions"]).unwrap();
    assert_eq!(descriptions["http_requests_total"], "Requests served.");
    assert_eq!(scraped["svc"].metadata["sampling_interval_ms"], "100");
    assert_eq!(scraped["svc"].metadata["transport"], "prometheus");
    let layers = metriken_storage::layers::Layers::read(&scraped["svc"].metadata)
        .unwrap()
        .unwrap();
    assert!(
        layers.recorder.metadata.contains_key("descriptions"),
        "the HELP text is a recorder entry"
    );
    assert_eq!(scraped["svc"].labels["instance"], prom.url.as_str());

    for id in [SourceId(0), SourceId(1)] {
        assert!(
            run.events
                .iter()
                .any(|e| matches!(e, Event::Committed { source, .. } if *source == id)),
            "{id}: {:?}",
            run.events
        );
    }
    assert!(run.events.iter().any(|e| matches!(
        e,
        Event::Active {
            transport: ActiveTransport::Stream,
            ..
        }
    )));
}

/// GO check 4: a streamed producer's `source` and `version` are its
/// handshake's when the caller names neither.
#[tokio::test(flavor = "multi_thread")]
async fn a_producers_handshake_names_its_recording() {
    let fake = serve(stream_only(
        &[("source", "cachecannon")],
        &[("version", "0.3.0"), ("build", "abc")],
    ))
    .await;
    let detected = detect(&client(), &detect_cfg(), vec![spec(&fake.url)], &NoProbe).await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(600),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    let recordings = recordings(&run.path, "fake/ops");
    let r = &recordings["cachecannon"];
    assert_eq!(r.labels["source"], "cachecannon");
    assert_eq!(r.labels["instance"], fake.url.as_str());
    assert!(
        !r.metadata.contains_key("source"),
        "the recorder writes no source of its own"
    );
    assert_eq!(r.metadata["version"], "0.3.0");
    assert_eq!(r.metadata["build"], "abc");
    assert_eq!(r.metadata["transport"], "stream");
    assert_eq!(
        recorder_epochs(&r.metadata),
        vec![("epoch-1".to_string(), None)]
    );
    assert!(!r.metadata.contains_key("producer_epoch"));
    assert!(run.events.iter().any(|e| matches!(
        e,
        Event::Active { name, inferred: false, .. } if name == "cachecannon"
    )));
}

/// A producer identity declared in the handshake's `metriken.producer.*`
/// keys is written as `producer_name`, `version` and the `host` label; the
/// `metriken.` keys themselves are not stored.
#[tokio::test(flavor = "multi_thread")]
async fn a_handshakes_producer_identity_is_written_as_producer_keys() {
    let fake = serve(stream_only(
        &[("source", "svc")],
        &[
            ("metriken.producer.name", "cachecannon"),
            ("metriken.producer.version", "0.4.0"),
            ("metriken.producer.host", "lg-1"),
        ],
    ))
    .await;
    let detected = detect(&client(), &detect_cfg(), vec![spec(&fake.url)], &NoProbe).await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(600),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    let r = &recordings(&run.path, "fake/ops")["svc"];
    assert_eq!(r.metadata["producer_name"], "cachecannon");
    assert_eq!(r.metadata["version"], "0.4.0");
    assert_eq!(r.labels["host"], "lg-1");
    assert!(
        !r.metadata
            .keys()
            .any(|k| k.starts_with("metriken.producer.")),
        "{:?}",
        r.metadata
    );
}

/// An endpoint nobody names is recorded with no `source`: its labels are
/// the recorder's `instance`. It is shown under the name inferred from its
/// URL.
#[tokio::test(flavor = "multi_thread")]
async fn an_unnamed_scrape_is_recorded_without_a_source() {
    let exporter = serve(exporter("http_requests_total")).await;
    let mut prom = spec(&exporter.url);
    prom.url.set_path("/metrics");
    let detected = detect(&client(), &detect_cfg(), vec![prom.clone()], &NoProbe).await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(600),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    let scraped = recordings(&run.path, PROMETHEUS_GROUP);
    let r = &scraped[""];
    assert_eq!(
        r.labels,
        BTreeMap::from([("instance".to_string(), prom.url.to_string())])
    );
    assert!(!r.metadata.contains_key("source"));
    assert!(run
        .events
        .iter()
        .any(|e| matches!(e, Event::Active { inferred: true, .. })));
}

/// The caller's labels and metadata, the user layer, outrank the
/// handshake's and the probe hook's.
#[tokio::test(flavor = "multi_thread")]
async fn the_callers_identity_outranks_the_hooks_and_the_handshakes() {
    let fake = serve(Behaviour {
        banner: Some("Rezolus 6.0.0 Agent\n".into()),
        ..stream_only(&[("zone", "a")], &[("version", "handshake")])
    })
    .await;
    let mut endpoint = named(&fake.url, "mine");
    endpoint.labels.insert("arm".into(), "baseline".into());
    endpoint.metadata.insert("version".into(), "pinned".into());
    let probe = Arc::new(AgentProbe(client()));
    let detected = detect(&client(), &detect_cfg(), vec![endpoint], probe.as_ref()).await;
    let run = record(
        detected,
        probe,
        Vec::new(),
        |_| {},
        Duration::from_millis(500),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    let r = &recordings(&run.path, "fake/ops")["mine"];
    assert_eq!(r.labels["source"], "mine", "the caller's over the hook's");
    assert_eq!(r.labels["arm"], "baseline");
    assert_eq!(r.labels["zone"], "a", "the handshake's");
    assert_eq!(r.metadata["version"], "pinned");
}

/// The handshake's metadata, the producer layer, outranks the probe hook's,
/// the recorder layer; the hook's `source` label is not written.
#[tokio::test(flavor = "multi_thread")]
async fn the_handshakes_identity_outranks_the_hooks() {
    let fake = serve(Behaviour {
        banner: Some("Rezolus 6.0.0 Agent\n".into()),
        ..stream_only(&[("zone", "a")], &[("version", "handshake")])
    })
    .await;
    let probe = Arc::new(AgentProbe(client()));
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![spec(&fake.url)],
        probe.as_ref(),
    )
    .await;
    let run = record(
        detected,
        probe,
        Vec::new(),
        |_| {},
        Duration::from_millis(500),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    let recordings = recordings(&run.path, "fake/ops");
    let r = &recordings[""];
    assert_eq!(r.metadata["version"], "handshake");
    assert!(!r.labels.contains_key("source"), "{:?}", r.labels);
    assert!(
        run.events.iter().any(|e| matches!(
            e,
            Event::Active { name, inferred: false, .. } if name == "rezolus"
        )),
        "shown under the hook's source: {:?}",
        run.events
    );
    let layers = metriken_storage::layers::Layers::read(&r.metadata)
        .unwrap()
        .unwrap();
    assert_eq!(
        layers.recorder.metadata["version"], "6.0.0",
        "the hook's, below"
    );
}

/// GO check 3, mid-run: an agent too old to stream that comes up after the
/// recording started is refused on activation; the exporter beside it is
/// recorded and the archive finalized.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_agent_without_a_stream_is_refused_and_the_rest_finalize() {
    let exporter = serve(exporter("up_total")).await;
    let agent = serve(Behaviour {
        down: true,
        ..old_agent("5.20.0")
    })
    .await;
    let probe = Arc::new(AgentProbe(client()));
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![named(&exporter.url, "svc"), named(&agent.url, "old")],
        probe.as_ref(),
    )
    .await;
    assert!(matches!(detected[1], Detected::Pending { .. }));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late.dendro");
    let (recorder, ids, mut rx) = Recorder::start(
        RecorderConfig::new(&path, INTERVAL),
        client(),
        probe,
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    agent.set(old_agent("5.20.0"));
    let refused = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Some(Event::Refused { source, reason }) => break (source, reason),
                Some(_) => {}
                None => panic!("the channel closed before a refusal"),
            }
        }
    })
    .await
    .expect("a refusal within 5 s");
    assert_eq!(refused.0, ids[1]);
    assert!(refused.1.contains("Activation"), "{}", refused.1);
    drop(rx);
    let summary = recorder
        .finish(Stop::Grace(Duration::ZERO), CancellationToken::new())
        .await
        .expect("finished");
    assert_eq!(summary.sources[1].state, SourceState::Refused);
    assert_eq!(summary.sources[0].state, SourceState::Active);
    let scraped = recordings(&path, PROMETHEUS_GROUP);
    assert_eq!(scraped.len(), 1, "no recording for the refused agent");
    assert!(scraped["svc"].complete && scraped["svc"].rows > 0);
}

/// A streamed source whose reconnect is answered with a 404 is refused on
/// its own: its rows stay, the hook words the refusal, and the rest
/// finalize.
#[tokio::test(flavor = "multi_thread")]
async fn a_source_refused_on_reconnect_keeps_its_rows() {
    let agent = serve(Behaviour {
        streams: Streams::OnceThen(3, 404),
        banner: Some("Rezolus 6.0.0 Agent\n".into()),
        ..Behaviour::default()
    })
    .await;
    let exporter = serve(exporter("up_total")).await;
    let probe = Arc::new(AgentProbe(client()));
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![named(&agent.url, "agent"), named(&exporter.url, "svc")],
        probe.as_ref(),
    )
    .await;
    let run = record(
        detected,
        probe,
        Vec::new(),
        |_| {},
        Duration::from_millis(2500),
        Stop::Grace(Duration::ZERO),
    )
    .await;
    let summary = run.summary.expect("finished");
    assert_eq!(summary.sources[0].state, SourceState::Refused);
    let reason = run
        .events
        .iter()
        .find_map(|e| match e {
            Event::Refused { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("a refusal event");
    assert!(reason.contains("Reconnect"), "the hook's wording: {reason}");
    assert!(run
        .events
        .iter()
        .any(|e| matches!(e, Event::Dropped { .. })));
    let streamed = recordings(&run.path, "fake/ops");
    assert!(
        (1..=3).contains(&streamed["agent"].rows),
        "the rows it streamed: {:?}",
        streamed["agent"]
    );
    assert!(streamed["agent"].complete);
}

/// A producer restart, seen as a new epoch on the reconnect's handshake, is
/// written into the recording's metadata: the epoch history with each
/// process's version, and the new process's metadata. A key the caller
/// patched is kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_producer_restart_is_written_into_the_recording() {
    let fake = serve(Behaviour {
        streams: Streams::Restarts(3),
        restarted_metadata: map(&[
            ("version", "1.1.0"),
            ("note", "after"),
            ("pinned", "producer"),
        ]),
        restarted_labels: Some(map(&[("source", "svc"), ("zone", "b")])),
        ..stream_only(
            &[("source", "svc")],
            &[("version", "1.0.0"), ("note", "before")],
        )
    })
    .await;
    let detected = detect(&client(), &detect_cfg(), vec![spec(&fake.url)], &NoProbe).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restart.dendro");
    let (recorder, ids, mut rx) = Recorder::start(
        RecorderConfig::new(&path, INTERVAL),
        client(),
        Arc::new(NoProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    recorder
        .patch_metadata(ids[0], map(&[("pinned", "caller")]))
        .await
        .unwrap();
    let restarted = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            match rx.recv().await {
                Some(Event::Restarted { epoch, version, .. }) => break (epoch, version),
                Some(_) => {}
                None => panic!("closed"),
            }
        }
    })
    .await
    .expect("the restart is written within 6 s");
    assert_eq!(
        restarted,
        ("epoch-2".to_string(), Some("1.1.0".to_string()))
    );
    drop(rx);
    recorder
        .finish(Stop::Grace(INTERVAL), CancellationToken::new())
        .await
        .unwrap();

    let r = &recordings(&path, "fake/ops")["svc"];
    let epochs = recorder_epochs(&r.metadata);
    assert_eq!(epochs.len(), 2, "{epochs:?}");
    assert_eq!(epochs[0], ("epoch-1".to_string(), None));
    assert_eq!(epochs[1].0, "epoch-2");
    assert!(
        epochs[1].1.is_some(),
        "the restart's epoch starts at its first row"
    );
    assert_eq!(
        producer_versions(&r.metadata),
        vec![
            (Some("epoch-1".to_string()), Some("1.0.0".to_string())),
            (Some("epoch-2".to_string()), Some("1.1.0".to_string())),
        ]
    );
    assert!(!r.metadata.contains_key("producer_epoch"));
    assert!(!r.metadata.contains_key("producer_epochs"));
    let layers = metriken_storage::layers::Layers::read(&r.metadata)
        .unwrap()
        .unwrap();
    let restarted = layers
        .producer
        .entries
        .iter()
        .find(|e| e.epoch.as_deref() == Some("epoch-2"))
        .unwrap();
    assert_eq!(
        restarted.from_ts, epochs[1].1,
        "from the restart's first row"
    );
    assert_eq!(r.labels["zone"], "b", "the restarted handshake's labels");
    assert_eq!(r.metadata["version"], "1.1.0");
    assert_eq!(r.metadata["note"], "after");
    assert_eq!(
        r.metadata["pinned"], "caller",
        "a patched key is the caller's"
    );
}

/// A pending endpoint becomes a recording once it streams, beside the
/// endpoint that was up from the start.
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_endpoint_is_added_once_it_streams() {
    let exporter = serve(exporter("up_total")).await;
    let late = serve(Behaviour {
        down: true,
        ..stream_only(&[], &[])
    })
    .await;
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![named(&exporter.url, "svc"), named(&late.url, "late")],
        &NoProbe,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late.dendro");
    let (recorder, ids, mut rx) = Recorder::start(
        RecorderConfig::new(&path, INTERVAL),
        client(),
        Arc::new(NoProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    let pending = rx.recv().await;
    assert!(
        matches!(
            pending,
            Some(Event::Active { .. }) | Some(Event::Pending { .. })
        ),
        "{pending:?}"
    );
    late.set(stream_only(&[], &[]));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Some(Event::Active {
                    source, transport, ..
                }) if source == ids[1] => {
                    assert_eq!(transport, ActiveTransport::Stream);
                    break;
                }
                Some(_) => {}
                None => panic!("closed"),
            }
        }
    })
    .await
    .expect("activated within 5 s");
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(rx);
    recorder
        .finish(Stop::Grace(INTERVAL), CancellationToken::new())
        .await
        .unwrap();
    let streamed = recordings(&path, "fake/ops");
    assert!(streamed["late"].rows > 0 && streamed["late"].complete);
}

/// A pending endpoint that accepts connections and never answers does not
/// slow the ticks of the endpoint beside it: detection runs off the tick.
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_endpoint_that_never_answers_does_not_stall_the_ticks() {
    let exporter = serve(exporter("up_total")).await;
    // Accepts, never answers.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let silent_url: url::Url = format!("http://{}/", silent.local_addr().unwrap())
        .parse()
        .unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in silent.incoming() {
            held.push(s);
        }
    });
    let detected = vec![
        Detected::Scraped {
            spec: named(&exporter.url, "svc"),
            url: {
                let mut u = exporter.url.clone();
                u.set_path("/metrics");
                u
            },
        },
        Detected::Pending {
            spec: named(&silent_url, "stuck"),
            reason: "not yet".into(),
        },
    ];
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |cfg| cfg.timeout = Duration::from_secs(2),
        Duration::from_millis(2000),
        Stop::Grace(Duration::ZERO),
    )
    .await;
    let summary = run.summary.unwrap();
    assert_eq!(summary.sources[1].state, SourceState::Pending);
    let rows = recordings(&run.path, PROMETHEUS_GROUP)["svc"].rows;
    // 2 s at 100 ms is 20 ticks; one waiting on detection gets one per 4 s.
    assert!(rows >= 12, "scraped every tick: {rows} rows");
}

/// `Stop::AfterExit` waits for each streamed source's frame stamped at or
/// after the exit, so the interval the exit landed in is recorded.
#[tokio::test(flavor = "multi_thread")]
async fn after_exit_waits_for_a_frame_stamped_after_the_exit() {
    let fake = serve(stream_only(&[("source", "svc")], &[])).await;
    let detected = detect(&client(), &detect_cfg(), vec![spec(&fake.url)], &NoProbe).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("exit.dendro");
    // A one-second interval: the exit lands well before the next frame.
    let interval = Duration::from_secs(1);
    let mut cfg = RecorderConfig::new(&path, interval);
    cfg.timeout = Duration::from_secs(3);
    let (recorder, _ids, rx) =
        Recorder::start(cfg, client(), Arc::new(NoProbe), detected, Vec::new())
            .await
            .unwrap();
    drop(rx);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let exit = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    recorder
        .finish(
            Stop::AfterExit {
                wall_ns: exit,
                bound: Duration::from_secs(3),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let r = &recordings(&path, "fake/ops")["svc"];
    let last = r.last_ts.expect("rows");
    assert!(
        last >= exit,
        "the last row ({last}) is at or after the exit ({exit})"
    );
}

/// Cancelling `finish` ends the after-exit wait at once and still
/// finalizes.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_finish_ends_the_wait_and_finalizes() {
    let fake = serve(stream_only(&[("source", "svc")], &[])).await;
    let detected = detect(&client(), &detect_cfg(), vec![spec(&fake.url)], &NoProbe).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cancel.dendro");
    let (recorder, _ids, rx) = Recorder::start(
        RecorderConfig::new(&path, INTERVAL),
        client(),
        Arc::new(NoProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    drop(rx);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let started = std::time::Instant::now();
    recorder
        .finish(
            Stop::AfterExit {
                wall_ns: u64::MAX,
                bound: Duration::from_secs(30),
            },
            cancel,
        )
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(recordings(&path, "fake/ops")["svc"].complete);
}

/// No source delivered anything: the archive is removed and the summary
/// says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_recording_with_nothing_delivered_is_discarded() {
    let detected = vec![Detected::Pending {
        spec: spec(&unused_url()),
        reason: "down".into(),
    }];
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(300),
        Stop::Grace(Duration::ZERO),
    )
    .await;
    assert!(run.summary.unwrap().discarded);
    assert!(!run.path.exists(), "nothing left behind");
}

/// The caller's `descriptions` is not replaced by a scrape's HELP text.
#[tokio::test(flavor = "multi_thread")]
async fn a_descriptions_the_caller_set_survives_the_help_text() {
    let exporter = serve(exporter("http_requests_total")).await;
    let mut endpoint = named(&exporter.url, "svc");
    endpoint.metadata.insert("descriptions".into(), "{}".into());
    let detected = detect(&client(), &detect_cfg(), vec![endpoint], &NoProbe).await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(500),
        Stop::Grace(Duration::ZERO),
    )
    .await;
    run.summary.unwrap();
    assert_eq!(
        recordings(&run.path, PROMETHEUS_GROUP)["svc"].metadata["descriptions"],
        "{}"
    );
}

/// The keys the recorder writes from the handshake are refused as the
/// caller's.
#[tokio::test(flavor = "multi_thread")]
async fn a_caller_cannot_set_the_producer_epoch() {
    for (key, as_label) in [
        ("producer_epoch", false),
        ("producer_epochs", false),
        ("metriken.layer.user", false),
        ("metriken.x", true),
        ("encoder", false),
    ] {
        let mut endpoint = spec(&unused_url());
        if as_label {
            endpoint.labels.insert(key.into(), "mine".into());
        } else {
            endpoint.metadata.insert(key.into(), "mine".into());
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.dendro");
        let started = Recorder::start(
            RecorderConfig::new(&path, INTERVAL),
            client(),
            Arc::new(NoProbe),
            vec![Detected::Pending {
                spec: endpoint,
                reason: "down".into(),
            }],
            Vec::new(),
        )
        .await;
        match started {
            Err(StartError::Reserved(e)) => assert!(e.contains(key), "{e}"),
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("{key} accepted"),
        }
        assert!(!path.exists());
    }
}

/// An existing output is refused, and left alone.
#[tokio::test(flavor = "multi_thread")]
async fn an_existing_output_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("taken.dendro");
    std::fs::write(&path, b"mine").unwrap();
    let started = Recorder::start_local(RecorderConfig::new(&path, INTERVAL), Vec::new()).await;
    assert!(matches!(started, Err(StartError::Create(_))));
    assert_eq!(std::fs::read(&path).unwrap(), b"mine");
}

// ── local sources and retention ───────────────────────────────────────

/// A local source: a histogram group staged as it is, on its own timeline.
struct Counting {
    anchor: u64,
    n: u64,
}

impl LocalSource for Counting {
    fn identity(&self) -> SourceIdentity {
        SourceIdentity::new(
            map(&[("source", "self")]),
            map(&[("version", "9.9.9")]),
            self.anchor,
        )
        .with_epoch("local-epoch")
        .with_producer(
            ProducerIdentity::new("counting")
                .with_build("b1")
                .with_host("h1"),
        )
    }

    fn pass(&mut self) -> Option<(metriken_model::Snapshot, Stamp)> {
        use metriken_model::{GroupSchema, GroupSnapshot, MetricDesc, Snapshot, SnapshotV3};
        self.n += 1;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let schema = GroupSchema {
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: vec![MetricDesc {
                name: "latency".into(),
                metadata: [
                    ("metric".to_string(), "latency".to_string()),
                    ("grouping_power".to_string(), "4".to_string()),
                    ("max_value_power".to_string(), "20".to_string()),
                ]
                .into_iter()
                .collect(),
            }],
            gauge_histograms: Vec::new(),
        };
        let mut h = histogram::Histogram::new(4, 20).unwrap();
        h.add(1000, self.n).unwrap();
        let group = GroupSnapshot {
            name: "self/latency".into(),
            schema_hash: schema.hash(),
            schema: Some(Arc::new(schema)),
            window: Some(metriken_types::Window::new(now - 1000, now)),
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: vec![Some(h)],
            gauge_histograms: Vec::new(),
        };
        Some((
            Snapshot::V3(SnapshotV3 {
                systemtime: std::time::SystemTime::now(),
                duration: Duration::ZERO,
                metadata: Default::default(),
                groups: vec![group],
            }),
            Stamp {
                ts: now,
                wall_offset: 0,
            },
        ))
    }
}

/// Reads `rx` until a `Committed` event reports at least `passes` passes.
/// A runner
/// slow enough to take fewer passes than a fixed sleep allows still gets
/// there; 30 s bounds a recorder that stops committing, and a writer
/// failure or a refused source panics with its reason.
async fn committed_passes(rx: &mut tokio::sync::mpsc::Receiver<Event>, passes: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Event::Committed { passes_total, .. })) if passes_total >= passes => return,
            Ok(Some(Event::Failed { error })) => panic!("{error}"),
            Ok(Some(Event::Refused { reason, .. })) => panic!("refused: {reason}"),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => panic!("no {passes} passes within 30 s"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_source_is_recorded_with_its_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local.dendro");
    let anchor = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let (recorder, ids, mut rx) = Recorder::start_local(
        RecorderConfig::new(&path, INTERVAL),
        vec![Box::new(Counting { anchor, n: 0 })],
    )
    .await
    .unwrap();
    assert_eq!(ids, vec![SourceId(0)]);
    committed_passes(&mut rx, 5).await;
    drop(rx);
    let summary = recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(summary.sources[0].transport, Some(ActiveTransport::Local));
    let r = &recordings(&path, "self/latency")["self"];
    assert!(r.rows >= 5, "{r:?}");
    assert!(r.complete);
    assert_eq!(r.metadata["version"], "9.9.9");
    assert_eq!(r.metadata["producer_name"], "counting");
    let layers = metriken_storage::layers::Layers::read(&r.metadata)
        .unwrap()
        .unwrap();
    assert_eq!(
        layers.user.labels["source"], "self",
        "the labels are the user's"
    );
    assert_eq!(r.metadata["build"], "b1");
    assert_eq!(r.labels["host"], "h1");
    assert_eq!(r.metadata["transport"], "local");
    assert_eq!(
        recorder_epochs(&r.metadata),
        vec![("local-epoch".to_string(), None)]
    );
    assert_eq!(
        producer_versions(&r.metadata),
        vec![(Some("local-epoch".to_string()), Some("9.9.9".to_string()))]
    );
}

/// With `retain`, rows older than the newest minus the retention are
/// evicted; `Committed` still reports the first stamp ever staged.
#[tokio::test(flavor = "multi_thread")]
async fn retention_evicts_rows_older_than_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("retain.dendro");
    let anchor = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let mut cfg = RecorderConfig::new(&path, INTERVAL);
    cfg.retain = Some(Duration::from_millis(400));
    let (recorder, _ids, mut rx) =
        Recorder::start_local(cfg, vec![Box::new(Counting { anchor, n: 0 })])
            .await
            .unwrap();
    // Record until the passes span 1.4 s, more than three windows. A slow
    // runner takes fewer passes in that time (7 in 2 s on one Windows CI
    // run), so the wait is on the span and the pass count, not on a fixed
    // sleep; 30 s bounds a recorder that stops committing.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut last = None;
    while !matches!(last, Some((passes, first, ts)) if passes >= 10 && ts - first >= 1_400_000_000)
    {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(Event::Committed {
                passes_total,
                first_ts,
                ts,
                ..
            })) => {
                // `first_ts` is the first pass ever, not the oldest kept.
                if let Some((_, first, _)) = last {
                    assert_eq!(first_ts, first, "first_ts moved with eviction");
                }
                last = Some((passes_total, first_ts, ts));
            }
            Ok(Some(Event::Failed { error })) => panic!("{error}"),
            Ok(Some(Event::Refused { reason, .. })) => panic!("refused: {reason}"),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => panic!("no 10 passes over 1.4 s within 30 s: {last:?}"),
        }
    }
    drop(rx);
    recorder
        .finish(Stop::Grace(Duration::ZERO), CancellationToken::new())
        .await
        .unwrap();
    let (passes, _, _) = last.unwrap();
    let r = &recordings(&path, "self/latency")["self"];
    assert!(
        r.rows < passes,
        "evicted: {} rows of {passes} passes",
        r.rows
    );
    // A segment is sealed when its first row falls behind the window and
    // dropped once its last row does, so up to two windows are kept.
    let span = r.last_ts.unwrap() - r.first_ts.unwrap();
    assert!(
        span <= 1_000_000_000,
        "kept at most about 800 ms: {span} ns"
    );
}

/// A tick that overruns its interval (here a scrape that takes 1 s at a
/// 100 ms interval) is ready again as soon as it ends; a stop that arrives
/// meanwhile is still taken after it rather than waiting behind every later
/// tick.
#[tokio::test(flavor = "multi_thread")]
async fn a_tick_that_overruns_its_interval_does_not_hold_off_finish() {
    let slow = serve(Behaviour {
        delay: Duration::from_secs(1),
        ..exporter("up_total")
    })
    .await;
    let mut url = slow.url.clone();
    url.set_path("/metrics");
    let detected = vec![Detected::Scraped {
        spec: named(&slow.url, "slow"),
        url,
    }];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("slow.dendro");
    let (recorder, _ids, rx) = Recorder::start(
        RecorderConfig::new(&path, INTERVAL),
        client(),
        Arc::new(NoProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    drop(rx);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let finished = tokio::time::timeout(
        Duration::from_secs(5),
        recorder.finish(Stop::Grace(Duration::ZERO), CancellationToken::new()),
    )
    .await
    .expect("finish returns while ticks overrun");
    assert!(recordings(&path, PROMETHEUS_GROUP)["slow"].complete);
    assert!(!finished.unwrap().discarded);
}

fn wall() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// A local source with one counter, named `name`.
struct Tiny {
    name: &'static str,
}

impl LocalSource for Tiny {
    fn identity(&self) -> SourceIdentity {
        SourceIdentity::new(map(&[("source", self.name)]), BTreeMap::new(), wall())
    }

    fn pass(&mut self) -> Option<(metriken_model::Snapshot, Stamp)> {
        use metriken_model::{GroupSchema, GroupSnapshot, MetricDesc, Snapshot, SnapshotV3};
        let t = wall();
        let schema = GroupSchema {
            counters: vec![MetricDesc {
                name: "c".into(),
                metadata: [("metric".to_string(), "c".to_string())]
                    .into_iter()
                    .collect(),
            }],
            gauges: Vec::new(),
            histograms: Vec::new(),
            gauge_histograms: Vec::new(),
        };
        let group = GroupSnapshot {
            name: format!("{}/g", self.name),
            schema_hash: schema.hash(),
            schema: Some(Arc::new(schema)),
            window: Some(metriken_types::Window::new(t - 1000, t)),
            counters: vec![Some(t)],
            gauges: Vec::new(),
            histograms: Vec::new(),
            gauge_histograms: Vec::new(),
        };
        Some((
            Snapshot::V3(SnapshotV3 {
                systemtime: std::time::SystemTime::now(),
                duration: Duration::ZERO,
                metadata: Default::default(),
                groups: vec![group],
            }),
            Stamp {
                ts: t,
                wall_offset: 0,
            },
        ))
    }
}

/// A local source whose pass takes `delay`, holding up the tick it is taken
/// on.
struct Slow {
    inner: Tiny,
    delay: Duration,
}

impl LocalSource for Slow {
    fn identity(&self) -> SourceIdentity {
        self.inner.identity()
    }

    fn pass(&mut self) -> Option<(metriken_model::Snapshot, Stamp)> {
        std::thread::sleep(self.delay);
        self.inner.pass()
    }
}

/// Streamed frames that queue up while a tick is busy are committed one per
/// commit, each followed by a seal check, so no segment holds more rows
/// than the seal policy's cap. A local source that takes 350 ms per pass at
/// a 100 ms interval holds every tick up for about three frames, which the
/// tick then drains together. Committed as one, they sealed as one segment
/// of four or more rows against a cap of two.
#[tokio::test(flavor = "multi_thread")]
async fn queued_frames_seal_within_the_row_cap() {
    let producer = serve(stream_only(&[("source", "producer")], &[])).await;
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![spec(&producer.url)],
        &NoProbe,
    )
    .await;
    let slow: Box<dyn LocalSource> = Box::new(Slow {
        inner: Tiny { name: "slow" },
        delay: Duration::from_millis(350),
    });
    let run = record(
        detected,
        Arc::new(NoProbe),
        vec![slow],
        |cfg| {
            cfg.writer.seal.max_rows = 2;
        },
        Duration::from_millis(2500),
        Stop::Grace(Duration::ZERO),
    )
    .await;
    run.summary.expect("finished");

    let archive = dendro::archive::Archive::open(&run.path).unwrap();
    let source = archive
        .read_sources()
        .unwrap()
        .into_iter()
        .find(|s| s.meta.labels.get("source").map(String::as_str) == Some("producer"))
        .expect("the producer's recording");
    let rows: Vec<u64> = archive
        .read_segments(source.id, "fake/ops")
        .unwrap()
        .iter()
        .map(|s| s.meta.rows)
        .collect();
    // Enough frames to have queued behind the slow ticks at all.
    assert!(rows.iter().sum::<u64>() >= 12, "{rows:?}");
    assert!(
        rows.iter().all(|&r| r <= 2),
        "a segment past the row cap of 2: {rows:?}"
    );
}

/// A caller that keeps the receiver and never reads it does not stall the
/// recorder: with a one-event channel, the second source's `Active` does
/// not fit, and `finish` still returns.
#[tokio::test(flavor = "multi_thread")]
async fn an_unread_receiver_does_not_hold_off_finish() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unread.dendro");
    let mut cfg = RecorderConfig::new(&path, INTERVAL);
    cfg.events = 1;
    let (recorder, _ids, _events) = Recorder::start_local(
        cfg,
        vec![Box::new(Tiny { name: "a" }), Box::new(Tiny { name: "b" })],
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    tokio::time::timeout(
        Duration::from_secs(5),
        recorder.finish(Stop::Now, CancellationToken::new()),
    )
    .await
    .expect("finish returns with an unread receiver")
    .unwrap();
}

/// `Committed` events fill the channel of a caller that does not read, and
/// a late activation's `Active` then has no room: queued, not awaited.
/// When the caller reads, it gets the `Active`, after a `Lagged`.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_channel_then_an_activation_does_not_hold_off_finish() {
    let fake = serve(Behaviour {
        down: true,
        ..exporter("up_total")
    })
    .await;
    let mut url = fake.url.clone();
    url.set_path("/metrics");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("full.dendro");
    let mut cfg = RecorderConfig::new(&path, Duration::from_millis(50));
    cfg.events = 8;
    let detected = vec![Detected::Pending {
        spec: EndpointSpec::new(url),
        reason: "down".into(),
    }];
    let (recorder, ids, mut events) = Recorder::start(
        cfg,
        client(),
        Arc::new(NoProbe),
        detected,
        vec![Box::new(Tiny { name: "a" })],
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    fake.set(exporter("up_total"));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let summary = tokio::time::timeout(
        Duration::from_secs(5),
        recorder.finish(Stop::Now, CancellationToken::new()),
    )
    .await
    .expect("finish returns with a full channel")
    .unwrap();
    assert_eq!(summary.sources[0].state, SourceState::Active);
    let mut seen = Vec::new();
    while let Some(e) = events.recv().await {
        seen.push(e);
    }
    assert!(
        seen.iter().any(|e| matches!(e, Event::Lagged { .. })),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Active { source, .. } if *source == ids[0])),
        "the queued Active is delivered: {seen:?}"
    );
}

/// The keys `start` refuses are refused by `patch_metadata` too.
#[tokio::test(flavor = "multi_thread")]
async fn patch_metadata_refuses_the_recorders_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reserved.dendro");
    let (recorder, ids, _events) = Recorder::start_local(
        RecorderConfig::new(&path, INTERVAL),
        vec![Box::new(Tiny { name: "a" })],
    )
    .await
    .unwrap();
    for key in [
        "producer_epoch",
        "producer_epochs",
        "metriken.layer.user",
        "encoder",
        "writer_sessions",
    ] {
        match recorder.patch_metadata(ids[0], map(&[(key, "mine")])).await {
            Err(Error::Reserved(e)) => assert!(e.contains(key), "{e}"),
            other => panic!("{key}: {other:?}"),
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();
    let r = &recordings(&path, "a/g")["a"];
    assert!(
        !r.metadata.contains_key("producer_epoch"),
        "{:?}",
        r.metadata
    );
}

/// A patch to a source refused before it was recorded has nowhere to go and
/// says so.
#[tokio::test(flavor = "multi_thread")]
async fn patch_metadata_on_a_refused_source_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refused.dendro");
    let detected = vec![Detected::Refused {
        spec: EndpointSpec::new(unused_url()),
        reason: "no".into(),
    }];
    let (recorder, ids, _events) = Recorder::start(
        RecorderConfig::new(&path, INTERVAL),
        client(),
        Arc::new(NoProbe),
        detected,
        vec![Box::new(Tiny { name: "a" })],
    )
    .await
    .unwrap();
    let patched = recorder.patch_metadata(ids[0], map(&[("k", "v")])).await;
    assert!(matches!(patched, Err(Error::Source(_))), "{patched:?}");
    recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();
}

/// A current-thread runtime records: commits, seals and local passes run
/// on blocking threads, which it has.
#[tokio::test(flavor = "current_thread")]
async fn a_current_thread_runtime_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ct.dendro");
    let (recorder, _ids, mut events) = Recorder::start_local(
        RecorderConfig::new(&path, INTERVAL),
        vec![Box::new(Tiny { name: "a" })],
    )
    .await
    .unwrap();
    committed_passes(&mut events, 4).await;
    let summary = recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();
    assert!(summary.sources[0].passes_total >= 4, "{summary:?}");
}

/// A hook whose `enrich` reports `hook_epoch`: the epoch it was asked about.
struct EpochProbe;

#[async_trait::async_trait]
impl Probe for EpochProbe {
    async fn not_streamable(&self, _url: &url::Url, _at: At, _answer: &StreamAnswer) -> Verdict {
        Verdict::Pending
    }

    async fn enrich(&self, _url: &url::Url, epoch: Option<&str>) -> Enrich {
        Enrich::Metadata {
            epoch: epoch.map(str::to_string),
            labels: BTreeMap::new(),
            metadata: map(&[("hook_epoch", epoch.unwrap_or("none"))]),
        }
    }
}

/// On a restart the probe hook is asked about the new process, and what it
/// reports reaches the recording's metadata.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_writes_the_hooks_metadata_for_the_new_process() {
    let fake = serve(Behaviour {
        streams: Streams::Restarts(3),
        ..stream_only(&[("source", "svc")], &[])
    })
    .await;
    let detected = detect(&client(), &detect_cfg(), vec![spec(&fake.url)], &EpochProbe).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hook.dendro");
    let (recorder, _ids, mut events) = Recorder::start(
        RecorderConfig::new(&path, INTERVAL),
        client(),
        Arc::new(EpochProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        while let Some(e) = events.recv().await {
            if matches!(e, Event::Restarted { .. }) {
                return;
            }
        }
        panic!("closed");
    })
    .await
    .expect("the restart is written within 6 s");
    recorder
        .finish(Stop::Grace(INTERVAL), CancellationToken::new())
        .await
        .unwrap();
    let r = &recordings(&path, "fake/ops")["svc"];
    assert_eq!(r.metadata["hook_epoch"], "epoch-2", "{:?}", r.metadata);
}

/// `Stop::AfterExit` takes one more pass of a scraped source when the exit
/// came after the last pass, so the interval the exit landed in is sampled.
#[tokio::test(flavor = "multi_thread")]
async fn after_exit_takes_one_more_scrape() {
    let exporter = serve(exporter("up_total")).await;
    let mut url = exporter.url.clone();
    url.set_path("/metrics");
    let detected = vec![Detected::Scraped {
        spec: named(&exporter.url, "svc"),
        url,
    }];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("extra.dendro");
    let (recorder, _ids, _events) = Recorder::start(
        RecorderConfig::new(&path, Duration::from_secs(1)),
        client(),
        Arc::new(NoProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let exit = wall();
    recorder
        .finish(
            Stop::AfterExit {
                wall_ns: exit,
                bound: Duration::from_secs(1),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    // On the wall clock, as `exit` is: a row's `ts` is on the recorder's
    // monotonic timeline, which can drift from `SystemTime` by more than the
    // gap between the exit and the extra scrape (0.585 ms over 1.3 s on a
    // macOS CI runner).
    let r = &recordings(&path, PROMETHEUS_GROUP)["svc"];
    let last = r.last_wall.expect("rows");
    assert!(
        last >= exit,
        "the last scrape ({last}) is after the exit ({exit})"
    );
}

/// An exit that lands while a tick's scrape is in flight still gets a pass
/// that starts after it: the scrape in flight was stamped before the exit,
/// and its body may have been read before the exit too.
#[tokio::test(flavor = "multi_thread")]
async fn after_exit_during_a_scrape_takes_one_more() {
    let exporter = serve(Behaviour {
        delay: Duration::from_millis(400),
        ..exporter("up_total")
    })
    .await;
    let mut url = exporter.url.clone();
    url.set_path("/metrics");
    let detected = vec![Detected::Scraped {
        spec: named(&exporter.url, "svc"),
        url,
    }];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inflight.dendro");
    let (recorder, _ids, _events) = Recorder::start(
        RecorderConfig::new(&path, Duration::from_secs(1)),
        client(),
        Arc::new(NoProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    // 100 ms past the second whole wall second from now: a tick (aligned to
    // whole seconds) began 100 ms ago and its scrape answers 300 ms from now.
    let now = wall();
    let at = (now / 1_000_000_000 + 2) * 1_000_000_000 + 100_000_000;
    tokio::time::sleep(Duration::from_nanos(at - now)).await;
    let exit = wall();
    recorder
        .finish(
            Stop::AfterExit {
                wall_ns: exit,
                bound: Duration::from_secs(1),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let r = &recordings(&path, PROMETHEUS_GROUP)["svc"];
    // The path under test was reached: a scrape started less than the
    // exporter's 400 ms delay before the exit, so it was in flight at the
    // exit. A tick that fired late, after the exit, fails here rather than
    // passing without the in-flight case.
    assert!(
        r.walls.iter().any(|&w| w < exit && exit - w < 400_000_000),
        "a scrape was in flight at the exit ({exit}): {:?}",
        r.walls
    );
    let last = r.last_wall.expect("rows");
    assert!(
        last >= exit,
        "the last scrape ({last}) started after the exit ({exit})"
    );
}

/// A pump whose reconnect is answered 503 (transient) keeps trying, and
/// reconnects once the producer streams again; it is not refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_pump_waits_out_a_transient_answer_and_reconnects() {
    let fake = serve(Behaviour {
        streams: Streams::OnceThen(2, 503),
        ..stream_only(&[("source", "svc")], &[])
    })
    .await;
    let sub = Subscription::connect(&client(), &fake.url, INTERVAL, Layout::Long, &[WIRE_NAMES])
        .await
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let pump = tokio::spawn(pump(
        0,
        sub,
        client(),
        fake.url.clone(),
        PumpConfig::new(INTERVAL, Duration::from_secs(2)),
        tx,
    ));
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        let mut dropped = false;
        while let Some((_, event)) = rx.recv().await {
            match event {
                StreamEvent::Dropped(_) => {
                    dropped = true;
                    // The reconnect is answered 503 for a while, then
                    // streams.
                    let fake = &fake;
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    fake.set(stream_only(&[("source", "svc")], &[]));
                }
                StreamEvent::Connected(_) => return Ok(dropped),
                StreamEvent::Refused(r) => return Err(r.to_string()),
                _ => {}
            }
        }
        Err("closed".to_string())
    })
    .await
    .expect("reconnected within 10 s");
    pump.abort();
    assert_eq!(outcome, Ok(true));
}

/// A stream whose interval is longer than the timeout is not dropped
/// between two frames: the silence bound is interval plus timeout.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_slower_than_the_timeout_is_not_dropped() {
    let fake = serve(stream_only(&[("source", "svc")], &[])).await;
    let interval = Duration::from_millis(1500);
    let mut cfg = DetectConfig::new(interval, Duration::from_secs(1));
    cfg.timeout = Duration::from_secs(1);
    let detected = detect(&client(), &cfg, vec![spec(&fake.url)], &NoProbe).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("slow.dendro");
    let mut rcfg = RecorderConfig::new(&path, interval);
    rcfg.timeout = Duration::from_secs(1);
    let (recorder, _ids, mut events) =
        Recorder::start(rcfg, client(), Arc::new(NoProbe), detected, Vec::new())
            .await
            .unwrap();
    tokio::time::sleep(Duration::from_millis(4000)).await;
    recorder
        .finish(Stop::Grace(Duration::ZERO), CancellationToken::new())
        .await
        .unwrap();
    let mut seen = Vec::new();
    while let Some(e) = events.recv().await {
        seen.push(e);
    }
    assert!(
        !seen.iter().any(|e| matches!(e, Event::Dropped { .. })),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .filter(|e| matches!(e, Event::Frame { .. }))
            .count()
            >= 2
    );
}

/// With `commit_streams_on_arrival`, a frame is committed when it arrives,
/// not on the next tick: at a 2 s interval, its `Committed` follows its
/// stamp by less than 1.4 s, where a frame left for the next tick lags by
/// about 2 s. The producer's frames and the recorder's ticks fall on the
/// same interval boundaries, so a frame usually arrives while its
/// boundary's tick commits, and its lag is the tick's commit plus its own:
/// the bound allows about 700 ms a commit. One that reaches the recorder
/// before the tick drains the stream is committed by the tick at once, and
/// this test cannot tell that case from arrival. Frames sent before the
/// recorder's loop runs wait in the subscription and are committed one per
/// commit until the first tick takes the rest, each late by the time it
/// waited, so only frames stamped an interval after the test sees the first
/// `Committed` are measured.
#[tokio::test(flavor = "multi_thread")]
async fn a_streamed_frame_can_be_committed_on_arrival() {
    let fake = serve(stream_only(&[("source", "svc")], &[])).await;
    let interval = Duration::from_secs(2);
    let detected = detect(
        &client(),
        &DetectConfig::new(interval, Duration::from_secs(2)),
        vec![spec(&fake.url)],
        &NoProbe,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("arrival.dendro");
    let mut cfg = RecorderConfig::new(&path, interval);
    cfg.commit_streams_on_arrival = true;
    let (recorder, _ids, mut events) =
        Recorder::start(cfg, client(), Arc::new(NoProbe), detected, Vec::new())
            .await
            .unwrap();
    let lags = tokio::time::timeout(Duration::from_secs(30), async {
        let mut fresh = None;
        let mut lags = Vec::new();
        while let Some(e) = events.recv().await {
            if let Event::Committed { wall_ns, .. } = e {
                let fresh = *fresh.get_or_insert_with(|| wall() + interval.as_nanos() as u64);
                if wall_ns < fresh {
                    continue;
                }
                lags.push(wall().saturating_sub(wall_ns));
                if lags.len() == 3 {
                    return lags;
                }
            }
        }
        lags
    })
    .await
    .expect("three fresh commits within 30 s");
    recorder
        .finish(Stop::Grace(Duration::ZERO), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(lags.len(), 3, "{lags:?}");
    assert!(
        lags.iter().all(|lag| *lag < 1_400_000_000),
        "committed within 1.4 s of the frame's stamp: {lags:?}"
    );
}

/// A caller can build the events a pump sends, to test its own handling of
/// them, though `Handshake` and `Refusal` are `#[non_exhaustive]`.
#[test]
fn a_caller_builds_the_events_a_pump_sends() {
    let handshake = Handshake::new(
        Some("epoch-1".into()),
        map(&[("source", "svc")]),
        map(&[("version", "1.0.0")]),
        42,
    );
    assert_eq!(handshake.epoch(), Some("epoch-1"));
    assert_eq!(handshake.labels["source"], "svc");
    assert_eq!(handshake.metadata["version"], "1.0.0");
    assert_eq!(handshake.clock_anchor_wall_ns, 42);
    let events = [
        StreamEvent::Connected(handshake),
        StreamEvent::Refused(Refusal::new(StreamAnswer::Status(404), "gone")),
    ];
    match &events[1] {
        StreamEvent::Refused(r) => {
            assert_eq!(r.answer, StreamAnswer::Status(404));
            assert_eq!(r.to_string(), "gone");
        }
        other => panic!("{other:?}"),
    }
}

/// A hook whose `enrich` answers on call `answer_on` (from 1), and before
/// that sleeps for `slow` (past the recorder's timeout) or, when `slow` is
/// zero, returns `Retry` at once. Counts its calls.
struct LateProbe {
    calls: std::sync::atomic::AtomicU32,
    answer_on: u32,
    slow: Duration,
}

impl LateProbe {
    fn new(answer_on: u32, slow: Duration) -> Arc<Self> {
        Arc::new(Self {
            calls: Default::default(),
            answer_on,
            slow,
        })
    }

    fn calls(&self) -> u32 {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Probe for LateProbe {
    async fn not_streamable(&self, _url: &url::Url, _at: At, _answer: &StreamAnswer) -> Verdict {
        Verdict::Pending
    }

    async fn enrich(&self, _url: &url::Url, epoch: Option<&str>) -> Enrich {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if call < self.answer_on {
            if self.slow.is_zero() {
                return Enrich::Retry;
            }
            tokio::time::sleep(self.slow).await;
        }
        Enrich::Metadata {
            epoch: epoch.map(str::to_string),
            labels: map(&[
                ("host", "web-01"),
                ("zone", "hook"),
                ("arm", "hook"),
                ("source", "hook"),
            ]),
            metadata: map(&[
                ("version", "1.2.3"),
                ("systeminfo", "{}"),
                ("owner", "hook"),
                ("sampling_interval_ms", "1"),
            ]),
        }
    }
}

/// A hook reply that misses the first ask's timeout is asked for again
/// after the recording is added, and its metadata is written into the
/// recording: not over the caller's keys or the recorder's own. Its late
/// labels are not applied.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_hook_reply_writes_its_metadata() {
    let fake = serve(stream_only(&[("source", "svc"), ("zone", "a")], &[])).await;
    let probe = LateProbe::new(2, Duration::from_secs(1));
    let mut endpoint = spec(&fake.url);
    endpoint.metadata.insert("owner".into(), "caller".into());
    // The caller's and the handshake's labels win, so the hook's `arm` and
    // `zone` are not named as lost.
    endpoint.labels.insert("arm".into(), "caller".into());
    let detected = detect(&client(), &detect_cfg(), vec![endpoint], probe.as_ref()).await;
    let run = record(
        detected,
        probe.clone(),
        Vec::new(),
        |cfg| cfg.timeout = Duration::from_millis(200),
        Duration::from_millis(1500),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.unwrap();
    assert_eq!(probe.calls(), 2, "the first ask and one retry");
    let r = &recordings(&run.path, "fake/ops")["svc"];
    assert_eq!(r.metadata["version"], "1.2.3", "{:?}", r.metadata);
    assert_eq!(r.metadata["systeminfo"], "{}");
    let layers = metriken_storage::layers::Layers::read(&r.metadata)
        .unwrap()
        .unwrap();
    assert_eq!(
        layers.recorder.metadata["version"], "1.2.3",
        "a recorder entry"
    );
    assert_eq!(r.metadata["owner"], "caller", "the caller's key is kept");
    assert_eq!(
        r.metadata["sampling_interval_ms"],
        INTERVAL.as_millis().to_string(),
        "the recorder's own key is kept"
    );
    assert!(!r.labels.contains_key("host"), "{:?}", r.labels);
    assert!(r.rows > 0);
    let named: Vec<_> = run
        .events
        .iter()
        .filter_map(|e| match e {
            Event::LabelsNotApplied { labels, .. } => Some(labels.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(named, [map(&[("host", "web-01")])], "{:?}", run.events);
}

/// A hook that never answers is asked `1 + ENRICH_RETRIES` times (5
/// retries) and then left alone; the recording keeps its handshake's
/// identity.
#[tokio::test(flavor = "multi_thread")]
async fn a_hook_that_never_answers_is_asked_a_bounded_number_of_times() {
    let fake = serve(stream_only(&[("source", "svc")], &[])).await;
    let probe = LateProbe::new(u32::MAX, Duration::ZERO);
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![spec(&fake.url)],
        probe.as_ref(),
    )
    .await;
    let run = record(
        detected,
        probe.clone(),
        Vec::new(),
        |_| {},
        Duration::from_millis(2500),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.unwrap();
    assert_eq!(probe.calls(), 6, "the first ask and five retries");
    let r = &recordings(&run.path, "fake/ops")["svc"];
    assert!(!r.metadata.contains_key("version"), "{:?}", r.metadata);
    assert!(r.rows > 0);
}

/// A hook that answers about the first process only once the producer has
/// restarted, and names no epoch in its replies, so `accept` cannot tell
/// them apart. Its first ask times out.
struct RestartProbe {
    restarted: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl Probe for RestartProbe {
    async fn not_streamable(&self, _url: &url::Url, _at: At, _answer: &StreamAnswer) -> Verdict {
        Verdict::Pending
    }

    async fn enrich(&self, _url: &url::Url, epoch: Option<&str>) -> Enrich {
        use std::sync::atomic::Ordering;
        if epoch == Some("epoch-2") {
            self.restarted.store(true, Ordering::SeqCst);
            return Enrich::Metadata {
                epoch: None,
                labels: BTreeMap::new(),
                metadata: map(&[("fresh", "epoch-2")]),
            };
        }
        while !self.restarted.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Enrich::Metadata {
            epoch: None,
            labels: BTreeMap::new(),
            metadata: map(&[("stale", "epoch-1")]),
        }
    }
}

/// A retry owed to the first process is dropped once the producer
/// restarted: the restart reads the new process's metadata, and a reply
/// about the old one, which names no epoch, is not written over it.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_drops_the_retry_owed_to_the_old_process() {
    let fake = serve(Behaviour {
        streams: Streams::Restarts(3),
        ..stream_only(&[("source", "svc")], &[])
    })
    .await;
    let probe = Arc::new(RestartProbe {
        restarted: Default::default(),
    });
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![spec(&fake.url)],
        probe.as_ref(),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restart.dendro");
    let mut cfg = RecorderConfig::new(&path, INTERVAL);
    cfg.timeout = Duration::from_millis(200);
    let (recorder, _ids, mut events) =
        Recorder::start(cfg, client(), probe.clone(), detected, Vec::new())
            .await
            .unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        while let Some(e) = events.recv().await {
            if matches!(e, Event::Restarted { .. }) {
                return;
            }
        }
        panic!("closed");
    })
    .await
    .expect("the restart is written within 6 s");
    // Long enough for a retry still asking about the first process to
    // answer and be written.
    tokio::time::sleep(Duration::from_millis(500)).await;
    tokio::spawn(async move { while events.recv().await.is_some() {} });
    recorder
        .finish(Stop::Grace(INTERVAL), CancellationToken::new())
        .await
        .unwrap();
    let r = &recordings(&path, "fake/ops")["svc"];
    assert_eq!(r.metadata.get("fresh").map(String::as_str), Some("epoch-2"));
    assert!(!r.metadata.contains_key("stale"), "{:?}", r.metadata);
}

/// A patch to an endpoint not recorded yet is written as a user entry when
/// the endpoint is added.
#[tokio::test(flavor = "multi_thread")]
async fn a_patch_before_the_add_is_written_at_the_add() {
    let fake = serve(Behaviour {
        down: true,
        ..exporter("up_total")
    })
    .await;
    let mut url = fake.url.clone();
    url.set_path("/metrics");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pending.dendro");
    let mut pending = EndpointSpec::new(url).with_label("source", "svc");
    pending
        .metadata
        .insert("events".into(), r#"{"events":["spec"]}"#.into());
    let detected = vec![Detected::Pending {
        spec: pending,
        reason: "down".into(),
    }];
    let (recorder, ids, _events) = Recorder::start(
        RecorderConfig::new(&path, Duration::from_millis(50)),
        client(),
        Arc::new(NoProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    recorder
        .patch_metadata(
            ids[0],
            map(&[
                ("owner", "caller"),
                ("events", r#"{"events":[]}"#),
                ("descriptions", r#"{"mine":"Mine."}"#),
            ]),
        )
        .await
        .unwrap();
    fake.set(exporter("up_total"));
    tokio::time::sleep(Duration::from_millis(1000)).await;
    recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();
    let r = &recordings(&path, PROMETHEUS_GROUP)["svc"];
    assert_eq!(r.metadata["owner"], "caller", "{:?}", r.metadata);
    assert_eq!(
        r.metadata["events"], r#"{"events":[]}"#,
        "the queued patch's over the endpoint's"
    );
    assert_eq!(
        r.metadata["descriptions"], r#"{"mine":"Mine."}"#,
        "a patched descriptions stops the HELP text"
    );
    let layers = metriken_storage::layers::Layers::read(&r.metadata)
        .unwrap()
        .unwrap();
    assert_eq!(layers.user.metadata["owner"], "caller");
}

/// A handshake carrying keys dendro maintains (`events`, `encoder`) is
/// recorded; those keys are left out of the producer entry.
#[tokio::test(flavor = "multi_thread")]
async fn a_handshakes_dendro_keys_are_left_out() {
    let fake = serve(stream_only(
        &[("source", "svc")],
        &[("events", "{}"), ("encoder", "theirs"), ("note", "kept")],
    ))
    .await;
    let detected = detect(&client(), &detect_cfg(), vec![spec(&fake.url)], &NoProbe).await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(500),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    let r = &recordings(&run.path, "fake/ops")["svc"];
    assert_eq!(r.metadata["note"], "kept");
    assert!(!r.metadata.contains_key("events"), "{:?}", r.metadata);
    assert_ne!(
        r.metadata.get("encoder").map(String::as_str),
        Some("theirs")
    );
}

/// Two restarts written together each get a producer entry with their own
/// handshake; only the newest recorder entry carries the hook's reply, and
/// `Restarted`'s version is the hook's.
#[tokio::test(flavor = "multi_thread")]
async fn two_restarts_written_together_keep_their_handshakes() {
    let fake = serve(Behaviour {
        streams: Streams::RestartsTwice(3),
        banner: Some("Rezolus 7.0.0 Agent\n".into()),
        restarted_metadata: map(&[("note", "second process")]),
        second_restarted_metadata: map(&[("note", "third process"), ("version", "handshake")]),
        ..stream_only(&[("source", "svc")], &[("note", "first process")])
    })
    .await;
    let probe = Arc::new(AgentProbe(client()));
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![spec(&fake.url)],
        probe.as_ref(),
    )
    .await;
    let run = record(
        detected,
        probe,
        Vec::new(),
        |_| {},
        Duration::from_millis(4000),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    let r = &recordings(&run.path, "fake/ops")["svc"];
    let layers = metriken_storage::layers::Layers::read(&r.metadata)
        .unwrap()
        .unwrap();
    let note = |epoch: &str| {
        layers
            .producer
            .entries
            .iter()
            .find(|e| e.epoch.as_deref() == Some(epoch))
            .map(|e| e.metadata["note"].clone())
    };
    assert_eq!(note("epoch-2").as_deref(), Some("second process"));
    assert_eq!(note("epoch-3").as_deref(), Some("third process"));
    let hook_version = |epoch: &str| {
        layers
            .recorder
            .entries
            .iter()
            .find(|e| e.epoch.as_deref() == Some(epoch))
            .map(|e| e.metadata.get("version").cloned())
    };
    assert_eq!(
        hook_version("epoch-2"),
        Some(None),
        "only the newest is asked"
    );
    assert_eq!(hook_version("epoch-3"), Some(Some("7.0.0".to_string())));
    let versions: Vec<_> = run
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Restarted { epoch, version, .. } => Some((epoch.clone(), version.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        versions,
        [("epoch-3".to_string(), Some("7.0.0".to_string()))],
        "the hook's version over the handshake's"
    );
}

/// `events` is the caller's, stored as given outside the layers, at start and
/// on a patch; a patch setting a key to `""` removes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_callers_events_are_stored_as_given() {
    let fake = serve(stream_only(&[("source", "svc")], &[])).await;
    let mut endpoint = spec(&fake.url);
    endpoint
        .metadata
        .insert("events".into(), r#"{"events":[]}"#.into());
    endpoint.metadata.insert("owner".into(), "caller".into());
    let detected = detect(&client(), &detect_cfg(), vec![endpoint], &NoProbe).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.dendro");
    let (recorder, ids, _events) = Recorder::start(
        RecorderConfig::new(&path, INTERVAL),
        client(),
        Arc::new(NoProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    // A patch of other keys leaves the endpoint's `events` as it was added.
    recorder
        .patch_metadata(ids[0], map(&[("owner", "")]))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = &recordings(&path, "fake/ops")["svc"];
    assert_eq!(r.metadata["events"], r#"{"events":[]}"#);
    assert!(!r.metadata.contains_key("owner"), "{:?}", r.metadata);

    let marked = r#"{"events":[{"timestamp":1,"description":"run_start"}]}"#;
    recorder
        .patch_metadata(ids[0], map(&[("events", marked)]))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    recorder
        .finish(Stop::Grace(INTERVAL), CancellationToken::new())
        .await
        .unwrap();
    let r = &recordings(&path, "fake/ops")["svc"];
    assert_eq!(r.metadata["events"], marked);
}

/// A local source's `events` in its identity's metadata is stored as given.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_sources_events_are_stored_as_given() {
    struct WithEvents(Tiny);
    impl LocalSource for WithEvents {
        fn identity(&self) -> SourceIdentity {
            SourceIdentity::new(
                map(&[("source", "a")]),
                map(&[("events", r#"{"events":[]}"#), ("version", "1")]),
                wall(),
            )
        }
        fn pass(&mut self) -> Option<(metriken_model::Snapshot, Stamp)> {
            self.0.pass()
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local-events.dendro");
    let (recorder, _ids, _events) = Recorder::start_local(
        RecorderConfig::new(&path, INTERVAL),
        vec![Box::new(WithEvents(Tiny { name: "a" }))],
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();
    let r = &recordings(&path, "a/g")["a"];
    assert_eq!(r.metadata["events"], r#"{"events":[]}"#);
    assert_eq!(r.metadata["version"], "1");
}

/// A local source's `metriken.` label is refused at start.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_sources_reserved_label_is_refused() {
    struct Reserved;
    impl LocalSource for Reserved {
        fn identity(&self) -> SourceIdentity {
            SourceIdentity::new(map(&[("metriken.x", "y")]), BTreeMap::new(), 1)
        }
        fn pass(&mut self) -> Option<(metriken_model::Snapshot, Stamp)> {
            None
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reserved-local.dendro");
    match Recorder::start_local(
        RecorderConfig::new(&path, INTERVAL),
        vec![Box::new(Reserved)],
    )
    .await
    {
        Err(StartError::Reserved(e)) => assert!(e.contains("metriken.x"), "{e}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("accepted"),
    }
}

/// An unnamed local source is shown as `source <id>`, inferred, and is
/// recorded without a `source` label.
#[tokio::test(flavor = "multi_thread")]
async fn an_unnamed_local_source_is_shown_by_its_id() {
    struct Unnamed(Tiny);
    impl LocalSource for Unnamed {
        fn identity(&self) -> SourceIdentity {
            SourceIdentity::new(BTreeMap::new(), BTreeMap::new(), wall())
        }
        fn pass(&mut self) -> Option<(metriken_model::Snapshot, Stamp)> {
            self.0.pass()
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unnamed.dendro");
    let (recorder, _ids, mut rx) = Recorder::start_local(
        RecorderConfig::new(&path, INTERVAL),
        vec![Box::new(Unnamed(Tiny { name: "a" }))],
    )
    .await
    .unwrap();
    let active = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Event::Active { name, inferred, .. }) = rx.recv().await {
                break (name, inferred);
            }
        }
    })
    .await
    .unwrap();
    drop(rx);
    recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(active, ("source 0".to_string(), true));
    let r = &recordings(&path, "a/g")[""];
    assert!(!r.labels.contains_key("source"), "{:?}", r.labels);
}

/// A reconnect whose handshake names a different producer ends the
/// recording and starts a new one for the endpoint, with the caller's
/// patches written to it again. A reconnect to the same producer does not.
#[tokio::test(flavor = "multi_thread")]
async fn a_different_producer_after_a_reconnect_starts_a_new_source() {
    let fake = serve(Behaviour {
        streams: Streams::Restarts(3),
        restarted_labels: Some(map(&[("source", "other")])),
        banner: Some("Rezolus 7.0.0 Agent\n".into()),
        ..stream_only(&[("source", "svc")], &[])
    })
    .await;
    let probe = Arc::new(AgentProbe(client()));
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![spec(&fake.url)],
        probe.as_ref(),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("changed.dendro");
    let (recorder, ids, mut rx) = Recorder::start(
        RecorderConfig::new(&path, INTERVAL),
        client(),
        probe,
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    recorder
        .patch_metadata(ids[0], map(&[("owner", "caller")]))
        .await
        .unwrap();
    let mut firsts = Vec::new();
    let changed = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            match rx.recv().await {
                Some(Event::ProducerChanged { from, to, name, .. }) => break (from, to, name),
                Some(Event::Committed {
                    first_ts,
                    passes_total,
                    ..
                }) => firsts.push((first_ts, passes_total)),
                Some(_) => {}
                None => panic!("closed"),
            }
        }
    })
    .await
    .expect("the producer change is seen within 6 s");
    assert_eq!(
        changed,
        ("svc".to_string(), "other".to_string(), "other".to_string())
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    while let Ok(event) = rx.try_recv() {
        if let Event::Committed {
            first_ts,
            passes_total,
            ..
        } = event
        {
            firsts.push((first_ts, passes_total));
        }
    }
    drop(rx);
    recorder
        .finish(Stop::Grace(INTERVAL), CancellationToken::new())
        .await
        .unwrap();
    let mut starts: Vec<u64> = firsts.iter().map(|(first, _)| *first).collect();
    starts.dedup();
    assert_eq!(starts.len(), 1, "first_ts counts the endpoint: {firsts:?}");
    assert!(
        firsts.windows(2).all(|w| w[0].1 <= w[1].1),
        "passes_total counts the endpoint: {firsts:?}"
    );

    let recordings = recordings(&path, "fake/ops");
    assert_eq!(recordings.len(), 2, "{recordings:?}");
    for (name, epoch) in [("svc", "epoch-1"), ("other", "epoch-2")] {
        let r = &recordings[name];
        assert!(r.rows > 0, "{name}: {r:?}");
        assert!(r.complete, "{name} is finalized");
        assert_eq!(r.metadata["owner"], "caller", "{name}");
        assert_eq!(
            recorder_epochs(&r.metadata),
            vec![(epoch.to_string(), None)],
            "{name}"
        );
    }
    assert!(
        recordings["svc"].last_ts < recordings["other"].first_ts,
        "the new recording starts after the old one ends"
    );
    let layers = metriken_storage::layers::Layers::read(&recordings["other"].metadata)
        .unwrap()
        .unwrap();
    assert_eq!(
        layers.recorder.metadata.get("version").map(String::as_str),
        Some("7.0.0"),
        "the hook is asked about the new producer"
    );
}

/// A reconnect whose handshake names no producer does not split, and the
/// producer on record stays the one named before, so a later different
/// producer still splits from it.
#[tokio::test(flavor = "multi_thread")]
async fn a_handshake_naming_no_producer_does_not_split() {
    let fake = serve(Behaviour {
        streams: Streams::RestartsTwice(3),
        restarted_metadata: BTreeMap::new(),
        second_restarted_metadata: map(&[("metriken.producer.name", "b")]),
        ..stream_only(&[], &[("metriken.producer.name", "a")])
    })
    .await;
    let detected = detect(&client(), &detect_cfg(), vec![spec(&fake.url)], &NoProbe).await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(4000),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    let changes: Vec<_> = run
        .events
        .iter()
        .filter_map(|e| match e {
            Event::ProducerChanged { from, to, .. } => Some((from.clone(), to.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(changes, [("a".to_string(), "b".to_string())]);
    // Producer a's restart to epoch-2 (a handshake naming none, which sends
    // no frame) has no row, so a's recording keeps its opening epoch alone.
    {
        use metriken_storage::Catalog;
        let catalog = metriken_storage::DendroCatalog::open(&run.path).unwrap();
        let sources = catalog.sources().unwrap();
        let a = sources
            .iter()
            .find(|s| s.metadata.get("producer_name").map(String::as_str) == Some("a"))
            .expect("a's recording");
        let metadata: BTreeMap<String, String> = a.metadata.clone().into_iter().collect();
        let epochs: Vec<String> = recorder_epochs(&metadata)
            .into_iter()
            .map(|(epoch, _)| epoch)
            .collect();
        assert_eq!(epochs, ["epoch-1"]);
    }

    let fake = serve(Behaviour {
        streams: Streams::Restarts(3),
        restarted_labels: Some(BTreeMap::new()),
        ..stream_only(&[("source", "svc")], &[])
    })
    .await;
    let detected = detect(&client(), &detect_cfg(), vec![spec(&fake.url)], &NoProbe).await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(3000),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    assert!(
        !run.events
            .iter()
            .any(|e| matches!(e, Event::ProducerChanged { .. })),
        "{:?}",
        run.events
    );
    use metriken_storage::Catalog;
    let catalog = metriken_storage::DendroCatalog::open(&run.path).unwrap();
    assert_eq!(catalog.sources().unwrap().len(), 1);

    // A recording whose handshake named no producer does not split when a
    // named one arrives.
    let fake = serve(Behaviour {
        streams: Streams::Restarts(3),
        restarted_labels: Some(map(&[("source", "b")])),
        ..stream_only(&[], &[])
    })
    .await;
    let detected = detect(&client(), &detect_cfg(), vec![spec(&fake.url)], &NoProbe).await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(3000),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    assert!(
        !run.events
            .iter()
            .any(|e| matches!(e, Event::ProducerChanged { .. })),
        "{:?}",
        run.events
    );
}

/// A scrape URL's user name and password reach neither the `instance` label
/// nor the summary.
#[tokio::test(flavor = "multi_thread")]
async fn a_scrape_urls_credentials_are_left_out() {
    let exporter = serve(exporter("up_total")).await;
    let mut url = exporter.url.clone();
    url.set_path("/metrics");
    let plain = url.to_string();
    url.set_username("user").unwrap();
    url.set_password(Some("secret")).unwrap();
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![EndpointSpec::new(url).with_label("source", "svc")],
        &NoProbe,
    )
    .await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(500),
        Stop::Grace(INTERVAL),
    )
    .await;
    let summary = run.summary.expect("finished");
    assert_eq!(summary.sources[0].endpoint.as_deref(), Some(plain.as_str()));
    let r = &recordings(&run.path, PROMETHEUS_GROUP)["svc"];
    assert_eq!(r.labels["instance"], plain);
    assert!(!format!("{:?}", r.metadata).contains("secret"));
}

/// A scrape that adds a metric name writes a recorder entry with that name's
/// HELP text alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_metric_name_writes_only_its_help_text() {
    let fake = serve(exporter("a_total")).await;
    let mut url = fake.url.clone();
    url.set_path("/metrics");
    let detected = detect(
        &client(),
        &detect_cfg(),
        vec![EndpointSpec::new(url).with_label("source", "svc")],
        &NoProbe,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("help.dendro");
    let (recorder, _ids, _events) = Recorder::start(
        RecorderConfig::new(&path, INTERVAL),
        client(),
        Arc::new(NoProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    fake.set(Behaviour {
        prometheus: Some(
            "# HELP a_total Requests served.\n# TYPE a_total counter\na_total 42\n\
             # HELP b_total Bytes served.\n# TYPE b_total counter\nb_total 7\n"
                .to_string(),
        ),
        ..Behaviour::default()
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    recorder
        .finish(Stop::Grace(INTERVAL), CancellationToken::new())
        .await
        .unwrap();
    let r = &recordings(&path, PROMETHEUS_GROUP)["svc"];
    let layers = metriken_storage::layers::Layers::read(&r.metadata)
        .unwrap()
        .unwrap();
    let written: Vec<BTreeMap<String, String>> = layers
        .recorder
        .entries
        .iter()
        .filter_map(|e| e.metadata.get("descriptions"))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    assert_eq!(
        written,
        vec![
            map(&[("a_total", "Requests served.")]),
            map(&[("b_total", "Bytes served.")]),
        ]
    );
    let merged: BTreeMap<String, String> =
        serde_json::from_str(&r.metadata["descriptions"]).unwrap();
    assert_eq!(merged.len(), 2);
}

/// With no hook, `Restarted`'s version is the newest restart's handshake's.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarts_version_is_the_newest_handshakes() {
    let fake = serve(Behaviour {
        streams: Streams::RestartsTwice(3),
        restarted_metadata: map(&[("version", "2")]),
        second_restarted_metadata: map(&[("version", "3")]),
        ..stream_only(&[("source", "svc")], &[("version", "1")])
    })
    .await;
    let detected = detect(&client(), &detect_cfg(), vec![spec(&fake.url)], &NoProbe).await;
    let run = record(
        detected,
        Arc::new(NoProbe),
        Vec::new(),
        |_| {},
        Duration::from_millis(4000),
        Stop::Grace(INTERVAL),
    )
    .await;
    run.summary.expect("finished");
    let versions: Vec<_> = run
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Restarted { version, .. } => Some(version.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(versions, [Some("3".to_string())]);
}
