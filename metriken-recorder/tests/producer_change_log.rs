//! What the recorder logs when a recording's producer or label set changes. Its own
//! test binary because it installs the process's global subscriber: the
//! recorder runs on runtime worker threads, which a thread-local
//! `set_default` would not reach.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use metriken_recorder::*;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A producer change that leaves the label set as it was logs the change and
/// no identical-labels warning: the endpoint's ended recording does not
/// count against its new one. The producers are named by their handshakes'
/// `source` labels, which the caller's `source` outranks, so the recording's
/// labels stay the same (a producer identity would set `__producer__`).
#[tokio::test(flavor = "multi_thread")]
async fn a_split_with_the_same_labels_does_not_warn_about_them() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish(),
    )
    .expect("the only subscriber in this binary");

    let fake = serve(Behaviour {
        streams: Streams::Restarts(3),
        restarted_labels: Some(map(&[("source", "b")])),
        ..stream_only(&[("source", "a")], &[])
    })
    .await;
    let interval = Duration::from_millis(100);
    let detected = detect(
        &client(),
        &DetectConfig::new(interval, Duration::from_secs(2)),
        vec![EndpointSpec::new(fake.url.clone()).with_label("source", "mine")],
        &NoProbe,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (recorder, _ids, mut rx) = Recorder::start(
        RecorderConfig::new(dir.path().join("split.dendro"), interval),
        client(),
        Arc::new(NoProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            match rx.recv().await {
                Some(Event::ProducerChanged { .. }) => break,
                Some(_) => {}
                None => panic!("closed"),
            }
        }
    })
    .await
    .expect("the producer change is seen within 6 s");
    drop(rx);
    recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();

    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("the producer at this endpoint changed (a -> b)"),
        "{log}"
    );
    assert!(!log.contains("carry identical labels"), "{log}");

    // Two endpoints given one label set (the caller sets `instance` and
    // `source` on both): the second warns at its add, and when the first changes
    // producer its new recording warns again, against the second.
    let before = captured.0.lock().unwrap().len();
    let changing = serve(Behaviour {
        streams: Streams::Restarts(3),
        restarted_labels: Some(map(&[("source", "b")])),
        ..stream_only(&[("source", "a")], &[])
    })
    .await;
    let steady = serve(stream_only(&[("source", "a")], &[])).await;
    let shared = |url: &url::Url| {
        EndpointSpec::new(url.clone())
            .with_label("instance", "shared")
            .with_label("source", "mine")
    };
    let detected = detect(
        &client(),
        &DetectConfig::new(interval, Duration::from_secs(2)),
        vec![shared(&changing.url), shared(&steady.url)],
        &NoProbe,
    )
    .await;
    let (recorder, _ids, mut rx) = Recorder::start(
        RecorderConfig::new(dir.path().join("shared.dendro"), interval),
        client(),
        Arc::new(NoProbe),
        detected,
        Vec::new(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            match rx.recv().await {
                Some(Event::ProducerChanged { .. }) => break,
                Some(_) => {}
                None => panic!("closed"),
            }
        }
    })
    .await
    .expect("the producer change is seen within 6 s");
    drop(rx);
    recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();
    let log = String::from_utf8(captured.0.lock().unwrap()[before..].to_vec()).unwrap();
    let warnings: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("carry identical labels"))
        .collect();
    assert_eq!(warnings.len(), 2, "{log}");
    let steady_port = steady.url.port().unwrap().to_string();
    assert!(
        warnings[1].contains(&steady_port),
        "the second warning names the endpoint still recording: {}",
        warnings[1]
    );

    // Two endpoints given one label set, the hook naming the producer at
    // once for one and late for the other: they differ at the add, and the
    // late reply gives the second the first's labels, which warns.
    let before = captured.0.lock().unwrap().len();
    let named = serve(stream_only(&[("source", "a")], &[])).await;
    let late = serve(stream_only(&[("source", "a")], &[])).await;
    let probe = Arc::new(NamesLate {
        late: late.url.clone(),
        asked: Default::default(),
    });
    let detected = detect(
        &client(),
        &DetectConfig::new(interval, Duration::from_secs(2)),
        vec![shared(&named.url), shared(&late.url)],
        probe.as_ref(),
    )
    .await;
    let mut cfg = RecorderConfig::new(dir.path().join("late.dendro"), interval);
    cfg.timeout = Duration::from_millis(200);
    let (recorder, _ids, rx) = Recorder::start(cfg, client(), probe, detected, Vec::new())
        .await
        .unwrap();
    drop(rx);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();
    let log = String::from_utf8(captured.0.lock().unwrap()[before..].to_vec()).unwrap();
    let warnings = log
        .lines()
        .filter(|l| l.contains("carry identical labels"))
        .count();
    assert_eq!(warnings, 1, "{log}");
}

/// A hook that names the producer `x`, answering `Retry` to its first ask
/// about `late`.
struct NamesLate {
    late: url::Url,
    asked: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl Probe for NamesLate {
    async fn not_streamable(&self, _url: &url::Url, _at: At, _answer: &StreamAnswer) -> Verdict {
        Verdict::Pending
    }

    async fn enrich(&self, url: &url::Url, epoch: Option<&str>) -> Enrich {
        if *url == self.late && !self.asked.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Enrich::Retry;
        }
        Enrich::Metadata {
            epoch: epoch.map(str::to_string),
            labels: Default::default(),
            metadata: map(&[("producer_name", "x")]),
        }
    }
}
