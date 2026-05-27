use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::path::Path;

use arrow::array::UInt64Array;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::ProjectionMask;
use parquet::file::metadata::RowGroupMetaData;
use parquet::file::statistics::Statistics;

use super::{build_targets, process_column, snap_timestamp, ColumnTarget, Tsdb};

pub(crate) struct ParquetFile {
    file: File,
    meta: ArrowReaderMetadata,
    pub(crate) sampling_interval_ms: u64,
    pub(crate) source: String,
    pub(crate) version: String,
    pub(crate) filename: String,
    pub(crate) file_metadata: HashMap<String, String>,
}

impl ParquetFile {
    pub(crate) fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        let filename = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("unknown")
            .to_string();

        let file = File::open(path)?;
        let meta = ArrowReaderMetadata::load(&file, ArrowReaderOptions::default())?;
        let pq_metadata = meta.metadata();

        let mut file_metadata = HashMap::new();
        if let Some(kv) = pq_metadata.file_metadata().key_value_metadata() {
            for entry in kv {
                file_metadata.insert(entry.key.clone(), entry.value.clone().unwrap_or_default());
            }
        }

        let sampling_interval_ms = file_metadata
            .get("sampling_interval_ms")
            .map(|v| v.parse::<u64>().expect("bad interval"))
            .unwrap_or(1000);
        let source = file_metadata
            .get("source")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        let version = file_metadata
            .get("version")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        Ok(Self {
            file,
            meta,
            sampling_interval_ms,
            source,
            version,
            filename,
            file_metadata,
        })
    }

    /// Load rows whose snapped timestamp falls within `[start_ns, end_ns]`.
    ///
    /// Row groups are classified using parquet column statistics:
    /// - **Before**: entirely before `start_ns` — the last such group is processed
    ///   with the range filter active so histogram `prev` state is initialised
    ///   without inserting any data points. This ensures the first in-range
    ///   histogram delta is computable.
    /// - **Overlaps**: processed normally; rows outside the window are not inserted.
    /// - **After**: skipped entirely.
    pub(crate) fn load_range(&self, start_ns: u64, end_ns: u64) -> Result<Tsdb, Box<dyn Error>> {
        self.load_impl(Some((start_ns, end_ns)))
    }

    /// Read the time range (min_ns, max_ns) from row-group statistics without
    /// decoding any column data.
    pub(crate) fn time_range_from_stats(&self) -> Option<(u64, u64)> {
        let schema = self.meta.schema();
        let ts_col_idx = schema.index_of("timestamp").ok()?;
        let pq_metadata = self.meta.metadata();

        let mut min_ns: Option<u64> = None;
        let mut max_ns: Option<u64> = None;

        for rg_idx in 0..pq_metadata.num_row_groups() {
            let Some(stats) = pq_metadata
                .row_group(rg_idx)
                .column(ts_col_idx)
                .statistics()
            else {
                continue;
            };
            if let Statistics::Int64(s) = stats {
                if let Some(v) = s.min_opt() {
                    let ts = *v as u64;
                    min_ns = Some(min_ns.map_or(ts, |m: u64| m.min(ts)));
                }
                if let Some(v) = s.max_opt() {
                    let ts = *v as u64;
                    max_ns = Some(max_ns.map_or(ts, |m: u64| m.max(ts)));
                }
            }
        }

        min_ns.zip(max_ns)
    }

    fn load_impl(&self, range: Option<(u64, u64)>) -> Result<Tsdb, Box<dyn Error>> {
        let arrow_schema = self.meta.schema().clone();
        let pq_metadata = self.meta.metadata().clone();
        let parquet_schema = pq_metadata.file_metadata().schema_descr_ptr();
        let num_row_groups = pq_metadata.num_row_groups();

        let mut data = Tsdb {
            sampling_interval_ms: self.sampling_interval_ms,
            source: self.source.clone(),
            version: self.version.clone(),
            filename: self.filename.clone(),
            file_metadata: self.file_metadata.clone(),
            ..Tsdb::default()
        };

        let interval_ns = self.sampling_interval_ms * 1_000_000;

        let ts_col_idx = arrow_schema
            .index_of("timestamp")
            .map_err(|_| "missing 'timestamp' column")?;

        let mut targets = build_targets(&arrow_schema, ts_col_idx);

        // For range queries, classify each row group and identify:
        // - `init_rg`: the last row group entirely before the range (histogram prev init)
        // - `load_rgs`: row groups that overlap or have unknown statistics
        let (init_rg, load_rgs): (Option<usize>, Vec<usize>) = if let Some((start, end)) = range {
            let mut init = None;
            let mut load = Vec::new();
            for rg_idx in 0..num_row_groups {
                match rg_classify(pq_metadata.row_group(rg_idx), ts_col_idx, start, end) {
                    RgClass::Before => init = Some(rg_idx),
                    RgClass::Overlaps | RgClass::Unknown => load.push(rg_idx),
                    RgClass::After => {}
                }
            }
            (init, load)
        } else {
            (None, (0..num_row_groups).collect())
        };

        let mut timestamps: Vec<Option<u64>> = Vec::new();

        // Process the pre-range init row group if present. All rows fall before
        // `start_ns`, so the range filter prevents any insertion. Histograms
        // update `prev` on each row so the first in-range delta is computable.
        if let Some(rg_idx) = init_rg {
            read_timestamps_into(
                &self.file,
                &self.meta,
                &parquet_schema,
                rg_idx,
                ts_col_idx,
                interval_ns,
                &mut timestamps,
            )?;
            for (col_idx, target) in targets.iter_mut().enumerate() {
                if col_idx == ts_col_idx || matches!(target, ColumnTarget::Skip) {
                    continue;
                }
                let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(
                    self.file.try_clone()?,
                    self.meta.clone(),
                )
                .with_row_groups(vec![rg_idx])
                .with_projection(ProjectionMask::roots(&parquet_schema, [col_idx]))
                .build()?;
                process_column(target, reader.flatten(), &timestamps, &mut data, range)?;
            }
        }

        // Load in-range (and unknown) row groups.
        for rg_idx in load_rgs {
            read_timestamps_into(
                &self.file,
                &self.meta,
                &parquet_schema,
                rg_idx,
                ts_col_idx,
                interval_ns,
                &mut timestamps,
            )?;
            for (col_idx, target) in targets.iter_mut().enumerate() {
                if col_idx == ts_col_idx || matches!(target, ColumnTarget::Skip) {
                    continue;
                }
                let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(
                    self.file.try_clone()?,
                    self.meta.clone(),
                )
                .with_row_groups(vec![rg_idx])
                .with_projection(ProjectionMask::roots(&parquet_schema, [col_idx]))
                .build()?;
                process_column(target, reader.flatten(), &timestamps, &mut data, range)?;
            }
        }

        Ok(data)
    }
}

enum RgClass {
    Before,   // rg_max_ts < start_ns — entirely before the range
    Overlaps, // overlaps [start_ns, end_ns]
    After,    // rg_min_ts > end_ns — entirely after the range
    Unknown,  // no statistics available; treat conservatively as Overlaps
}

/// Classify a row group relative to `[start_ns, end_ns]` using the timestamp
/// column statistics.
///
/// Timestamps are Arrow UInt64 stored as parquet INT64 physical type. Current
/// nanosecond epoch values (~1.7e18) are positive i64, so `*v as u64` is a
/// lossless reinterpretation.
fn rg_classify(rg: &RowGroupMetaData, ts_col_idx: usize, start_ns: u64, end_ns: u64) -> RgClass {
    let Some(stats) = rg.column(ts_col_idx).statistics() else {
        return RgClass::Unknown;
    };
    let Statistics::Int64(s) = stats else {
        return RgClass::Unknown;
    };
    let (Some(rg_min), Some(rg_max)) = (s.min_opt(), s.max_opt()) else {
        return RgClass::Unknown;
    };
    let rg_min = *rg_min as u64;
    let rg_max = *rg_max as u64;

    if rg_max < start_ns {
        RgClass::Before
    } else if rg_min > end_ns {
        RgClass::After
    } else {
        RgClass::Overlaps
    }
}

fn read_timestamps_into(
    file: &File,
    meta: &ArrowReaderMetadata,
    parquet_schema: &parquet::schema::types::SchemaDescPtr,
    rg_idx: usize,
    ts_col_idx: usize,
    interval_ns: u64,
    timestamps: &mut Vec<Option<u64>>,
) -> Result<(), Box<dyn Error>> {
    let ts_reader =
        ParquetRecordBatchReaderBuilder::new_with_metadata(file.try_clone()?, meta.clone())
            .with_row_groups(vec![rg_idx])
            .with_projection(ProjectionMask::roots(parquet_schema, [ts_col_idx]))
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
    Ok(())
}
