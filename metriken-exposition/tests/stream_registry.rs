//! `RegistryPasses`: one pass per freshness floor shared by every caller,
//! and a subscription over it as a body stream.

#![cfg(feature = "stream")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use metriken::metric;
use metriken_exposition::group_builder::{DefaultRouter, ExtraGroup, GroupBuilder};
use metriken_exposition::stream::{
    Layout, Pass, RegistryPasses, StreamRequest, StreamRow, StreamSubscription,
};
use metriken_exposition::MetricDesc;

#[metric(name = "registry_passes_requests")]
static REQUESTS: metriken::Counter = metriken::Counter::new();

fn at(ns: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_nanos(ns)
}

const BASE: u64 = 1_700_000_000_000_000_000;

fn passes() -> RegistryPasses<DefaultRouter> {
    REQUESTS.add(1);
    RegistryPasses::new(
        GroupBuilder::new(DefaultRouter::new("registry")),
        Duration::from_secs(1),
    )
    .with_extra(|| {
        vec![ExtraGroup {
            namespace: "registry".to_string(),
            name: "pushed".to_string(),
            window: None,
            counters: vec![(
                MetricDesc {
                    name: "pushed_total".to_string(),
                    metadata: [("metric".to_string(), "pushed_total".to_string())].into(),
                },
                Some(7),
            )],
            gauges: Vec::new(),
            histograms: Vec::new(),
        }]
    })
}

#[test]
fn a_pass_is_shared_within_the_floor_and_rebuilt_after_it() {
    let passes = passes();
    let first = passes.pass(Layout::Wide, at(BASE)).expect("a pass");
    let again = passes
        .pass(Layout::Wide, at(BASE + 999_999_999))
        .expect("a pass");
    assert!(
        Arc::ptr_eq(&first, &again),
        "inside the floor: the same pass"
    );

    let later = passes
        .pass(Layout::Wide, at(BASE + 1_000_000_000))
        .expect("a pass");
    assert!(!Arc::ptr_eq(&first, &later), "at the floor: a new pass");
    assert_ne!(first.stamp().wall_ns, later.stamp().wall_ns);

    let long = passes.pass(Layout::Long, at(BASE)).expect("a pass");
    assert!(!Arc::ptr_eq(&later, &long), "one pass per layout");

    let streams: Vec<&str> = first.rows().iter().map(StreamRow::stream).collect();
    assert!(streams.contains(&"registry/main"), "{streams:?}");
    assert!(streams.contains(&"registry/pushed"), "{streams:?}");
}

/// A clock that went backwards past the last build is not inside the floor.
#[test]
fn a_pass_built_in_the_future_is_rebuilt() {
    let passes = passes();
    let first = passes.pass(Layout::Wide, at(BASE)).unwrap();
    let earlier = passes.pass(Layout::Wide, at(BASE - 1)).unwrap();
    assert!(!Arc::ptr_eq(&first, &earlier));
}

/// Two passes, the second inside the floor: the second interval's frame is
/// empty, and the first carries every group.
#[test]
fn a_subscription_sends_a_shared_pass_once() {
    let passes = passes();
    let mut sub = StreamSubscription::new(
        StreamRequest::default(),
        passes.floor(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    sub.opening().unwrap();
    let rows = |bytes: &[u8]| {
        let mut body = Vec::new();
        dendro::replicate::wire::write_preamble(&mut body).unwrap();
        body.extend_from_slice(bytes);
        let mut reader =
            dendro::replicate::wire::FrameReader::new(std::io::Cursor::new(body)).unwrap();
        match reader.next_frame().unwrap().unwrap() {
            dendro::replicate::Frame::Rows { rows, .. } => rows.len(),
            other => panic!("{other:?}"),
        }
    };
    let now = at(BASE);
    let pass = passes.pass(Layout::Wide, now);
    assert!(rows(&sub.frame_at(pass.as_ref(), now).unwrap()) >= 2);
    let now = at(BASE + 500_000_000);
    let pass = passes.pass(Layout::Wide, now);
    assert_eq!(rows(&sub.frame_at(pass.as_ref(), now).unwrap()), 0);
}

/// The body stream over `RegistryPasses` is `Send + 'static`, as a server's
/// response body needs.
#[cfg(feature = "stream-tokio")]
#[tokio::test(start_paused = true)]
async fn a_registry_subscription_streams() {
    use futures_util::StreamExt;

    fn assert_send<T: Send + 'static>(t: T) -> T {
        t
    }

    let passes = Arc::new(passes());
    let request = StreamRequest::parse("interval=1s&layout=long").unwrap();
    let sub = StreamSubscription::new(request, passes.floor(), BTreeMap::new(), BTreeMap::new());
    let layout = request.layout;
    let fetch = {
        let passes = Arc::clone(&passes);
        move |now| {
            let p = Arc::clone(&passes);
            async move { p.pass(layout, now) }
        }
    };
    let stream = assert_send(sub.into_stream(fetch, (), SystemTime::now));
    futures_util::pin_mut!(stream);
    let opening = stream.next().await.unwrap().unwrap();
    assert!(!opening.is_empty());
    let frame = stream.next().await.unwrap().unwrap();
    assert!(!frame.is_empty());
}
