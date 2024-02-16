use crate::*;

// TODO(bmartin): derive Debug for Snapshot once the histogram snapshot has its
// own debug impl.

/// Contains a snapshot of metric readings.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone)]
#[non_exhaustive]
pub struct Snapshot {
    pub datetime: DateTime<Utc>,
    pub systemtime: SystemTime,
    pub counters: Vec<(String, u64)>,
    pub gauges: Vec<(String, i64)>,
    pub histograms: Vec<(String, HistogramSnapshot)>,
}

impl Snapshot {
    /// The UTC datetime when the snapshot was created.
    pub fn datetime(&self) -> DateTime<Utc> {
        self.datetime
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
}

impl Snapshot {
    pub(crate) fn new() -> Self {
        let now = SystemTime::now();
        let datetime = DateTime::<Utc>::from(now);

        Self {
            datetime,
            systemtime: now,
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        }
    }
}
