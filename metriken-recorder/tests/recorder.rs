//! The recorder against in-process producers: detection, identity,
//! orchestration and the archive it writes.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
    assert_eq!(r.metadata["source"], "cachecannon");
    assert_eq!(r.metadata["version"], "0.3.0");
    assert_eq!(r.metadata["build"], "abc");
    assert_eq!(r.metadata["producer_epoch"], "epoch-1");
    assert!(run.events.iter().any(|e| matches!(
        e,
        Event::Active { name, inferred: false, .. } if name == "cachecannon"
    )));
}

/// The caller's labels and metadata outrank the handshake's; the probe
/// hook's labels sit between.
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
    assert_eq!(r.metadata["producer_epoch"], "epoch-2");
    let epochs: serde_json::Value = serde_json::from_str(&r.metadata["producer_epochs"]).unwrap();
    let epochs = epochs.as_array().unwrap();
    assert_eq!(epochs.len(), 2, "{epochs:?}");
    assert_eq!(epochs[0]["epoch"], "epoch-1");
    assert_eq!(epochs[0]["version"], "1.0.0");
    assert_eq!(epochs[1]["epoch"], "epoch-2");
    assert_eq!(epochs[1]["version"], "1.1.0");
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
    for key in ["producer_epoch", "producer_epochs"] {
        let mut endpoint = spec(&unused_url());
        endpoint.metadata.insert(key.into(), "mine".into());
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
        SourceIdentity {
            labels: map(&[("source", "self")]),
            metadata: map(&[("version", "9.9.9")]),
            clock_anchor_wall_ns: self.anchor,
            epoch: Some("local-epoch".into()),
        }
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

#[tokio::test(flavor = "multi_thread")]
async fn a_local_source_is_recorded_with_its_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local.dendro");
    let anchor = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let (recorder, ids, rx) = Recorder::start_local(
        RecorderConfig::new(&path, INTERVAL),
        vec![Box::new(Counting { anchor, n: 0 })],
    )
    .await
    .unwrap();
    assert_eq!(ids, vec![SourceId(0)]);
    drop(rx);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let summary = recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(summary.sources[0].transport, Some(ActiveTransport::Local));
    let r = &recordings(&path, "self/latency")["self"];
    assert!(r.rows >= 5, "{r:?}");
    assert!(r.complete);
    assert_eq!(r.metadata["version"], "9.9.9");
    assert_eq!(r.metadata["producer_epoch"], "local-epoch");
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
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let mut last = None;
    while let Ok(e) = rx.try_recv() {
        if let Event::Committed {
            passes_total,
            first_ts,
            ts,
            ..
        } = e
        {
            last = Some((passes_total, first_ts, ts));
        }
    }
    drop(rx);
    recorder
        .finish(Stop::Grace(Duration::ZERO), CancellationToken::new())
        .await
        .unwrap();
    let (passes, first_ts, newest) = last.expect("Committed events");
    assert!(passes >= 15, "{passes}");
    assert!(
        newest - first_ts >= 1_400_000_000,
        "first_ts is the first ever"
    );
    let r = &recordings(&path, "self/latency")["self"];
    assert!(
        r.rows < passes,
        "evicted: {} rows of {passes} passes",
        r.rows
    );
    let span = r.last_ts.unwrap() - r.first_ts.unwrap();
    assert!(span <= 600_000_000, "kept about 400 ms: {span} ns");
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
