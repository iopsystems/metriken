use std::collections::BTreeMap;
use std::fs::File;
use std::sync::Arc;

use crate::snapshot::{HashedSnapshot, Snapshot};
use crate::HistogramSnapshot;
use arrow::array::*;
use arrow::datatypes::*;
use arrow::error::ArrowError;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::errors::ParquetError;
use parquet::file::properties::WriterProperties;
use parquet::format::FileMetaData;

/// Options for `ParquetWriter` controlling the output parquet file.
pub struct ParquetOptions {
    /// Supported compression types are None or Zstd at specified level
    compression: Compression,
    /// Number of rows cached in memory before being written as a `RecordBatch`
    max_batch_size: usize,
}

impl ParquetOptions {
    pub fn builder() -> ParquetOptionsBuilder {
        ParquetOptionsBuilder::default()
    }

    pub fn compression(&self) -> Compression {
        self.compression
    }

    pub fn max_batch_size(&self) -> usize {
        self.max_batch_size
    }
}

impl Default for ParquetOptions {
    fn default() -> Self {
        Self {
            compression: Compression::UNCOMPRESSED,
            max_batch_size: 1024 * 1024,
        }
    }
}

/// Used to build `ParquetOptions`.
#[derive(Default)]
pub struct ParquetOptionsBuilder {
    options: ParquetOptions,
}

impl ParquetOptionsBuilder {
    pub fn build(self) -> ParquetOptions {
        self.options
    }

    pub fn compression_level(mut self, level: i32) -> Result<Self, ParquetError> {
        let compression = Compression::ZSTD(ZstdLevel::try_new(level)?);
        self.options.compression = compression;
        Ok(self)
    }

    pub fn max_batch_size(mut self, batch_size: usize) -> Self {
        self.options.max_batch_size = batch_size;
        self
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
pub struct ParquetWriter {
    /// File handle for output parquet file
    file: Option<File>,
    writer: Option<ArrowWriter<File>>,
    options: ParquetOptions,

    /// Schema and columnar data for timestamps
    timestamps: Vec<u64>,

    /// Schema and columnar data for counters
    counters: BTreeMap<String, Vec<Option<u64>>>,

    /// Schema and columnar data for gauges
    gauges: BTreeMap<String, Vec<Option<i64>>>,

    /// Schema and columnar data for histograms
    histograms: BTreeMap<String, Vec<Option<HistogramSnapshot>>>,

    /// Schema generated for the parquet file after finalizing metadata
    schema: Arc<Schema>,
}

impl ParquetWriter {
    pub fn new(file: File, options: ParquetOptions) -> Self {
        ParquetWriter {
            file: Some(file),
            writer: None,
            options,
            timestamps: Vec::new(),
            counters: BTreeMap::new(),
            gauges: BTreeMap::new(),
            histograms: BTreeMap::new(),
            schema: Arc::new(Schema::empty()),
        }
    }

    /// Process and store metadata for all metrics seen in the snapshot.
    pub fn process_snapshot_schema(&mut self, snapshot: Snapshot) -> Result<(), ParquetError> {
        if self.writer.is_some() {
            return Err(ParquetError::General(
                "Schema already finalized".to_string(),
            ));
        }

        let (counters, gauges, histograms) =
            (snapshot.counters, snapshot.gauges, snapshot.histograms);

        for counter in counters {
            self.counters.entry(counter.0).or_default();
        }

        for gauge in gauges {
            self.gauges.entry(gauge.0).or_default();
        }

        for h in histograms {
            self.histograms.entry(h.0).or_default();
        }

        Ok(())
    }

    /// Create a schema from the union of saved metrics metadata.
    pub fn finalize_schema(&mut self) -> Result<(), ParquetError> {
        if self.writer.is_some() {
            return Err(ParquetError::General(
                "Schema already finalized".to_string(),
            ));
        }

        let mut fields: Vec<Field> = Vec::with_capacity(
            1 + self.counters.len() + self.gauges.len() + (self.histograms.len() * 3),
        );

        // Create one column for the timestamp
        fields.push(Field::new("timestamp", DataType::UInt64, false));

        // Create one column field per-counter
        for counter in self.counters.keys() {
            fields.push(Field::new(counter.clone(), DataType::UInt64, true));
        }

        // Create one column field per-gauge
        for gauge in self.gauges.keys() {
            fields.push(Field::new(gauge.clone(), DataType::Int64, true));
        }

        // Create three column fields per-snapshot: two for configuration data
        // around histogram size and one for the actual buckets. The latter
        // is a nested list type where each list element is an array of `u64`s.
        for h in self.histograms.keys() {
            fields.push(Field::new(
                format!("{}_grouping_power", h),
                DataType::UInt8,
                true,
            ));
            fields.push(Field::new(
                format!("{}_max_config_power", h),
                DataType::UInt8,
                true,
            ));
            fields.push(Field::new(
                format!("{}_buckets", h),
                DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
                true,
            ));
        }

        self.schema = Arc::new(Schema::new(fields));

        let file = std::mem::take(&mut self.file).unwrap();
        let props = WriterProperties::builder()
            .set_compression(self.options.compression())
            .build();
        self.writer = Some(ArrowWriter::try_new(
            file,
            self.schema.clone(),
            Some(props),
        )?);

        Ok(())
    }

    /// Process individual snapshots of metrics and store them in a columnar
    /// representation. Fill in the gaps for missing data, i.e., missing
    /// or dynamic metrics with `None` so that all columns have the same length.
    /// Writes out batches of the aggregated columns once they reach a certain size.
    pub fn process_snapshot(&mut self, snapshot: Snapshot) -> Result<(), ParquetError> {
        if self.writer.is_none() {
            return Err(ParquetError::General("Schema not finalized".to_string()));
        }

        let mut hs: HashedSnapshot = HashedSnapshot::from(snapshot);

        // Aggregate timestamps into a column
        self.timestamps.push(hs.ts);

        // Aggregate metrics in the existing columns. Since `remove` returns
        // `None` if a metric in the schema does not exist in the snapshot gaps
        // are automatically filled without additional handling.
        for (key, v) in self.counters.iter_mut() {
            v.push(hs.counters.remove(key));
        }

        for (key, v) in self.gauges.iter_mut() {
            v.push(hs.gauges.remove(key));
        }

        for (key, v) in self.histograms.iter_mut() {
            v.push(hs.histograms.remove(key));
        }

        // Check and flush if the max batch size of rows have been processed
        if self.timestamps.len() == self.options.max_batch_size() {
            let batch = self.snapshots_to_recordbatch()?;
            let writer = self.writer.as_mut().unwrap();
            writer.write(&batch)?;
        }

        Ok(())
    }

    pub fn finalize(&mut self) -> Result<FileMetaData, ParquetError> {
        if self.writer.is_none() {
            return Err(ParquetError::General("Schema not finalized".to_string()));
        }

        let batch = self.snapshots_to_recordbatch()?;
        let mut writer = std::mem::take(&mut self.writer).unwrap();
        writer.write(&batch)?;
        writer.close()
    }

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

        // Three columns per-histogram: two for configuration (the grouping
        // power and the max capacity power), and one for the buckets.
        for (_, val) in self.histograms.iter_mut() {
            let hists = std::mem::take(val);

            let mut gps: Vec<Option<u8>> = Vec::with_capacity(hists.len());
            let mut maxes: Vec<Option<u8>> = Vec::with_capacity(hists.len());
            let mut buckets = ListBuilder::new(UInt64Builder::new());

            for h in hists {
                if let Some(x) = h {
                    gps.push(Some(x.config().grouping_power()));
                    maxes.push(Some(x.config().max_value_power()));
                    buckets.append_value(
                        x.into_iter()
                            .map(|x| Some(x.count()))
                            .collect::<Vec<Option<u64>>>(),
                    );
                } else {
                    gps.push(None);
                    maxes.push(None);
                    buckets.append_null();
                }
            }
            columns.push(Arc::new(UInt8Array::from(gps)));
            columns.push(Arc::new(UInt8Array::from(maxes)));
            columns.push(Arc::new(buckets.finish()));
        }

        RecordBatch::try_new(self.schema.clone(), columns)
    }
}
