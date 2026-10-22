//! GO check 6 of `docs/journal/2026-10-11-stream-route.md`, the wide half: a
//! `CounterHistogramGroup` through `GroupBuilder::build_groups` gives the members
//! and buckets `Snapshotter` gives. The long half, decoded by
//! metriken-storage's `StreamDecoder`, is storage's `stream_histogram` test.
//!
//! Its own test binary, so the registry holds only these metrics.

#![cfg(feature = "msgpack")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use metriken::metric;
use metriken_exposition::group_builder::{DefaultRouter, GroupBuilder, StreamGroup};
use metriken_exposition::{Snapshot, Snapshotter};

#[metric(name = "hg_latency", metadata = { acq_group = "latency" })]
static LATENCY: metriken::CounterHistogramGroup = metriken::CounterHistogramGroup::new(4, 4, 10);

#[metric(name = "hg_long_latency", metadata = { acq_group = "long" })]
static LONG_LATENCY: metriken::CounterHistogramGroup =
    metriken::CounterHistogramGroup::new(4, 4, 10);

/// Indices 0 and 2 carry metadata and hold values; index 1 holds values and
/// no metadata, so neither path makes it a member; index 3 is empty.
fn record(group: &metriken::CounterHistogramGroup) {
    group.set_metadata(0, HashMap::from([("op".to_string(), "read".to_string())]));
    group.set_metadata(2, HashMap::from([("op".to_string(), "write".to_string())]));
    for v in [1, 5, 9, 200] {
        group.increment(0, v).unwrap();
    }
    for v in [3, 300] {
        group.increment(1, v).unwrap();
        group.increment(2, v * 2).unwrap();
    }
}

/// `(op label, buckets)` by member name, from the snapshotter.
fn snapshotter() -> BTreeMap<String, (Option<String>, Vec<u64>)> {
    let Snapshot::V1(v1) = Snapshotter::default().snapshot() else {
        panic!("a V1 snapshot")
    };
    v1.histograms
        .into_iter()
        .filter(|h| h.metadata.get("metric").map(String::as_str) == Some("hg_latency"))
        .map(|h| {
            (
                h.name,
                (h.metadata.get("op").cloned(), h.value.as_slice().to_vec()),
            )
        })
        .collect()
}

#[test]
fn a_histogram_group_through_build_groups_matches_the_snapshotter() {
    record(&LATENCY);
    let expected = snapshotter();
    assert_eq!(expected.len(), 2, "{expected:?}");

    let mut builder = GroupBuilder::new(DefaultRouter::new("hg"));
    let mut schemas = Vec::new();
    for build in 0..2 {
        let groups = builder.build_groups(Vec::new());
        let group = groups
            .iter()
            .find(|g| g.name == "hg/latency")
            .expect("the histogram group's group");
        let schema = group.schema.as_ref().unwrap();
        schemas.push(Arc::clone(schema));
        assert!(schema.counters.is_empty() && schema.gauges.is_empty());
        let got: BTreeMap<String, (Option<String>, Vec<u64>)> = schema
            .histograms
            .iter()
            .zip(&group.histograms)
            .map(|(desc, h)| {
                assert_eq!(desc.metadata.get("grouping_power").unwrap(), "4");
                assert_eq!(desc.metadata.get("max_value_power").unwrap(), "10");
                assert_eq!(
                    desc.metadata.get("id"),
                    desc.name.rsplit('x').next().map(str::to_string).as_ref(),
                    "the member's id is its index"
                );
                (
                    desc.name.clone(),
                    (
                        desc.metadata.get("op").cloned(),
                        h.as_ref().expect("a loaded member").as_slice().to_vec(),
                    ),
                )
            })
            .collect();
        assert_eq!(got, expected, "build {build}");
        group.validate().unwrap();
    }
    assert!(
        Arc::ptr_eq(&schemas[0], &schemas[1]),
        "the second build reuses the schema"
    );

    // A new member's metadata rebuilds the schema.
    LATENCY.set_metadata(3, HashMap::from([("op".to_string(), "sync".to_string())]));
    let groups = builder.build_groups(Vec::new());
    let group = groups.iter().find(|g| g.name == "hg/latency").unwrap();
    assert_eq!(group.histograms.len(), 3);
    assert!(!Arc::ptr_eq(&schemas[0], group.schema.as_ref().unwrap()));
    LATENCY.clear_metadata(3);
}

#[test]
fn a_histogram_group_is_sent_long() {
    record(&LONG_LATENCY);
    let mut builder = GroupBuilder::new(DefaultRouter::new("hg"));
    let groups = builder.build_stream(Vec::new());
    let Some(StreamGroup::Long(group)) = groups.iter().find(|g| g.name() == "hg/long") else {
        panic!("the histogram group is long: {groups:?}")
    };
    assert_eq!(group.columns.histograms.len(), 1);
    let ids: Vec<&str> = group
        .occupants
        .iter()
        .map(|o| o.labels.get("id").unwrap().as_str())
        .collect();
    assert_eq!(ids, ["0", "2"]);
    for o in &group.occupants {
        let idx: usize = o.labels["id"].parse().unwrap();
        assert_eq!(
            o.histograms[0].as_ref().unwrap().as_slice(),
            LONG_LATENCY.load(idx).unwrap().as_slice()
        );
    }
}
