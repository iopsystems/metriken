//! Snapshots carrying histograms written by `histogram` 0.8.2 through 0.11.3,
//! which encoded `Config.max` as `2^max_value_power` rather than the current
//! `2^max_value_power - 1`, decode with `histogram` 1.6.3 or later. A
//! histogram with any other `max` fails the snapshot.

use std::collections::HashMap;

use metriken_model::Snapshot;

/// `histogram::Config`'s wire fields in order: max, grouping_power,
/// max_value_power, cutoff_power, cutoff_value, lower_bin_count,
/// upper_bin_divisions, upper_bin_count. These are the derived values for
/// grouping power 4 and max value power 32.
type WireConfig = (u64, u8, u8, u8, u64, u32, u32, u32);

fn config(max: u64) -> WireConfig {
    (max, 4, 32, 5, 32, 32, 16, 432)
}

fn buckets() -> Vec<u64> {
    let mut b = vec![0; 464];
    b[3] = 5;
    b
}

/// A version-1 snapshot (systemtime, metadata, counters, gauges, histograms)
/// with one counter and one histogram, encoded positionally as rmp-serde
/// encodes the struct.
fn snapshot_bytes(histogram_config: WireConfig) -> Vec<u8> {
    let systemtime = (1_700_000_000u64, 0u32);
    let metadata: HashMap<String, String> = HashMap::new();
    let counters = vec![("c".to_string(), 1u64, HashMap::<String, String>::new())];
    let gauges: Vec<(String, i64, HashMap<String, String>)> = Vec::new();
    let histograms = vec![(
        "lat".to_string(),
        (histogram_config, buckets()),
        HashMap::<String, String>::new(),
    )];
    rmp_serde::to_vec(&(systemtime, metadata, counters, gauges, histograms)).unwrap()
}

#[test]
fn current_and_legacy_max_both_decode() {
    let expected = histogram::Histogram::from_buckets(4, 32, buckets()).unwrap();
    for max in [(1u64 << 32) - 1, 1u64 << 32] {
        let mut snapshot = Snapshot::from_msgpack(&snapshot_bytes(config(max)))
            .unwrap_or_else(|e| panic!("max {max}: {e}"));
        let histograms = snapshot.histograms();
        assert_eq!(histograms.len(), 1, "max {max}");
        assert_eq!(histograms[0].value, expected, "max {max}");
        assert_eq!(snapshot.counters().len(), 1, "max {max}");
    }
}

#[test]
fn any_other_inconsistent_max_fails_the_snapshot() {
    let bytes = snapshot_bytes(config(12_345));
    assert!(Snapshot::from_msgpack(&bytes).is_err());
    // `Snapshot` is an untagged enum, whose error names no cause; decoding as
    // the version-1 type shows the failure is the config, not the layout.
    let err = rmp_serde::from_slice::<metriken_model::SnapshotV1>(&bytes).unwrap_err();
    assert!(
        err.to_string().contains("inconsistent derived fields"),
        "{err}"
    );
}
