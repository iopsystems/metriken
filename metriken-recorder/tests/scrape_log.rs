//! A scrape's skipped-line warning, as tracing-subscriber's default format
//! prints it. Its own test binary because it installs the process's global
//! subscriber: the scrape runs on runtime worker threads, which a
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

/// The warning names the endpoint, through the `scrape` span the recorder
/// opens around the converter, and quotes the skipped line.
#[tokio::test(flavor = "multi_thread")]
async fn a_skipped_line_is_logged_with_its_endpoint() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_target(true)
            .with_ansi(false)
            .finish(),
    )
    .expect("the only subscriber in this binary");

    let fake = serve(Behaviour {
        prometheus: Some("ok_total 1\nbad{a.b=\"1\"} 2\n".to_string()),
        ..Behaviour::default()
    })
    .await;
    let mut url = fake.url.clone();
    url.set_path("/metrics");
    let mut spec = EndpointSpec::new(url.clone());
    spec.labels.insert("source".into(), "svc".into());
    let interval = Duration::from_millis(100);

    let dir = tempfile::tempdir().unwrap();
    let cfg = RecorderConfig::new(dir.path().join("out.dendro"), interval);
    let (recorder, _ids, mut rx) = Recorder::start(
        cfg,
        client(),
        Arc::new(NoProbe),
        vec![Detected::Scraped {
            spec,
            url: url.clone(),
        }],
        Vec::new(),
    )
    .await
    .expect("the recording starts");
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    tokio::time::sleep(Duration::from_millis(500)).await;
    recorder
        .finish(Stop::Grace(interval), CancellationToken::new())
        .await
        .expect("finished");

    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let warnings: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("unparseable exposition line"))
        .collect();
    assert_eq!(
        warnings.iter().filter(|l| l.contains(" WARN ")).count(),
        1,
        "one warning per endpoint:\n{log}"
    );
    let warning = warnings[0];
    assert!(
        warning.contains(&format!("endpoint=svc ({url})")),
        "{warning}"
    );
    assert!(
        warning.contains(r#"the first: "bad{a.b=\"1\"} 2""#),
        "{warning}"
    );
}
