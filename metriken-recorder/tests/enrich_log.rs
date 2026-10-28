//! What the recorder logs about a probe hook reply that is still owed when
//! the run ends. Its own test binary because it installs the process's
//! global subscriber: the recorder runs on runtime worker threads, which a
//! thread-local `set_default` would not reach.

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

/// A hook that never answers within the timeout.
struct Silent;

#[async_trait::async_trait]
impl Probe for Silent {
    async fn not_streamable(&self, _url: &url::Url, _at: At, _answer: &StreamAnswer) -> Verdict {
        Verdict::Pending
    }

    async fn enrich(&self, _url: &url::Url, _epoch: Option<&str>) -> Enrich {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Enrich::Retry
    }
}

/// A run that ends before the first retry of the hook was started says so.
/// The interval is an hour, so no tick (which would start the retry) runs
/// between the start and the stop.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_ends_with_a_reply_owed_says_so() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish(),
    )
    .expect("the only subscriber in this binary");

    let fake = serve(stream_only(&[("source", "svc")], &[])).await;
    let interval = Duration::from_secs(3600);
    let detected = detect(
        &client(),
        &DetectConfig::new(interval, Duration::from_secs(2)),
        vec![EndpointSpec::new(fake.url.clone())],
        &Silent,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = RecorderConfig::new(dir.path().join("out.dendro"), interval);
    cfg.timeout = Duration::from_millis(200);
    let (recorder, _ids, mut rx) =
        Recorder::start(cfg, client(), Arc::new(Silent), detected, Vec::new())
            .await
            .expect("the recording starts");
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .expect("finished");

    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let ended: Vec<&str> = log
        .lines()
        .filter(|l| {
            l.contains(
                "its metadata had not arrived when the run ended; the recording keeps the \
                        labels and metadata it was added with",
            )
        })
        .collect();
    assert_eq!(ended.len(), 1, "{log}");
}
