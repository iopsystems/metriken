use crate::*;

/// Contains a snapshot of metric readings.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Snapshot {
    datetime: DateTime<Utc>,
    unix_ns: u64,
    pub(crate) counters: Vec<(String, u64)>,
    pub(crate) gauges: Vec<(String, i64)>,
    pub(crate) histograms: Vec<(String, HistogramSnapshot)>,
}

impl Snapshot {
    /// The UTC datetime when the snapshot was created.
    pub fn datetime(&self) -> DateTime<Utc> {
        self.datetime
    }

    /// The number of whole nanoseconds since the UNIX epoch when the snapshot
    /// was created.
    pub fn unix_ns(&self) -> u64 {
        self.unix_ns
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
}

impl Snapshot {
    pub(crate) fn new() -> Self {
        let datetime: DateTime<Utc> = Utc::now();

        let unix_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        Self {
            datetime,
            unix_ns,
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        }
    }
}
