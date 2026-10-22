//! The gauge histogram fields on the wire: absent from the encoding when
//! empty, so a row or snapshot without one decodes with the types as they
//! were before the fields (copied below as `Old*`); present, an array one
//! longer, which those types refuse. The published releases are checked the
//! same way in `docs/journal/2026-10-21-gauge-histogram.md`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use metriken_model::wal::{
    decode_wal_group_row, encode_wal_group_row, encode_wal_long_row, encode_wal_row, LongOccupant,
    WalCell, WalGroupRow, WalLongRow, WalValue,
};
use metriken_model::{
    GroupSchema, GroupSnapshot, Histogram, MetricDesc, Snapshot, SnapshotV1, SnapshotV2, SnapshotV3,
};
use serde::{Deserialize, Serialize};

// ── The types as they were (metriken-model 0.1.3) ──

#[derive(Serialize, Deserialize, Debug)]
struct OldSchema {
    counters: Vec<MetricDesc>,
    gauges: Vec<MetricDesc>,
    histograms: Vec<MetricDesc>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldGroupRow {
    schema_hash: (u64, u64),
    schema: Option<OldSchema>,
    window: Option<(u64, u64)>,
    counters: Vec<Option<u64>>,
    gauges: Vec<Option<i64>>,
    histograms: Vec<Option<(u8, u8, Vec<u64>)>>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldOccupant {
    occupant: u64,
    counters: Vec<Option<u64>>,
    gauges: Vec<Option<i64>>,
    histograms: Vec<Option<(u8, u8, Vec<u64>)>>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldLongRow {
    schema_hash: (u64, u64),
    schema: Option<OldSchema>,
    window: Option<(u64, u64)>,
    occupants: Vec<OldOccupant>,
}

#[derive(Serialize, Deserialize, Debug)]
enum OldValue {
    Counter(u64),
    Gauge(i64),
    Histogram(u8, u8, Vec<u64>),
}

#[derive(Serialize, Deserialize, Debug)]
struct OldCell {
    name: String,
    metadata: Option<BTreeMap<String, String>>,
    value: OldValue,
    window: Option<(u64, u64)>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldGroup {
    name: String,
    schema_hash: (u64, u64),
    schema: Option<Arc<OldSchema>>,
    window: Option<metriken_model::Window>,
    counters: Vec<Option<u64>>,
    gauges: Vec<Option<i64>>,
    histograms: Vec<Option<histogram::Histogram>>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldV1 {
    systemtime: SystemTime,
    #[serde(default)]
    metadata: HashMap<String, String>,
    counters: Vec<metriken_model::Counter>,
    gauges: Vec<metriken_model::Gauge>,
    histograms: Vec<Histogram>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldV2 {
    systemtime: SystemTime,
    duration: Duration,
    #[serde(default)]
    metadata: HashMap<String, String>,
    counters: Vec<metriken_model::Counter>,
    gauges: Vec<metriken_model::Gauge>,
    histograms: Vec<Histogram>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldV3 {
    systemtime: SystemTime,
    duration: Duration,
    #[serde(default)]
    metadata: HashMap<String, String>,
    groups: Vec<OldGroup>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
enum OldSnapshot {
    V1(OldV1),
    V2(OldV2),
    V3(OldV3),
}

// ── Fixtures ──

fn desc(name: &str) -> MetricDesc {
    MetricDesc {
        name: name.to_string(),
        metadata: [("metric".to_string(), name.to_string())].into(),
    }
}

fn h2() -> histogram::Histogram {
    let mut h = histogram::Histogram::new(0, 8).unwrap();
    h.as_mut_slice()[1] = 3;
    h
}

fn schema(gauge: bool) -> GroupSchema {
    GroupSchema {
        counters: vec![desc("c")],
        gauges: Vec::new(),
        histograms: vec![desc("lat")],
        gauge_histograms: if gauge {
            vec![desc("free")]
        } else {
            Vec::new()
        },
    }
}

fn cell() -> Option<(u8, u8, Vec<u64>)> {
    Some((0, 8, h2().as_slice().to_vec()))
}

fn group_row(gauge: bool) -> WalGroupRow {
    WalGroupRow {
        schema_hash: schema(gauge).hash(),
        schema: Some(schema(gauge)),
        window: Some((1, 2)),
        counters: vec![Some(1)],
        gauges: Vec::new(),
        histograms: vec![cell()],
        gauge_histograms: if gauge { vec![cell()] } else { Vec::new() },
    }
}

fn hist(name: &str) -> Histogram {
    Histogram::new(name.to_string(), h2(), HashMap::new())
}

fn snapshots(gauge: bool) -> [Snapshot; 3] {
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let gauges = || {
        if gauge {
            vec![hist("free")]
        } else {
            Vec::new()
        }
    };
    [
        Snapshot::V1(SnapshotV1 {
            systemtime: t,
            metadata: HashMap::new(),
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: vec![hist("lat")],
            gauge_histograms: gauges(),
        }),
        Snapshot::V2(SnapshotV2 {
            systemtime: t,
            duration: Duration::from_millis(1),
            metadata: HashMap::new(),
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: vec![hist("lat")],
            gauge_histograms: gauges(),
        }),
        Snapshot::V3(SnapshotV3 {
            systemtime: t,
            duration: Duration::from_millis(1),
            metadata: HashMap::new(),
            groups: vec![GroupSnapshot {
                name: "fs/ext4".to_string(),
                schema_hash: schema(gauge).hash(),
                schema: Some(Arc::new(schema(gauge))),
                window: None,
                counters: vec![Some(1)],
                gauges: Vec::new(),
                histograms: vec![Some(h2())],
                gauge_histograms: if gauge { vec![Some(h2())] } else { Vec::new() },
            }],
        }),
    ]
}

// ── Tests ──

/// Without a gauge histogram the schema's encoding, and so its hash, is
/// the old type's.
#[test]
fn a_schema_without_one_encodes_as_before() {
    let old = OldSchema {
        counters: vec![desc("c")],
        gauges: Vec::new(),
        histograms: vec![desc("lat")],
    };
    assert_eq!(
        rmp_serde::to_vec(&schema(false)).unwrap(),
        rmp_serde::to_vec(&old).unwrap()
    );
    let back: GroupSchema = rmp_serde::from_slice(&rmp_serde::to_vec(&old).unwrap()).unwrap();
    assert_eq!(back, schema(false));
}

#[test]
fn rows_without_one_decode_with_the_old_types() {
    let bytes = encode_wal_group_row(&group_row(false)).unwrap();
    let old: OldGroupRow = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(old.histograms.len(), 1);
    let long = WalLongRow {
        schema_hash: (1, 2),
        schema: Some(schema(false)),
        window: None,
        occupants: vec![LongOccupant {
            occupant: 0,
            counters: vec![Some(1)],
            gauges: Vec::new(),
            histograms: vec![cell()],
            gauge_histograms: Vec::new(),
        }],
    };
    let _: OldLongRow = rmp_serde::from_slice(&encode_wal_long_row(&long).unwrap()).unwrap();
    let cells = vec![WalCell {
        name: "7".to_string(),
        metadata: None,
        value: WalValue::Histogram(0, 8, vec![0; 9]),
        window: None,
    }];
    let _: Vec<OldCell> = rmp_serde::from_slice(&encode_wal_row(&cells).unwrap()).unwrap();
    for s in snapshots(false) {
        let bytes = rmp_serde::to_vec(&s).unwrap();
        let old: OldSnapshot = rmp_serde::from_slice(&bytes).unwrap();
        // The same version, not another one the shapes happen to fit.
        let same = matches!(
            (&s, &old),
            (Snapshot::V1(_), OldSnapshot::V1(_))
                | (Snapshot::V2(_), OldSnapshot::V2(_))
                | (Snapshot::V3(_), OldSnapshot::V3(_))
        );
        assert!(same, "{old:?}");
    }
}

/// With a gauge histogram every carrier is refused by the old types, and
/// read back whole by the new ones.
#[test]
fn rows_with_one_are_refused_by_the_old_types() {
    let bytes = encode_wal_group_row(&group_row(true)).unwrap();
    let err = rmp_serde::from_slice::<OldGroupRow>(&bytes).unwrap_err();
    assert!(err.to_string().contains("length"), "{err}");
    assert_eq!(decode_wal_group_row(&bytes).unwrap(), group_row(true));

    let err =
        rmp_serde::from_slice::<OldSchema>(&rmp_serde::to_vec(&schema(true)).unwrap()).unwrap_err();
    assert!(err.to_string().contains("length"), "{err}");

    let long = WalLongRow {
        schema_hash: (1, 2),
        schema: None,
        window: None,
        occupants: vec![LongOccupant {
            occupant: 0,
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: Vec::new(),
            gauge_histograms: vec![cell()],
        }],
    };
    assert!(rmp_serde::from_slice::<OldLongRow>(&encode_wal_long_row(&long).unwrap()).is_err());

    let cells = vec![WalCell {
        name: "7".to_string(),
        metadata: None,
        value: WalValue::GaugeHistogram(0, 8, vec![0; 9]),
        window: None,
    }];
    assert!(rmp_serde::from_slice::<Vec<OldCell>>(&encode_wal_row(&cells).unwrap()).is_err());

    for s in snapshots(true) {
        let bytes = rmp_serde::to_vec(&s).unwrap();
        assert!(
            rmp_serde::from_slice::<OldSnapshot>(&bytes).is_err(),
            "an old decoder read {s:?}"
        );
        let mut back = Snapshot::from_msgpack(&bytes).unwrap();
        assert_eq!(back.gauge_histograms().len(), 1);
        assert_eq!(back.histograms().len(), 1);
    }
}

/// The splice that adds a schema to a schema-less group row keeps working
/// on a row carrying gauge histograms.
#[test]
fn the_schema_splice_takes_a_longer_row() {
    let mut bare = group_row(true);
    bare.schema = None;
    let bytes = encode_wal_group_row(&bare).unwrap();
    let spliced =
        metriken_model::wal::encode_wal_group_row_with_schema(&bytes, &schema(true)).unwrap();
    assert_eq!(decode_wal_group_row(&spliced).unwrap(), group_row(true));
}
