//! Synthetic Parquet fixture builder for the metriken-query regression harness.
//!
//! Wraps `metriken-exposition`'s `ParquetSchema` / `ParquetWriter` so tests can
//! emit small, fully deterministic Parquet files that exercise every branch
//! of the metriken-query loader and PromQL evaluator (counters with resets,
//! gauges with negatives, histograms with empty periods and resets, multi-label
//! series, custom sampling intervals, etc.).
//!
//! Two ergonomic differences from raw `metriken-exposition`:
//!
//! 1. The builder is *series-shaped*, not snapshot-shaped — you describe each
//!    labeled series once with all its `(timestamp, value)` points, and the
//!    builder transposes them into per-timestamp `Snapshot`s before writing.
//! 2. Timestamps are nanoseconds offset from the Unix epoch, so output is
//!    byte-identical across machines and runs.
//!
//! Multiple labeled series for the same metric are emitted as distinct Parquet
//! columns whose `metadata["metric"]` field carries the canonical metric name
//! and whose remaining metadata entries become PromQL labels — exactly the
//! shape the loader at `metriken-query/src/tsdb/mod.rs:139-163` consumes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use histogram::Histogram as H2Histogram;
use metriken_exposition::{
    Counter, Gauge, Histogram as ExpoHistogram, ParquetOptions, ParquetSchema, Snapshot,
    SnapshotV2,
};

/// Default sampling interval matches the loader default at `tsdb/mod.rs:112`.
pub const DEFAULT_SAMPLING_INTERVAL_MS: u64 = 1000;

/// Histogram config used by every Rezolus latency histogram in the existing
/// test suite (see `tsdb/series/histogram.rs:290`). Equivalent to ~496 buckets.
pub const REZOLUS_HISTOGRAM_GP: u8 = 4;
pub const REZOLUS_HISTOGRAM_MVP: u8 = 16;

/// One time-ordered counter series. `column_name` must be unique across all
/// counter series in a fixture (it becomes the Parquet column name).
pub struct CounterSeries {
    pub column_name: String,
    pub metric: String,
    pub labels: BTreeMap<String, String>,
    /// `(timestamp_ns, cumulative_value)` pairs. Counter semantics: typically
    /// monotonically non-decreasing; a decrease is treated as a reset by the
    /// PromQL evaluator.
    pub points: Vec<(u64, u64)>,
}

/// One time-ordered gauge series.
pub struct GaugeSeries {
    pub column_name: String,
    pub metric: String,
    pub labels: BTreeMap<String, String>,
    pub points: Vec<(u64, i64)>,
}

/// One time-ordered histogram series.
///
/// `points` holds `(timestamp_ns, cumulative_buckets)`. The first snapshot in
/// any histogram series is consumed as a primer by the loader (it has no
/// `prev` to difference against), so callers needing N usable delta snapshots
/// must supply N+1 points. A `None` value is allowed for null/absent rows.
pub struct HistogramSeries {
    pub column_name: String,
    pub metric: String,
    pub labels: BTreeMap<String, String>,
    pub grouping_power: u8,
    pub max_value_power: u8,
    pub points: Vec<(u64, Option<Vec<u64>>)>,
}

/// Top-level fixture description.
#[derive(Default)]
pub struct FixtureBuilder {
    sampling_interval_ms: Option<u64>,
    file_metadata: HashMap<String, String>,
    counters: Vec<CounterSeries>,
    gauges: Vec<GaugeSeries>,
    histograms: Vec<HistogramSeries>,
}

impl FixtureBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sampling_interval_ms(mut self, ms: u64) -> Self {
        self.sampling_interval_ms = Some(ms);
        self
    }

    /// Set arbitrary file-level kv metadata (`source`, `version`, …).
    pub fn file_metadata(mut self, key: &str, value: &str) -> Self {
        self.file_metadata.insert(key.into(), value.into());
        self
    }

    pub fn add_counter(mut self, series: CounterSeries) -> Self {
        self.counters.push(series);
        self
    }

    pub fn add_gauge(mut self, series: GaugeSeries) -> Self {
        self.gauges.push(series);
        self
    }

    pub fn add_histogram(mut self, series: HistogramSeries) -> Self {
        self.histograms.push(series);
        self
    }

    /// Write the fixture to `path`. Output is deterministic — same input,
    /// byte-identical Parquet bytes.
    pub fn write(self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let interval_ms = self
            .sampling_interval_ms
            .unwrap_or(DEFAULT_SAMPLING_INTERVAL_MS);

        let mut file_metadata = self.file_metadata.clone();
        file_metadata
            .entry("sampling_interval_ms".into())
            .or_insert_with(|| interval_ms.to_string());
        file_metadata
            .entry("source".into())
            .or_insert_with(|| "metriken-query-fixtures".into());
        file_metadata
            .entry("version".into())
            .or_insert_with(|| env!("CARGO_PKG_VERSION").into());

        let snapshots = self.transpose_to_snapshots()?;

        let mut schema = ParquetSchema::new();
        for s in &snapshots {
            schema.push(s.clone());
        }

        let file = File::create(path)?;
        let options = ParquetOptions::new().max_batch_size(256);
        let mut writer = schema.finalize(file, options, Some(file_metadata))?;
        for s in snapshots {
            writer.push(s)?;
        }
        writer.finalize()?;
        Ok(())
    }

    /// Transpose the per-series description into a sequence of per-timestamp
    /// `SnapshotV2`s ordered by timestamp. Each snapshot includes only the
    /// series that have a value at that timestamp.
    fn transpose_to_snapshots(self) -> Result<Vec<Snapshot>, Box<dyn std::error::Error>> {
        let mut all_ts: BTreeSet<u64> = BTreeSet::new();
        for s in &self.counters {
            all_ts.extend(s.points.iter().map(|p| p.0));
        }
        for s in &self.gauges {
            all_ts.extend(s.points.iter().map(|p| p.0));
        }
        for s in &self.histograms {
            all_ts.extend(s.points.iter().map(|p| p.0));
        }

        let counter_lookup: Vec<(&CounterSeries, BTreeMap<u64, u64>)> = self
            .counters
            .iter()
            .map(|s| (s, s.points.iter().copied().collect()))
            .collect();
        let gauge_lookup: Vec<(&GaugeSeries, BTreeMap<u64, i64>)> = self
            .gauges
            .iter()
            .map(|s| (s, s.points.iter().copied().collect()))
            .collect();
        let histogram_lookup: Vec<(&HistogramSeries, BTreeMap<u64, Option<Vec<u64>>>)> = self
            .histograms
            .iter()
            .map(|s| (s, s.points.iter().cloned().collect()))
            .collect();

        let mut prev_ts: Option<u64> = None;
        let mut out = Vec::with_capacity(all_ts.len());
        for ts in all_ts {
            let duration = Duration::from_nanos(prev_ts.map(|p| ts - p).unwrap_or(0));
            prev_ts = Some(ts);
            let systemtime = UNIX_EPOCH + Duration::from_nanos(ts);

            let mut counters = Vec::new();
            for (s, idx) in &counter_lookup {
                if let Some(&v) = idx.get(&ts) {
                    counters.push(Counter {
                        name: s.column_name.clone(),
                        value: v,
                        metadata: build_metadata(&s.metric, &s.labels, None),
                    });
                }
            }

            let mut gauges = Vec::new();
            for (s, idx) in &gauge_lookup {
                if let Some(&v) = idx.get(&ts) {
                    gauges.push(Gauge {
                        name: s.column_name.clone(),
                        value: v,
                        metadata: build_metadata(&s.metric, &s.labels, None),
                    });
                }
            }

            let mut histograms = Vec::new();
            for (s, idx) in &histogram_lookup {
                let Some(v) = idx.get(&ts) else { continue };
                let Some(buckets) = v else { continue };
                let h = H2Histogram::from_buckets(
                    s.grouping_power,
                    s.max_value_power,
                    buckets.clone(),
                )?;
                let extra = Some((s.grouping_power, s.max_value_power));
                histograms.push(ExpoHistogram {
                    name: s.column_name.clone(),
                    value: h,
                    metadata: build_metadata(&s.metric, &s.labels, extra),
                });
            }

            out.push(Snapshot::V2(SnapshotV2 {
                systemtime,
                duration,
                metadata: HashMap::new(),
                counters,
                gauges,
                histograms,
            }));
        }
        Ok(out)
    }
}

/// Produce per-column metadata in the shape the loader expects.
fn build_metadata(
    metric: &str,
    labels: &BTreeMap<String, String>,
    histogram_config: Option<(u8, u8)>,
) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("metric".to_string(), metric.to_string());
    for (k, v) in labels {
        m.insert(k.clone(), v.clone());
    }
    if let Some((gp, mvp)) = histogram_config {
        m.insert("grouping_power".to_string(), gp.to_string());
        m.insert("max_value_power".to_string(), mvp.to_string());
    }
    m
}

/// Convenience: turn a slice of `(key, value)` pairs into a label map.
pub fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// A starter histogram: all-zero cumulative buckets sized for the given config.
/// The histogram crate expects `grouping_power < max_value_power`.
pub fn empty_buckets(grouping_power: u8, max_value_power: u8) -> Vec<u64> {
    let n_buckets = histogram::Config::new(grouping_power, max_value_power)
        .expect("invalid histogram config")
        .total_buckets();
    vec![0u64; n_buckets]
}

/// Build a cumulative bucket array by adding `(value, count)` events to the
/// previous cumulative state. Useful for assembling histogram series step by
/// step without reaching for the full `Histogram` API at the call site.
pub fn add_events(
    prev: &[u64],
    grouping_power: u8,
    max_value_power: u8,
    events: &[(u64, u64)],
) -> Result<Vec<u64>, Box<dyn std::error::Error>> {
    let mut h = H2Histogram::from_buckets(grouping_power, max_value_power, prev.to_vec())?;
    for &(value, count) in events {
        h.add(value, count)?;
    }
    Ok(h.as_slice().to_vec())
}

/// Convert a count of seconds into the canonical fixture timestamp (ns from
/// the Unix epoch).
pub fn ts_secs(s: u64) -> u64 {
    s * 1_000_000_000
}
