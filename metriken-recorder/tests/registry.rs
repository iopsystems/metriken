//! The process's own registry as a local source (feature `registry`).

use std::time::Duration;

use metriken::{metric, Counter};
use metriken_exposition::group_builder::{DefaultRouter, GroupBuilder};
use metriken_recorder::{CancellationToken, Recorder, RecorderConfig, RegistrySource, Stop};
use metriken_storage::Catalog;

#[metric(name = "registry_test_ops", description = "Operations.")]
static OPS: Counter = Counter::new();

#[tokio::test(flavor = "multi_thread")]
async fn the_registry_is_recorded_on_the_process_timeline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("self.dendro");
    let source = RegistrySource::new(
        GroupBuilder::new(DefaultRouter::new("selftest")),
        [("source".to_string(), "selftest".to_string())].into(),
        Default::default(),
    );
    let (recorder, _ids, rx) = Recorder::start_local(
        RecorderConfig::new(&path, Duration::from_millis(100)),
        vec![Box::new(source)],
    )
    .await
    .unwrap();
    drop(rx);
    for _ in 0..5 {
        OPS.add(7);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    recorder
        .finish(Stop::Now, CancellationToken::new())
        .await
        .unwrap();

    let catalog = metriken_storage::DendroCatalog::open(&path).unwrap();
    let sources = catalog.sources().unwrap();
    assert_eq!(sources.len(), 1);
    let s = &sources[0];
    assert_eq!(s.labels.get("source").map(String::as_str), Some("selftest"));
    assert_eq!(
        s.metadata.get("producer_epoch").map(String::as_str),
        Some(metriken::epoch::producer_epoch())
    );
    let tables = catalog.tables(s.id).unwrap();
    assert!(
        tables.iter().any(|t| t.starts_with("selftest/")),
        "{tables:?}"
    );
    assert!(s.complete);
}
