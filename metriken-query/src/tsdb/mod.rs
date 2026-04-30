use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::num::ParseIntError;
use std::ops::*;
use std::path::Path;

use arrow::array::{Int64Array, ListArray, UInt64Array};
use arrow::datatypes::DataType;
use bytes::Bytes;
use histogram::{CumulativeROHistogram, Histogram};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::reader::FileReader;
use parquet::file::serialized_reader::SerializedFileReader;
use serde::Serialize;

mod collection;
mod heatmap;
mod labels;
mod series;

pub use collection::*;
pub use heatmap::Heatmap;
pub use labels::Labels;
use series::delta_to_32;
pub use series::*;

/// Snap a nanosecond timestamp to the nearest multiple of `interval_ns`.
/// Returns the timestamp unchanged when `interval_ns` is zero (i.e. unknown).
#[allow(clippy::manual_checked_ops)]
fn snap_timestamp(ts: u64, interval_ns: u64) -> u64 {
    if interval_ns > 0 {
        ((ts + interval_ns / 2) / interval_ns) * interval_ns
    } else {
        ts
    }
}

#[derive(Default, Clone)]
pub struct Tsdb {
    sampling_interval_ms: u64,
    source: String,
    version: String,
    filename: String,
    file_metadata: HashMap<String, String>,
    counters: HashMap<String, CounterCollection>,
    gauges: HashMap<String, GaugeCollection>,
    histograms: HashMap<String, HistogramCollection>,
    /// Sidecar holding the most-recent cumulative-since-start histogram per
    /// series, used by the streaming `ingest` path to compute the per-period
    /// delta against the next snapshot.  Not populated by the parquet load
    /// path (which differences in-place during column iteration).
    #[cfg(feature = "ingest")]
    prev_histograms: HashMap<String, HashMap<Labels, CumulativeROHistogram>>,
}

impl Tsdb {
    pub fn load(path: &Path) -> Result<Self, Box<dyn Error>> {
        let raw = std::fs::read(path)?;
        let filename = path
            .file_name()
            .map(|v| v.to_str().unwrap_or("unknown"))
            .unwrap_or("unknown")
            .to_string();
        let mut data = Self::load_from_bytes(Bytes::from(raw))?;
        data.filename = filename;
        Ok(data)
    }

    pub fn load_from_bytes(bytes: Bytes) -> Result<Self, Box<dyn Error>> {
        let mut data = Tsdb::default();

        let reader = SerializedFileReader::new(bytes.clone()).unwrap();
        let parquet_metadata = reader.metadata();
        let key_value_metadata = parquet_metadata
            .file_metadata()
            .key_value_metadata()
            .unwrap();

        let mut metadata = HashMap::new();

        for kv in key_value_metadata {
            metadata.insert(kv.key.clone(), kv.value.clone().unwrap_or("".to_string()));
        }

        let interval = metadata
            .get("sampling_interval_ms")
            .map(|v| v.parse::<u64>().expect("bad interval"))
            .unwrap_or(1000);
        data.sampling_interval_ms = interval;

        data.source = metadata
            .get("source")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        data.version = match metadata.get("version").map(|v| v.as_str()) {
            Some(s) => s.to_string(),
            _ => "unknown".to_string(),
        };

        data.file_metadata = metadata;

        let builder = ParquetRecordBatchReaderBuilder::try_new(bytes)?;
        let reader = builder.build()?;

        for batch in reader.into_iter().flatten() {
            let schema = batch.schema().clone();

            // row to timestamp in seconds
            let mut timestamps: BTreeMap<usize, u64> = BTreeMap::new();

            // loop to find the timestamp column, convert it to seconds, and
            // store it in the map
            for (id, field) in schema.fields().iter().enumerate() {
                if field.name() == "timestamp" {
                    let column = batch.column(id);

                    if *column.data_type() != DataType::UInt64 {
                        panic!("invalid timestamp column data type");
                    }

                    let values = column
                        .as_any()
                        .downcast_ref::<UInt64Array>()
                        .expect("Failed to downcast");

                    let interval_ns = data.sampling_interval_ms * 1_000_000;

                    for (id, value) in values.iter().enumerate() {
                        if let Some(v) = value {
                            // Snap timestamps to the nearest multiple of the
                            // sampling interval.  Different samplers may fire
                            // at slightly different wall-clock times within
                            // the same collection cycle, so their raw
                            // timestamps can diverge by microseconds.
                            // Aligning here ensures that binary operations
                            // between metrics from different samplers (e.g.
                            // `irate(cpu_usage) / cpu_cores`) find matching
                            // timestamps.
                            let snapped = snap_timestamp(v, interval_ns);
                            timestamps.insert(id, snapped);
                        }
                    }

                    break;
                }
            }

            // loop through all non-timestamp columns, and insert them into the
            // tsdb
            for (id, field) in schema.fields().iter().enumerate() {
                if field.name() == "timestamp" {
                    continue;
                }

                let mut meta = field.metadata().clone();

                let name = if let Some(n) = meta.get("metric").cloned() {
                    n
                } else {
                    let col_name = field.name();
                    // Strip :buckets suffix from histogram column names
                    // (e.g., "request_latency:buckets" -> "request_latency")
                    col_name
                        .strip_suffix(":buckets")
                        .unwrap_or(col_name)
                        .to_string()
                };

                let grouping_power: Option<Result<u8, ParseIntError>> =
                    meta.remove("grouping_power").map(|v| v.parse());

                let max_value_power: Option<Result<u8, ParseIntError>> =
                    meta.remove("max_value_power").map(|v| v.parse());

                let mut labels = Labels::default();

                for (k, v) in meta.iter() {
                    match k.as_str() {
                        // Internal metadata — not user-facing labels
                        "metric" | "metric_type" | "unit" => continue,
                        _ => {
                            labels.inner.insert(k.to_string(), v.to_string());
                        }
                    }
                }

                let column = batch.column(id);

                match column.data_type() {
                    DataType::UInt64 => {
                        let counters = data.counters.entry(name.to_string()).or_default();
                        let series = counters.entry(labels).or_default();

                        let values = column
                            .as_any()
                            .downcast_ref::<UInt64Array>()
                            .expect("Failed to downcast");

                        for (id, value) in values.iter().enumerate() {
                            if let Some(v) = value {
                                if let Some(ts) = timestamps.get(&id) {
                                    series.insert(*ts, v);
                                }
                            }
                        }
                    }
                    DataType::Int64 => {
                        let collection = data.gauges.entry(name.to_string()).or_default();
                        let series = collection.entry(labels).or_default();

                        let values = column
                            .as_any()
                            .downcast_ref::<Int64Array>()
                            .expect("Failed to downcast");

                        for (id, value) in values.iter().enumerate() {
                            if let Some(v) = value {
                                if let Some(ts) = timestamps.get(&id) {
                                    series.insert(*ts, v);
                                }
                            }
                        }
                    }
                    DataType::List(field_type) => {
                        if field_type.data_type() == &DataType::UInt64 {
                            let collection = data.histograms.entry(name.to_string()).or_default();
                            let series = collection.entry(labels).or_default();

                            let list_array = column
                                .as_any()
                                .downcast_ref::<ListArray>()
                                .expect("Failed to downcast to ListArray");

                            let grouping_power = if let Some(Ok(v)) = grouping_power {
                                v
                            } else {
                                continue;
                            };

                            let max_value_power = if let Some(Ok(v)) = max_value_power {
                                v
                            } else {
                                continue;
                            };

                            // Pre-difference consecutive snapshots into u32
                            // delta histograms.  The first snapshot in a series
                            // has no predecessor so it's kept only as the
                            // anchor for the next delta.
                            let mut prev: Option<CumulativeROHistogram> = None;

                            for (id, value) in list_array.iter().enumerate() {
                                let ts = match timestamps.get(&id) {
                                    Some(ts) => *ts,
                                    None => continue,
                                };
                                let list_value = match value {
                                    Some(v) => v,
                                    None => continue,
                                };

                                let data = list_value
                                    .as_any()
                                    .downcast_ref::<UInt64Array>()
                                    .expect("Failed to downcast to UInt64Array");

                                let buckets: Vec<u64> = data.iter().flatten().collect();

                                let h = match Histogram::from_buckets(
                                    grouping_power,
                                    max_value_power,
                                    buckets,
                                ) {
                                    Ok(h) => h,
                                    Err(_) => continue,
                                };

                                let curr = CumulativeROHistogram::from(&h);

                                if let Some(prev_cumu) = &prev {
                                    if let Some(d) = delta_to_32(prev_cumu, &curr) {
                                        series.insert(ts, d);
                                    }
                                }
                                prev = Some(curr);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(data)
    }

    pub fn set_sampling_interval_ms(&mut self, ms: u64) {
        self.sampling_interval_ms = ms;
    }

    pub fn set_source(&mut self, source: String) {
        self.source = source;
    }

    pub fn set_version(&mut self, version: String) {
        self.version = version;
    }

    pub fn set_filename(&mut self, filename: String) {
        self.filename = filename;
    }

    /// Ingest a snapshot from a running agent, inserting all metrics into the
    /// TSDB.
    #[cfg(feature = "ingest")]
    pub fn ingest(&mut self, mut snapshot: metriken_exposition::Snapshot) {
        let raw_ts = snapshot
            .systemtime()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("system clock is earlier than 1970")
            .as_nanos() as u64;

        // Snap to the nearest sampling interval boundary so that metrics
        // from different samplers within the same collection cycle share
        // an identical timestamp.
        let interval_ns = self.sampling_interval_ms * 1_000_000;
        let ts = snap_timestamp(raw_ts, interval_ns);

        for counter in snapshot.counters() {
            let (name, labels) = Self::extract_name_labels(&counter.metadata);
            self.counters
                .entry(name)
                .or_default()
                .entry(labels)
                .or_default()
                .insert(ts, counter.value);
        }

        for gauge in snapshot.gauges() {
            let (name, labels) = Self::extract_name_labels(&gauge.metadata);
            self.gauges
                .entry(name)
                .or_default()
                .entry(labels)
                .or_default()
                .insert(ts, gauge.value);
        }

        for histogram in snapshot.histograms() {
            let (name, labels) = Self::extract_name_labels(&histogram.metadata);
            let curr = CumulativeROHistogram::from(&histogram.value);

            let prev_for_metric = self.prev_histograms.entry(name.clone()).or_default();

            if let Some(prev) = prev_for_metric.get(&labels) {
                if let Some(d) = delta_to_32(prev, &curr) {
                    self.histograms
                        .entry(name.clone())
                        .or_default()
                        .entry(labels.clone())
                        .or_default()
                        .insert(ts, d);
                }
            }

            prev_for_metric.insert(labels, curr);
        }
    }

    /// Extract the metric name and labels from snapshot metric metadata.
    #[cfg(feature = "ingest")]
    fn extract_name_labels(metadata: &HashMap<String, String>) -> (String, Labels) {
        let name = metadata.get("metric").cloned().unwrap_or_default();

        let mut labels = Labels::default();
        for (k, v) in metadata {
            match k.as_str() {
                "metric" | "unit" | "grouping_power" | "max_value_power" => continue,
                _ => {
                    labels.inner.insert(k.clone(), v.clone());
                }
            }
        }

        (name, labels)
    }

    pub fn counters(&self, name: &str, labels: impl Into<Labels>) -> Option<CounterCollection> {
        if let Some(counters) = self.counters.get(name) {
            let counters = counters.filter(&labels.into());

            if counters.is_empty() {
                None
            } else {
                Some(counters)
            }
        } else {
            None
        }
    }

    pub fn gauges(&self, name: &str, labels: impl Into<Labels>) -> Option<GaugeCollection> {
        if let Some(gauges) = self.gauges.get(name) {
            let gauges = gauges.filter(&labels.into());

            if gauges.is_empty() {
                None
            } else {
                Some(gauges)
            }
        } else {
            None
        }
    }

    pub fn histograms(&self, name: &str, labels: impl Into<Labels>) -> Option<HistogramCollection> {
        if let Some(histograms) = self.histograms.get(name) {
            let histograms = histograms.filter(&labels.into());

            if histograms.is_empty() {
                None
            } else {
                Some(histograms)
            }
        } else {
            None
        }
    }

    #[allow(dead_code)]
    pub fn percentiles(
        &self,
        metric: &str,
        labels: impl Into<Labels>,
        percentiles: &[f64],
    ) -> Option<Vec<UntypedSeries>> {
        if let Some(collection) = self.histograms(metric, labels) {
            collection.sum().percentiles(percentiles, None)
        } else {
            None
        }
    }

    // sampling interval in seconds
    pub fn interval(&self) -> f64 {
        self.sampling_interval_ms as f64 / 1000.0
    }

    /// Returns the time range (min, max) in nanoseconds across all data, or
    /// None if empty.
    pub fn time_range(&self) -> Option<(u64, u64)> {
        let mut min_time: Option<u64> = None;
        let mut max_time: Option<u64> = None;

        for collection in self.counters.values() {
            if let Some((coll_min, coll_max)) = collection.time_bounds() {
                min_time = Some(min_time.map_or(coll_min, |m| m.min(coll_min)));
                max_time = Some(max_time.map_or(coll_max, |m| m.max(coll_max)));
            }
        }

        for collection in self.gauges.values() {
            if let Some((coll_min, coll_max)) = collection.time_bounds() {
                min_time = Some(min_time.map_or(coll_min, |m| m.min(coll_min)));
                max_time = Some(max_time.map_or(coll_max, |m| m.max(coll_max)));
            }
        }

        for collection in self.histograms.values() {
            if let Some((coll_min, coll_max)) = collection.time_bounds() {
                min_time = Some(min_time.map_or(coll_min, |m| m.min(coll_min)));
                max_time = Some(max_time.map_or(coll_max, |m| m.max(coll_max)));
            }
        }

        min_time.zip(max_time)
    }

    // data source
    pub fn source(&self) -> &str {
        &self.source
    }

    // data source version
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn filename(&self) -> &str {
        &self.filename
    }

    pub fn file_metadata(&self) -> &HashMap<String, String> {
        &self.file_metadata
    }

    // Get all counter metric names
    pub fn counter_names(&self) -> Vec<&str> {
        self.counters.keys().map(|s| s.as_str()).collect()
    }

    // Get all gauge metric names
    pub fn gauge_names(&self) -> Vec<&str> {
        self.gauges.keys().map(|s| s.as_str()).collect()
    }

    // Get all histogram metric names
    pub fn histogram_names(&self) -> Vec<&str> {
        self.histograms.keys().map(|s| s.as_str()).collect()
    }

    // Get labels for a specific counter metric
    pub fn counter_labels(&self, name: &str) -> Option<Vec<Labels>> {
        self.counters.get(name).map(|collection| {
            collection
                .iter()
                .map(|(labels, _)| labels.clone())
                .collect()
        })
    }

    // Get labels for a specific gauge metric
    pub fn gauge_labels(&self, name: &str) -> Option<Vec<Labels>> {
        self.gauges.get(name).map(|collection| {
            collection
                .iter()
                .map(|(labels, _)| labels.clone())
                .collect()
        })
    }

    // Get labels for a specific histogram metric
    pub fn histogram_labels(&self, name: &str) -> Option<Vec<Labels>> {
        self.histograms.get(name).map(|collection| {
            collection
                .iter()
                .map(|(labels, _)| labels.clone())
                .collect()
        })
    }
}
