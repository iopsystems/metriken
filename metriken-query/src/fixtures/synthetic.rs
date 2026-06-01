use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array, ListArray, UInt64Array};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use tempfile::NamedTempFile;

type CounterFn = Box<dyn Fn(u64) -> u64 + Send + Sync>;
type GaugeFn = Box<dyn Fn(u64) -> i64 + Send + Sync>;
type HistogramFn = Box<dyn Fn(u64) -> Vec<u64> + Send + Sync>;

struct CounterSpec {
    name: String,
    labels: Vec<(String, String)>,
    values: CounterFn,
}

struct GaugeSpec {
    name: String,
    labels: Vec<(String, String)>,
    values: GaugeFn,
}

struct HistogramSpec {
    name: String,
    labels: Vec<(String, String)>,
    grouping_power: u8,
    max_value_power: u8,
    snapshots: HistogramFn,
}

/// Programmatic builder for synthetic parquet fixtures.
///
/// Produces a parquet file matching the rezolus capture schema:
/// - `timestamp: UInt64` (nanoseconds since epoch)
/// - Counter columns: `UInt64` with `metric_type=counter` field metadata
/// - Gauge columns: `Int64` with `metric_type=gauge` field metadata
/// - Histogram columns: `List<UInt64>` with histogram config in field metadata
///
/// File-level metadata: `sampling_interval_ms`, plus any custom keys added
/// via [`metadata()`](Self::metadata).
pub struct FixtureBuilder {
    sampling_interval_ms: u64,
    file_metadata: Vec<(String, String)>,
    row_group_size: usize,
    n_samples: usize,
    counters: Vec<CounterSpec>,
    gauges: Vec<GaugeSpec>,
    histograms: Vec<HistogramSpec>,
}

impl Default for FixtureBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl FixtureBuilder {
    pub fn new() -> Self {
        Self {
            sampling_interval_ms: 1000,
            file_metadata: Vec::new(),
            row_group_size: 1024,
            n_samples: 0,
            counters: Vec::new(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        }
    }

    pub fn sampling_interval_ms(mut self, ms: u64) -> Self {
        self.sampling_interval_ms = ms;
        self
    }

    /// Add a key-value pair to file-level metadata. `sampling_interval_ms`
    /// is set automatically from [`sampling_interval_ms()`](Self::sampling_interval_ms);
    /// don't add it here.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.file_metadata.push((key.into(), value.into()));
        self
    }

    pub fn row_group_size(mut self, n: usize) -> Self {
        assert!(n > 0, "row_group_size must be positive");
        self.row_group_size = n;
        self
    }

    pub fn samples(mut self, n: usize) -> Self {
        self.n_samples = n;
        self
    }

    pub fn counter<F: Fn(u64) -> u64 + Send + Sync + 'static>(
        mut self,
        name: impl Into<String>,
        labels: &[(&str, &str)],
        values_fn: F,
    ) -> Self {
        self.counters.push(CounterSpec {
            name: name.into(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            values: Box::new(values_fn),
        });
        self
    }

    pub fn gauge<F: Fn(u64) -> i64 + Send + Sync + 'static>(
        mut self,
        name: impl Into<String>,
        labels: &[(&str, &str)],
        values_fn: F,
    ) -> Self {
        self.gauges.push(GaugeSpec {
            name: name.into(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            values: Box::new(values_fn),
        });
        self
    }

    pub fn histogram<F: Fn(u64) -> Vec<u64> + Send + Sync + 'static>(
        mut self,
        name: impl Into<String>,
        labels: &[(&str, &str)],
        grouping_power: u8,
        max_value_power: u8,
        snapshots_fn: F,
    ) -> Self {
        self.histograms.push(HistogramSpec {
            name: name.into(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            grouping_power,
            max_value_power,
            snapshots: Box::new(snapshots_fn),
        });
        self
    }

    // Convenience patterns:

    pub fn monotonic_counter(
        self,
        name: impl Into<String>,
        labels: &[(&str, &str)],
        scale: u64,
    ) -> Self {
        self.counter(name, labels, move |t| t * scale)
    }

    pub fn resetting_counter(
        self,
        name: impl Into<String>,
        labels: &[(&str, &str)],
        scale: u64,
        reset_at: u64,
    ) -> Self {
        self.counter(name, labels, move |t| {
            if t < reset_at {
                t * scale
            } else {
                (t - reset_at) * scale
            }
        })
    }

    /// Histogram with all observations in a single bucket, accumulating at `rate` per tick.
    pub fn point_histogram(
        self,
        name: impl Into<String>,
        labels: &[(&str, &str)],
        grouping_power: u8,
        max_value_power: u8,
        bucket: u32,
        rate: u64,
    ) -> Self {
        let config = ::histogram::Config::new(grouping_power, max_value_power)
            .expect("invalid histogram config");
        let n_buckets = config.total_buckets();
        self.histogram(name, labels, grouping_power, max_value_power, move |t| {
            let mut buckets = vec![0u64; n_buckets];
            if (bucket as usize) < n_buckets {
                buckets[bucket as usize] = t * rate;
            }
            buckets
        })
    }

    pub fn build(self) -> Result<Fixture, Box<dyn Error + Send + Sync>> {
        assert!(self.n_samples > 0, "must set samples(n > 0)");

        // Build schema: timestamp + counters + gauges + histograms
        let mut fields = vec![Field::new("timestamp", DataType::UInt64, false)];

        let mut used_names: HashSet<String> = HashSet::new();
        let counter_columns: Vec<(String, &CounterSpec)> = self
            .counters
            .iter()
            .map(|c| (uniquify(&c.name, "", &mut used_names), c))
            .collect();
        let gauge_columns: Vec<(String, &GaugeSpec)> = self
            .gauges
            .iter()
            .map(|g| (uniquify(&g.name, "", &mut used_names), g))
            .collect();
        let histogram_columns: Vec<(String, &HistogramSpec)> = self
            .histograms
            .iter()
            .map(|h| (uniquify(&h.name, ":buckets", &mut used_names), h))
            .collect();

        for (col_name, spec) in &counter_columns {
            let mut metadata: HashMap<String, String> = HashMap::new();
            metadata.insert("metric".to_string(), spec.name.clone());
            metadata.insert("metric_type".to_string(), "counter".to_string());
            for (k, v) in &spec.labels {
                metadata.insert(k.clone(), v.clone());
            }
            let field = Field::new(col_name, DataType::UInt64, true).with_metadata(metadata);
            fields.push(field);
        }
        for (col_name, spec) in &gauge_columns {
            let mut metadata: HashMap<String, String> = HashMap::new();
            metadata.insert("metric".to_string(), spec.name.clone());
            metadata.insert("metric_type".to_string(), "gauge".to_string());
            for (k, v) in &spec.labels {
                metadata.insert(k.clone(), v.clone());
            }
            let field = Field::new(col_name, DataType::Int64, true).with_metadata(metadata);
            fields.push(field);
        }
        for (col_name, spec) in &histogram_columns {
            let mut metadata: HashMap<String, String> = HashMap::new();
            metadata.insert("metric".to_string(), spec.name.clone());
            metadata.insert("metric_type".to_string(), "histogram".to_string());
            metadata.insert(
                "grouping_power".to_string(),
                spec.grouping_power.to_string(),
            );
            metadata.insert(
                "max_value_power".to_string(),
                spec.max_value_power.to_string(),
            );
            for (k, v) in &spec.labels {
                metadata.insert(k.clone(), v.clone());
            }
            let inner = Field::new("item", DataType::UInt64, true);
            let field =
                Field::new(col_name, DataType::List(Arc::new(inner)), true).with_metadata(metadata);
            fields.push(field);
        }

        let schema = Arc::new(Schema::new(fields));

        // File metadata: sampling_interval_ms + user-supplied
        let mut kv = vec![KeyValue {
            key: "sampling_interval_ms".to_string(),
            value: Some(self.sampling_interval_ms.to_string()),
        }];
        for (k, v) in &self.file_metadata {
            kv.push(KeyValue {
                key: k.clone(),
                value: Some(v.clone()),
            });
        }

        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED) // faster for tests
            .set_key_value_metadata(Some(kv))
            .set_max_row_group_row_count(Some(self.row_group_size))
            .build();

        let named = NamedTempFile::with_suffix(".parquet")?;
        let file = named.reopen()?;
        let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props))?;

        let interval_ns = self.sampling_interval_ms * 1_000_000;
        // Generate batches of row_group_size rows
        let mut t = 0u64;
        while (t as usize) < self.n_samples {
            let batch_end = (t + self.row_group_size as u64).min(self.n_samples as u64);
            let batch_size = (batch_end - t) as usize;

            let timestamps: Vec<u64> = (0..batch_size)
                .map(|i| (t + i as u64) * interval_ns)
                .collect();
            let ts_array = Arc::new(UInt64Array::from(timestamps)) as ArrayRef;

            let mut columns: Vec<ArrayRef> = vec![ts_array];

            for (_, spec) in &counter_columns {
                let values: Vec<u64> = (0..batch_size)
                    .map(|i| (spec.values)(t + i as u64))
                    .collect();
                columns.push(Arc::new(UInt64Array::from(values)) as ArrayRef);
            }
            for (_, spec) in &gauge_columns {
                let values: Vec<i64> = (0..batch_size)
                    .map(|i| (spec.values)(t + i as u64))
                    .collect();
                columns.push(Arc::new(Int64Array::from(values)) as ArrayRef);
            }
            for (_, spec) in &histogram_columns {
                // Build a ListArray of UInt64 buckets per row.
                let mut offsets: Vec<i32> = Vec::with_capacity(batch_size + 1);
                offsets.push(0);
                let mut all_values: Vec<u64> = Vec::new();
                for i in 0..batch_size {
                    let buckets = (spec.snapshots)(t + i as u64);
                    all_values.extend(buckets);
                    offsets.push(all_values.len() as i32);
                }
                let value_field = Arc::new(Field::new("item", DataType::UInt64, true));
                let values_array = Arc::new(UInt64Array::from(all_values));
                let list_array = ListArray::new(
                    value_field,
                    OffsetBuffer::new(offsets.into()),
                    values_array,
                    None,
                );
                columns.push(Arc::new(list_array) as ArrayRef);
            }

            let batch = RecordBatch::try_new(schema.clone(), columns)?;
            writer.write(&batch)?;
            t = batch_end;
        }

        writer.close()?;

        let size_bytes = std::fs::metadata(named.path())?.len();
        Ok(Fixture {
            file: named,
            size_bytes,
        })
    }
}

fn uniquify(base: &str, suffix: &str, seen: &mut HashSet<String>) -> String {
    let mut candidate = format!("{base}{suffix}");
    if !seen.contains(&candidate) {
        seen.insert(candidate.clone());
        return candidate;
    }
    for i in 1.. {
        candidate = format!("{base}_{i}{suffix}");
        if !seen.contains(&candidate) {
            seen.insert(candidate.clone());
            return candidate;
        }
    }
    unreachable!()
}

/// A built parquet fixture. Backed by a `NamedTempFile` — deleted on drop.
pub struct Fixture {
    file: NamedTempFile,
    size_bytes: u64,
}

impl Fixture {
    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// Consume the fixture and return the underlying file handle.
    /// The file is unlinked from disk but stays open via the returned `File`,
    /// matching the open-then-unlink Unix pattern.
    pub fn into_file(self) -> File {
        self.file.into_file()
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parquet::ParquetReader;
    use crate::MetricsSource;

    #[test]
    fn test_minimal_counter_fixture() {
        let fixture = FixtureBuilder::new()
            .samples(10)
            .monotonic_counter("requests", &[("zone", "us-east")], 100)
            .build()
            .unwrap();
        let reader = ParquetReader::open(fixture.path()).unwrap();
        assert!(reader.counter_names().contains(&"requests".to_string()));
        let labels = reader.counter_labels("requests");
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].get("zone").map(String::as_str), Some("us-east"));
    }

    #[test]
    fn test_multi_row_group_fixture() {
        let fixture = FixtureBuilder::new()
            .samples(100)
            .row_group_size(10) // 10 row groups
            .monotonic_counter("x", &[], 1)
            .build()
            .unwrap();
        let reader = ParquetReader::open(fixture.path()).unwrap();
        let (lo, hi) = reader.time_range_ns().unwrap();
        assert_eq!(lo, 0);
        assert_eq!(hi, 99 * 1_000_000_000); // tick 99 * 1s
    }

    #[test]
    fn test_mixed_metrics_fixture() {
        let fixture = FixtureBuilder::new()
            .samples(5)
            .monotonic_counter("counter_a", &[], 1)
            .gauge("gauge_a", &[], |t| t as i64 * 10)
            .point_histogram("hist_a", &[], 4, 16, 2, 100)
            .build()
            .unwrap();
        let reader = ParquetReader::open(fixture.path()).unwrap();
        assert!(reader.has_counter("counter_a"));
        assert!(reader.has_gauge("gauge_a"));
        assert!(reader.has_histogram("hist_a"));
    }

    #[test]
    fn test_file_metadata_roundtrip() {
        let fixture = FixtureBuilder::new()
            .samples(5)
            .metadata("source", "test")
            .metadata("version", "1.0")
            .monotonic_counter("x", &[], 1)
            .build()
            .unwrap();
        let reader = ParquetReader::open(fixture.path()).unwrap();
        assert_eq!(reader.source(), "test");
        assert_eq!(reader.version(), "1.0");
        assert_eq!(reader.interval(), 1.0);
    }

    #[test]
    fn test_sampling_interval_round_trip() {
        let fixture = FixtureBuilder::new()
            .samples(5)
            .sampling_interval_ms(500)
            .monotonic_counter("x", &[], 1)
            .build()
            .unwrap();
        let reader = ParquetReader::open(fixture.path()).unwrap();
        assert_eq!(reader.interval(), 0.5);
    }

    #[test]
    fn test_resetting_counter_value_drops() {
        let fixture = FixtureBuilder::new()
            .samples(20)
            .resetting_counter("requests", &[], 10, 10)
            .build()
            .unwrap();
        let reader = ParquetReader::open(fixture.path()).unwrap();
        // Just verify the fixture compiled and the metric exists;
        // semantic tests against the rate/irate behavior come in Task 2.
        assert!(reader.has_counter("requests"));
    }
}
