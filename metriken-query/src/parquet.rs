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

use crate::histogram_stream::{HistogramRow, HistogramStream, HistogramStreamMeta};
use crate::labels::Labels;
use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::types::{Counter, Counters, Gauge, Gauges, HistogramSnapshot};
use crate::DataSource;

// ─── Public entry point ───────────────────────────────────────────────────────

pub struct Parquet {
    engine: QueryEngine,
}

impl Parquet {
    pub fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        let source: Arc<ParquetSource> = ParquetSource::open(path)?;
        // Arc<ParquetSource> implements DataSource, so wrap it in another Arc
        // to satisfy Arc<dyn DataSource>.
        let ds: Arc<dyn DataSource> = Arc::new(source);
        Ok(Self { engine: QueryEngine::new(ds) })
    }

    /// Open multiple parquet files and merge their data into a single query engine.
    /// Histograms are streamed via k-way merge; counters and gauges are concatenated.
    pub fn open_many(paths: &[&Path]) -> Result<Self, Box<dyn Error>> {
        let files: Result<Vec<Arc<ParquetSource>>, Box<dyn Error>> =
            paths.iter().map(|p| ParquetSource::open(p)).collect();
        let source: Arc<dyn DataSource> = Arc::new(MultiParquetSource { files: files? });
        Ok(Self { engine: QueryEngine::new(source) })
    }

    pub fn query_range(&self, expr: &str, start_s: f64, end_s: f64, step_s: f64) -> Result<QueryResult, QueryError> {
        self.engine.query_range(expr, start_s, end_s, step_s)
    }

    /// Time range of data in the file in seconds, or `None` if the file is empty.
    pub fn time_range(&self) -> Option<(f64, f64)> {
        self.engine.time_range().map(|(min_ns, max_ns)| (min_ns as f64 / 1e9, max_ns as f64 / 1e9))
    }
}

// ─── Multi-file source ────────────────────────────────────────────────────────

struct MultiParquetSource {
    files: Vec<Arc<ParquetSource>>,
}

impl DataSource for MultiParquetSource {
    fn counters(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Counters> {
        let series: Vec<Counter> = self.files.iter()
            .flat_map(|pf| {
                read_counters(pf, name, filter, start_ns, end_ns)
                    .ok()
                    .into_iter()
                    .flat_map(|c| c.series)
            })
            .collect();
        if series.is_empty() { None } else { Some(Counters { series }) }
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        let series: Vec<Gauge> = self.files.iter()
            .flat_map(|pf| {
                read_gauges(pf, name, filter, start_ns, end_ns)
                    .ok()
                    .into_iter()
                    .flat_map(|g| g.series)
            })
            .collect();
        if series.is_empty() { None } else { Some(Gauges { series }) }
    }

    fn histogram_stream(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<HistogramStream> {
        let streams: Vec<HistogramStream> = self.files.iter()
            .filter_map(|pf| pf.histogram_stream(name, filter, start_ns, end_ns))
            .collect();
        HistogramStream::merge(streams)
    }

    fn interval(&self) -> f64 {
        self.files.iter()
            .map(|pf| pf.sampling_interval_ms as f64 / 1000.0)
            .fold(f64::MAX, f64::min)
    }

    fn time_range(&self) -> Option<(u64, u64)> {
        let (mut lo, mut hi): (Option<u64>, Option<u64>) = (None, None);
        for pf in &self.files {
            if let Some((a, b)) = pf.time_range_from_stats() {
                lo = Some(lo.map_or(a, |m: u64| m.min(a)));
                hi = Some(hi.map_or(b, |m: u64| m.max(b)));
            }
        }
        lo.zip(hi)
    }

    #[cfg(test)]
    fn column_map(&self) -> std::collections::HashMap<String, std::collections::HashMap<Labels, String>> {
        let mut out = std::collections::HashMap::new();
        for pf in &self.files {
            for (metric, cols) in pf.column_map() {
                out.entry(metric)
                    .or_insert_with(std::collections::HashMap::new)
                    .extend(cols);
            }
        }
        out
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

impl DataSource for Arc<ParquetSource> {
    fn counters(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Counters> {
        read_counters(self, name, filter, start_ns, end_ns).ok().filter(|c| !c.series.is_empty())
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        read_gauges(self, name, filter, start_ns, end_ns).ok().filter(|g| !g.series.is_empty())
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let ts_col_idx = self.meta.schema().index_of("timestamp").ok()?;
        let interval_ns = self.sampling_interval_ms * 1_000_000;
        let num_rgs = self.meta.metadata().num_row_groups();

        let col_descs: Vec<ColDesc> = parse_schema(self, ts_col_idx)
            .into_iter()
            .filter(|c| {
                matches!(c.kind, ColKind::Histogram { .. })
                    && c.name == name
                    && (filter.inner.is_empty() || c.labels.matches(filter))
            })
            .collect();

        if col_descs.is_empty() {
            return None;
        }

        let config = col_descs.iter().find_map(|c| match c.kind {
            ColKind::Histogram { grouping_power: gp, max_value_power: mvp } => {
                ::histogram::Config::new(gp, mvp).ok()
            }
            _ => None,
        })?;

        let series: Vec<Labels> = col_descs.iter().map(|c| c.labels.clone()).collect();

        let rg_queue: std::collections::VecDeque<usize> = (0..num_rgs)
            .filter(|&rg_idx| !matches!(
                rg_classify(
                    self.meta.metadata().row_group(rg_idx),
                    ts_col_idx,
                    start_ns,
                    end_ns,
                ),
                RgClass::Before | RgClass::After,
            ))
            .collect();

        let cursor = ParquetHistogramCursor {
            pf: Arc::clone(self),
            ts_col_idx,
            interval_ns,
            start_ns,
            end_ns,
            col_descs,
            rg_queue,
            pending: std::collections::VecDeque::new(),
        };

        Some(HistogramStream {
            meta: HistogramStreamMeta { config, series },
            rows: Box::new(cursor),
        })
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

// ─── Histogram cursor ─────────────────────────────────────────────────────────

/// Streams histogram rows from a parquet file one row group at a time.
/// Assumes row groups appear in chronological timestamp order, which
/// metriken-exposition guarantees. Rows within each row group are sorted
/// by (timestamp, series_idx) before being buffered.
struct ParquetHistogramCursor {
    pf: Arc<ParquetSource>,
    ts_col_idx: usize,
    interval_ns: u64,
    start_ns: u64,
    end_ns: u64,
    /// Pre-filtered histogram columns for this metric, in series-index order.
    col_descs: Vec<ColDesc>,
    /// Overlapping row group indices remaining to process.
    rg_queue: std::collections::VecDeque<usize>,
    /// Buffered rows from the current row group (sorted, ready to yield).
    pending: std::collections::VecDeque<HistogramRow>,
}

impl ParquetHistogramCursor {
    /// Load the next overlapping row group into `self.pending`.
    /// Returns false if no more row groups remain.
    fn fill_next_rg(&mut self) -> bool {
        while let Some(rg_idx) = self.rg_queue.pop_front() {
            let Ok(timestamps) = read_timestamps(
                &self.pf, rg_idx, self.ts_col_idx, self.interval_ns
            ) else { continue; };

            let parquet_schema = self.pf.meta.metadata().file_metadata().schema_descr_ptr();
            let mut rg_rows: Vec<HistogramRow> = Vec::new();

            for (si, col) in self.col_descs.iter().enumerate() {
                let ColKind::Histogram { .. } = col.kind else { continue; };
                // Iterator::next cannot propagate errors; fd exhaustion is treated as
                // unrecoverable. Counter/gauge readers use ? instead.
                let Ok(reader) = ParquetRecordBatchReaderBuilder::new_with_metadata(
                    self.pf.file.try_clone().expect("clone file handle"),
                    self.pf.meta.clone(),
                )
                .with_row_groups(vec![rg_idx])
                .with_projection(ProjectionMask::roots(&parquet_schema, [col.col_idx]))
                .build() else { continue; };

                let mut row = 0usize;
                for batch in reader.flatten() {
                    let list = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<ListArray>()
                        .expect("histogram column is not List");
                    for value in list.iter() {
                        let ts = timestamps.get(row).copied().flatten();
                        row += 1;
                        let Some(ts) = ts else { continue; };
                        if ts < self.start_ns || ts > self.end_ns { continue; }
                        let snap = value
                            .and_then(|lv| lv.as_any().downcast_ref::<UInt64Array>()
                                .map(raw_to_sparse_cumulative))
                            .unwrap_or_else(|| HistogramSnapshot { index: vec![], count: vec![] });
                        rg_rows.push(HistogramRow {
                            series_idx: si,
                            timestamp: ts,
                            snapshot: snap,
                        });
                    }
                }
            }

            if !rg_rows.is_empty() {
                rg_rows.sort_unstable_by_key(|r| (r.timestamp, r.series_idx));
                self.pending.extend(rg_rows);
                return true;
            }
        }
        false
    }
}

impl Iterator for ParquetHistogramCursor {
    type Item = HistogramRow;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.pending.pop_front() {
                return Some(row);
            }
            if !self.fill_next_rg() {
                return None;
            }
        }
    }
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
