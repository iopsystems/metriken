use crate::*;

/// Contains a snapshot of metric readings.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Snapshot {
    datetime: DateTime<Utc>,
    unix_ns: u128,
    pub(crate) counters: Vec<(String, u64)>,
    pub(crate) gauges: Vec<(String, i64)>,
    pub(crate) histograms: Vec<(String, HistogramSnapshot)>,
}

impl Snapshot {
    pub fn datetime(&self) -> DateTime<Utc> {
        self.datetime
    }

    pub fn unix_ns(&self) -> u128 {
        self.unix_ns
    }

    pub fn counters(&self) -> &[(String, u64)] {
        &self.counters
    }

    pub fn gauges(&self) -> &[(String, i64)] {
        &self.gauges
    }

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
            .as_nanos();

        Self {
            datetime,
            unix_ns,
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        }
    }
}
