use std::collections::{BTreeMap, HashMap};
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

// SnapshotV3, GroupSnapshot and GroupSchema deliberately do NOT follow the
// `#[non_exhaustive]` + `new(..)`/`with_window(..)` convention used above for
// Counter/Gauge/Histogram. Their arity is frozen the moment they ship (see
// the arity note on SnapshotV3 and on the Snapshot enum below); advertising
// room to grow via `#[non_exhaustive]` would be false. A future field means
// a V4, not an addition here.

/// One metric's identity within a [`GroupSchema`]: its column key (the
/// snapshot entry name, e.g. `"5"` / `"5x3"`) plus its annotations
/// (`metric`, `sampler`, labels, and for histograms `grouping_power` /
/// `max_value_power`). Metadata is a `BTreeMap` so serialization — and
/// therefore the group schema hash — is deterministic.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MetricDesc {
    pub name: String,
    pub metadata: BTreeMap<String, String>,
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

impl GroupSchema {
    /// FNV-1a-128 over the schema's canonical msgpack encoding, returned as
    /// `(hi, lo)` because msgpack (and rmp-serde) has no 128-bit integer.
    ///
    /// Deterministic because `MetricDesc.metadata` is a `BTreeMap`. Only the
    /// producer computes this; receivers treat it as an opaque cache key —
    /// but the algorithm is still pinned by a known-answer test so hashes
    /// stay comparable across producer versions. 128 bits because a
    /// collision mis-associates an entire group's values with the wrong
    /// schema.
    #[cfg(feature = "msgpack")]
    pub fn hash(&self) -> (u64, u64) {
        const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
        const PRIME: u128 = 0x0000000001000000000000000000013b;
        let bytes =
            rmp_serde::encode::to_vec(self).expect("GroupSchema serialization is infallible");
        let mut h = OFFSET;
        for &b in &bytes {
            h ^= b as u128;
            h = h.wrapping_mul(PRIME);
        }
        ((h >> 64) as u64, h as u64)
    }
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
///
/// Arity is frozen: this struct (and [`GroupSnapshot`]) round-trip through
/// msgpack as positional arrays via the untagged [`Snapshot`] enum. Adding,
/// removing, or reordering a field once this ships breaks decoding for every
/// deployed consumer — extend by adding a `V4` variant, not by editing this
/// struct.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SnapshotV3 {
    pub systemtime: SystemTime,
    pub duration: Duration,

    #[cfg_attr(feature = "serde", serde(default))]
    pub metadata: HashMap<String, String>,

    pub groups: Vec<GroupSnapshot>,
}

/// A versioned snapshot of metric readings.
///
/// # Version detection
///
/// This enum is `#[serde(untagged)]`: under the compact msgpack encoding
/// (`Snapshot::to_msgpack`, i.e. `rmp_serde::to_vec`) each struct serializes
/// positionally as an array, so serde picks the variant by trying each in
/// order and taking the first whose field count/types match. That is a
/// **compact-msgpack-only** contract. Map-keyed encodings — `to_vec_named`,
/// JSON — are NOT a supported input for version detection: a V2 value
/// encoded as a map can mis-decode as V1 there (pre-existing behavior,
/// out of scope for this change). Always decode with `rmp_serde::from_slice`
/// against bytes produced by `Snapshot::to_msgpack`.
///
/// # Deliberately not `#[non_exhaustive]`
///
/// A new wire version must be a compile-time event for every consumer that
/// matches on `Snapshot`. A wildcard arm silently swallowing an unknown
/// version is exactly the failure a version enum exists to prevent, so this
/// type stays exhaustive even though it costs a downstream compile break
/// each time a version is added (as this change itself does).
///
/// # Arity is frozen per version
///
/// Once a version ships, its field count and order can never change:
/// positional decoding means trailing extra fields make an untagged decode
/// attempt fail rather than ignore them. Adding a field to `SnapshotV3` (or
/// `GroupSnapshot`) is a breaking change for every deployed consumer —
/// extensions require a new `V4` variant, not an edit to an existing one.
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

    // V3 expansion semantics (shared by counters()/gauges()/histograms()):
    // each group's value slots are zipped positionally against its
    // `GroupSchema` to recover names/metadata; a `None` slot ("registered
    // but no reading this tick") is skipped entirely, matching V2 semantics
    // where an absent metric is simply not present in the flat list; the
    // group's acquisition `window` (or `None` for a windowless group) is
    // attached to every member it produces; and a group whose `schema` is
    // `None` cannot be named at all, so it expands to nothing rather than
    // fabricating names — the V3-native path resolves that via its schema
    // cache, but the legacy accessors here are only ever fed self-contained
    // payloads.

    pub fn counters(&mut self) -> Vec<Counter> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.counters),
            Snapshot::V2(s) => std::mem::take(&mut s.counters),
            Snapshot::V3(s) => {
                let mut out = Vec::new();
                for g in &mut s.groups {
                    // Take the values before borrowing the schema, or the
                    // mutable take conflicts with the immutable schema
                    // borrow.
                    let values = std::mem::take(&mut g.counters);
                    let Some(schema) = &g.schema else { continue };
                    for (desc, value) in schema.counters.iter().zip(values) {
                        let Some(value) = value else { continue };
                        out.push(
                            Counter::new(
                                desc.name.clone(),
                                value,
                                desc.metadata
                                    .iter()
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect(),
                            )
                            .with_window(g.window),
                        );
                    }
                }
                out
            }
        }
    }

    pub fn gauges(&mut self) -> Vec<Gauge> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.gauges),
            Snapshot::V2(s) => std::mem::take(&mut s.gauges),
            Snapshot::V3(s) => {
                let mut out = Vec::new();
                for g in &mut s.groups {
                    let values = std::mem::take(&mut g.gauges);
                    let Some(schema) = &g.schema else { continue };
                    for (desc, value) in schema.gauges.iter().zip(values) {
                        let Some(value) = value else { continue };
                        out.push(
                            Gauge::new(
                                desc.name.clone(),
                                value,
                                desc.metadata
                                    .iter()
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect(),
                            )
                            .with_window(g.window),
                        );
                    }
                }
                out
            }
        }
    }

    pub fn histograms(&mut self) -> Vec<Histogram> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.histograms),
            Snapshot::V2(s) => std::mem::take(&mut s.histograms),
            Snapshot::V3(s) => {
                let mut out = Vec::new();
                for g in &mut s.groups {
                    let values = std::mem::take(&mut g.histograms);
                    let Some(schema) = &g.schema else { continue };
                    for (desc, value) in schema.histograms.iter().zip(values) {
                        let Some(value) = value else { continue };
                        out.push(
                            Histogram::new(
                                desc.name.clone(),
                                value,
                                desc.metadata
                                    .iter()
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect(),
                            )
                            .with_window(g.window),
                        );
                    }
                }
                out
            }
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

    #[test]
    fn empty_groups_decodes_as_v3() {
        // An empty `groups` array is structurally identical, element for
        // element, to V2's empty `counters` array: both are just "an empty
        // seq" at that position. V3 wins the untagged-enum race only
        // because the V2 attempt still needs a `gauges` and `histograms`
        // element afterward, and a 4-element V3 payload doesn't have them
        // (V2's `metadata` field defaults but `gauges`/`histograms` do not).
        let s = SnapshotV3 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            duration: Duration::from_millis(10),
            metadata: HashMap::new(),
            groups: vec![],
        };
        let bytes = Snapshot::to_msgpack(&Snapshot::V3(s)).unwrap();
        let back: Snapshot = rmp_serde::from_slice(&bytes).unwrap();
        let Snapshot::V3(s) = back else {
            panic!("empty-groups V3 bytes decoded as a different version")
        };
        assert!(s.groups.is_empty());
    }

    #[test]
    fn gauges_and_histograms_roundtrip() {
        let mut h = histogram::Histogram::new(3, 64).unwrap();
        h.increment(10).unwrap();

        let mut s = v3();
        s.groups[0].counters = vec![];
        s.groups[0].schema = Some(GroupSchema::default());
        s.groups[0].gauges = vec![Some(i64::MIN), None, Some(-1)];
        s.groups[0].histograms = vec![Some(h.clone()), None];

        let bytes = Snapshot::to_msgpack(&Snapshot::V3(s.clone())).unwrap();
        let back: Snapshot = rmp_serde::from_slice(&bytes).unwrap();
        let Snapshot::V3(back) = back else {
            panic!("V3 bytes decoded as a different version")
        };
        assert_eq!(back.groups[0].gauges, vec![Some(i64::MIN), None, Some(-1)]);
        assert_eq!(back.groups[0].histograms, vec![Some(h), None]);
        assert_eq!(back.groups[0], s.groups[0]);
    }

    #[test]
    fn pre_v3_consumers_reject_v3_bytes() {
        // A consumer built before V3 existed only knows about V1/V2. Once
        // V3 ships, its bytes must never silently mis-decode as one of the
        // older shapes on such a consumer — this is the direction that
        // cannot be fixed after release, since it's every already-deployed
        // binary we don't control.
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        #[allow(dead_code)] // only decode success/failure is asserted, never the payload
        enum LegacySnapshot {
            V1(SnapshotV1),
            V2(SnapshotV2),
        }

        let empty_groups = SnapshotV3 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            duration: Duration::from_millis(10),
            metadata: HashMap::new(),
            groups: vec![],
        };
        let empty_bytes = Snapshot::to_msgpack(&Snapshot::V3(empty_groups)).unwrap();
        assert!(
            rmp_serde::from_slice::<LegacySnapshot>(&empty_bytes).is_err(),
            "empty-groups V3 bytes must not decode as a pre-V3 snapshot"
        );

        let populated_bytes = Snapshot::to_msgpack(&Snapshot::V3(v3())).unwrap();
        assert!(
            rmp_serde::from_slice::<LegacySnapshot>(&populated_bytes).is_err(),
            "populated V3 bytes must not decode as a pre-V3 snapshot"
        );
    }

    #[test]
    fn truncated_arrays_error_not_misdecode() {
        // Hand-build a 4-element msgpack array shaped like "V1 minus
        // histograms": (systemtime, metadata, counters, gauges). SnapshotV1
        // has a `#[serde(default)]` on `metadata`, which creates a
        // positional hazard in principle: a too-short array could let a
        // default-annotated field silently fill in rather than erroring.
        // This pins that it does not — decoding still fails outright rather
        // than mis-decoding as any version.
        #[allow(clippy::type_complexity)]
        let truncated: (
            SystemTime,
            HashMap<String, String>,
            Vec<Counter>,
            Vec<Counter>,
        ) = (
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            HashMap::new(),
            vec![Counter::new("0".to_string(), 7, HashMap::new())],
            vec![],
        );
        let bytes = rmp_serde::to_vec(&truncated).unwrap();
        assert!(
            rmp_serde::from_slice::<Snapshot>(&bytes).is_err(),
            "a truncated positional payload must error, not silently decode as some version"
        );
    }

    #[test]
    fn schema_hash_is_stable_and_content_addressed() {
        let a = GroupSchema {
            counters: vec![desc("0", "cpu_cycles")],
            gauges: vec![],
            histograms: vec![],
        };
        let b = a.clone();
        assert_eq!(a.hash(), b.hash(), "identical schemas hash identically");
        assert_ne!(
            a.hash(),
            (0, 0),
            "hash of a non-empty schema is non-trivial"
        );

        let mut c = a.clone();
        c.counters[0]
            .metadata
            .insert("id".to_string(), "3".to_string());
        assert_ne!(a.hash(), c.hash(), "metadata change changes the hash");

        let mut d = a.clone();
        d.counters.push(desc("1", "cpu_instructions"));
        assert_ne!(a.hash(), d.hash(), "membership change changes the hash");

        let mut e = a.clone();
        e.gauges = std::mem::take(&mut e.counters);
        assert_ne!(
            a.hash(),
            e.hash(),
            "same members in a different kind changes the hash"
        );
    }

    #[test]
    fn schema_hash_known_answer() {
        // Golden value: pins the algorithm (FNV-1a-128 over the msgpack
        // encoding) so a refactor cannot silently change hashes and
        // invalidate receiver caches across versions. Replace only with a
        // deliberate design decision.
        let h = GroupSchema::default().hash();
        println!("empty-schema hash: ({:#x}, {:#x})", h.0, h.1);
        let expected = (0x6370115622757277, 0xb806e79deae054ae);
        assert_eq!(h, expected);
    }

    fn v3_rich() -> SnapshotV3 {
        let mut s = v3();
        s.groups.push(GroupSnapshot {
            name: "memory/main".to_string(),
            schema_hash: (3, 4),
            schema: Some(GroupSchema {
                counters: vec![],
                gauges: vec![desc("9", "memory_free")],
                histograms: vec![],
            }),
            window: None,
            counters: vec![],
            gauges: vec![Some(-5)],
            histograms: vec![],
        });
        s
    }

    #[test]
    fn v3_counters_expand_with_group_window_and_skip_none() {
        let mut snap = Snapshot::V3(v3_rich());
        let counters = snap.counters();
        // v3() has counters [Some(7), None] — the None slot is skipped,
        // matching V2 semantics where absent metrics are simply not present.
        assert_eq!(counters.len(), 1);
        assert_eq!(counters[0].name, "0");
        assert_eq!(counters[0].value, 7);
        assert_eq!(counters[0].window, Some(Window::new(999_000, 999_400)));
        assert_eq!(
            counters[0].metadata.get("metric").map(String::as_str),
            Some("cpu_cycles")
        );
    }

    #[test]
    fn v3_gauges_expand_without_window_for_windowless_group() {
        let mut snap = Snapshot::V3(v3_rich());
        let gauges = snap.gauges();
        assert_eq!(gauges.len(), 1);
        assert_eq!(gauges[0].name, "9");
        assert_eq!(gauges[0].value, -5);
        assert_eq!(gauges[0].window, None);
    }

    #[test]
    fn v3_group_missing_schema_expands_to_nothing() {
        // A payload whose schema section was omitted cannot be expanded to
        // named metrics; the legacy path yields nothing rather than
        // fabricating names. (The V3-native recorder path resolves this via
        // its schema cache; the legacy path is only ever fed self-contained
        // payloads.)
        let mut s = v3();
        s.groups[0].schema = None;
        let mut snap = Snapshot::V3(s);
        assert!(snap.counters().is_empty());
    }

    #[test]
    #[cfg(feature = "parquet")]
    fn v3_and_equivalent_v2_hash_identically_for_parquet() {
        // The compatibility contract for MsgpackToParquet: a V3 snapshot and
        // the V2 snapshot describing the same readings produce the same
        // HashedSnapshot, so legacy parquet output is unchanged by V3 input.
        let hv3: HashedSnapshot = Snapshot::V3(v3()).into();
        let mut v2 = v2();
        v2.counters = vec![Counter::new(
            "0".to_string(),
            7,
            [("metric".to_string(), "cpu_cycles".to_string())].into(),
        )
        .with_window(Some(Window::new(999_000, 999_400)))];
        let hv2: HashedSnapshot = Snapshot::V2(v2).into();
        assert_eq!(hv3.ts, hv2.ts);
        assert_eq!(hv3.duration, hv2.duration);
        assert_eq!(hv3.counters.len(), hv2.counters.len());
        let (a, b) = (&hv3.counters["0"], &hv2.counters["0"]);
        assert_eq!(
            (a.value, &a.metadata, a.window),
            (b.value, &b.metadata, b.window)
        );
    }
}
