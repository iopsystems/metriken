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

pub struct Gauges {
    pub series: Vec<Gauge>,
}
