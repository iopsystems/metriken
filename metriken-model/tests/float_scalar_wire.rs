//! The float counter and float gauge fields on the wire.
//!
//! The types as published in metriken-model 0.2.1 are copied below as
//! `Old*`. Without floats a value encodes byte for byte as those types encode
//! it, so schema hashes and existing archives are unchanged. With floats it is
//! a longer array, which those types refuse; in particular, an empty
//! `gauge_histograms` before the floats is written rather than left out, so
//! the floats never land in the position 0.2.1 reads as gauge histograms.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use metriken_model::wal::{
    decode_wal_group_row, decode_wal_long_row, encode_wal_group_row,
    encode_wal_group_row_with_schema, encode_wal_long_row, LongOccupant, WalGroupRow, WalLongRow,
};
use metriken_model::{
    GroupSchema, GroupSnapshot, GroupValidationError, MetricDesc, Snapshot, SnapshotV3, Window,
};
use serde::{Deserialize, Serialize};

// ── The types as they were (metriken-model 0.2.1) ──

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct OldSchema {
    counters: Vec<MetricDesc>,
    gauges: Vec<MetricDesc>,
    histograms: Vec<MetricDesc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    gauge_histograms: Vec<MetricDesc>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldGroupRow {
    schema_hash: (u64, u64),
    schema: Option<OldSchema>,
    window: Option<(u64, u64)>,
    counters: Vec<Option<u64>>,
    gauges: Vec<Option<i64>>,
    histograms: Vec<Option<(u8, u8, Vec<u64>)>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    gauge_histograms: Vec<Option<(u8, u8, Vec<u64>)>>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldOccupant {
    occupant: u64,
    counters: Vec<Option<u64>>,
    gauges: Vec<Option<i64>>,
    histograms: Vec<Option<(u8, u8, Vec<u64>)>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    gauge_histograms: Vec<Option<(u8, u8, Vec<u64>)>>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldLongRow {
    schema_hash: (u64, u64),
    schema: Option<OldSchema>,
    window: Option<(u64, u64)>,
    occupants: Vec<OldOccupant>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldGroup {
    name: String,
    schema_hash: (u64, u64),
    schema: Option<Arc<OldSchema>>,
    window: Option<Window>,
    counters: Vec<Option<u64>>,
    gauges: Vec<Option<i64>>,
    histograms: Vec<Option<histogram::Histogram>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    gauge_histograms: Vec<Option<histogram::Histogram>>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldV1 {
    systemtime: SystemTime,
    #[serde(default)]
    metadata: HashMap<String, String>,
    counters: Vec<metriken_model::Counter>,
    gauges: Vec<metriken_model::Gauge>,
    histograms: Vec<metriken_model::Histogram>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    gauge_histograms: Vec<metriken_model::Histogram>,
}

#[derive(Serialize, Deserialize, Debug)]
struct OldV2 {
    systemtime: SystemTime,
    duration: Duration,
    #[serde(default)]
    metadata: HashMap<String, String>,
    counters: Vec<metriken_model::Counter>,
    gauges: Vec<metriken_model::Gauge>,
    histograms: Vec<metriken_model::Histogram>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    gauge_histograms: Vec<metriken_model::Histogram>,
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

/// The schema as it would be had the float fields been appended the way
/// `gauge_histograms` was, each left out when empty: the encoding the design
/// rejects. Used to show that the cases below are the ones that encoding gets
/// wrong.
#[derive(Serialize)]
struct NaiveSchema {
    counters: Vec<MetricDesc>,
    gauges: Vec<MetricDesc>,
    histograms: Vec<MetricDesc>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    gauge_histograms: Vec<MetricDesc>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    float_counters: Vec<MetricDesc>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    float_gauges: Vec<MetricDesc>,
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

fn cell() -> Option<(u8, u8, Vec<u64>)> {
    Some((0, 8, h2().as_slice().to_vec()))
}

/// Which of the trailing lists a fixture fills.
#[derive(Clone, Copy, Debug)]
struct Fill {
    gauge_histograms: bool,
    float_counters: bool,
    float_gauges: bool,
}

const NONE: Fill = Fill {
    gauge_histograms: false,
    float_counters: false,
    float_gauges: false,
};
const FLOAT_COUNTERS: Fill = Fill {
    float_counters: true,
    ..NONE
};
const FLOAT_GAUGES: Fill = Fill {
    float_gauges: true,
    ..NONE
};
const ALL: Fill = Fill {
    gauge_histograms: true,
    float_counters: true,
    float_gauges: true,
};

/// Every fill that holds a float.
const WITH_FLOATS: [Fill; 5] = [
    FLOAT_COUNTERS,
    FLOAT_GAUGES,
    Fill {
        float_counters: true,
        float_gauges: true,
        ..NONE
    },
    Fill {
        gauge_histograms: true,
        float_counters: true,
        float_gauges: false,
    },
    ALL,
];

fn when<T>(on: bool, v: Vec<T>) -> Vec<T> {
    if on {
        v
    } else {
        Vec::new()
    }
}

fn schema(f: Fill) -> GroupSchema {
    GroupSchema {
        counters: vec![desc("c")],
        gauges: vec![desc("g")],
        histograms: vec![desc("lat")],
        gauge_histograms: when(f.gauge_histograms, vec![desc("free")]),
        float_counters: when(f.float_counters, vec![desc("cpu_seconds"), desc("io")]),
        float_gauges: when(f.float_gauges, vec![desc("cpu_limit")]),
    }
}

fn old_schema(gauge_histograms: bool) -> OldSchema {
    OldSchema {
        counters: vec![desc("c")],
        gauges: vec![desc("g")],
        histograms: vec![desc("lat")],
        gauge_histograms: when(gauge_histograms, vec![desc("free")]),
    }
}

/// Float slots all `None` when `nones`: the shape the naive encoding lets a
/// 0.2.1 decoder read as gauge histogram slots.
fn group_row(f: Fill, nones: bool) -> WalGroupRow {
    let v = |x: f64| if nones { None } else { Some(x) };
    WalGroupRow {
        schema_hash: schema(f).hash(),
        schema: Some(schema(f)),
        window: Some((1, 2)),
        counters: vec![Some(1)],
        gauges: vec![Some(-1)],
        histograms: vec![cell()],
        gauge_histograms: when(f.gauge_histograms, vec![cell()]),
        float_counters: when(f.float_counters, vec![v(0.25), v(1e-9)]),
        float_gauges: when(f.float_gauges, vec![v(-0.5)]),
    }
}

fn old_group_row(gauge_histograms: bool) -> OldGroupRow {
    OldGroupRow {
        schema_hash: schema(Fill {
            gauge_histograms,
            ..NONE
        })
        .hash(),
        schema: Some(old_schema(gauge_histograms)),
        window: Some((1, 2)),
        counters: vec![Some(1)],
        gauges: vec![Some(-1)],
        histograms: vec![cell()],
        gauge_histograms: when(gauge_histograms, vec![cell()]),
    }
}

fn occupant(f: Fill, nones: bool) -> LongOccupant {
    let v = |x: f64| if nones { None } else { Some(x) };
    LongOccupant {
        occupant: 3,
        counters: vec![Some(1)],
        gauges: vec![Some(-1)],
        histograms: vec![cell()],
        gauge_histograms: when(f.gauge_histograms, vec![cell()]),
        float_counters: when(f.float_counters, vec![v(0.25), v(1e-9)]),
        float_gauges: when(f.float_gauges, vec![v(-0.5)]),
    }
}

fn long_row(f: Fill, nones: bool) -> WalLongRow {
    WalLongRow {
        schema_hash: schema(f).hash(),
        schema: Some(schema(f)),
        window: None,
        occupants: vec![occupant(NONE, false), occupant(f, nones)],
    }
}

fn old_long_row(gauge_histograms: bool) -> OldLongRow {
    let occupant = |gh: bool| OldOccupant {
        occupant: 3,
        counters: vec![Some(1)],
        gauges: vec![Some(-1)],
        histograms: vec![cell()],
        gauge_histograms: when(gh, vec![cell()]),
    };
    OldLongRow {
        schema_hash: schema(Fill {
            gauge_histograms,
            ..NONE
        })
        .hash(),
        schema: Some(old_schema(gauge_histograms)),
        window: None,
        occupants: vec![occupant(false), occupant(gauge_histograms)],
    }
}

fn group(f: Fill, nones: bool) -> GroupSnapshot {
    let v = |x: f64| if nones { None } else { Some(x) };
    GroupSnapshot {
        name: "prom/endpoint".to_string(),
        schema_hash: schema(f).hash(),
        schema: Some(Arc::new(schema(f))),
        window: Some(Window::new(10, 20)),
        counters: vec![Some(1)],
        gauges: vec![Some(-1)],
        histograms: vec![Some(h2())],
        gauge_histograms: when(f.gauge_histograms, vec![Some(h2())]),
        float_counters: when(f.float_counters, vec![v(0.25), v(1e-9)]),
        float_gauges: when(f.float_gauges, vec![v(-0.5)]),
    }
}

fn old_group(gauge_histograms: bool) -> OldGroup {
    OldGroup {
        name: "prom/endpoint".to_string(),
        schema_hash: schema(Fill {
            gauge_histograms,
            ..NONE
        })
        .hash(),
        schema: Some(Arc::new(old_schema(gauge_histograms))),
        window: Some(Window::new(10, 20)),
        counters: vec![Some(1)],
        gauges: vec![Some(-1)],
        histograms: vec![Some(h2())],
        gauge_histograms: when(gauge_histograms, vec![Some(h2())]),
    }
}

fn t() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

fn snapshot(f: Fill, nones: bool) -> Snapshot {
    Snapshot::V3(SnapshotV3 {
        systemtime: t(),
        duration: Duration::from_millis(1),
        metadata: HashMap::new(),
        groups: vec![group(f, nones)],
    })
}

fn old_snapshot(gauge_histograms: bool) -> OldSnapshot {
    OldSnapshot::V3(OldV3 {
        systemtime: t(),
        duration: Duration::from_millis(1),
        metadata: HashMap::new(),
        groups: vec![old_group(gauge_histograms)],
    })
}

fn array_len(bytes: &[u8]) -> u32 {
    rmp::decode::read_array_len(&mut &bytes[..]).unwrap()
}

fn assert_length_refusal<E: std::fmt::Display>(r: Result<impl std::fmt::Debug, E>, what: &str) {
    match r {
        Ok(v) => panic!("0.2.1 decoded {what}: {v:?}"),
        Err(e) => assert!(e.to_string().contains("length"), "{what}: {e}"),
    }
}

// ── (a) Without floats: the 0.2.1 encoding, byte for byte ──

#[test]
fn without_floats_every_carrier_encodes_as_0_2_1() {
    for gh in [false, true] {
        let f = Fill {
            gauge_histograms: gh,
            ..NONE
        };
        assert_eq!(
            rmp_serde::to_vec(&schema(f)).unwrap(),
            rmp_serde::to_vec(&old_schema(gh)).unwrap(),
            "schema, gauge histograms {gh}"
        );
        assert_eq!(
            encode_wal_group_row(&group_row(f, false)).unwrap(),
            rmp_serde::to_vec(&old_group_row(gh)).unwrap(),
            "group row, gauge histograms {gh}"
        );
        assert_eq!(
            encode_wal_long_row(&long_row(f, false)).unwrap(),
            rmp_serde::to_vec(&old_long_row(gh)).unwrap(),
            "long row, gauge histograms {gh}"
        );
        assert_eq!(
            rmp_serde::to_vec(&group(f, false)).unwrap(),
            rmp_serde::to_vec(&old_group(gh)).unwrap(),
            "group snapshot, gauge histograms {gh}"
        );
        assert_eq!(
            Snapshot::to_msgpack(&snapshot(f, false)).unwrap(),
            rmp_serde::to_vec(&old_snapshot(gh)).unwrap(),
            "snapshot, gauge histograms {gh}"
        );
    }
}

#[test]
fn without_floats_0_2_1_decodes_it_and_its_bytes_decode_here() {
    for gh in [false, true] {
        let f = Fill {
            gauge_histograms: gh,
            ..NONE
        };
        let old: OldSchema =
            rmp_serde::from_slice(&rmp_serde::to_vec(&schema(f)).unwrap()).unwrap();
        assert_eq!(old, old_schema(gh));
        let back: GroupSchema =
            rmp_serde::from_slice(&rmp_serde::to_vec(&old_schema(gh)).unwrap()).unwrap();
        assert_eq!(back, schema(f));

        let bytes = encode_wal_group_row(&group_row(f, false)).unwrap();
        let old: OldGroupRow = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(old.gauge_histograms.len(), usize::from(gh));
        let old_bytes = rmp_serde::to_vec(&old_group_row(gh)).unwrap();
        assert_eq!(
            decode_wal_group_row(&old_bytes).unwrap(),
            group_row(f, false)
        );

        let bytes = encode_wal_long_row(&long_row(f, false)).unwrap();
        let _: OldLongRow = rmp_serde::from_slice(&bytes).unwrap();
        let old_bytes = rmp_serde::to_vec(&old_long_row(gh)).unwrap();
        assert_eq!(decode_wal_long_row(&old_bytes).unwrap(), long_row(f, false));

        let bytes = Snapshot::to_msgpack(&snapshot(f, false)).unwrap();
        let old: OldSnapshot = rmp_serde::from_slice(&bytes).unwrap();
        assert!(matches!(old, OldSnapshot::V3(_)), "{old:?}");
        let Snapshot::V3(back) =
            Snapshot::from_msgpack(&rmp_serde::to_vec(&old_snapshot(gh)).unwrap()).unwrap()
        else {
            panic!("not V3");
        };
        assert_eq!(back.groups, vec![group(f, false)]);
    }
}

// ── (b) With floats: round-trips here, refused by 0.2.1 ──

/// The control: under the rejected encoding, a schema with float counter
/// descriptors and no gauge histograms, and a row whose float slots are all
/// `None`, decode under 0.2.1 with the floats read as gauge histograms. The
/// tests below use exactly these shapes.
#[test]
fn the_naive_encoding_is_misread_by_0_2_1() {
    let s = schema(FLOAT_COUNTERS);
    let naive = NaiveSchema {
        counters: s.counters.clone(),
        gauges: s.gauges.clone(),
        histograms: s.histograms.clone(),
        gauge_histograms: s.gauge_histograms.clone(),
        float_counters: s.float_counters.clone(),
        float_gauges: s.float_gauges.clone(),
    };
    let old: OldSchema = rmp_serde::from_slice(&rmp_serde::to_vec(&naive).unwrap()).unwrap();
    assert_eq!(old.gauge_histograms, s.float_counters);

    // A row with the float slots in gauge_histograms' position: what
    // leaving out the empty gauge_histograms would write.
    #[derive(Serialize)]
    struct NaiveRow {
        schema_hash: (u64, u64),
        schema: Option<()>,
        window: Option<(u64, u64)>,
        counters: Vec<Option<u64>>,
        gauges: Vec<Option<i64>>,
        histograms: Vec<Option<(u8, u8, Vec<u64>)>>,
        float_counters: Vec<Option<f64>>,
    }
    let naive = NaiveRow {
        schema_hash: (1, 2),
        schema: None,
        window: None,
        counters: Vec::new(),
        gauges: Vec::new(),
        histograms: Vec::new(),
        float_counters: vec![None, None],
    };
    let old: OldGroupRow = rmp_serde::from_slice(&rmp_serde::to_vec(&naive).unwrap()).unwrap();
    assert_eq!(old.gauge_histograms, vec![None, None]);
}

#[test]
fn a_schema_with_floats_round_trips_and_0_2_1_refuses_it() {
    for f in WITH_FLOATS {
        let bytes = rmp_serde::to_vec(&schema(f)).unwrap();
        assert_length_refusal(
            rmp_serde::from_slice::<OldSchema>(&bytes),
            &format!("schema {f:?}"),
        );
        let back: GroupSchema = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, schema(f), "{f:?}");
    }
    // The empty gauge_histograms before the floats is written: 3 required
    // fields, then up to the last non-empty trailing one.
    assert_eq!(
        array_len(&rmp_serde::to_vec(&schema(FLOAT_COUNTERS)).unwrap()),
        5
    );
    assert_eq!(
        array_len(&rmp_serde::to_vec(&schema(FLOAT_GAUGES)).unwrap()),
        6
    );
}

#[test]
fn a_group_row_with_floats_round_trips_and_0_2_1_refuses_it() {
    for f in WITH_FLOATS {
        for nones in [false, true] {
            let row = group_row(f, nones);
            let bytes = encode_wal_group_row(&row).unwrap();
            assert_length_refusal(
                rmp_serde::from_slice::<OldGroupRow>(&bytes),
                &format!("group row {f:?}, nones {nones}"),
            );
            assert_eq!(decode_wal_group_row(&bytes).unwrap(), row, "{f:?} {nones}");
            // Without the schema too, so the refusal is the row's own length
            // and not only the nested schema's.
            let bare = WalGroupRow {
                schema: None,
                ..row.clone()
            };
            let bytes = encode_wal_group_row(&bare).unwrap();
            assert_length_refusal(
                rmp_serde::from_slice::<OldGroupRow>(&bytes),
                &format!("bare group row {f:?}, nones {nones}"),
            );
            assert_eq!(decode_wal_group_row(&bytes).unwrap(), bare);
        }
    }
}

#[test]
fn a_long_row_with_floats_round_trips_and_0_2_1_refuses_it() {
    for f in WITH_FLOATS {
        for nones in [false, true] {
            let row = WalLongRow {
                schema: None,
                ..long_row(f, nones)
            };
            let bytes = encode_wal_long_row(&row).unwrap();
            assert_length_refusal(
                rmp_serde::from_slice::<OldLongRow>(&bytes),
                &format!("long row {f:?}, nones {nones}"),
            );
            assert_eq!(decode_wal_long_row(&bytes).unwrap(), row, "{f:?} {nones}");
        }
    }
}

#[test]
fn a_snapshot_with_floats_round_trips_and_0_2_1_refuses_it() {
    for f in WITH_FLOATS {
        for nones in [false, true] {
            let bytes = Snapshot::to_msgpack(&snapshot(f, nones)).unwrap();
            // An untagged enum names no length; any refusal will do.
            assert!(
                rmp_serde::from_slice::<OldSnapshot>(&bytes).is_err(),
                "0.2.1 decoded the snapshot {f:?}, nones {nones}"
            );
            assert_length_refusal(
                rmp_serde::from_slice::<OldGroup>(&rmp_serde::to_vec(&group(f, nones)).unwrap()),
                &format!("group {f:?}, nones {nones}"),
            );
            let Snapshot::V3(back) = Snapshot::from_msgpack(&bytes).unwrap() else {
                panic!("not V3");
            };
            assert_eq!(back.groups, vec![group(f, nones)]);
        }
    }
}

/// The schema splice gives a row with floats the bytes of encoding it with
/// its schema. (That the splice path, not the decode-and-re-encode fallback,
/// is taken for these lengths is `wal.rs`'s
/// `the_splice_is_taken_for_every_row_length`.)
#[test]
fn the_schema_splice_takes_rows_with_floats() {
    for f in WITH_FLOATS {
        let bare = WalGroupRow {
            schema: None,
            ..group_row(f, false)
        };
        let bytes = encode_wal_group_row(&bare).unwrap();
        let spliced = encode_wal_group_row_with_schema(&bytes, &schema(f)).unwrap();
        assert_eq!(
            spliced,
            encode_wal_group_row(&group_row(f, false)).unwrap(),
            "{f:?}"
        );
    }
}

// ── (c) The schema hash ──

#[test]
fn the_hash_is_unchanged_without_floats_and_changed_with_them() {
    for gh in [false, true] {
        let f = Fill {
            gauge_histograms: gh,
            ..NONE
        };
        assert_eq!(
            schema(f).hash(),
            metriken_model::schema::fnv1a_128(&rmp_serde::to_vec(&old_schema(gh)).unwrap()),
            "gauge histograms {gh}"
        );
    }
    let base = schema(NONE).hash();
    let mut seen = vec![base];
    for f in WITH_FLOATS {
        let h = schema(f).hash();
        assert!(!seen.contains(&h), "{f:?} hashes as another schema");
        seen.push(h);
    }
    // The same descriptor as a float gauge, an integer gauge or a float
    // counter is three schemas.
    let as_kind = |k: usize| {
        let mut s = schema(NONE);
        s.gauges.clear();
        match k {
            0 => s.gauges.push(desc("x")),
            1 => s.float_gauges.push(desc("x")),
            _ => s.float_counters.push(desc("x")),
        }
        s.hash()
    };
    assert_ne!(as_kind(0), as_kind(1));
    assert_ne!(as_kind(1), as_kind(2));
    assert_ne!(as_kind(0), as_kind(2));
}

// ── (d) validate and the accessors ──

#[test]
fn validate_checks_float_arity() {
    assert_eq!(group(ALL, false).validate(), Ok(()));
    let mut g = group(ALL, false);
    g.float_counters.pop();
    assert_eq!(g.validate(), Err(GroupValidationError::ArityMismatch));
    let mut g = group(ALL, false);
    g.float_gauges.push(Some(1.0));
    assert_eq!(g.validate(), Err(GroupValidationError::ArityMismatch));
    // Values with no descriptors.
    let mut g = group(NONE, false);
    g.float_gauges.push(None);
    assert_eq!(g.validate(), Err(GroupValidationError::ArityMismatch));
    // A float descriptor makes a hash that differs from the schema without.
    let mut g = group(FLOAT_GAUGES, false);
    g.schema_hash = schema(NONE).hash();
    assert_eq!(g.validate(), Err(GroupValidationError::SchemaHashMismatch));
}

#[test]
fn the_accessors_read_float_values_by_name() {
    let mut s = snapshot(ALL, false);
    let counters = s.float_counters();
    let named: Vec<(&str, f64)> = counters
        .iter()
        .map(|c| (c.name.as_str(), c.value))
        .collect();
    assert_eq!(named, vec![("cpu_seconds", 0.25), ("io", 1e-9)]);
    assert_eq!(counters[0].window, Some(Window::new(10, 20)));
    assert_eq!(
        counters[0].metadata.get("metric").map(String::as_str),
        Some("cpu_seconds")
    );
    let gauges = s.float_gauges();
    assert_eq!(gauges.len(), 1);
    assert_eq!(
        (gauges[0].name.as_str(), gauges[0].value),
        ("cpu_limit", -0.5)
    );
    // Drained, as the other accessors drain; the integer lists are untouched.
    assert!(s.float_counters().is_empty());
    assert_eq!(s.counters().len(), 1);
    assert_eq!(s.gauges().len(), 1);

    // A None slot is skipped; a group whose float arity disagrees with its
    // schema is skipped whole.
    let mut s = snapshot(ALL, true);
    assert!(s.float_counters().is_empty());
    let mut g = group(ALL, false);
    g.float_counters.pop();
    let mut s = Snapshot::V3(SnapshotV3 {
        systemtime: t(),
        duration: Duration::from_millis(1),
        metadata: HashMap::new(),
        groups: vec![g],
    });
    assert!(s.float_counters().is_empty());
    assert_eq!(s.float_gauges().len(), 1);
}
