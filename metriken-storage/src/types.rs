use crate::labels::Labels;

/// A counter series' values: `u64` from an integer counter column, or `f64`
/// from a float counter column (`Float64`, `metric_type = counter`, which a
/// Prometheus endpoint's counters are stored as).
///
/// One series' values are one variant, so an integer counter keeps exact
/// `u64` differencing. A series spliced from segments that stored it both
/// ways is `Float` (see [`extend`](Self::extend)).
#[derive(Clone, Debug, PartialEq)]
pub enum CounterValues {
    Int(Vec<u64>),
    Float(Vec<f64>),
}

/// One counter reading: an integer counter's `u64` or a float counter's
/// `f64`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CounterValue {
    Int(u64),
    Float(f64),
}

impl CounterValue {
    /// The value as an `f64`: exact for a float, and for an integer up to
    /// 2^53.
    pub fn as_f64(self) -> f64 {
        match self {
            CounterValue::Int(v) => v as f64,
            CounterValue::Float(v) => v,
        }
    }

    /// The integer value; `None` for a float.
    pub fn as_int(self) -> Option<u64> {
        match self {
            CounterValue::Int(v) => Some(v),
            CounterValue::Float(_) => None,
        }
    }
}

/// A gauge series' values: `i64` from an integer gauge column, or `f64` from
/// a float gauge column (`Float64`, `metric_type = gauge`). As
/// [`CounterValues`], one series is one variant, and a series spliced from
/// segments that stored it both ways is `Float`.
#[derive(Clone, Debug, PartialEq)]
pub enum GaugeValues {
    Int(Vec<i64>),
    Float(Vec<f64>),
}

/// Methods shared by [`CounterValues`] and [`GaugeValues`], which differ in
/// their integer type only.
macro_rules! scalar_values {
    ($ty:ident, $int:ty) => {
        impl $ty {
            pub fn len(&self) -> usize {
                match self {
                    $ty::Int(v) => v.len(),
                    $ty::Float(v) => v.len(),
                }
            }

            pub fn is_empty(&self) -> bool {
                self.len() == 0
            }

            /// Whether these are a float column's values.
            pub fn is_float(&self) -> bool {
                matches!(self, $ty::Float(_))
            }

            /// The integer values; `None` for floats.
            pub fn as_int(&self) -> Option<&[$int]> {
                match self {
                    $ty::Int(v) => Some(v),
                    $ty::Float(_) => None,
                }
            }

            /// Every value as an `f64`: exact for floats, and for integers
            /// whose magnitude is at most 2^53.
            pub fn to_f64(&self) -> Vec<f64> {
                match self {
                    $ty::Int(v) => v.iter().map(|x| *x as f64).collect(),
                    $ty::Float(v) => v.clone(),
                }
            }

            /// The values in `range`, of the same variant.
            pub fn slice(&self, range: std::ops::Range<usize>) -> Self {
                match self {
                    $ty::Int(v) => $ty::Int(v[range].to_vec()),
                    $ty::Float(v) => $ty::Float(v[range].to_vec()),
                }
            }

            /// Append an integer value; to a float series, as an `f64`.
            pub fn push_int(&mut self, v: $int) {
                match self {
                    $ty::Int(a) => a.push(v),
                    $ty::Float(a) => a.push(v as f64),
                }
            }

            /// Append a float value; an integer series becomes `Float`, as
            /// in [`extend`](Self::extend).
            pub fn push_float(&mut self, v: f64) {
                match self {
                    $ty::Float(a) => a.push(v),
                    $ty::Int(_) => self.extend($ty::Float(vec![v])),
                }
            }

            /// Append `other`'s values. Two of one variant stay that
            /// variant; an integer and a float series (one series stored
            /// as an integer column in some segments and a float column in
            /// others) become `Float`, each integer converted as
            /// [`to_f64`](Self::to_f64) converts it. So a series that turns
            /// float loses exactness for its integer readings above 2^53,
            /// in any range that holds both.
            pub fn extend(&mut self, other: Self) {
                match (&mut *self, other) {
                    ($ty::Int(a), $ty::Int(b)) => a.extend(b),
                    ($ty::Float(a), $ty::Float(b)) => a.extend(b),
                    ($ty::Float(a), $ty::Int(b)) => a.extend(b.iter().map(|x| *x as f64)),
                    ($ty::Int(a), $ty::Float(b)) => {
                        let mut f: Vec<f64> = a.iter().map(|x| *x as f64).collect();
                        f.extend(b);
                        *self = $ty::Float(f);
                    }
                }
            }
        }

        impl Default for $ty {
            fn default() -> Self {
                $ty::Int(Vec::new())
            }
        }

        impl From<Vec<$int>> for $ty {
            fn from(v: Vec<$int>) -> Self {
                $ty::Int(v)
            }
        }

        impl From<Vec<f64>> for $ty {
            fn from(v: Vec<f64>) -> Self {
                $ty::Float(v)
            }
        }

        impl FromIterator<$int> for $ty {
            fn from_iter<I: IntoIterator<Item = $int>>(iter: I) -> Self {
                $ty::Int(iter.into_iter().collect())
            }
        }

        impl FromIterator<f64> for $ty {
            fn from_iter<I: IntoIterator<Item = f64>>(iter: I) -> Self {
                $ty::Float(iter.into_iter().collect())
            }
        }
    };
}

scalar_values!(CounterValues, u64);
scalar_values!(GaugeValues, i64);

impl CounterValues {
    /// The value at `i`.
    pub fn get(&self, i: usize) -> Option<CounterValue> {
        match self {
            CounterValues::Int(v) => v.get(i).copied().map(CounterValue::Int),
            CounterValues::Float(v) => v.get(i).copied().map(CounterValue::Float),
        }
    }
}

impl GaugeValues {
    /// The value at `i`, as an `f64` (see [`to_f64`](Self::to_f64)).
    pub fn get_f64(&self, i: usize) -> Option<f64> {
        match self {
            GaugeValues::Int(v) => v.get(i).map(|x| *x as f64),
            GaugeValues::Float(v) => v.get(i).copied(),
        }
    }
}

pub struct Counter {
    pub labels: Labels,
    pub timestamps: Vec<u64>,
    pub values: CounterValues,
    /// Per-sample reconstructed acquisition windows `[begin_ns, end_ns]`, aligned
    /// with `timestamps`/`values`. `None` when the source carried no `:window_*`
    /// sidecar columns for this metric. Populated by the parquet reader; read by
    /// rate()/irate() to attach per-point uncertainty bounds.
    pub windows: Option<Vec<(u64, u64)>>,
}

pub struct Gauge {
    pub labels: Labels,
    pub timestamps: Vec<u64>,
    pub values: GaugeValues,
    /// Per-sample reconstructed acquisition windows `[begin_ns, end_ns]`, aligned
    /// with `timestamps`/`values`. `None` when the source carried no `:window_*`
    /// sidecar columns for this metric. Populated by the parquet reader; consumed
    /// by a later phase (rate/increase error bars), hence `allow(dead_code)`.
    #[allow(dead_code)]
    pub windows: Option<Vec<(u64, u64)>>,
}

/// Raw cumulative sparse prefix-sum snapshot for one histogram sample.
///
/// `index` is ascending with no repeats. `index[i]` is the bucket position;
/// `count[i]` is the monotonically
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
    pub values: CounterValues,
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
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct CounterSample {
    pub ts: u64,
    pub value: CounterValue,
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
    /// Whether the samples can be `CounterValue::Float`: the series is held
    /// in a float counter column in some part of the range read. Known
    /// before the first sample, as `windowed` is.
    pub float: bool,
    /// The row spacing, in seconds, of the table it is read from, where a
    /// source knows it (`LabeledSource::with_interval`).
    pub interval: Option<f64>,
    pub samples: Box<dyn Iterator<Item = CounterSample> + 'a>,
}

impl<'a> From<Counter> for CounterStream<'a> {
    fn from(c: Counter) -> Self {
        let windowed = c.windows.is_some();
        let float = c.values.is_float();
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
            value: values
                .get(i)
                .expect("a series has as many values as timestamps"),
            window: windows.get(i).copied(),
        });
        CounterStream {
            labels,
            windowed,
            float,
            interval: None,
            samples: Box::new(samples),
        }
    }
}

pub struct Gauges {
    pub series: Vec<Gauge>,
}
