use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::sync::Arc;

use arrow::array::*;
use arrow::datatypes::*;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::errors::ParquetError;
use parquet::file::properties::WriterProperties;
use parquet::format::{FileMetaData, KeyValue};

use crate::snapshot::{HashedSnapshot, Snapshot};

const DEFAULT_MAX_BATCH_SIZE: usize = 50_000;

#[derive(Clone, Debug)]
pub struct ParquetCompression {
    inner: Compression,
}

impl ParquetCompression {
    /// This returns a variant that indicates that no compression will be
    /// preformed.
    pub fn none() -> Self {
        Self {
            inner: Compression::UNCOMPRESSED,
        }
    }

    /// Takes a zstd compression level and returns a variant that means that the
    /// parquet will be compressed with zstd at the specified level. Returns an
    /// error if the level is not a valid zstd compression level.
    pub fn zstd(level: i32) -> Result<Self, ParquetError> {
        Ok(Self {
            inner: Compression::ZSTD(ZstdLevel::try_new(level)?),
        })
    }
}

impl Default for ParquetCompression {
    fn default() -> Self {
        Self::zstd(3).unwrap()
    }
}

/// Type representation for histograms within the parquet file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParquetHistogramType {
    Standard,
    Sparse,
}

/// Options for `ParquetWriter` controlling the output parquet file.
#[derive(Clone, Debug)]
pub struct ParquetOptions {
    /// Supported compression types are None or Zstd at specified level
    compression: ParquetCompression,
    /// Number of rows cached in memory before being written as a `RecordBatch`
    max_batch_size: usize,
    /// Type of representation used to store histograms
    histogram_type: ParquetHistogramType,
}

impl ParquetOptions {
    /// Create a new set of `ParquetOption` with the default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the compression level for the parquet file. The default is no
    /// compression. Set the compression level to a corresponding zstd level to
    /// enable compression.
    pub fn with_compression(mut self, compression: ParquetCompression) -> Self {
        self.compression = compression;
        self
    }

    /// Sets the number of rows to be cache in memory before being written as a
    /// `RecordBatch`. Large values have better performance at the cost of
    /// additional memory usage. The default is ~1M rows (2^20).
    pub fn with_max_batch_size(mut self, batch_size: usize) -> Self {
        self.max_batch_size = batch_size;
        self
    }

    /// Sets the type for histogram data: standard or sparse. The default is
    /// the standard (dense) histogram.
    pub fn with_histogram_type(mut self, histogram: ParquetHistogramType) -> Self {
        self.histogram_type = histogram;
        self
    }
}

impl Default for ParquetOptions {
    fn default() -> Self {
        Self {
            compression: Default::default(),
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            histogram_type: ParquetHistogramType::Standard,
        }
    }
}

/// Converts metrics snapshot data into parquet
///
/// Parquet files need the entire schema to be known prior to starting to
/// write any data and all the columns to be of the same length. Missing
/// data can be filled-up using NULL/empty values (columns can be marked
/// nullable), but must be accounted for.
///
/// Metrics snapshot data does not have a well-defined schema, i.e., dynamic
/// metrics may appear in some snapshots, but not in others, all the snapshots
/// must be scanned and a complete list of all metrics extracted before
/// the parquet file can be written. Further, since the parquet file may be
/// too large to fit into memory, snapshots should be processed in batches.
///
/// This means that we need to make two passes over the snapshots:
/// a) First, we collect the cumulative union of all the metrics that have
///    been seen in any snapshot and build the schema.
/// b) We process batches of snapshots extracing the row-centric data and
///    converting it to a columnar form while filling in `None` values for
///    metrics which are missing in specific snapshots.
///
/// The `ParquetSchema` is responsible for generating the schema and for
/// building a `ParquetWriter` which actually writes the parquet file.
/// The `ParquetSchema` optionally accepts a list of percentiles which is
/// tracks as summary statistics for every histogram encountered.
#[derive(Default)]
pub struct ParquetSchema {
    counters: BTreeMap<String, HashMap<String, String>>,
    gauges: BTreeMap<String, HashMap<String, String>>,
    histograms: BTreeMap<String, HashMap<String, String>>,
    metadata: HashMap<String, String>,
    rows: usize,
}

impl ParquetSchema {
    pub fn new() -> Self {
        ParquetSchema {
            counters: BTreeMap::new(),
            gauges: BTreeMap::new(),
            histograms: BTreeMap::new(),
            metadata: HashMap::new(),
            rows: 0,
        }
    }

    /// Process and store metadata for all metrics seen in the snapshot.
    pub fn push(&mut self, snapshot: Snapshot) {
        let (counters, gauges, histograms) =
            (snapshot.counters, snapshot.gauges, snapshot.histograms);

        for counter in counters {
            self.counters
                .entry(counter.name)
                .or_insert(counter.metadata);
        }

        for gauge in gauges {
            self.gauges.entry(gauge.name).or_insert(gauge.metadata);
        }

        for histogram in histograms {
            self.histograms
                .entry(histogram.name)
                .or_insert(histogram.metadata);
        }

        if self.metadata.is_empty() && !snapshot.metadata.is_empty() {
            self.metadata = snapshot.metadata;
        }

        self.rows += 1;
    }

    /// Finalize the schema and build a `ParquetWriter`.
    pub fn finalize(
        self,
        writer: impl Write + Send,
        options: ParquetOptions,
    ) -> Result<ParquetWriter<impl Write + Send>, ParquetError> {
        let mut fields: Vec<Field> = Vec::with_capacity(
            1 + self.counters.len() + self.gauges.len() + (self.histograms.len() * 3),
        );

        // Create one column for the timestamp
        fields.push(
            Field::new("timestamp", DataType::UInt64, false).with_metadata(HashMap::from([(
                "metric_type".to_owned(),
                "timestamp".to_owned(),
            )])),
        );

        let mut counters = Vec::with_capacity(self.counters.len());

        // Create one column field per-counter
        for (counter, mut metadata) in self.counters.into_iter() {
            // merge metric annotations into the metric metadata
            metadata.insert("metric_type".to_string(), "counter".to_string());

            // add column to schema
            fields
                .push(Field::new(counter.clone(), DataType::UInt64, true).with_metadata(metadata));

            // initialize storage for the counter values
            counters.push(counter);
        }

        let mut gauges = Vec::with_capacity(self.gauges.len());

        // Create one column field per-gauge
        for (gauge, mut metadata) in self.gauges.into_iter() {
            // merge metric annotations into the metric metadata
            metadata.insert("metric_type".to_string(), "gauge".to_string());

            // add column to schema
            fields.push(Field::new(gauge.clone(), DataType::Int64, true).with_metadata(metadata));

            // initialize storage for the gauge values
            gauges.push(gauge);
        }

        let mut histograms = Vec::with_capacity(self.histograms.len());

        // Create columns for the snapshot: the buckets are stored as a
        // nested list type where each list element is an array of `u64`s.
        // The histogram configuration parameters are part of the metadata.
        // If the histogram is stored in its standard representation, the
        // buckets are stored in a single column, while in its sparse
        // representation, the non-zero bucket indices and counts are stored
        // in separate columns.
        for (histogram, mut metadata) in self.histograms.into_iter() {
            match options.histogram_type {
                ParquetHistogramType::Standard => {
                    // merge metric annotations into the metric metadata
                    metadata.insert("metric_type".to_string(), "histogram".to_string());

                    fields.push(
                        Field::new(
                            format!("{histogram}:buckets"),
                            DataType::new_list(DataType::UInt64, true),
                            true,
                        )
                        .with_metadata(metadata.clone()),
                    );
                }
                ParquetHistogramType::Sparse => {
                    // merge metric annotations into the metric metadata
                    metadata.insert("metric_type".to_string(), "sparse histogram".to_string());

                    fields.push(
                        Field::new(
                            format!("{histogram}:bucket_indices"),
                            DataType::new_list(DataType::UInt64, true),
                            true,
                        )
                        .with_metadata(metadata.clone()),
                    );
                    fields.push(
                        Field::new(
                            format!("{histogram}:bucket_counts"),
                            DataType::new_list(DataType::UInt64, true),
                            true,
                        )
                        .with_metadata(metadata.clone()),
                    );
                }
            };

            // initialize storage for the histogram values
            histograms.push(histogram);
        }

        let metadata: Option<Vec<KeyValue>> = if self.metadata.is_empty() {
            None
        } else {
            Some(
                self.metadata
                    .into_iter()
                    .map(|(key, value)| KeyValue {
                        key,
                        value: Some(value),
                    })
                    .collect(),
            )
        };

        let schema = Arc::new(Schema::new(fields));
        let props = WriterProperties::builder()
            .set_compression(options.compression.inner)
            .set_key_value_metadata(metadata)
            .set_max_row_group_size(options.max_batch_size)
            .build();
        let arrow_writer = ArrowWriter::try_new(writer, schema.clone(), Some(props))?;

        Ok(ParquetWriter {
            writer: arrow_writer,
            options,
            schema,
            counters,
            gauges,
            histograms,
        })
    }
}

pub struct ParquetWriter<W: Write + Send> {
    /// Writer, options, and schema of the parquet file
    writer: ArrowWriter<W>,
    options: ParquetOptions,
    schema: Arc<Schema>,

    /// Schema-ordered list of counters, gauges, and histograms
    counters: Vec<String>,
    gauges: Vec<String>,
    histograms: Vec<String>,
}

impl<W: Write + Send> ParquetWriter<W> {
    /// Process individual snapshots of metrics and store them in a columnar
    /// representation. Fill in the gaps for missing data, i.e., missing or
    /// dynamic metrics with `None` so that all columns have the same length.
    /// Writes them to the ArrowWriter, which internally buffers batches until
    /// the maximum row group size is reached.
    pub fn push(&mut self, snapshot: Snapshot) -> Result<(), ParquetError> {
        let mut columns: Vec<Arc<dyn Array>> = Vec::with_capacity(self.schema.fields().len());

        let mut hs: HashedSnapshot = HashedSnapshot::from(snapshot);

        // Create a single element column for the timestamp
        columns.push(Arc::new(UInt64Array::from(vec![hs.ts])));

        // Create single element columns for metrics. Since `remove` returns
        // `None` if a metric in the schema does not exist in the snapshot gaps
        // are automatically filled without additional handling.
        for counter in self.counters.iter_mut() {
            columns.push(Arc::new(UInt64Array::from(vec![hs
                .counters
                .remove(counter)
                .map(|v| v.value)])));
        }

        for gauge in self.gauges.iter_mut() {
            columns.push(Arc::new(Int64Array::from(vec![hs
                .gauges
                .remove(gauge)
                .map(|v| v.value)])));
        }

        for h in self.histograms.iter_mut() {
            let histogram = hs.histograms.remove(h).map(|v| v.value);
            if let Some(hist) = histogram {
                match self.options.histogram_type {
                    ParquetHistogramType::Standard => {
                        columns.push(Self::listu64_entry_from_slice(hist.as_slice()))
                    }
                    ParquetHistogramType::Sparse => {
                        let sparse = histogram::SparseHistogram::from(&hist);
                        columns.push(Self::listu64_entry_from_vec(sparse.index));
                        columns.push(Self::listu64_entry_from_slice(sparse.count.as_slice()));
                    }
                };
            } else {
                match self.options.histogram_type {
                    ParquetHistogramType::Standard => columns.push(Self::listu64_entry_null()),
                    ParquetHistogramType::Sparse => columns.append(&mut vec![
                        Self::listu64_entry_null(),
                        Self::listu64_entry_null(),
                    ]),
                };
            }
        }

        let batch = RecordBatch::try_new(self.schema.clone(), columns)?;
        self.writer.write(&batch)
    }

    /// Finish writing any buffered metrics and the parquet footer.
    pub fn finalize(self) -> Result<FileMetaData, ParquetError> {
        self.writer.close()
    }

    /// Create a list entry for an arrow lists of u64s from a slice.
    fn listu64_entry_from_slice(v: &[u64]) -> Arc<ListArray> {
        Arc::new(ListArray::from_iter_primitive::<UInt64Type, _, _>([Some(
            v.iter().map(|x| Some(*x)).collect::<Vec<Option<u64>>>(),
        )]))
    }

    /// Create a list entry for an arrow lists of u64s from a vector.
    fn listu64_entry_from_vec(v: Vec<usize>) -> Arc<ListArray> {
        Arc::new(ListArray::from_iter_primitive::<UInt64Type, _, _>([Some(
            v.into_iter()
                .map(|x| Some(x as u64))
                .collect::<Vec<Option<u64>>>(),
        )]))
    }

    /// Create a null list entry for an arrow lists of u64s.
    fn listu64_entry_null() -> Arc<ListArray> {
        Arc::new(ListArray::from_iter_primitive::<
            UInt64Type,
            Vec<Option<u64>>,
            _,
        >([None]))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs::File;
    use std::io::Seek;
    use std::time::{Duration, SystemTime};

    use ::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use arrow::array::*;
    use metriken::histogram::Histogram as H2Histogram;

    use crate::*;

    fn build_snapshots() -> Vec<Snapshot> {
        let h1 = H2Histogram::from_buckets(1, 3, vec![0, 1, 1, 0, 0, 0]).unwrap();
        let s1 = Snapshot {
            systemtime: SystemTime::now(),
            metadata: HashMap::new(),
            counters: vec![Counter {
                name: "counter".to_string(),
                value: 100,
                metadata: HashMap::new(),
            }],
            gauges: vec![Gauge {
                name: "gauge".to_string(),
                value: 16,
                metadata: HashMap::new(),
            }],
            histograms: vec![Histogram {
                name: "histogram".to_string(),
                value: h1,
                metadata: HashMap::new(),
            }],
        };

        let h2 = H2Histogram::from_buckets(1, 3, vec![0, 1, 1, 0, 1, 0]).unwrap();
        let s2 = Snapshot {
            systemtime: SystemTime::now()
                .checked_add(Duration::from_secs(600))
                .unwrap(),
            metadata: HashMap::new(),
            counters: vec![Counter {
                name: "counter".to_string(),
                value: 121,
                metadata: HashMap::new(),
            }],
            gauges: vec![Gauge {
                name: "gauge".to_string(),
                value: 6,
                metadata: HashMap::new(),
            }],
            histograms: vec![Histogram {
                name: "histogram".to_string(),
                value: h2,
                metadata: HashMap::new(),
            }],
        };

        vec![s1, s2]
    }

    fn write_parquet(snapshots: Vec<Snapshot>, options: ParquetOptions) -> File {
        let mut schema = ParquetSchema::new();
        for s in &snapshots {
            schema.push(s.clone());
        }

        let mut tmpfile = tempfile::tempfile().unwrap();
        let mut writer = schema
            .finalize(tmpfile.try_clone().unwrap(), options)
            .unwrap();
        for s in &snapshots {
            let _ = writer.push(s.clone());
        }
        let _ = writer.finalize();

        let _ = tmpfile.rewind();
        tmpfile
    }

    fn validate_i64_array(col: ArrayRef, vals: &[i64]) {
        let v = col.as_any().downcast_ref::<array::Int64Array>().unwrap();
        assert_eq!(v.values(), vals);
    }

    fn validate_u64_array(col: ArrayRef, vals: &[u64]) {
        let v = col.as_any().downcast_ref::<array::UInt64Array>().unwrap();
        assert_eq!(v.values(), vals);
    }

    #[test]
    fn test_row_groups() {
        let snapshots = build_snapshots();
        let tmpfile = write_parquet(snapshots, ParquetOptions::new().with_max_batch_size(1));
        let builder = ParquetRecordBatchReaderBuilder::try_new(tmpfile).unwrap();

        // Check row groups
        assert_eq!(builder.metadata().row_groups().len(), 2);
        assert_eq!(builder.metadata().row_group(0).num_rows(), 1);
        assert_eq!(builder.metadata().row_group(1).num_rows(), 1);
    }

    #[test]
    fn test_default() {
        let snapshots = build_snapshots();
        let tmpfile = write_parquet(snapshots, ParquetOptions::new());
        let builder = ParquetRecordBatchReaderBuilder::try_new(tmpfile).unwrap();

        // Check row groups
        assert_eq!(builder.metadata().row_groups().len(), 1);
        assert_eq!(builder.metadata().row_group(0).num_rows(), 2);

        // Check schema
        let fields: Vec<&String> = builder.schema().fields().iter().map(|x| x.name()).collect();
        let expected = vec!["timestamp", "counter", "gauge", "histogram:buckets"];
        assert_eq!(fields.len(), expected.len());
        assert_eq!(fields, expected);

        // Check data
        let batch = builder.build().unwrap().next().unwrap().unwrap();
        assert_eq!(batch.num_columns(), 4);
        assert_eq!(batch.num_rows(), 2);

        validate_u64_array(batch.column(1).clone(), &[100, 121]);
        validate_i64_array(batch.column(2).clone(), &[16, 6]);

        let histograms = batch
            .column(3)
            .as_any()
            .downcast_ref::<array::ListArray>()
            .unwrap();
        validate_u64_array(histograms.value(0), &[0, 1, 1, 0, 0, 0]);
        validate_u64_array(histograms.value(1), &[0, 1, 1, 0, 1, 0]);
    }

    #[test]
    fn test_sparse() {
        let snapshots = build_snapshots();
        let tmpfile = write_parquet(
            snapshots,
            ParquetOptions::new().with_histogram_type(ParquetHistogramType::Sparse),
        );
        let builder = ParquetRecordBatchReaderBuilder::try_new(tmpfile).unwrap();

        // Check row groups
        assert_eq!(builder.metadata().row_groups().len(), 1);
        assert_eq!(builder.metadata().row_group(0).num_rows(), 2);

        // Check schema
        let fields: Vec<&String> = builder.schema().fields().iter().map(|x| x.name()).collect();
        let expected = vec![
            "timestamp",
            "counter",
            "gauge",
            "histogram:bucket_indices",
            "histogram:bucket_counts",
        ];
        assert_eq!(fields.len(), expected.len());
        assert_eq!(fields, expected);

        // Check data
        let batch = builder.build().unwrap().next().unwrap().unwrap();
        assert_eq!(batch.num_columns(), 5);
        assert_eq!(batch.num_rows(), 2);

        validate_u64_array(batch.column(1).clone(), &[100, 121]);
        validate_i64_array(batch.column(2).clone(), &[16, 6]);

        let indices = batch
            .column(3)
            .as_any()
            .downcast_ref::<array::ListArray>()
            .unwrap();
        validate_u64_array(indices.value(0), &[1, 2]);
        validate_u64_array(indices.value(1), &[1, 2, 4]);

        let counts = batch
            .column(4)
            .as_any()
            .downcast_ref::<array::ListArray>()
            .unwrap();
        validate_u64_array(counts.value(0), &[1, 1]);
        validate_u64_array(counts.value(1), &[1, 1, 1]);
    }
}
