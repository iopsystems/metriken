//! A producer's registry, end to end: `GroupBuilder` groups, `FrameProducer`
//! frames, dendro's wire and subscriber, and `ArchiveReader` reading the
//! values back.
#![cfg(all(feature = "stream", feature = "write"))]

use std::collections::BTreeMap;

use dendro::replicate::wire::{self, FrameReader};
use dendro::replicate::{Frame, Subscriber};
use dendro::writer::Writer;
use metriken::{metric, MetricEntry};
use metriken_exposition::group_builder::{
    is_family, Acquisition, GroupBuilder, GroupId, Membership, NoGuard, Route, Router, Stamp,
};
use metriken_query::{BufferPool, MetricsSource, QueryResult};
use metriken_storage::stream::{encode_groups, FrameProducer, SchemaCache};
use metriken_storage::wal::decode_wal_group_row;
use metriken_storage::{ArchiveReader, DendroCatalog, Encoder};

#[metric(name = "stream_requests")]
static REQUESTS: metriken::Counter = metriken::Counter::new();

#[metric(name = "stream_tenant_bytes")]
static TENANT_BYTES: metriken::CounterFamily = metriken::CounterFamily::new();

/// This file's two metrics, in two groups.
struct StreamRouter {
    window: std::sync::Mutex<Option<metriken::Window>>,
}

impl Router for StreamRouter {
    type Guard = NoGuard;

    fn route<'a>(&'a self, metric: &'a MetricEntry) -> Option<Route<'a>> {
        match metric.name() {
            "stream_requests" => Some(Route {
                group: GroupId::new("svc", "main"),
                membership: Membership::All,
            }),
            "stream_tenant_bytes" => Some(Route {
                group: GroupId::new("svc", "tenants"),
                membership: if is_family(metric) {
                    Membership::Slots
                } else {
                    Membership::Present
                },
            }),
            _ => None,
        }
    }

    fn acquire(&self, _: GroupId<'_>) -> Acquisition<NoGuard> {
        Acquisition::Stamped(*self.window.lock().unwrap())
    }
}

const S: i64 = 1_000_000_000;

#[test]
fn a_registry_streams_into_an_archive_that_reads_back() {
    let mut builder = GroupBuilder::new(StreamRouter {
        window: std::sync::Mutex::new(None),
    });
    let mut schema_cache = SchemaCache::new();
    let mut producer = FrameProducer::new(
        [("source".to_string(), "svc".to_string())].into(),
        BTreeMap::new(),
    );
    let mut bytes = producer.opening().unwrap();

    let base = metriken::epoch::clock_anchor_wall_ns() + 10 * S;
    let acme = TENANT_BYTES.member([("tenant", "acme")]);
    let mut globex = Some(TENANT_BYTES.member([("tenant", "globex")]));
    let mut sent = Vec::new();

    for tick in 0..6i64 {
        REQUESTS.add(10);
        acme.add(100);
        if let Some(g) = &globex {
            g.add(1);
        }
        if tick == 3 {
            // globex leaves: the tenants group's schema changes.
            globex = None;
        }
        let ts = (base + tick * S) as u64;
        *builder.router().window.lock().unwrap() =
            Some(metriken::Window::new(ts - S as u64 / 2, ts));
        let stamp = Stamp {
            ts: base + tick * S,
            wall_offset: 0,
        };
        let snapshot = builder.snapshot(
            stamp,
            std::time::Duration::from_millis(1),
            Vec::new(),
            Default::default(),
        );
        assert_eq!(snapshot.groups.len(), 2);
        let rows = encode_groups(&snapshot.groups, &mut schema_cache).unwrap();
        let frame = producer.interval(&rows, stamp.ts, stamp.wall_offset, tick as u64);
        wire::encode_frame(&frame, &mut bytes).unwrap();
        sent.push(frame);
    }

    // The schema rides on the first row of each stream and on a change.
    let schemas: Vec<Vec<(String, bool)>> = sent
        .iter()
        .map(|f| {
            let Frame::Rows { rows, .. } = f else {
                panic!("rows")
            };
            rows.iter()
                .map(|r| {
                    let row = decode_wal_group_row(&r.row).unwrap();
                    (r.stream.clone(), row.schema.is_some())
                })
                .collect()
        })
        .collect();
    let tenants_carry: Vec<bool> = schemas
        .iter()
        .map(|t| t.iter().find(|(s, _)| s == "svc/tenants").unwrap().1)
        .collect();
    assert_eq!(
        tenants_carry,
        vec![true, false, false, true, false, false],
        "the tenants schema on the first tick and after globex left"
    );
    let main_carry: Vec<bool> = schemas
        .iter()
        .map(|t| t.iter().find(|(s, _)| s == "svc/main").unwrap().1)
        .collect();
    assert_eq!(main_carry, vec![true, false, false, false, false, false]);

    // Through dendro's subscriber into an archive.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("svc.dendro");
    let mut subscriber = Subscriber::new(
        Writer::create(&path, Box::new(Encoder::for_streams(std::iter::empty()))).unwrap(),
    );
    let mut reader = FrameReader::new(std::io::Cursor::new(bytes)).unwrap();
    let (mut applied, mut skipped) = (0, 0);
    while let Some(frame) = reader.next_frame().unwrap() {
        let a = subscriber.apply(frame).unwrap();
        applied += a.rows;
        skipped += a.rows_skipped;
    }
    subscriber.finish().unwrap();
    assert_eq!((applied, skipped), (12, 0));

    let mut recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(&path).unwrap()),
        None,
        BufferPool::new(16 * 1024 * 1024),
        None,
    )
    .unwrap();
    assert_eq!(recordings.len(), 1);
    let archive = recordings.remove(0).1;

    // Each series over the whole recording: its labels and its values.
    let (start, end) = archive.time_range().unwrap();
    let series = |query: &str| -> Vec<(BTreeMap<String, String>, Vec<f64>)> {
        let QueryResult::Matrix { result } = archive.query_range(query, start, end, 1.0).unwrap()
        else {
            panic!("{query}: not a matrix")
        };
        let mut out: Vec<_> = result
            .into_iter()
            .map(|s| {
                (
                    s.metric.into_iter().collect::<BTreeMap<_, _>>(),
                    s.values.into_iter().map(|(_, v)| v).collect(),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    };

    // Counters are read as rates: 10 requests and 100 bytes per 1 s tick.
    let requests = series("rate(stream_requests[2s])");
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0].1.iter().all(|v| (v - 10.0).abs() < 1e-6),
        "{requests:?}"
    );

    // Two members, each with its labels and uid; the one that left stops.
    let tenants = series("rate(stream_tenant_bytes[2s])");
    assert_eq!(tenants.len(), 2, "{tenants:?}");
    let (acme_series, globex_series) = (&tenants[0], &tenants[1]);
    let tenant = |s: &(BTreeMap<String, String>, Vec<f64>)| s.0.get("tenant").cloned();
    let (acme_series, globex_series) = if tenant(acme_series).as_deref() == Some("acme") {
        (acme_series, globex_series)
    } else {
        (globex_series, acme_series)
    };
    assert_eq!(tenant(acme_series).as_deref(), Some("acme"));
    assert_eq!(tenant(globex_series).as_deref(), Some("globex"));
    for s in [acme_series, globex_series] {
        assert!(s.0.contains_key(metriken::group::UID_LABEL), "{s:?}");
    }
    assert_ne!(
        acme_series.0[metriken::group::UID_LABEL],
        globex_series.0[metriken::group::UID_LABEL]
    );
    assert!(acme_series.1.iter().all(|v| (v - 100.0).abs() < 1e-6));
    assert!(
        globex_series.1.len() < acme_series.1.len(),
        "globex has no samples after it left: {tenants:?}"
    );
    drop(acme);
}
