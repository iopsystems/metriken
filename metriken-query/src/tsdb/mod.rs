use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::io::Write;
use std::ops::*;
use std::path::Path;

use arrow::datatypes::DataType;
use bytes::Bytes;
use duckdb::types::Value as DuckValue;
use histogram::{CumulativeROHistogram, Histogram};
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};
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
/// `column_name` is the parquet field name, preserved so callers can map
/// a resolved series back to its on-disk column without re-parsing the
/// schema.
enum ColumnTarget {
    Skip,
    Counter {
        name: String,
        labels: Labels,
        column_name: String,
    },
    Gauge {
        name: String,
        labels: Labels,
        column_name: String,
    },
    Histogram {
        name: String,
        labels: Labels,
        column_name: String,
        grouping_power: u8,
        max_value_power: u8,
        config: Option<::histogram::Config>,
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

/// Quote a parquet column name for inclusion in a DuckDB SQL statement.
/// SQL identifiers can't be bound as parameters, so callers building
/// queries over schema-derived column names must route every name
/// through here rather than interpolating raw. NUL bytes are rejected
/// because some SQL parsers treat them as string terminators; every
/// other character is permitted inside ANSI/DuckDB quoted identifiers as
/// long as embedded double quotes are doubled.
fn quote_ident(name: &str) -> Result<String, Box<dyn Error>> {
    if name.as_bytes().contains(&0) {
        return Err(format!("column name contains NUL byte: {name:?}").into());
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
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
    /// Parquet column name for each loaded `(metric_name, labels)`
    /// pair: populated from `field.name()` on the parquet load path,
    /// synthesized on the ingest path. Consumed by
    /// `QueryEngine::columns()`.
    columns: HashMap<String, HashMap<Labels, String>>,
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

        // Parse the footer once for schema/labels inspection. We continue to
        // read per-column Arrow field metadata via the `parquet` crate because
        // DuckDB doesn't expose it through SQL; data fetch is delegated to
        // DuckDB below.
        let arrow_reader_meta = ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::default())?;
        let arrow_schema = arrow_reader_meta.schema().clone();
        let pq_metadata = arrow_reader_meta.metadata();

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
        let targets: Vec<ColumnTarget> = arrow_schema
            .fields()
            .iter()
            .enumerate()
            .map(|(col_idx, field)| {
                if col_idx == ts_col_idx {
                    return ColumnTarget::Skip;
                }
                let mut meta = field.metadata().clone();
                let column_name = field.name().to_string();
                let name = if let Some(n) = meta.get("metric").cloned() {
                    n
                } else {
                    column_name
                        .strip_suffix(":buckets")
                        .unwrap_or(&column_name)
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
                    DataType::UInt64 => ColumnTarget::Counter {
                        name,
                        labels,
                        column_name,
                    },
                    DataType::Int64 => ColumnTarget::Gauge {
                        name,
                        labels,
                        column_name,
                    },
                    DataType::List(inner) if inner.data_type() == &DataType::UInt64 => {
                        let (Some(gp), Some(mvp)) = (grouping_power, max_value_power) else {
                            return ColumnTarget::Skip;
                        };
                        let config = ::histogram::Config::new(gp, mvp).ok();
                        ColumnTarget::Histogram {
                            name,
                            labels,
                            column_name,
                            grouping_power: gp,
                            max_value_power: mvp,
                            config,
                        }
                    }
                    _ => ColumnTarget::Skip,
                }
            })
            .collect();

        // DuckDB needs a filesystem path for `read_parquet`. Materialize the
        // input bytes to a tempfile so the loader has a single code path for
        // both `load(path)` (which round-trips through bytes) and direct
        // `load_from_bytes` callers. The schema work above already happened
        // on `bytes` directly, so the tempfile is only used for the data
        // fetch below.
        let mut tmp = tempfile::NamedTempFile::new()?;
        tmp.as_file_mut().write_all(&bytes)?;
        tmp.as_file_mut().sync_all()?;
        let parquet_path = tmp
            .path()
            .to_str()
            .ok_or("tempfile path is not valid UTF-8")?
            .to_string();

        let conn = duckdb::Connection::open_in_memory()?;

        // One SELECT per non-Skip column, returning (timestamp, value) pairs.
        // Pairing timestamp with the value in the same query removes the
        // row-index alignment dance the prior `parquet` code carried across
        // batches. `ORDER BY timestamp` keeps inserts in ascending order so
        // `Series::insert` stays on its O(1) append fast path (and is
        // load-bearing for histograms, where deltas are computed against the
        // immediately preceding snapshot via the stack-local `prev`).
        for target in targets.iter() {
            match target {
                ColumnTarget::Skip => continue,
                ColumnTarget::Counter {
                    name,
                    labels,
                    column_name,
                } => {
                    data.columns
                        .entry(name.clone())
                        .or_default()
                        .insert(labels.clone(), column_name.clone());
                    let series = data
                        .counters
                        .entry(name.clone())
                        .or_default()
                        .entry(labels.clone())
                        .or_default();

                    let col = quote_ident(column_name)?;
                    let sql = format!(
                        "SELECT timestamp, {col} FROM read_parquet(?) \
                         WHERE timestamp IS NOT NULL AND {col} IS NOT NULL \
                         ORDER BY timestamp"
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt.query_map([&parquet_path], |row| {
                        Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?))
                    })?;
                    for r in rows {
                        let (ts_raw, v) = r?;
                        series.insert(snap_timestamp(ts_raw, interval_ns), v);
                    }
                }
                ColumnTarget::Gauge {
                    name,
                    labels,
                    column_name,
                } => {
                    data.columns
                        .entry(name.clone())
                        .or_default()
                        .insert(labels.clone(), column_name.clone());
                    let series = data
                        .gauges
                        .entry(name.clone())
                        .or_default()
                        .entry(labels.clone())
                        .or_default();

                    let col = quote_ident(column_name)?;
                    let sql = format!(
                        "SELECT timestamp, {col} FROM read_parquet(?) \
                         WHERE timestamp IS NOT NULL AND {col} IS NOT NULL \
                         ORDER BY timestamp"
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt.query_map([&parquet_path], |row| {
                        Ok((row.get::<_, u64>(0)?, row.get::<_, i64>(1)?))
                    })?;
                    for r in rows {
                        let (ts_raw, v) = r?;
                        series.insert(snap_timestamp(ts_raw, interval_ns), v);
                    }
                }
                ColumnTarget::Histogram {
                    name,
                    labels,
                    column_name,
                    grouping_power,
                    max_value_power,
                    config,
                } => {
                    let gp = *grouping_power;
                    let mvp = *max_value_power;
                    let cfg = *config;
                    data.columns
                        .entry(name.clone())
                        .or_default()
                        .insert(labels.clone(), column_name.clone());
                    let series = data
                        .histograms
                        .entry(name.clone())
                        .or_default()
                        .entry(labels.clone())
                        .or_default();

                    // Null bucket lists are kept (not WHERE-filtered) so a
                    // missing snapshot still produces an explicit empty
                    // delta against `prev` — matches the prior loader and
                    // keeps offset-based timestamp axes aligned.
                    let col = quote_ident(column_name)?;
                    let sql = format!(
                        "SELECT timestamp, {col} FROM read_parquet(?) \
                         WHERE timestamp IS NOT NULL \
                         ORDER BY timestamp"
                    );
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt.query_map([&parquet_path], |row| {
                        Ok((row.get::<_, u64>(0)?, row.get::<_, DuckValue>(1)?))
                    })?;

                    let mut prev: Option<CumulativeROHistogram> = None;
                    for r in rows {
                        let (ts_raw, val) = r?;
                        let ts = snap_timestamp(ts_raw, interval_ns);

                        let curr = match val {
                            DuckValue::List(elems) => {
                                let buckets: Vec<u64> = elems
                                    .iter()
                                    .map(|v| match v {
                                        DuckValue::UBigInt(x) => *x,
                                        DuckValue::Null => 0,
                                        _ => panic!("histogram inner is not UBIGINT"),
                                    })
                                    .collect();
                                Histogram::from_buckets(gp, mvp, buckets)
                                    .ok()
                                    .map(|h| CumulativeROHistogram::from(&h))
                            }
                            _ => None,
                        };

                        match (prev.as_ref(), curr.as_ref()) {
                            (Some(prev_cumu), Some(curr_cumu)) => {
                                series.insert(ts, delta_to_32_or_empty(prev_cumu, curr_cumu));
                            }
                            (Some(_), None) => {
                                if let Some(cfg) = cfg {
                                    series.insert(ts, empty_delta_32(cfg));
                                }
                            }
                            _ => {}
                        }
                        if curr.is_some() {
                            prev = curr;
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
            self.columns
                .entry(name.clone())
                .or_default()
                .entry(labels.clone())
                .or_insert_with(|| name.clone());
            self.counters
                .entry(name)
                .or_default()
                .entry(labels)
                .or_default()
                .insert(ts, counter.value);
        }

        for gauge in snapshot.gauges() {
            let (name, labels) = Self::extract_name_labels(&gauge.metadata);
            self.columns
                .entry(name.clone())
                .or_default()
                .entry(labels.clone())
                .or_insert_with(|| name.clone());
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
                self.columns
                    .entry(name.clone())
                    .or_default()
                    .entry(labels.clone())
                    .or_insert_with(|| format!("{name}:buckets"));
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

    /// Parquet column name for the `(metric_name, labels)` pair, or
    /// `None` if no such series was loaded.
    pub fn column(&self, name: &str, labels: &Labels) -> Option<&str> {
        self.columns.get(name)?.get(labels).map(String::as_str)
    }

    /// Iteration-friendly view of the column map, for resolvers that
    /// need to scan every series (e.g. regex / negated `__name__`).
    pub(crate) fn columns_ref(&self) -> &HashMap<String, HashMap<Labels, String>> {
        &self.columns
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
