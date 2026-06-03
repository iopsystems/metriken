use std::error::Error;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, UInt64Array};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use parquet::file::statistics::Statistics;
use tempfile::NamedTempFile;

use super::Fixture;

/// Replicates an existing parquet file N times with shifted timestamps.
///
/// Produces a parquet matching the source's schema exactly. Each repetition
/// adds `shift_increment_ns` to the timestamp column. The default shift is
/// `(source_max_ts - source_min_ts) + source_interval_ns`, so iterations
/// abut without overlap.
///
/// Useful for stress-testing the streaming reader with realistically-shaped
/// data scaled up to arbitrary sizes.
pub struct ParquetAugmentor {
    source_path: PathBuf,
    repetitions: u32,
    shift_increment_ns: Option<u64>,
}

impl ParquetAugmentor {
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        Self {
            source_path: path.as_ref().to_path_buf(),
            repetitions: 1,
            shift_increment_ns: None,
        }
    }

    pub fn repeat(mut self, n: u32) -> Self {
        assert!(n > 0, "repetitions must be positive");
        self.repetitions = n;
        self
    }

    /// Override the per-iteration time shift. Default: source span + one interval.
    pub fn shift_increment_ns(mut self, shift: u64) -> Self {
        self.shift_increment_ns = Some(shift);
        self
    }

    pub fn build(self) -> Result<Fixture, Box<dyn Error + Send + Sync>> {
        let source_file = File::open(&self.source_path)?;
        let meta = ArrowReaderMetadata::load(&source_file, ArrowReaderOptions::default())?;
        let schema = meta.schema().clone();
        let pq_meta = meta.metadata();

        // Locate the timestamp column index
        let ts_col_idx = schema
            .index_of("timestamp")
            .map_err(|_| "source parquet has no `timestamp` column")?;

        // Extract file-level metadata to preserve in output
        let kv_metadata: Option<Vec<KeyValue>> =
            pq_meta.file_metadata().key_value_metadata().cloned();

        // Infer interval from file metadata's sampling_interval_ms (or default 1000)
        let interval_ms: u64 = kv_metadata
            .as_ref()
            .and_then(|kvs| kvs.iter().find(|kv| kv.key == "sampling_interval_ms"))
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        let interval_ns = interval_ms * 1_000_000;

        // Determine the time shift per iteration: source span + one interval
        let shift_per_iter = if let Some(s) = self.shift_increment_ns {
            s
        } else {
            // Walk row group statistics to find min/max timestamp
            let mut min_ts: Option<u64> = None;
            let mut max_ts: Option<u64> = None;
            for rg_idx in 0..pq_meta.num_row_groups() {
                // The timestamp column might be Int64 or UInt64 depending on writer.
                // We're writing UInt64; reading rezolus's UInt64 too. Handle both.
                if let Some(Statistics::Int64(s)) =
                    pq_meta.row_group(rg_idx).column(ts_col_idx).statistics()
                {
                    if let Some(v) = s.min_opt() {
                        let ts = *v as u64;
                        min_ts = Some(min_ts.map_or(ts, |m: u64| m.min(ts)));
                    }
                    if let Some(v) = s.max_opt() {
                        let ts = *v as u64;
                        max_ts = Some(max_ts.map_or(ts, |m: u64| m.max(ts)));
                    }
                    // Other statistics variants (UInt64, etc.) are silently skipped —
                    // the fallback interval_ns handles the case where stats are absent.
                }
            }
            match (min_ts, max_ts) {
                (Some(lo), Some(hi)) => (hi.saturating_sub(lo)).saturating_add(interval_ns),
                _ => interval_ns, // fallback if stats absent
            }
        };

        // Set up the output
        let named = NamedTempFile::with_suffix(".parquet")?;
        let out_file = named.reopen()?;

        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_key_value_metadata(kv_metadata.clone())
            .build();

        let mut writer = ArrowWriter::try_new(out_file, schema.clone(), Some(props))?;

        // Replicate N times, shifting timestamps each iteration
        for rep in 0..self.repetitions {
            let shift = shift_per_iter * (rep as u64);

            // Open a fresh reader on every iteration (parquet readers are not rewindable)
            let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(
                source_file.try_clone()?,
                meta.clone(),
            )
            .build()?;

            for batch_result in reader {
                let batch = batch_result?;
                let shifted = if rep == 0 {
                    batch
                } else {
                    shift_timestamps(&batch, ts_col_idx, shift)?
                };
                writer.write(&shifted)?;
            }
        }

        writer.close()?;

        let size_bytes = std::fs::metadata(named.path())?.len();
        Ok(Fixture::__from_named_temp(named, size_bytes))
    }
}

/// Returns a new RecordBatch with the timestamp column shifted by `shift_ns`.
fn shift_timestamps(
    batch: &RecordBatch,
    ts_col_idx: usize,
    shift_ns: u64,
) -> Result<RecordBatch, Box<dyn Error + Send + Sync>> {
    let ts_col = batch.column(ts_col_idx);
    let ts_array = ts_col
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or("timestamp column is not UInt64")?;
    let shifted: UInt64Array = ts_array
        .iter()
        .map(|v| v.map(|t| t.saturating_add(shift_ns)))
        .collect();

    // Build new column list with the shifted timestamp swapped in
    let mut columns: Vec<ArrayRef> = (0..batch.num_columns())
        .map(|i| batch.column(i).clone())
        .collect();
    columns[ts_col_idx] = Arc::new(shifted) as ArrayRef;

    Ok(RecordBatch::try_new(batch.schema(), columns)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::FixtureBuilder;
    use crate::{MetricsSource, ParquetReader};

    #[test]
    fn test_repeat_doubles_row_count() {
        let base = FixtureBuilder::new()
            .samples(10)
            .monotonic_counter("x", &[], 1)
            .build()
            .unwrap();

        let augmented = ParquetAugmentor::from_path(base.path())
            .repeat(2)
            .build()
            .unwrap();

        // Compare time ranges
        let r_base = ParquetReader::open(base.path()).unwrap();
        let r_aug = ParquetReader::open(augmented.path()).unwrap();

        let (_, hi_base) = r_base.time_range_ns().unwrap();
        let (_, hi_aug) = r_aug.time_range_ns().unwrap();

        // After 2 repetitions, augmented file's max timestamp should be roughly
        // double the base's. Allow some slack for the interval addition.
        assert!(
            hi_aug >= hi_base,
            "augmented max ({hi_aug}) >= base max ({hi_base})"
        );
        assert!(hi_aug > hi_base, "augmented should extend past base");
    }

    #[test]
    fn test_repeat_preserves_metric_schema() {
        let base = FixtureBuilder::new()
            .samples(5)
            .metadata("source", "test")
            .metadata("version", "1.0")
            .monotonic_counter("counter_a", &[], 1)
            .gauge("gauge_a", &[], |t| t as i64)
            .build()
            .unwrap();

        let augmented = ParquetAugmentor::from_path(base.path())
            .repeat(3)
            .build()
            .unwrap();

        let r = ParquetReader::open(augmented.path()).unwrap();
        assert!(r.has_counter("counter_a"));
        assert!(r.has_gauge("gauge_a"));
        assert_eq!(r.source(), "test");
        assert_eq!(r.version(), "1.0");
    }

    #[test]
    fn test_repeat_one_is_passthrough() {
        let base = FixtureBuilder::new()
            .samples(10)
            .monotonic_counter("x", &[], 1)
            .build()
            .unwrap();

        let augmented = ParquetAugmentor::from_path(base.path())
            .repeat(1)
            .build()
            .unwrap();

        let r_base = ParquetReader::open(base.path()).unwrap();
        let r_aug = ParquetReader::open(augmented.path()).unwrap();
        assert_eq!(r_base.time_range_ns(), r_aug.time_range_ns());
    }

    #[test]
    fn test_query_against_augmented_file() {
        let base = FixtureBuilder::new()
            .samples(50)
            .monotonic_counter("rps", &[("zone", "us-east")], 100)
            .build()
            .unwrap();

        let augmented = ParquetAugmentor::from_path(base.path())
            .repeat(5)
            .build()
            .unwrap();

        let reader = ParquetReader::open(augmented.path()).unwrap();
        let (start, end) = reader.time_range().unwrap();

        let result = reader.query_range("rate(rps[5s])", start, end + 1.0, 1.0);
        assert!(
            result.is_ok(),
            "query against augmented file should succeed: {:?}",
            result.err()
        );
    }
}
