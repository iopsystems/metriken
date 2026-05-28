use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Int64Array, ListArray, UInt64Array};
use arrow::datatypes::DataType;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder};
use parquet::arrow::ProjectionMask;
use parquet::file::metadata::RowGroupMetaData;
use parquet::file::statistics::Statistics;

use crate::labels::Labels;
use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::types::{Counter, Counters, Gauge, Gauges, Histogram, HistogramSnapshot, Histograms};
use crate::DataSource;

// ─── Public entry point ───────────────────────────────────────────────────────

pub struct Parquet {
    engine: QueryEngine,
}

impl Parquet {
    pub fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        Ok(Self { engine: QueryEngine::new(ParquetSource::open(path)?) })
    }

    pub fn query_range(&self, expr: &str, start_s: f64, end_s: f64, step_s: f64) -> Result<QueryResult, QueryError> {
        self.engine.query_range(expr, start_s, end_s, step_s)
    }

    /// Time range of data in the file in seconds, or `None` if the file is empty.
    pub fn time_range(&self) -> Option<(f64, f64)> {
        self.engine.time_range().map(|(min_ns, max_ns)| (min_ns as f64 / 1e9, max_ns as f64 / 1e9))
    }
}

// ─── Private file reader ──────────────────────────────────────────────────────

struct ParquetSource {
    file: File,
    meta: ArrowReaderMetadata,
    sampling_interval_ms: u64,
}

impl ParquetSource {
    fn open(path: &Path) -> Result<Arc<Self>, Box<dyn Error>> {
        let file = File::open(path)?;
        let meta = ArrowReaderMetadata::load(&file, ArrowReaderOptions::default())?;
        let pq_metadata = meta.metadata();

        let mut file_metadata: HashMap<String, String> = HashMap::new();
        if let Some(kv) = pq_metadata.file_metadata().key_value_metadata() {
            for entry in kv {
                file_metadata.insert(entry.key.clone(), entry.value.clone().unwrap_or_default());
            }
        }

        let sampling_interval_ms = file_metadata
            .get("sampling_interval_ms")
            .map(|v| v.parse::<u64>().expect("bad interval"))
            .unwrap_or(1000);

        Ok(Arc::new(Self { file, meta, sampling_interval_ms }))
    }

    fn time_range_from_stats(&self) -> Option<(u64, u64)> {
        let ts_col_idx = self.meta.schema().index_of("timestamp").ok()?;
        let pq_metadata = self.meta.metadata();
        let mut min_ns: Option<u64> = None;
        let mut max_ns: Option<u64> = None;
        for rg_idx in 0..pq_metadata.num_row_groups() {
            let Some(stats) = pq_metadata.row_group(rg_idx).column(ts_col_idx).statistics() else {
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
}

impl DataSource for ParquetSource {
    fn counters(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Counters> {
        read_counters(self, name, filter, start_ns, end_ns).ok().filter(|c| !c.series.is_empty())
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        read_gauges(self, name, filter, start_ns, end_ns).ok().filter(|g| !g.series.is_empty())
    }

    fn histograms(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Histograms> {
        read_histograms(self, name, filter, start_ns, end_ns).ok().filter(|h| !h.series.is_empty())
    }

    fn interval(&self) -> f64 {
        self.sampling_interval_ms as f64 / 1000.0
    }

    fn time_range(&self) -> Option<(u64, u64)> {
        self.time_range_from_stats()
    }

    #[cfg(test)]
    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        let ts_col_idx = self.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
        let mut out: HashMap<String, HashMap<Labels, String>> = HashMap::new();
        for c in parse_schema(self, ts_col_idx) {
            out.entry(c.name).or_default().insert(c.labels, c.column_name);
        }
        out
    }
}

// ─── Row group classification ─────────────────────────────────────────────────

enum RgClass {
    Before,
    Overlaps,
    After,
    Unknown,
}

fn rg_classify(rg: &RowGroupMetaData, ts_col_idx: usize, start_ns: u64, end_ns: u64) -> RgClass {
    let Some(stats) = rg.column(ts_col_idx).statistics() else { return RgClass::Unknown; };
    let Statistics::Int64(s) = stats else { return RgClass::Unknown; };
    let (Some(rg_min), Some(rg_max)) = (s.min_opt(), s.max_opt()) else { return RgClass::Unknown; };
    let (rg_min, rg_max) = (*rg_min as u64, *rg_max as u64);
    if rg_max < start_ns { RgClass::Before }
    else if rg_min > end_ns { RgClass::After }
    else { RgClass::Overlaps }
}

// ─── Schema parsing ───────────────────────────────────────────────────────────

enum ColKind {
    Counter,
    Gauge,
    Histogram { grouping_power: u8, max_value_power: u8 },
}

struct ColDesc {
    col_idx: usize,
    name: String,
    labels: Labels,
    #[cfg(test)]
    column_name: String,
    kind: ColKind,
}

fn parse_schema(pf: &ParquetSource, ts_col_idx: usize) -> Vec<ColDesc> {
    pf.meta.schema().fields().iter().enumerate().filter_map(|(col_idx, field)| {
        if col_idx == ts_col_idx { return None; }
        let mut meta = field.metadata().clone();
        let column_name = field.name().to_string();
        let name = meta.get("metric").cloned().unwrap_or_else(|| {
            column_name.strip_suffix(":buckets").unwrap_or(&column_name).to_string()
        });
        let grouping_power: Option<u8> = meta.remove("grouping_power").and_then(|v| v.parse().ok());
        let max_value_power: Option<u8> = meta.remove("max_value_power").and_then(|v| v.parse().ok());
        let mut labels = Labels::default();
        for (k, v) in meta.iter() {
            match k.as_str() {
                "metric" | "metric_type" | "unit" => continue,
                _ => { labels.inner.insert(k.clone(), v.clone()); }
            }
        }
        let kind = match field.data_type() {
            DataType::UInt64 => ColKind::Counter,
            DataType::Int64 => ColKind::Gauge,
            DataType::List(inner) if inner.data_type() == &DataType::UInt64 => {
                let (Some(gp), Some(mvp)) = (grouping_power, max_value_power) else { return None; };
                ColKind::Histogram { grouping_power: gp, max_value_power: mvp }
            }
            _ => return None,
        };
        Some(ColDesc { col_idx, name, labels, #[cfg(test)] column_name, kind })
    }).collect()
}

// ─── Timestamp reader ─────────────────────────────────────────────────────────

fn snap_timestamp(ts: u64, interval_ns: u64) -> u64 {
    (ts + interval_ns / 2).checked_div(interval_ns).map_or(ts, |q| q * interval_ns)
}

fn read_timestamps(pf: &ParquetSource, rg_idx: usize, ts_col_idx: usize, interval_ns: u64) -> Result<Vec<Option<u64>>, Box<dyn Error>> {
    let parquet_schema = pf.meta.metadata().file_metadata().schema_descr_ptr();
    let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(pf.file.try_clone()?, pf.meta.clone())
        .with_row_groups(vec![rg_idx])
        .with_projection(ProjectionMask::roots(&parquet_schema, [ts_col_idx]))
        .build()?;
    let mut out = Vec::new();
    for batch in reader.flatten() {
        let arr = batch.column(0).as_any().downcast_ref::<UInt64Array>().ok_or("timestamp column is not UInt64")?;
        out.reserve(arr.len());
        for v in arr.iter() { out.push(v.map(|raw| snap_timestamp(raw, interval_ns))); }
    }
    Ok(out)
}

// ─── Counter reader ───────────────────────────────────────────────────────────

fn read_counters(pf: &ParquetSource, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Result<Counters, Box<dyn Error>> {
    let ts_col_idx = pf.meta.schema().index_of("timestamp").map_err(|_| "missing timestamp")?;
    let interval_ns = pf.sampling_interval_ms * 1_000_000;
    let parquet_schema = pf.meta.metadata().file_metadata().schema_descr_ptr();
    let num_rgs = pf.meta.metadata().num_row_groups();

    let cols: Vec<ColDesc> = parse_schema(pf, ts_col_idx).into_iter()
        .filter(|c| matches!(c.kind, ColKind::Counter) && c.name == name && (filter.inner.is_empty() || c.labels.matches(filter)))
        .collect();
    if cols.is_empty() { return Ok(Counters { series: vec![] }); }

    let mut ts_acc: Vec<Vec<u64>> = vec![Vec::new(); cols.len()];
    let mut val_acc: Vec<Vec<u64>> = vec![Vec::new(); cols.len()];

    for rg_idx in 0..num_rgs {
        match rg_classify(pf.meta.metadata().row_group(rg_idx), ts_col_idx, start_ns, end_ns) {
            RgClass::Before | RgClass::After => continue,
            _ => {}
        }
        let timestamps = read_timestamps(pf, rg_idx, ts_col_idx, interval_ns)?;
        for (i, col) in cols.iter().enumerate() {
            let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(pf.file.try_clone()?, pf.meta.clone())
                .with_row_groups(vec![rg_idx])
                .with_projection(ProjectionMask::roots(&parquet_schema, [col.col_idx]))
                .build()?;
            let mut row = 0usize;
            for batch in reader.flatten() {
                let arr = batch.column(0).as_any().downcast_ref::<UInt64Array>().expect("counter column is not UInt64");
                for v in arr.iter() {
                    if let (Some(v), Some(Some(ts))) = (v, timestamps.get(row)) {
                        if *ts >= start_ns && *ts <= end_ns { ts_acc[i].push(*ts); val_acc[i].push(v); }
                    }
                    row += 1;
                }
            }
        }
    }

    Ok(Counters { series: cols.into_iter().zip(ts_acc).zip(val_acc)
        .filter(|((_, ts), _)| !ts.is_empty())
        .map(|((col, timestamps), values)| Counter { labels: col.labels, timestamps, values })
        .collect() })
}

// ─── Gauge reader ─────────────────────────────────────────────────────────────

fn read_gauges(pf: &ParquetSource, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Result<Gauges, Box<dyn Error>> {
    let ts_col_idx = pf.meta.schema().index_of("timestamp").map_err(|_| "missing timestamp")?;
    let interval_ns = pf.sampling_interval_ms * 1_000_000;
    let parquet_schema = pf.meta.metadata().file_metadata().schema_descr_ptr();
    let num_rgs = pf.meta.metadata().num_row_groups();

    let cols: Vec<ColDesc> = parse_schema(pf, ts_col_idx).into_iter()
        .filter(|c| matches!(c.kind, ColKind::Gauge) && c.name == name && (filter.inner.is_empty() || c.labels.matches(filter)))
        .collect();
    if cols.is_empty() { return Ok(Gauges { series: vec![] }); }

    let mut ts_acc: Vec<Vec<u64>> = vec![Vec::new(); cols.len()];
    let mut val_acc: Vec<Vec<i64>> = vec![Vec::new(); cols.len()];

    for rg_idx in 0..num_rgs {
        match rg_classify(pf.meta.metadata().row_group(rg_idx), ts_col_idx, start_ns, end_ns) {
            RgClass::Before | RgClass::After => continue,
            _ => {}
        }
        let timestamps = read_timestamps(pf, rg_idx, ts_col_idx, interval_ns)?;
        for (i, col) in cols.iter().enumerate() {
            let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(pf.file.try_clone()?, pf.meta.clone())
                .with_row_groups(vec![rg_idx])
                .with_projection(ProjectionMask::roots(&parquet_schema, [col.col_idx]))
                .build()?;
            let mut row = 0usize;
            for batch in reader.flatten() {
                let arr = batch.column(0).as_any().downcast_ref::<Int64Array>().expect("gauge column is not Int64");
                for v in arr.iter() {
                    if let (Some(v), Some(Some(ts))) = (v, timestamps.get(row)) {
                        if *ts >= start_ns && *ts <= end_ns { ts_acc[i].push(*ts); val_acc[i].push(v); }
                    }
                    row += 1;
                }
            }
        }
    }

    Ok(Gauges { series: cols.into_iter().zip(ts_acc).zip(val_acc)
        .filter(|((_, ts), _)| !ts.is_empty())
        .map(|((col, timestamps), values)| Gauge { labels: col.labels, timestamps, values })
        .collect() })
}

// ─── Histogram reader ─────────────────────────────────────────────────────────

fn read_histograms(pf: &ParquetSource, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Result<Histograms, Box<dyn Error>> {
    let ts_col_idx = pf.meta.schema().index_of("timestamp").map_err(|_| "missing timestamp")?;
    let interval_ns = pf.sampling_interval_ms * 1_000_000;
    let parquet_schema = pf.meta.metadata().file_metadata().schema_descr_ptr();
    let num_rgs = pf.meta.metadata().num_row_groups();

    let cols: Vec<ColDesc> = parse_schema(pf, ts_col_idx).into_iter()
        .filter(|c| matches!(c.kind, ColKind::Histogram { .. }) && c.name == name && (filter.inner.is_empty() || c.labels.matches(filter)))
        .collect();
    if cols.is_empty() { return Ok(Histograms { series: vec![] }); }

    let mut ts_acc: Vec<Vec<u64>> = vec![Vec::new(); cols.len()];
    let mut snap_acc: Vec<Vec<HistogramSnapshot>> = vec![Vec::new(); cols.len()];
    let mut cfg_acc: Vec<Option<::histogram::Config>> = vec![None; cols.len()];

    for rg_idx in 0..num_rgs {
        match rg_classify(pf.meta.metadata().row_group(rg_idx), ts_col_idx, start_ns, end_ns) {
            RgClass::Before | RgClass::After => continue,
            _ => {}
        }
        let timestamps = read_timestamps(pf, rg_idx, ts_col_idx, interval_ns)?;
        for (i, col) in cols.iter().enumerate() {
            let (gp, mvp) = match col.kind {
                ColKind::Histogram { grouping_power: gp, max_value_power: mvp } => (gp, mvp),
                _ => continue,
            };
            if cfg_acc[i].is_none() { cfg_acc[i] = ::histogram::Config::new(gp, mvp).ok(); }
            let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(pf.file.try_clone()?, pf.meta.clone())
                .with_row_groups(vec![rg_idx])
                .with_projection(ProjectionMask::roots(&parquet_schema, [col.col_idx]))
                .build()?;
            let mut row = 0usize;
            for batch in reader.flatten() {
                let list = batch.column(0).as_any().downcast_ref::<ListArray>().expect("histogram column is not List");
                for value in list.iter() {
                    let Some(Some(ts)) = timestamps.get(row).copied() else { row += 1; continue; };
                    if ts >= start_ns && ts <= end_ns {
                        let snap = value
                            .and_then(|lv| lv.as_any().downcast_ref::<UInt64Array>().map(raw_to_sparse_cumulative))
                            .unwrap_or_else(|| HistogramSnapshot { index: vec![], count: vec![] });
                        snap_acc[i].push(snap);
                        ts_acc[i].push(ts);
                    }
                    row += 1;
                }
            }
        }
    }

    Ok(Histograms { series: cols.into_iter().zip(ts_acc).zip(snap_acc).zip(cfg_acc)
        .filter(|(((_, ts), _), _)| !ts.is_empty())
        .filter_map(|(((col, timestamps), snapshots), cfg)| {
            Some(Histogram { labels: col.labels, config: cfg?, timestamps, snapshots })
        })
        .collect() })
}

// ─── Histogram snapshot helper ────────────────────────────────────────────────

/// Convert a raw bucket array (individual counts per bucket) into a
/// sparse cumulative prefix-sum snapshot. Only non-zero buckets are stored.
fn raw_to_sparse_cumulative(arr: &UInt64Array) -> HistogramSnapshot {
    let mut index = Vec::new();
    let mut count = Vec::new();
    let mut running = 0u64;
    for (i, v) in arr.iter().enumerate() {
        let v = v.unwrap_or(0);
        if v > 0 {
            running = running.saturating_add(v);
            index.push(i as u32);
            count.push(running);
        }
    }
    HistogramSnapshot { index, count }
}
