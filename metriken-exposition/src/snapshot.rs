use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use metriken::Window;

#[cfg(feature = "msgpack")]
use rmp_serde::encode::Error as SerializeMsgpackError;
#[cfg(feature = "json")]
use serde_json::Error as JsonError;

// These carry all-public fields but are `#[non_exhaustive]` so future
// per-observation fields (beyond `window`) can be added without breaking
// downstream construction. Build them with `new(..)` + `with_window(..)`
// rather than a struct literal.

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct Counter {
    pub name: String,
    pub value: u64,
    pub metadata: HashMap<String, String>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub window: Option<Window>,
}

impl Counter {
    /// A counter with no acquisition window. Add one with [`with_window`](Self::with_window).
    pub fn new(name: String, value: u64, metadata: HashMap<String, String>) -> Self {
        Self {
            name,
            value,
            metadata,
            window: None,
        }
    }

    /// Attach (or clear) the acquisition window.
    pub fn with_window(mut self, window: Option<Window>) -> Self {
        self.window = window;
        self
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct Gauge {
    pub name: String,
    pub value: i64,
    pub metadata: HashMap<String, String>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub window: Option<Window>,
}

impl Gauge {
    /// A gauge with no acquisition window. Add one with [`with_window`](Self::with_window).
    pub fn new(name: String, value: i64, metadata: HashMap<String, String>) -> Self {
        Self {
            name,
            value,
            metadata,
            window: None,
        }
    }

    /// Attach (or clear) the acquisition window.
    pub fn with_window(mut self, window: Option<Window>) -> Self {
        self.window = window;
        self
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub struct Histogram {
    pub name: String,
    pub value: histogram::Histogram,
    pub metadata: HashMap<String, String>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub window: Option<Window>,
}

impl Histogram {
    /// A histogram with no acquisition window. Add one with [`with_window`](Self::with_window).
    pub fn new(
        name: String,
        value: histogram::Histogram,
        metadata: HashMap<String, String>,
    ) -> Self {
        Self {
            name,
            value,
            metadata,
            window: None,
        }
    }

    /// Attach (or clear) the acquisition window.
    pub fn with_window(mut self, window: Option<Window>) -> Self {
        self.window = window;
        self
    }
}

/// Contains a snapshot of metric readings.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SnapshotV1 {
    pub systemtime: SystemTime,

    #[cfg_attr(feature = "serde", serde(default))]
    pub metadata: HashMap<String, String>,

    pub counters: Vec<Counter>,
    pub gauges: Vec<Gauge>,
    pub histograms: Vec<Histogram>,
}

/// Contains a snapshot of metric readings.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SnapshotV2 {
    pub systemtime: SystemTime,
    pub duration: Duration,

    #[cfg_attr(feature = "serde", serde(default))]
    pub metadata: HashMap<String, String>,

    pub counters: Vec<Counter>,
    pub gauges: Vec<Gauge>,
    pub histograms: Vec<Histogram>,
}

/// One metric's identity within a [`GroupSchema`]: its column key (the
/// snapshot entry name, e.g. `"5"` / `"5x3"`) plus its annotations
/// (`metric`, `sampler`, labels, and for histograms `grouping_power` /
/// `max_value_power`). Metadata is a `BTreeMap` so serialization — and
/// therefore the group schema hash — is deterministic.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MetricDesc {
    pub name: String,
    pub metadata: std::collections::BTreeMap<String, String>,
}

/// The membership of one acquisition group: descriptors for every counter,
/// gauge and histogram slot, in the order the value arrays use.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GroupSchema {
    pub counters: Vec<MetricDesc>,
    pub gauges: Vec<MetricDesc>,
    pub histograms: Vec<MetricDesc>,
}

/// One acquisition group's readings for one tick.
///
/// The value vectors align positionally with the group's [`GroupSchema`];
/// `None` means "member registered but no reading this tick".
///
/// Membership comes from REGISTRATION, not from values. V2 producers used
/// value sentinels to detect membership (skip counters at 0, gauges at
/// `i64::MIN`) — schema information smuggled through the data channel,
/// which made per-tick membership a function of the data. In V3 the schema
/// is what the producer declares (real CPUs, live cgroups/tasks, present
/// devices): unpopulated dense slots are never members at all, a
/// registered-but-quiet counter sends its zero (1 byte) so its first
/// increment does not change the schema, and the schema (hence its hash)
/// changes only on true membership events.
///
/// Serialization note: every field is serialized unconditionally (an absent
/// `Option` is msgpack nil). Do not add `skip_serializing_if` to any field —
/// the untagged [`Snapshot`] enum distinguishes versions by positional
/// shape, and variable arity would break decoding.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GroupSnapshot {
    /// Stable group identity, `"<sampler>/<group>"` (e.g. `"cpu_usage/percpu"`).
    pub name: String,
    /// FNV-1a-128 of the group's canonical schema encoding, split
    /// `(hi, lo)` because msgpack has no 128-bit integer. Content-addressed:
    /// receivers cache parsed schemas by `(name, schema_hash)`, so it
    /// survives agent restarts with no generation counter.
    pub schema_hash: (u64, u64),
    /// The membership. Producers may always include it (stateless payloads);
    /// receivers skip parsing on a hash match. Correctness must never depend
    /// on it being omitted.
    pub schema: Option<GroupSchema>,
    /// The acquisition window shared by every member, or `None` for a
    /// windowless (derived/ambient) group.
    pub window: Option<Window>,
    pub counters: Vec<Option<u64>>,
    pub gauges: Vec<Option<i64>>,
    pub histograms: Vec<Option<histogram::Histogram>>,
}

/// Contains a snapshot of metric readings organized by acquisition group.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SnapshotV3 {
    pub systemtime: SystemTime,
    pub duration: Duration,

    #[cfg_attr(feature = "serde", serde(default))]
    pub metadata: HashMap<String, String>,

    pub groups: Vec<GroupSnapshot>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum Snapshot {
    V1(SnapshotV1),
    V2(SnapshotV2),
    V3(SnapshotV3),
}

#[cfg(feature = "parquet")]
pub(crate) struct HashedSnapshot {
    pub(crate) ts: u64,
    pub(crate) duration: Option<u64>,
    pub(crate) counters: HashMap<String, Counter>,
    pub(crate) gauges: HashMap<String, Gauge>,
    pub(crate) histograms: HashMap<String, Histogram>,
}

impl Snapshot {
    pub fn systemtime(&self) -> SystemTime {
        match self {
            Snapshot::V1(s) => s.systemtime,
            Snapshot::V2(s) => s.systemtime,
            Snapshot::V3(s) => s.systemtime,
        }
    }

    pub fn duration(&self) -> Option<Duration> {
        match self {
            Snapshot::V1(_) => None,
            Snapshot::V2(s) => Some(s.duration),
            Snapshot::V3(s) => Some(s.duration),
        }
    }

    pub fn metadata(&mut self) -> HashMap<String, String> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.metadata),
            Snapshot::V2(s) => std::mem::take(&mut s.metadata),
            Snapshot::V3(s) => std::mem::take(&mut s.metadata),
        }
    }

    pub fn counters(&mut self) -> Vec<Counter> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.counters),
            Snapshot::V2(s) => std::mem::take(&mut s.counters),
            // Task 3 replaces this stub with group expansion.
            Snapshot::V3(_) => Vec::new(),
        }
    }

    pub fn gauges(&mut self) -> Vec<Gauge> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.gauges),
            Snapshot::V2(s) => std::mem::take(&mut s.gauges),
            // Task 3 replaces this stub with group expansion.
            Snapshot::V3(_) => Vec::new(),
        }
    }

    pub fn histograms(&mut self) -> Vec<Histogram> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.histograms),
            Snapshot::V2(s) => std::mem::take(&mut s.histograms),
            // Task 3 replaces this stub with group expansion.
            Snapshot::V3(_) => Vec::new(),
        }
    }

    #[cfg(feature = "json")]
    pub fn to_json<T>(val: &T) -> Result<Vec<u8>, JsonError>
    where
        T: serde::Serialize + ?Sized,
    {
        let mut res = serde_json::to_vec(val)?;
        res.push(b'\n');
        Ok(res)
    }

    #[cfg(feature = "msgpack")]
    pub fn to_msgpack<T>(val: &T) -> Result<Vec<u8>, SerializeMsgpackError>
    where
        T: serde::Serialize + ?Sized,
    {
        rmp_serde::encode::to_vec(val)
    }
}

#[cfg(feature = "parquet")]
impl From<Snapshot> for HashedSnapshot {
    fn from(mut snapshot: Snapshot) -> Self {
        let ts: u64 = snapshot
            .systemtime()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("System Clock is earlier than 1970; needs reset")
            .as_nanos() as u64;

        let duration: Option<u64> = snapshot.duration().map(|x| x.as_nanos() as u64);

        let counters: HashMap<String, Counter> =
            HashMap::from_iter(snapshot.counters().into_iter().map(|v| (v.name.clone(), v)));
        let gauges: HashMap<String, Gauge> =
            HashMap::from_iter(snapshot.gauges().into_iter().map(|v| (v.name.clone(), v)));
        let histograms: HashMap<String, Histogram> = HashMap::from_iter(
            snapshot
                .histograms()
                .into_iter()
                .map(|v| (v.name.clone(), v)),
        );

        Self {
            ts,
            duration,
            counters,
            gauges,
            histograms,
        }
    }
}

#[cfg(all(test, feature = "json"))]
mod window_tests {
    use super::*;
    use metriken::Window;

    fn gauge(window: Option<Window>) -> Gauge {
        Gauge {
            name: "g".into(),
            value: 42,
            metadata: HashMap::new(),
            window,
        }
    }

    #[test]
    fn window_absent_is_omitted_and_roundtrips() {
        let json = serde_json::to_string(&gauge(None)).unwrap();
        assert!(
            !json.contains("window"),
            "absent window must be skipped: {json}"
        );
        let back: Gauge = serde_json::from_str(&json).unwrap();
        assert!(back.window.is_none());
    }

    #[test]
    fn window_present_roundtrips() {
        let g = gauge(Some(Window::new(100, 250)));
        let json = serde_json::to_string(&g).unwrap();
        let back: Gauge = serde_json::from_str(&json).unwrap();
        assert_eq!(back.window, Some(Window::new(100, 250)));
    }

    #[test]
    fn v2_bytes_without_window_still_deserialize() {
        let old = r#"{"name":"g","value":42,"metadata":{}}"#;
        let g: Gauge = serde_json::from_str(old).unwrap();
        assert!(g.window.is_none());
    }

    #[cfg(feature = "msgpack")]
    #[test]
    fn window_msgpack_roundtrip() {
        for w in [None, Some(Window::new(1, 2))] {
            let bytes = rmp_serde::to_vec(&gauge(w)).unwrap();
            let back: Gauge = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(back.window, w);
        }
    }
}

#[cfg(all(test, feature = "msgpack"))]
mod v3_tests {
    use super::*;
    use metriken::Window;

    fn desc(name: &str, metric: &str) -> MetricDesc {
        MetricDesc {
            name: name.to_string(),
            metadata: [("metric".to_string(), metric.to_string())].into(),
        }
    }

    fn v3() -> SnapshotV3 {
        SnapshotV3 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            duration: Duration::from_millis(10),
            metadata: [("source".to_string(), "rezolus".to_string())].into(),
            groups: vec![GroupSnapshot {
                name: "cpu_usage/percpu".to_string(),
                schema_hash: (1, 2),
                schema: Some(GroupSchema {
                    counters: vec![desc("0", "cpu_cycles"), desc("1", "cpu_instructions")],
                    gauges: vec![],
                    histograms: vec![],
                }),
                window: Some(Window::new(999_000, 999_400)),
                counters: vec![Some(7), None],
                gauges: vec![],
                histograms: vec![],
            }],
        }
    }

    fn v2() -> SnapshotV2 {
        SnapshotV2 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            duration: Duration::from_millis(10),
            metadata: HashMap::new(),
            counters: vec![Counter::new("0".to_string(), 7, HashMap::new())],
            gauges: vec![],
            histograms: vec![],
        }
    }

    fn v1() -> SnapshotV1 {
        SnapshotV1 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            metadata: HashMap::new(),
            counters: vec![Counter::new("0".to_string(), 7, HashMap::new())],
            gauges: vec![],
            histograms: vec![],
        }
    }

    #[test]
    fn v3_roundtrips_through_msgpack() {
        let bytes = Snapshot::to_msgpack(&Snapshot::V3(v3())).unwrap();
        let back: Snapshot = rmp_serde::from_slice(&bytes).unwrap();
        let Snapshot::V3(s) = back else {
            panic!("V3 bytes decoded as a different version")
        };
        assert_eq!(s.groups.len(), 1);
        let g = &s.groups[0];
        assert_eq!(g.name, "cpu_usage/percpu");
        assert_eq!(g.schema_hash, (1, 2));
        assert_eq!(g.counters, vec![Some(7), None]);
        assert_eq!(g.window, Some(Window::new(999_000, 999_400)));
        assert_eq!(
            g.schema.as_ref().unwrap().counters[1]
                .metadata
                .get("metric"),
            Some(&"cpu_instructions".to_string())
        );
    }

    #[test]
    fn each_version_decodes_as_itself() {
        // The untagged enum distinguishes versions by positional shape.
        // These pins are the compatibility contract: V1/V2 bytes produced by
        // old agents must keep decoding as V1/V2 after V3 exists, and V3
        // bytes must never mis-decode as an older version.
        let b1 = Snapshot::to_msgpack(&Snapshot::V1(v1())).unwrap();
        let b2 = Snapshot::to_msgpack(&Snapshot::V2(v2())).unwrap();
        let b3 = Snapshot::to_msgpack(&Snapshot::V3(v3())).unwrap();
        assert!(matches!(
            rmp_serde::from_slice::<Snapshot>(&b1).unwrap(),
            Snapshot::V1(_)
        ));
        assert!(matches!(
            rmp_serde::from_slice::<Snapshot>(&b2).unwrap(),
            Snapshot::V2(_)
        ));
        assert!(matches!(
            rmp_serde::from_slice::<Snapshot>(&b3).unwrap(),
            Snapshot::V3(_)
        ));
    }

    #[test]
    fn empty_option_fields_keep_fixed_arity() {
        // A group with no schema and no window must still round-trip: every
        // field serializes (nil for absent Options), never skip_serializing_if.
        let mut s = v3();
        s.groups[0].schema = None;
        s.groups[0].window = None;
        let bytes = Snapshot::to_msgpack(&Snapshot::V3(s)).unwrap();
        let back: Snapshot = rmp_serde::from_slice(&bytes).unwrap();
        let Snapshot::V3(s) = back else {
            panic!("wrong version")
        };
        assert!(s.groups[0].schema.is_none());
        assert!(s.groups[0].window.is_none());
        assert_eq!(s.groups[0].counters, vec![Some(7), None]);
    }
}
