use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::sync::Arc;

use arrow::array::*;
use arrow::datatypes::*;
use arrow::error::ArrowError;
use histogram::Histogram;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::errors::ParquetError;
use parquet::file::properties::WriterProperties;
use parquet::format::{FileMetaData, KeyValue};

use crate::snapshot::{HashedSnapshot, Snapshot};

const DEFAULT_MAX_BATCH_SIZE: usize = 1024 * 1024;

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

/// Storage representation for histograms within the parquet file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParquetHistogramStorage {
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
    histogram: ParquetHistogramStorage,
}

impl ParquetOptions {
    /// Create a new set of `ParquetOption` with the default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the compression level for the parquet file. The default is no
    /// compression. Set the compression level to a corresponding zstd level to
    /// enable compression.
    pub fn compression(mut self, compression: ParquetCompression) -> Self {
        self.compression = compression;
        self
    }

    /// Sets the number of rows to be cache in memory before being written as a
    /// `RecordBatch`. Large values have better performance at the cost of
    /// additional memory usage. The default is ~1M rows (2^20).
    pub fn max_batch_size(mut self, batch_size: usize) -> Self {
        self.max_batch_size = batch_size;
        self
    }

    /// Sets the storage type for histogram data: standard or sparse. The
    /// default is the standard (dense) histogram.
    pub fn histogram(mut self, histogram: ParquetHistogramStorage) -> Self {
        self.histogram = histogram;
        self
    }
}

impl Default for ParquetOptions {
    fn default() -> Self {
        Self {
            compression: Default::default(),
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            histogram: ParquetHistogramStorage::Standard,
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
    summary_percentiles: Option<Vec<f64>>,
    metadata: HashMap<String, String>,
    rows: usize,
}

impl ParquetSchema {
    pub fn new(percentiles: Option<Vec<f64>>) -> Self {
        ParquetSchema {
            counters: BTreeMap::new(),
            gauges: BTreeMap::new(),
            histograms: BTreeMap::new(),
            summary_percentiles: percentiles,
            metadata: HashMap::new(),
            rows: 0,
        }
    }

    /// Process and store metadata for all metrics seen in the snapshot.
    pub fn push(&mut self, snapshot: Snapshot) {
        let (counters, gauges, histograms) =
            (snapshot.counters, snapshot.gauges, snapshot.histograms);

        for counter in counters {
            let mut metadata = counter.metadata;

            if let Some(description) = counter.description {
                metadata.insert("description".to_string(), description);
            }

            self.counters.entry(counter.name).or_insert(metadata);
        }

        for gauge in gauges {
            let mut metadata = gauge.metadata;

            if let Some(description) = gauge.description {
                metadata.insert("description".to_string(), description);
            }

            self.gauges.entry(gauge.name).or_insert(metadata);
        }

        for histogram in histograms {
            let mut metadata = histogram.metadata;

            if let Some(description) = histogram.description {
                metadata.insert("description".to_string(), description);
            }

            self.histograms.entry(histogram.name).or_insert(metadata);
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

        let mut counters = BTreeMap::new();

        // Create one column field per-counter
        for (counter, mut metadata) in self.counters.into_iter() {
            // merge metric annotations into the metric metadata
            metadata.insert("metric_type".to_string(), "counter".to_string());

            // add column to schema
            fields
                .push(Field::new(counter.clone(), DataType::UInt64, true).with_metadata(metadata));

            // initialize storage for the counter values
            counters.insert(counter, Vec::with_capacity(self.rows));
        }

        let mut gauges = BTreeMap::new();

        // Create one column field per-gauge
        for (gauge, mut metadata) in self.gauges.into_iter() {
            // merge metric annotations into the metric metadata
            metadata.insert("metric_type".to_string(), "gauge".to_string());

            // add column to schema
            fields.push(Field::new(gauge.clone(), DataType::Int64, true).with_metadata(metadata));

            // initialize storage for the gauge values
            gauges.insert(gauge, Vec::with_capacity(self.rows));
        }

        let mut histograms = BTreeMap::new();

        // Create columns for the snapshot: the buckets are stored as a
        // nested list type where each list element is an array of `u64`s.
        // The histogram configuration parameters are part of the metadata.
        // If summary percentiles are desired, add a column per-percentile.
        // If the histogram is stored in its standard representation, the
        // buckets are stored in a single column, while in its sparse
        // representation, the non-zero bucket indices and counts are stored
        // in separate columns.
        for (histogram, mut metadata) in self.histograms.into_iter() {
            match options.histogram {
                ParquetHistogramStorage::Standard => {
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
                ParquetHistogramStorage::Sparse => {
                    // merge metric annotations into the metric metadata
                    metadata.insert("metric_type".to_string(), "sparse histogram".to_string());

                    fields.push(
                        Field::new(
                            format!("{histogram}:bucket_index"),
                            DataType::new_list(DataType::UInt64, true),
                            true,
                        )
                        .with_metadata(metadata.clone()),
                    );
                    fields.push(
                        Field::new(
                            format!("{histogram}:bucket_count"),
                            DataType::new_list(DataType::UInt64, true),
                            true,
                        )
                        .with_metadata(metadata.clone()),
                    );
                }
            };

            if let Some(ref x) = self.summary_percentiles {
                for percentile in x {
                    fields.push(
                        Field::new(format!("{histogram}:p{percentile}"), DataType::UInt64, true)
                            .with_metadata(metadata.clone()),
                    );
                }
            }

            // initialize storage for the histogram values
            histograms.insert(histogram, Vec::with_capacity(self.rows));
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
            .build();
        let arrow_writer = ArrowWriter::try_new(writer, schema.clone(), Some(props))?;

        Ok(ParquetWriter {
            writer: arrow_writer,
            options,
            schema,
            timestamps: Vec::new(),
            counters,
            gauges,
            histograms,
            summary_percentiles: self.summary_percentiles,
        })
    }
}

pub struct ParquetWriter<W: Write + Send> {
    /// Writer, options, and schema of the parquet file
    writer: ArrowWriter<W>,
    options: ParquetOptions,
    schema: Arc<Schema>,

    /// Columnar data for timestamps
    timestamps: Vec<u64>,

    /// Schema-ordered columnar data for counters
    counters: BTreeMap<String, Vec<Option<u64>>>,

    /// Schema-ordered columnar data for gauges
    gauges: BTreeMap<String, Vec<Option<i64>>>,

    /// Schema-ordered columnar data for histograms
    histograms: BTreeMap<String, Vec<Option<Histogram>>>,

    /// Summary percentiles to store for histograms
    summary_percentiles: Option<Vec<f64>>,
}

impl<W: Write + Send> ParquetWriter<W> {
    /// Process individual snapshots of metrics and store them in a columnar
    /// representation. Fill in the gaps for missing data, i.e., missing or
    /// dynamic metrics with `None` so that all columns have the same length.
    /// Writes out batches of aggregated columns once they reach a certain size.
    pub fn push(&mut self, snapshot: Snapshot) -> Result<(), ParquetError> {
        let mut hs: HashedSnapshot = HashedSnapshot::from(snapshot);

        // Aggregate timestamps into a column
        self.timestamps.push(hs.ts);

        // Aggregate metrics in the existing columns. Since `remove` returns
        // `None` if a metric in the schema does not exist in the snapshot gaps
        // are automatically filled without additional handling.
        for (key, v) in self.counters.iter_mut() {
            v.push(hs.counters.remove(key).map(|v| v.value));
        }

        for (key, v) in self.gauges.iter_mut() {
            v.push(hs.gauges.remove(key).map(|v| v.value));
        }

        for (key, v) in self.histograms.iter_mut() {
            v.push(hs.histograms.remove(key).map(|v| v.value));
        }

        // Check and flush if the max batch size of rows have been processed
        if self.timestamps.len() == self.options.max_batch_size {
            let batch = self.snapshots_to_recordbatch()?;
            self.writer.write(&batch)?;
        }

        Ok(())
    }

    /// Finish writing any buffered metrics and the parquet footer.
    pub fn finalize(mut self) -> Result<FileMetaData, ParquetError> {
        let batch = self.snapshots_to_recordbatch()?;
        self.writer.write(&batch)?;
        self.writer.close()
    }

    /// Convert buffered metrics to a parquet `RecordBatch`.
    fn snapshots_to_recordbatch(&mut self) -> Result<RecordBatch, ArrowError> {
        let mut columns: Vec<Arc<dyn Array>> = Vec::with_capacity(self.schema.fields().len());

        // Move existing timestamp array into an arrow array and clear
        columns.push(Arc::new(UInt64Array::from(std::mem::take(
            &mut self.timestamps,
        ))));

        // One column per-counter with a similar swap-and-clear of the vector
        for (_, val) in self.counters.iter_mut() {
            let col = std::mem::take(val);
            columns.push(Arc::new(UInt64Array::from(col)));
        }

        // One column per-gauge with a similar swap-and-clear of the vector
        for (_, val) in self.gauges.iter_mut() {
            let col = std::mem::take(val);
            columns.push(Arc::new(Int64Array::from(col)));
        }

        // One column, per-histogram, for the buckets if the histogram is
        // stored in its standard representation; two columns for buckets
        // per-histogram if it is stored in its sparse representation.
        // If summary percentiles are desired, add one column per-summary.
        for (_, val) in self.histograms.iter_mut() {
            let hists = std::mem::take(val);
            let mut buckets = match self.options.histogram {
                ParquetHistogramStorage::Standard => vec![ListBuilder::new(UInt64Builder::new())],
                ParquetHistogramStorage::Sparse => vec![
                    ListBuilder::new(UInt64Builder::new()),
                    ListBuilder::new(UInt64Builder::new()),
                ],
            };

            // Store one column per-summary percentile, with one-row per-histogram
            let mut summaries: Vec<Vec<Option<u64>>> = match self.summary_percentiles {
                None => Vec::new(),
                Some(ref x) => {
                    let mut outer = Vec::with_capacity(x.len());
                    for _ in 0..x.len() {
                        outer.push(Vec::with_capacity(hists.len()));
                    }
                    outer
                }
            };

            for h in hists {
                if let Some(x) = h {
                    match self.options.histogram {
                        ParquetHistogramStorage::Standard => buckets[0].append_value(
                            x.into_iter()
                                .map(|x| Some(x.count()))
                                .collect::<Vec<Option<u64>>>(),
                        ),
                        ParquetHistogramStorage::Sparse => {
                            let sparse = histogram::SparseHistogram::from(&x);
                            buckets[0].append_value(
                                sparse
                                    .index
                                    .into_iter()
                                    .map(|x| Some(x as u64))
                                    .collect::<Vec<Option<u64>>>(),
                            );
                            buckets[1].append_value(
                                sparse
                                    .count
                                    .into_iter()
                                    .map(Some)
                                    .collect::<Vec<Option<u64>>>(),
                            );
                        }
                    };

                    // Columnize histogram summary percentiles
                    if let Some(ref percentiles) = self.summary_percentiles {
                        for (idx, percentile) in percentiles.iter().enumerate() {
                            let v = x.percentile(*percentile).map(|x| x.end()).ok();
                            summaries[idx].push(v);
                        }
                    }
                } else {
                    // Histogram missing; store `None` for buckets and summaries
                    buckets[0].append_null();
                    if self.options.histogram == ParquetHistogramStorage::Sparse {
                        buckets[1].append_null();
                    }

                    if let Some(ref percentiles) = self.summary_percentiles {
                        for (idx, _) in percentiles.iter().enumerate() {
                            summaries[idx].push(None);
                        }
                    }
                }
            }
            columns.push(Arc::new(buckets[0].finish()));
            if self.options.histogram == ParquetHistogramStorage::Sparse {
                columns.push(Arc::new(buckets[1].finish()));
            }

            if !summaries.is_empty() {
                for mut col in summaries {
                    let v = std::mem::take(&mut col);
                    columns.push(Arc::new(UInt64Array::from(v)));
                }
            }
        }

        RecordBatch::try_new(self.schema.clone(), columns)
    }
}
