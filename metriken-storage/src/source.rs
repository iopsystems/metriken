//! The data-access trait the query engine reads through, and the column
//! positions a reader indexes.

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::types::{self, CounterStream, Counters, Gauges};
use crate::{parquet, scan};

/// A counter column of a parquet schema, as a reader indexes it at open:
/// which metric and label set it carries, and where to read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CounterColumnRef {
    pub name: String,
    pub labels: Labels,
    pub position: ColumnPosition,
}

/// Where a column sits in a parquet schema: its own index and, if it
/// carries acquisition windows, its begin/width sidecar columns. What
/// [`DataSource::counter_column`] takes, so a reader that indexed a table's
/// columns at open can read one back without re-parsing the schema. Small on
/// purpose — a reader keeps one per column per segment, which on a wide
/// table is hundreds of thousands — so it carries positions, whether the
/// column can hold a value, and nothing that names the column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColumnPosition {
    pub col_idx: u32,
    pub begin_col: Option<u32>,
    pub width_col: Option<u32>,
    /// In a [long](crate::long) segment, the occupant whose rows are this
    /// series; `None` for a column that is one series.
    pub occupant: Option<u64>,
    /// False when the footer's statistics say every cell of the column is
    /// null (a wide table keeps a series' column after it stops); true
    /// when they do not say. Fits the struct's padding.
    pub holds_values: bool,
}
pub trait DataSource: Send + Sync {
    /// A source emits every sample at the timestamp it was recorded with.
    /// These used to take a `raw` flag to ask for that instead of a
    /// rounded-to-the-nominal-grid copy; there is no longer a second form to
    /// choose between. RateMode::Raw still selects point PLACEMENT in the
    /// streaming layer — see `Placement`.
    fn counters(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64)
        -> Option<Counters>;
    /// The counter series as streams — see [`CounterStream`]. The default
    /// materializes through [`counters`](Self::counters); a source that can
    /// read one series at a time overrides it.
    fn counter_streams<'s>(
        &'s self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<CounterStream<'s>>> {
        let counters = self.counters(name, filter, start_ns, end_ns)?;
        Some(
            counters
                .series
                .into_iter()
                .map(CounterStream::from)
                .collect(),
        )
    }
    /// Every counter column of this source's parquet schema, with what a
    /// direct read of it needs. Empty for a source without a parquet schema.
    fn counter_column_refs(&self) -> Vec<CounterColumnRef> {
        Vec::new()
    }
    /// Read one counter column by its schema position, every row group the
    /// range touches. `None` for a source without a parquet schema.
    ///
    /// `selective` says the query reads few series, so a long segment may
    /// decode only the pages holding this one's rows. An all-series query
    /// passes `false`, and each segment then decodes a row group once and
    /// shares it through the pool across the series reading it.
    fn counter_column(
        &self,
        at: &ColumnPosition,
        start_ns: u64,
        end_ns: u64,
        selective: bool,
    ) -> Option<types::ColumnChunk> {
        let _ = (at, start_ns, end_ns, selective);
        None
    }
    /// Columns `cols` (schema indices) of every row group `[start_ns,
    /// end_ns]` touches, decoded once, for a read of many series at a time.
    /// `None` for a source that is not a single parquet file, or when the
    /// read fails (logged).
    fn batch_columns(
        &self,
        cols: &[usize],
        start_ns: u64,
        end_ns: u64,
    ) -> Option<parquet::BatchColumns> {
        let _ = (cols, start_ns, end_ns);
        None
    }
    /// Counter `name`'s matched series and their decoded columns between
    /// `start_ns` and `end_ns`, for the batched rate path
    /// (`batch_rate::grid_rates`). `None` when the source cannot scan this
    /// counter; the dispatcher then uses the per-series path.
    fn counter_scan(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<scan::CounterScan<'_>> {
        let _ = (name, filter, start_ns, end_ns);
        None
    }
    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges>;
    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream>;
    /// Gauge histogram `name`'s rows: each row is the state of the series at
    /// its timestamp, not a count since start, so a reader must take each
    /// row as it is and never difference two. Not listed by
    /// [`histogram_names`](Self::histogram_names), and never returned by
    /// [`histogram_stream`](Self::histogram_stream): a caller that reads
    /// histograms as cumulative does not see one. A column is a gauge
    /// histogram when its field metadata has `metric_type =
    /// gauge_histogram` (`metriken_model::GAUGE_HISTOGRAM`).
    ///
    /// The default is `None`. A source that wraps others forwards this and
    /// the other `gauge_histogram_*` methods, or its gauge histograms are
    /// lost (never misread).
    fn gauge_histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let _ = (name, filter, start_ns, end_ns);
        None
    }
    /// The row spacing, in seconds, of the table holding gauge histogram
    /// `name`, answered as [`counter_interval`](Self::counter_interval) is.
    fn gauge_histogram_interval(&self, name: &str) -> f64 {
        let _ = name;
        self.interval()
    }
    /// Sampling interval in seconds.
    fn interval(&self) -> f64;
    /// The row spacing, in seconds, of the table holding histogram `name`,
    /// answered as [`counter_interval`](Self::counter_interval) is.
    fn histogram_interval(&self, name: &str) -> f64 {
        let _ = name;
        self.interval()
    }
    /// The row spacing, in seconds, of the table holding counter `name`.
    /// `LabeledSource` answers the interval set with `with_interval`, else
    /// its source's; `UnionSource` the holding table's; `RecordingsSource`
    /// and `MultiParquetSource` the longest among the children holding it;
    /// other sources answer `interval`. No counter series a source returns
    /// carries a longer `CounterStream::interval` or
    /// `ScanSeries::interval`: `LabeledSource::with_interval` sets its
    /// interval on every series, and `RecordingsSource` and
    /// `MultiParquetSource` set each child's on a series that has none.
    fn counter_interval(&self, name: &str) -> f64 {
        let _ = name;
        self.interval()
    }
    /// The row spacing, in seconds, of the table holding gauge `name`,
    /// answered as [`counter_interval`](Self::counter_interval) is.
    fn gauge_interval(&self, name: &str) -> f64 {
        let _ = name;
        self.interval()
    }
    /// Full time extent of the stored data in nanoseconds, or `None` if empty.
    fn time_range(&self) -> Option<(u64, u64)>;
    /// Names of all counter metrics (sorted, deduplicated).
    fn counter_names(&self) -> Vec<String>;
    /// Names of all gauge metrics (sorted, deduplicated).
    fn gauge_names(&self) -> Vec<String>;
    /// Names of all histogram metrics (sorted, deduplicated). Gauge
    /// histograms are not among them; see
    /// [`gauge_histogram_names`](Self::gauge_histogram_names).
    fn histogram_names(&self) -> Vec<String>;
    /// Names of all gauge histogram metrics (sorted, deduplicated). Empty by
    /// default.
    fn gauge_histogram_names(&self) -> Vec<String> {
        Vec::new()
    }
    /// All label combinations for the named counter metric. Empty if unknown.
    fn counter_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;
    /// All label combinations for the named gauge metric. Empty if unknown.
    fn gauge_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;
    /// All label combinations for the named histogram metric. Empty if unknown.
    fn histogram_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;
    /// All label combinations for the named gauge histogram metric. Empty if
    /// unknown, and by default.
    fn gauge_histogram_labels(
        &self,
        name: &str,
    ) -> Vec<std::collections::BTreeMap<String, String>> {
        let _ = name;
        Vec::new()
    }
    /// Key-value metadata from the file footer. Default returns empty.
    fn file_metadata(&self) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }
    /// Look up a single metadata value by key without cloning the full map.
    fn metadata_get(&self, key: &str) -> Option<String> {
        self.file_metadata().get(key).cloned()
    }
    /// Parquet column name for every `(metric_name, labels)` pair.
    fn column_map(
        &self,
    ) -> std::collections::HashMap<String, std::collections::HashMap<Labels, String>>;
    /// Per-sample collection timestamps (ns since epoch), in row order — the
    /// `timestamp` column as recorded. Default empty for sources that do not
    /// track one (e.g. a live `MemoryStore`).
    ///
    /// This is on `DataSource` rather than only on the readers because a
    /// composite source has to gather it from its children through the trait;
    /// it cannot reach past them into row groups.
    fn sample_timestamps(&self) -> Vec<u64> {
        Vec::new()
    }
    /// The label keys that say which recording a series came from (`source`,
    /// `host`, `__recording__`, …), for a source whose series come from
    /// several. Two series that carry one of these keys with different
    /// values are of different recordings, and a binary operator's
    /// broadcast never pairs them. Empty for a source of one recording that
    /// adds no such labels. A source wrapping one that reports keys forwards
    /// this and [`gauge_parts`](Self::gauge_parts); the defaults lose them.
    fn recording_label_keys(&self) -> Vec<String> {
        Vec::new()
    }
    /// Gauge `name`'s series for a grid from `start_ns` to `end_ns` at
    /// `step_ns`, as parts whose series share one sampling interval, each
    /// returned with that interval in seconds. A gauge is held a step, or its
    /// interval when that is longer, past each sample, so each part is read
    /// from that far before `start_ns`. A source joining recordings of different intervals gives
    /// each recording's series its own. The default is one part at
    /// [`interval`](Self::interval).
    fn gauge_parts(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
    ) -> Option<Vec<(Gauges, f64)>> {
        let interval = self.interval();
        let lookback_ns = step_ns.max((interval * 1e9) as u64);
        let gauges = self.gauges(name, filter, start_ns.saturating_sub(lookback_ns), end_ns)?;
        Some(vec![(gauges, interval)])
    }
    /// Parsed parquet column descriptors, for callers that need schema-level
    /// detail the metric-name accessors above do not carry (histogram bucket
    /// configs, per-column label sets). Empty for sources with no parquet
    /// schema behind them.
    fn columns_desc(&self) -> Vec<crate::parquet::ColDesc> {
        Vec::new()
    }
    /// Columns in the source's schema, for sizing what an open reader holds.
    /// Zero for a source without one.
    fn column_count(&self) -> usize {
        0
    }
    /// Bytes the source keeps in memory for its data — the whole file for one
    /// opened from bytes, nothing for one that reads a file on demand.
    fn resident_bytes(&self) -> usize {
        0
    }
    /// Number of distinct series across all metric types: the label-set
    /// count of every name. On `DataSource` so a composite can ask each
    /// child, letting a lazy child answer from its catalog instead of
    /// loading to walk its labels.
    fn series_count(&self) -> usize {
        label_walk_series_count(self)
    }
}

/// The label-set count of every name: the default
/// [`DataSource::series_count`], and the fallback for a composite whose
/// children may share series.
pub fn label_walk_series_count<S: DataSource + ?Sized>(source: &S) -> usize {
    let mut count = 0;
    for name in source.counter_names() {
        count += source.counter_labels(&name).len();
    }
    for name in source.gauge_names() {
        count += source.gauge_labels(&name).len();
    }
    for name in source.histogram_names() {
        count += source.histogram_labels(&name).len();
    }
    for name in source.gauge_histogram_names() {
        count += source.gauge_histogram_labels(&name).len();
    }
    count
}
