#[cfg(feature = "parquet")]
use std::collections::HashMap;
use std::time::SystemTime;

#[cfg(feature = "msgpack")]
use rmp_serde::encode::Error as SerializeMsgpackError;
#[cfg(feature = "json")]
use serde_json::Error as JsonError;

use crate::HistogramSnapshot;

// TODO(bmartin): derive Debug for Snapshot once the histogram snapshot has its
// own debug impl.

/// Contains a snapshot of metric readings.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone)]
#[non_exhaustive]
pub struct Snapshot {
    pub systemtime: SystemTime,
    pub counters: Vec<(String, u64)>,
    pub gauges: Vec<(String, i64)>,
    pub histograms: Vec<(String, HistogramSnapshot)>,
}

#[cfg(feature = "parquet")]
pub(crate) struct HashedSnapshot {
    pub(crate) ts: u64,
    pub(crate) counters: HashMap<String, u64>,
    pub(crate) gauges: HashMap<String, i64>,
    pub(crate) histograms: HashMap<String, HistogramSnapshot>,
}

impl Snapshot {
    pub(crate) fn new() -> Self {
        Self {
            systemtime: SystemTime::now(),
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        }
    }

    /// The system time when the snapshot was created.
    pub fn systemtime(&self) -> SystemTime {
        self.systemtime
    }

    /// A view into the counters for this snapshot.
    pub fn counters(&self) -> &[(String, u64)] {
        &self.counters
    }

    /// A view into the gauges for this snapshot.
    pub fn gauges(&self) -> &[(String, i64)] {
        &self.gauges
    }

    /// A view into the histograms for this snapshot.
    pub fn histograms(&self) -> &[(String, HistogramSnapshot)] {
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
        let mut counters: HashMap<String, u64> = HashMap::new();
        let mut gauges: HashMap<String, i64> = HashMap::new();
        let mut histograms: HashMap<String, HistogramSnapshot> = HashMap::new();

        let ts: u64 = snapshot
            .systemtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("System Clock is earlier than 1970; needs reset")
            .as_nanos() as u64;

        for counter in snapshot.counters {
            counters.insert(counter.0, counter.1);
        }

        for gauge in snapshot.gauges {
            gauges.insert(gauge.0, gauge.1);
        }

        for h in snapshot.histograms {
            histograms.insert(h.0, h.1);
        }

        Self {
            ts,
            counters,
            gauges,
            histograms,
        }
    }
}
