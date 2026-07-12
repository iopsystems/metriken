use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use metriken::Window;

#[cfg(feature = "msgpack")]
use rmp_serde::encode::Error as SerializeMsgpackError;
#[cfg(feature = "json")]
use serde_json::Error as JsonError;

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum Snapshot {
    V1(SnapshotV1),
    V2(SnapshotV2),
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
        }
    }

    pub fn duration(&self) -> Option<Duration> {
        match self {
            Snapshot::V1(_) => None,
            Snapshot::V2(s) => Some(s.duration),
        }
    }

    pub fn metadata(&mut self) -> HashMap<String, String> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.metadata),
            Snapshot::V2(s) => std::mem::take(&mut s.metadata),
        }
    }

    pub fn counters(&mut self) -> Vec<Counter> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.counters),
            Snapshot::V2(s) => std::mem::take(&mut s.counters),
        }
    }

    pub fn gauges(&mut self) -> Vec<Gauge> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.gauges),
            Snapshot::V2(s) => std::mem::take(&mut s.gauges),
        }
    }

    pub fn histograms(&mut self) -> Vec<Histogram> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.histograms),
            Snapshot::V2(s) => std::mem::take(&mut s.histograms),
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
