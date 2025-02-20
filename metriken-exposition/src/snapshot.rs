use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

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
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Gauge {
    pub name: String,
    pub value: i64,
    pub metadata: HashMap<String, String>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Histogram {
    pub name: String,
    pub value: histogram::Histogram,
    pub metadata: HashMap<String, String>,
}

/// Contains a snapshot of metric readings.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub systemtime: SystemTime,

    #[cfg_attr(feature = "serde", serde(default))]
    pub metadata: HashMap<String, String>,

    pub counters: Vec<Counter>,
    pub gauges: Vec<Gauge>,
    pub histograms: Vec<Histogram>,
}

#[cfg(feature = "parquet")]
pub(crate) struct HashedSnapshot {
    pub(crate) ts: u64,
    pub(crate) counters: HashMap<String, Counter>,
    pub(crate) gauges: HashMap<String, Gauge>,
    pub(crate) histograms: HashMap<String, Histogram>,
}

impl Snapshot {
    pub(crate) fn new() -> Self {
        Self {
            systemtime: SystemTime::now(),
            metadata: HashMap::new(),
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        }
    }

    /// Return the metric name: for Rezolus v4 data, this is the metric name
    /// from the snapshot. Rezolus v5 snapshots have metrics with opaque names
    /// with the real name being in the metadata.
    pub(crate) fn derive_metric_name(
        snapshot_name: &str,
        metadata: &HashMap<String, String>,
    ) -> String {
        // Check for Rezolus v4 snapshot, if so return the name as-is
        if snapshot_name.contains("/")
            && snapshot_name
                .chars()
                .next()
                .map(|x| x.is_ascii_digit())
                .is_some_and(|x| !x)
        {
            return snapshot_name.to_string();
        }

        // Rezolus v5 snapshot: append all the metadata, except few known keys,
        // to the name to ensure a unique name.
        let ordered = ["name", "op", "state"];
        let mut ignore: HashSet<&str> =
            ["metric", "unit", "grouping_power", "max_value_power"].into();
        ignore.extend(ordered);

        let Some(name) = metadata.get("metric") else {
            return snapshot_name.to_string();
        };
        let mut unique_name = name.to_string();

        for k in ordered {
            if let Some(v) = metadata.get(k) {
                unique_name = unique_name + "/" + v;
            }
        }

        for (k, v) in metadata {
            if ignore.contains(k.as_str()) {
                continue;
            }
            unique_name = unique_name + "/" + v;
        }

        unique_name
    }

    /// The system time when the snapshot was created.
    pub fn systemtime(&self) -> SystemTime {
        self.systemtime
    }

    /// Fetch the value for a snapshot metadata key.
    pub fn get_metadata(&self, key: &str) -> Option<&str> {
        self.metadata.get(key).map(|x| x.as_str())
    }

    /// A view into the counters for this snapshot.
    pub fn counters(&self) -> &[Counter] {
        &self.counters
    }

    /// A view into the gauges for this snapshot.
    pub fn gauges(&self) -> &[Gauge] {
        &self.gauges
    }

    /// A view into the histograms for this snapshot.
    pub fn histograms(&self) -> &[Histogram] {
        &self.histograms
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
    fn from(snapshot: Snapshot) -> Self {
        let ts: u64 = snapshot
            .systemtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("System Clock is earlier than 1970; needs reset")
            .as_nanos() as u64;

        let counters: HashMap<String, Counter> = HashMap::from_iter(
            snapshot
                .counters
                .into_iter()
                .map(|v| (Snapshot::derive_metric_name(&v.name, &v.metadata), v)),
        );
        let gauges: HashMap<String, Gauge> = HashMap::from_iter(
            snapshot
                .gauges
                .into_iter()
                .map(|v| (Snapshot::derive_metric_name(&v.name, &v.metadata), v)),
        );
        let histograms: HashMap<String, Histogram> = HashMap::from_iter(
            snapshot
                .histograms
                .into_iter()
                .map(|v| (Snapshot::derive_metric_name(&v.name, &v.metadata), v)),
        );

        Self {
            ts,
            counters,
            gauges,
            histograms,
        }
    }
}
