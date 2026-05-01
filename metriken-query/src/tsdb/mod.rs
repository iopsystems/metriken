use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::ops::*;
use std::path::Path;

use arrow::array::{Int64Array, ListArray, UInt64Array};
use arrow::datatypes::DataType;
use bytes::Bytes;
use histogram::{CumulativeROHistogram, Histogram};
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::ProjectionMask;
use serde::Serialize;

mod collection;
mod heatmap;
mod labels;
mod series;

pub use collection::*;
pub use heatmap::Heatmap;
pub use labels::Labels;
pub use series::*;
use series::{delta_to_32_or_empty, empty_delta_32};

/// Per-column dispatch target precomputed from the parquet schema.
/// Histogram targets carry the rolling `prev` cumulative so per-period
/// deltas work across batches.
enum ColumnTarget {
    Skip,
    Counter {
        name: String,
        labels: Labels,
    },
    Gauge {
        name: String,
        labels: Labels,
    },
    Histogram {
        name: String,
        labels: Labels,
        grouping_power: u8,
        max_value_power: u8,
        config: Option<::histogram::Config>,
        prev: Option<CumulativeROHistogram>,
    },
}

/// Snap a nanosecond timestamp to the nearest multiple of `interval_ns`.
/// Returns the timestamp unchanged when `interval_ns` is zero (i.e. unknown).
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
    /// Most-recent cumulative per series; `ingest` differences against the
    /// next snapshot to produce the per-period delta. Unused on the parquet
    /// load path (which differences in-place).
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

        // Parse the footer once and reuse for every per-(row-group, column)
        // reader below — `try_new` re-parses on every call (multi-second on
        // wide files).
        let arrow_reader_meta = ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::default())?;
        let arrow_schema = arrow_reader_meta.schema().clone();
        let pq_metadata = arrow_reader_meta.metadata().clone();
        let parquet_schema = pq_metadata.file_metadata().schema_descr_ptr();
        let num_row_groups = pq_metadata.num_row_groups();

        let mut metadata = HashMap::new();
        if let Some(kv) = pq_metadata.file_metadata().key_value_metadata() {
            for entry in kv {
                metadata.insert(entry.key.clone(), entry.value.clone().unwrap_or_default());
            }
        }

        data.sampling_interval_ms = metadata
            .get("sampling_interval_ms")
            .map(|v| v.parse::<u64>().expect("bad interval"))
            .unwrap_or(1000);
        data.source = metadata
            .get("source")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        data.version = metadata
            .get("version")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        data.file_metadata = metadata;

        let interval_ns = data.sampling_interval_ms * 1_000_000;

        let ts_col_idx = arrow_schema
            .index_of("timestamp")
            .map_err(|_| "missing 'timestamp' column")?;

        // Precompute targets so the hot loop doesn't re-parse schema
        // metadata per batch.
        let mut targets: Vec<ColumnTarget> = arrow_schema
            .fields()
            .iter()
            .enumerate()
            .map(|(col_idx, field)| {
                if col_idx == ts_col_idx {
                    return ColumnTarget::Skip;
                }
                let mut meta = field.metadata().clone();
                let name = if let Some(n) = meta.get("metric").cloned() {
                    n
                } else {
                    let col_name = field.name();
                    col_name
                        .strip_suffix(":buckets")
                        .unwrap_or(col_name)
                        .to_string()
                };
                let grouping_power: Option<u8> =
                    meta.remove("grouping_power").and_then(|v| v.parse().ok());
                let max_value_power: Option<u8> =
                    meta.remove("max_value_power").and_then(|v| v.parse().ok());

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

                match field.data_type() {
                    DataType::UInt64 => ColumnTarget::Counter { name, labels },
                    DataType::Int64 => ColumnTarget::Gauge { name, labels },
                    DataType::List(inner) if inner.data_type() == &DataType::UInt64 => {
                        let (Some(gp), Some(mvp)) = (grouping_power, max_value_power) else {
                            return ColumnTarget::Skip;
                        };
                        let config = ::histogram::Config::new(gp, mvp).ok();
                        ColumnTarget::Histogram {
                            name,
                            labels,
                            grouping_power: gp,
                            max_value_power: mvp,
                            config,
                            prev: None,
                        }
                    }
                    _ => ColumnTarget::Skip,
                }
            })
            .collect();

        // Decode one column at a time within each row group; peak resident
        // is bounded by one decoded Arrow array.
        let mut timestamps: Vec<Option<u64>> = Vec::new();
        for rg_idx in 0..num_row_groups {
            let ts_reader = ParquetRecordBatchReaderBuilder::new_with_metadata(
                bytes.clone(),
                arrow_reader_meta.clone(),
            )
            .with_row_groups(vec![rg_idx])
            .with_projection(ProjectionMask::roots(&parquet_schema, [ts_col_idx]))
            .build()?;
            timestamps.clear();
            for batch in ts_reader.flatten() {
                let ts_arr = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or("timestamp column is not UInt64")?;
                timestamps.reserve(ts_arr.len());
                for v in ts_arr.iter() {
                    timestamps.push(v.map(|raw| snap_timestamp(raw, interval_ns)));
                }
            }

            for (col_idx, target) in targets.iter_mut().enumerate() {
                if col_idx == ts_col_idx {
                    continue;
                }
                if matches!(target, ColumnTarget::Skip) {
                    continue;
                }

                let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(
                    bytes.clone(),
                    arrow_reader_meta.clone(),
                )
                .with_row_groups(vec![rg_idx])
                .with_projection(ProjectionMask::roots(&parquet_schema, [col_idx]))
                .build()?;

                match target {
                    ColumnTarget::Skip => unreachable!(),
                    ColumnTarget::Counter { name, labels } => {
                        let series = data
                            .counters
                            .entry(name.clone())
                            .or_default()
                            .entry(labels.clone())
                            .or_default();
                        let mut row = 0usize;
                        for batch in reader.flatten() {
                            let arr = batch
                                .column(0)
                                .as_any()
                                .downcast_ref::<UInt64Array>()
                                .expect("counter column is not UInt64");
                            for v in arr.iter() {
                                if let (Some(v), Some(Some(ts))) = (v, timestamps.get(row)) {
                                    series.insert(*ts, v);
                                }
                                row += 1;
                            }
                        }
                    }
                    ColumnTarget::Gauge { name, labels } => {
                        let series = data
                            .gauges
                            .entry(name.clone())
                            .or_default()
                            .entry(labels.clone())
                            .or_default();
                        let mut row = 0usize;
                        for batch in reader.flatten() {
                            let arr = batch
                                .column(0)
                                .as_any()
                                .downcast_ref::<Int64Array>()
                                .expect("gauge column is not Int64");
                            for v in arr.iter() {
                                if let (Some(v), Some(Some(ts))) = (v, timestamps.get(row)) {
                                    series.insert(*ts, v);
                                }
                                row += 1;
                            }
                        }
                    }
                    ColumnTarget::Histogram {
                        ref name,
                        ref labels,
                        grouping_power,
                        max_value_power,
                        ref config,
                        ref mut prev,
                    } => {
                        let gp = *grouping_power;
                        let mvp = *max_value_power;
                        let cfg = *config;
                        let series = data
                            .histograms
                            .entry(name.clone())
                            .or_default()
                            .entry(labels.clone())
                            .or_default();

                        let mut row = 0usize;
                        for batch in reader.flatten() {
                            let list = batch
                                .column(0)
                                .as_any()
                                .downcast_ref::<ListArray>()
                                .expect("histogram column is not List");
                            for value in list.iter() {
                                let Some(Some(ts)) = timestamps.get(row).copied() else {
                                    row += 1;
                                    continue;
                                };

                                let curr = value.and_then(|list_value| {
                                    let arr = list_value
                                        .as_any()
                                        .downcast_ref::<UInt64Array>()
                                        .expect("histogram inner is not UInt64");
                                    let buckets: Vec<u64> = arr.iter().flatten().collect();
                                    Histogram::from_buckets(gp, mvp, buckets)
                                        .ok()
                                        .map(|h| CumulativeROHistogram::from(&h))
                                });

                                match (prev.as_ref(), curr.as_ref()) {
                                    (Some(prev_cumu), Some(curr_cumu)) => {
                                        series
                                            .insert(ts, delta_to_32_or_empty(prev_cumu, curr_cumu));
                                    }
                                    (Some(_), None) => {
                                        if let Some(cfg) = cfg {
                                            series.insert(ts, empty_delta_32(cfg));
                                        }
                                    }
                                    _ => {}
                                }
                                if curr.is_some() {
                                    *prev = curr;
                                }
                                row += 1;
                            }
                        }
                    }
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
                let d = delta_to_32_or_empty(prev, &curr);
                self.histograms
                    .entry(name.clone())
                    .or_default()
                    .entry(labels.clone())
                    .or_default()
                    .insert(ts, d);
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

    /// Borrow the raw counter collection without cloning, so streaming
    /// iterator chains can reference TSDB storage directly.
    pub fn counters_ref(&self, name: &str) -> Option<&CounterCollection> {
        self.counters.get(name)
    }

    /// See [`Tsdb::counters_ref`].
    pub fn gauges_ref(&self, name: &str) -> Option<&GaugeCollection> {
        self.gauges.get(name)
    }

    /// See [`Tsdb::counters_ref`].
    pub fn histograms_ref(&self, name: &str) -> Option<&HistogramCollection> {
        self.histograms.get(name)
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

#[cfg(all(test, feature = "ingest"))]
mod ingest_tests {
    use std::time::{Duration, SystemTime};

    use histogram::Histogram;
    use metriken_exposition::{Histogram as SnapHistogram, Snapshot, SnapshotV2};

    use super::*;

    fn snapshot_at(ts_secs: u64, hist: Histogram, name: &str) -> Snapshot {
        let systemtime = SystemTime::UNIX_EPOCH + Duration::from_secs(ts_secs);
        let mut metadata = HashMap::new();
        metadata.insert("metric".to_string(), name.to_string());
        Snapshot::V2(SnapshotV2 {
            systemtime,
            duration: Duration::from_secs(1),
            metadata: HashMap::new(),
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: vec![SnapHistogram {
                name: name.to_string(),
                value: hist,
                metadata,
            }],
        })
    }

    /// End-to-end check that the ingest path preserves every observed
    /// snapshot's timestamp, even when the per-period delta is empty (no
    /// events) or unrepresentable (counter reset).  Without explicit empties
    /// the offset-aligned lookup pattern would silently shift entries onto
    /// the wrong timestamps.
    #[test]
    fn ingest_preserves_timestamps_for_empty_and_reset_deltas() {
        let mut tsdb = Tsdb {
            sampling_interval_ms: 1000,
            ..Tsdb::default()
        };

        // Construct four snapshots of one histogram metric.  Bucket index 10
        // grows: 0 → 5 → 5 (no events) → 8 → 1 (reset).
        let snapshots: Vec<(u64, &[u32])> = vec![
            (1, &[]),                               // first cumulative — no delta produced
            (2, &[10, 10, 10, 10, 10]),             // 5 events bucket 10 (vs s1)
            (3, &[10, 10, 10, 10, 10]),             // identical → empty delta
            (4, &[10, 10, 10, 10, 10, 10, 10, 10]), // +3 events vs s3
            (5, &[10]),                             // reset (cumu went down) → empty delta
        ];

        for (ts, samples) in &snapshots {
            let mut h = Histogram::new(4, 16).unwrap();
            for v in *samples {
                h.increment(*v as u64).unwrap();
            }
            tsdb.ingest(snapshot_at(*ts, h, "lat"));
        }

        let collection = tsdb.histograms_ref("lat").expect("histogram series exists");
        let (_, series) = collection.iter().next().expect("one labelset");

        // s1 produces no delta; s2..s5 each produce one.  4 entries expected.
        let times: Vec<u64> = series.iter().map(|(t, _)| t).collect();
        let expected: Vec<u64> = vec![2_000_000_000, 3_000_000_000, 4_000_000_000, 5_000_000_000];
        assert_eq!(
            times, expected,
            "every observed snapshot timestamp must be present"
        );

        // Spot-check empties: s2->s3 had no new events, s4->s5 reset.
        let entry = |ts: u64| {
            series
                .iter()
                .find(|(t, _)| *t == ts)
                .map(|(_, h)| h)
                .unwrap()
        };
        assert!(entry(3_000_000_000).is_empty(), "no-event delta is empty");
        assert!(entry(5_000_000_000).is_empty(), "reset delta is empty");
        assert!(!entry(2_000_000_000).is_empty());
        assert!(!entry(4_000_000_000).is_empty());
    }
}
