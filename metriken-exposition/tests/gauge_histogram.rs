//! A `GaugeHistogram` through every exposition path: the snapshotter and the
//! group builder mark it `metric_type = gauge_histogram`, and leave a
//! counting histogram unmarked; Prometheus text writes it as gauges.
//!
//! Its own test binary, so the registry holds only these metrics.

#![cfg(feature = "msgpack")]

use metriken::{metric, AtomicCounterHistogram, GaugeHistogram, Window};
use metriken_exposition::group_builder::{DefaultRouter, GroupBuilder};
use metriken_exposition::{prometheus_text, PrometheusOptions, Snapshot, Snapshotter};

#[metric(name = "gh_free_extents", metadata = { acq_group = "ext4" })]
static FREE_EXTENTS: GaugeHistogram = GaugeHistogram::new(0, 8);

#[metric(name = "gh_latency", metadata = { acq_group = "ext4" })]
static LATENCY: AtomicCounterHistogram = AtomicCounterHistogram::new(0, 8);

/// 3 extents of size 1, 2 of size 2 or 3, 1 of size 4 to 7.
fn set_state() -> Vec<u64> {
    let mut buckets = vec![0u64; FREE_EXTENTS.config().total_buckets()];
    buckets[1] = 3;
    buckets[2] = 2;
    buckets[3] = 1;
    FREE_EXTENTS
        .set_with_window(&buckets, Window::new(100, 200))
        .unwrap();
    LATENCY.increment(5).unwrap();
    buckets
}

#[test]
fn the_snapshotter_lists_a_gauge_histogram_apart() {
    let buckets = set_state();
    let Snapshot::V1(v1) = Snapshotter::default().snapshot() else {
        panic!("a V1 snapshot")
    };
    let named = |list: &[metriken_exposition::Histogram], name: &str| {
        list.iter()
            .filter(|h| h.metadata.get("metric").map(String::as_str) == Some(name))
            .count()
    };
    assert_eq!(named(&v1.histograms, "gh_latency"), 1);
    assert_eq!(named(&v1.histograms, "gh_free_extents"), 0);
    assert_eq!(named(&v1.gauge_histograms, "gh_latency"), 0);
    let [gauge] = v1.gauge_histograms.as_slice() else {
        panic!("one gauge histogram: {:?}", v1.gauge_histograms)
    };
    assert_eq!(gauge.value.as_slice(), buckets.as_slice());
    assert_eq!(gauge.metadata.get("grouping_power").unwrap(), "0");
    assert_eq!(gauge.window, Some(Window::new(100, 200)));
    // The list says the kind; the entry carries no `metric_type`.
    assert_eq!(gauge.metadata.get(metriken_model::METRIC_TYPE), None);
}

/// A V1 snapshot holding a gauge histogram encodes one field longer, so a
/// decoder from before the field (the same struct without it) refuses it;
/// one without stays the old encoding.
#[test]
fn a_snapshot_with_a_gauge_histogram_is_longer() {
    set_state();
    let with = Snapshotter::default().snapshot();
    let Snapshot::V1(mut without) = with.clone() else {
        panic!("a V1 snapshot")
    };
    without.gauge_histograms.clear();
    // A msgpack fixarray: marker 0x90 | length.
    let len = |bytes: &[u8]| {
        assert_eq!(bytes[0] & 0xf0, 0x90);
        bytes[0] & 0x0f
    };
    assert_eq!(len(&rmp_serde::to_vec(&with).unwrap()), 6);
    assert_eq!(len(&rmp_serde::to_vec(&Snapshot::V1(without)).unwrap()), 5);
}

#[test]
fn the_group_builder_lists_a_gauge_histogram_apart() {
    let buckets = set_state();
    let mut builder = GroupBuilder::new(DefaultRouter::new("gh"));
    for build in 0..2 {
        let groups = builder.build_groups(Vec::new());
        let group = groups
            .iter()
            .find(|g| g.name == "gh/ext4")
            .expect("the ext4 group");
        let schema = group.schema.as_ref().unwrap();
        let metrics = |list: &[metriken_exposition::MetricDesc]| -> Vec<String> {
            list.iter().map(|d| d.metadata["metric"].clone()).collect()
        };
        assert_eq!(metrics(&schema.histograms), ["gh_latency"], "build {build}");
        assert_eq!(
            metrics(&schema.gauge_histograms),
            ["gh_free_extents"],
            "build {build}"
        );
        assert_eq!(group.gauge_histograms.len(), 1);
        assert_eq!(
            group.gauge_histograms[0].as_ref().unwrap().as_slice(),
            buckets.as_slice()
        );
        assert_eq!(
            schema.gauge_histograms[0]
                .metadata
                .get(metriken_model::METRIC_TYPE),
            None
        );
        group.validate().unwrap();
    }
}

#[test]
fn prometheus_text_writes_a_gauge_histogram_as_gauges() {
    set_state();
    let body = prometheus_text(&PrometheusOptions::default().without_help());
    let lines: Vec<&str> = body
        .lines()
        .filter(|l| l.contains("gh_free_extents"))
        .collect();
    assert_eq!(
        lines,
        vec![
            "# TYPE gh_free_extents_bucket gauge",
            "gh_free_extents_bucket{le=\"0\"} 0",
            "gh_free_extents_bucket{le=\"1\"} 3",
            "gh_free_extents_bucket{le=\"3\"} 5",
            "gh_free_extents_bucket{le=\"7\"} 6",
            "gh_free_extents_bucket{le=\"15\"} 6",
            "gh_free_extents_bucket{le=\"31\"} 6",
            "gh_free_extents_bucket{le=\"63\"} 6",
            "gh_free_extents_bucket{le=\"127\"} 6",
            "gh_free_extents_bucket{le=\"255\"} 6",
            "gh_free_extents_bucket{le=\"+Inf\"} 6",
            "# TYPE gh_free_extents_gcount gauge",
            "gh_free_extents_gcount 6",
            "# TYPE gh_free_extents_gsum gauge",
            "gh_free_extents_gsum 16",
        ],
        "{body}"
    );
    assert!(
        !body.contains("gaugehistogram"),
        "0.0.4 text has no such type"
    );

    let body = prometheus_text(
        &PrometheusOptions::default()
            .without_help()
            .with_percentiles(vec![0.5, 1.0]),
    );
    let lines: Vec<&str> = body
        .lines()
        .filter(|l| l.contains("gh_free_extents"))
        .collect();
    assert_eq!(
        lines,
        vec![
            "# TYPE gh_free_extents gauge",
            "gh_free_extents{quantile=\"0.5\"} 1",
            "gh_free_extents{quantile=\"1\"} 7",
            "# TYPE gh_free_extents_gcount gauge",
            "gh_free_extents_gcount 6",
            "# TYPE gh_free_extents_gsum gauge",
            // Midpoints, as the summary's `_sum`: 3·1 + 2·2 + 1·5.
            "gh_free_extents_gsum 12",
        ],
        "{body}"
    );
}
