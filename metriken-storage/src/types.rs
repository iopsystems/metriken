use crate::labels::Labels;

pub struct Counter {
    pub labels: Labels,
    pub timestamps: Vec<u64>,
    pub values: Vec<u64>,
    /// Per-sample reconstructed acquisition windows `[begin_ns, end_ns]`, aligned
    /// with `timestamps`/`values`. `None` when the source carried no `:window_*`
    /// sidecar columns for this metric. Populated by the parquet reader; read by
    /// rate()/irate() to attach per-point uncertainty bounds.
    pub windows: Option<Vec<(u64, u64)>>,
}

pub struct Gauge {
    pub labels: Labels,
    pub timestamps: Vec<u64>,
    pub values: Vec<i64>,
    /// Per-sample reconstructed acquisition windows `[begin_ns, end_ns]`, aligned
    /// with `timestamps`/`values`. `None` when the source carried no `:window_*`
    /// sidecar columns for this metric. Populated by the parquet reader; consumed
    /// by a later phase (rate/increase error bars), hence `allow(dead_code)`.
    #[allow(dead_code)]
    pub windows: Option<Vec<(u64, u64)>>,
}

/// Raw cumulative sparse prefix-sum snapshot for one histogram sample.
///
/// `index[i]` is the bucket position; `count[i]` is the monotonically
/// non-decreasing cumulative observation count up to and including that
/// bucket since the last counter reset. Only buckets with at least one
/// cumulative observation are stored (sparse). The streaming operators
/// compute per-interval deltas from consecutive snapshots, mirroring
/// how counter irate/rate works on raw cumulative values.
#[derive(Clone)]
pub struct HistogramSnapshot {
    pub index: Vec<u32>,
    pub count: Vec<u64>,
}

pub struct Histogram {
    pub labels: Labels,
    pub config: ::histogram::Config,
    pub timestamps: Vec<u64>,
    pub snapshots: Vec<HistogramSnapshot>,
}

pub struct Counters {
    pub series: Vec<Counter>,
}

/// One column's samples out of one parquet source: what a direct read by
/// position returns. The caller knows whose it is.
pub struct ColumnChunk {
    pub timestamps: Vec<u64>,
    pub values: Vec<u64>,
    pub windows: Option<Vec<(u64, u64)>>,
}

impl ColumnChunk {
    /// As a series under `labels`.
    pub fn labeled(self, labels: Labels) -> Counter {
        Counter {
            labels,
            timestamps: self.timestamps,
            values: self.values,
            windows: self.windows,
        }
    }
}

/// One counter reading, as a stream carries it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CounterSample {
    pub ts: u64,
    pub value: u64,
    /// The acquisition window `(begin_ns, end_ns)`, when the source has one.
    pub window: Option<(u64, u64)>,
}

/// A counter series whose samples are produced as they are consumed.
///
/// [`Counters`] is every series' every sample, resident at once; for a
/// wide table that is the whole table in memory before a single point is
/// computed. A stream is pulled: a source that can read one column of one
/// segment at a time (the segmented reader) hands out samples segment by
/// segment, and a rate over it holds only the samples bracketing its
/// current interval. `windowed` says whether the samples carry acquisition
/// windows, which a rate needs to know before the first one arrives.
pub struct CounterStream<'a> {
    pub labels: Labels,
    pub windowed: bool,
    pub samples: Box<dyn Iterator<Item = CounterSample> + 'a>,
}

impl<'a> From<Counter> for CounterStream<'a> {
    fn from(c: Counter) -> Self {
        let windowed = c.windows.is_some();
        let Counter {
            labels,
            timestamps,
            values,
            windows,
        } = c;
        let n = timestamps.len();
        let windows = windows.unwrap_or_default();
        let samples = (0..n).map(move |i| CounterSample {
            ts: timestamps[i],
            value: values[i],
            window: windows.get(i).copied(),
        });
        CounterStream {
            labels,
            windowed,
            samples: Box::new(samples),
        }
    }
}

pub struct Gauges {
    pub series: Vec<Gauge>,
}
