//! What the recorder logs when a reconnect names another producer. Its own
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

/// A producer change that leaves the label set as it was (the producers are
/// named by identity) logs the change and no identical-labels warning: the
/// endpoint's ended recording does not count against its new one.
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
        restarted_metadata: map(&[("metriken.producer.name", "b")]),
        ..stream_only(&[], &[("metriken.producer.name", "a")])
    })
    .await;
    let interval = Duration::from_millis(100);
    let detected = detect(
        &client(),
        &DetectConfig::new(interval, Duration::from_secs(2)),
        vec![EndpointSpec::new(fake.url.clone())],
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
}
