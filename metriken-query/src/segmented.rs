//! SegmentedParquetReader: presents an ordered list of parquet byte blobs
//! (segments of one logical table) as a single MetricsSource. Open is
//! footer-only per segment; queries decode only the row groups they touch,
//! spliced in segment order. Same-identity columns across segments are ONE
//! series (unlike MultiParquetSource, which duplicates).

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::sync::Arc;

use crate::histogram_stream::{HistogramStream, HistogramStreamMeta};
use crate::labels::Labels;
use crate::promql::QueryEngine;
use crate::types::{Counter, Counters, Gauge, Gauges};
use crate::{
    BufferPool, DataSource, MetricsSource, ParquetReader, QueryError, QueryOptions, QueryResult,
};

/// Reads an ordered list of parquet segments (byte blobs) that together form
/// one logical per-sampler table, and presents them as a single
/// [`MetricsSource`] with a unioned identity surface: the same `(name,
/// labels)` pair appearing in more than one segment is ONE series, not one
/// per segment (unlike `MultiParquetSource`, which duplicates same-identity
/// series across files).
///
/// Opening is footer-only per segment, via
/// [`ParquetReader::open_bytes_with_pool`] — no row-group data is decoded
/// until a query touches it.
///
/// Queries (`query_range` / `query` / `columns`) evaluate over a
/// [`DataSource`] that splices raw per-series samples across segments in
/// segment order, *below* PromQL evaluation — so range functions like
/// `rate()` see one continuous timeline and boundary-spanning windows are
/// computed on complete data. Each segment decodes only the row groups the
/// query's time range touches.
pub struct SegmentedParquetReader {
    /// Segments in logical (time) order. Each is opened footer-only and all
    /// share `pool`'s decode-cache budget.
    segments: Vec<ParquetReader>,
    /// PromQL engine over the splicing [`SegmentedSource`].
    engine: QueryEngine,
}

impl SegmentedParquetReader {
    /// Open `segments` (raw parquet bytes, in logical/time order) footer-only,
    /// wiring every segment to the same shared `pool` for decoded row-group
    /// caching. No row-group data is read here.
    pub fn open_bytes_with_pool(
        segments: Vec<Vec<u8>>,
        pool: Arc<BufferPool>,
    ) -> Result<Self, Box<dyn Error>> {
        if segments.is_empty() {
            return Err("SegmentedParquetReader requires at least one segment".into());
        }
        let segments = segments
            .into_iter()
            .map(|bytes| ParquetReader::open_bytes_with_pool(bytes, pool.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        let source = SegmentedSource {
            segments: segments.iter().map(ParquetReader::data_source).collect(),
        };
        let engine = QueryEngine::new(Arc::new(source));
        Ok(Self { segments, engine })
    }

    /// Number of segments backing this reader.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Names of all counter metrics across every segment (sorted, deduplicated union).
    pub fn counter_names(&self) -> Vec<String> {
        union_names(self.segments.iter().map(ParquetReader::counter_names))
    }

    /// Names of all gauge metrics across every segment (sorted, deduplicated union).
    pub fn gauge_names(&self) -> Vec<String> {
        union_names(self.segments.iter().map(ParquetReader::gauge_names))
    }

    /// Names of all histogram metrics across every segment (sorted, deduplicated union).
    pub fn histogram_names(&self) -> Vec<String> {
        union_names(self.segments.iter().map(ParquetReader::histogram_names))
    }

    /// All label combinations for the named counter metric, unioned (and
    /// deduplicated) across every segment.
    pub fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        union_labels(self.segments.iter().map(|s| s.counter_labels(name)))
    }

    /// All label combinations for the named gauge metric, unioned (and
    /// deduplicated) across every segment.
    pub fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        union_labels(self.segments.iter().map(|s| s.gauge_labels(name)))
    }

    /// All label combinations for the named histogram metric, unioned (and
    /// deduplicated) across every segment.
    pub fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        union_labels(self.segments.iter().map(|s| s.histogram_labels(name)))
    }

    /// Full time extent across all segments in nanoseconds, or `None` if empty.
    pub fn time_range_ns(&self) -> Option<(u64, u64)> {
        self.segments
            .iter()
            .filter_map(ParquetReader::time_range_ns)
            .fold(None, |acc, (lo, hi)| match acc {
                None => Some((lo, hi)),
                Some((alo, ahi)) => Some((alo.min(lo), ahi.max(hi))),
            })
    }

    /// Full time extent across all segments in seconds, or `None` if empty.
    pub fn time_range(&self) -> Option<(f64, f64)> {
        self.time_range_ns()
            .map(|(lo, hi)| (lo as f64 / 1e9, hi as f64 / 1e9))
    }

    /// Sampling interval in seconds; the finest across all segments.
    pub fn interval(&self) -> f64 {
        self.segments
            .iter()
            .map(ParquetReader::interval)
            .fold(f64::MAX, f64::min)
    }

    /// Key-value metadata merged across all segment footers (last segment,
    /// in `segments` order, wins on key collision).
    pub fn file_metadata(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for s in &self.segments {
            out.extend(s.file_metadata());
        }
        out
    }

    /// Look up a single metadata value by key without cloning the full map.
    /// Last segment wins on collision (matches [`file_metadata`](Self::file_metadata)).
    pub fn metadata_get(&self, key: &str) -> Option<String> {
        let mut last = None;
        for s in &self.segments {
            if let Some(v) = s.metadata_get(key) {
                last = Some(v);
            }
        }
        last
    }

    /// Convenience: the `source` key from file metadata (e.g. "rezolus").
    /// Returns an empty string if absent.
    pub fn source(&self) -> String {
        self.metadata_get("source").unwrap_or_default()
    }

    /// Convenience: the `version` key from file metadata.
    /// Returns an empty string if absent.
    pub fn version(&self) -> String {
        self.metadata_get("version").unwrap_or_default()
    }
}

/// Sorted, deduplicated union of metric names across segments.
fn union_names<I: IntoIterator<Item = Vec<String>>>(lists: I) -> Vec<String> {
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for list in lists {
        names.extend(list);
    }
    names.into_iter().collect()
}

/// Deduplicated union of label sets across segments. Mirrors the
/// sort-then-dedup pattern `MultiParquetSource` uses for its own
/// (non-splicing) union of `*_labels()` results.
fn union_labels<I: IntoIterator<Item = Vec<BTreeMap<String, String>>>>(
    lists: I,
) -> Vec<BTreeMap<String, String>> {
    let mut sets: Vec<BTreeMap<String, String>> = Vec::new();
    for list in lists {
        sets.extend(list);
    }
    sets.sort();
    sets.dedup();
    sets
}

/// The splicing [`DataSource`] the PromQL engine evaluates over: raw
/// per-series samples from each segment, concatenated in segment order,
/// with same-`(name, labels)` series merged into ONE series. Splicing at
/// this seam — below PromQL evaluation — means range functions (`rate()`
/// windows spanning a segment boundary) are computed on the complete
/// timeline, and each segment decodes only the row groups the queried
/// time range touches.
///
/// Timestamps are NOT sorted or deduplicated: a spliced series carries
/// exactly the samples a single-file table with the same rows would.
struct SegmentedSource {
    /// Per-segment sample providers, in logical (time) order.
    segments: Vec<Arc<dyn DataSource>>,
}

/// Append `chunk`'s counter series onto `acc`, merging by label identity
/// (concatenate in arrival order; first appearance fixes series order).
///
/// Windows policy: per-point acquisition windows concatenate only when every
/// contributing chunk of a series carries them; a mixed series drops to
/// `None` (no uncertainty band) rather than risk misaligned windows.
fn splice_counters(acc: &mut Vec<Counter>, chunk: Counters) {
    for c in chunk.series {
        if let Some(a) = acc.iter_mut().find(|a| a.labels == c.labels) {
            a.timestamps.extend(c.timestamps);
            a.values.extend(c.values);
            a.windows = match (a.windows.take(), c.windows) {
                (Some(mut aw), Some(cw)) => {
                    aw.extend(cw);
                    Some(aw)
                }
                _ => None,
            };
        } else {
            acc.push(c);
        }
    }
}

/// Gauge twin of [`splice_counters`] (same identity-merge and windows policy).
fn splice_gauges(acc: &mut Vec<Gauge>, chunk: Gauges) {
    for g in chunk.series {
        if let Some(a) = acc.iter_mut().find(|a| a.labels == g.labels) {
            a.timestamps.extend(g.timestamps);
            a.values.extend(g.values);
            a.windows = match (a.windows.take(), g.windows) {
                (Some(mut aw), Some(cw)) => {
                    aw.extend(cw);
                    Some(aw)
                }
                _ => None,
            };
        } else {
            acc.push(g);
        }
    }
}

/// Chain per-segment histogram streams in segment order, remapping each
/// stream's series indices onto a unified series list so the same labels are
/// ONE series across segments. Unlike [`HistogramStream::merge`] (a k-way
/// sort-merge for independent files), this concatenates — preserving
/// single-file row-order semantics for segments of one table.
fn splice_histogram_streams(streams: Vec<HistogramStream>) -> Option<HistogramStream> {
    if streams.len() <= 1 {
        return streams.into_iter().next();
    }
    let config = streams[0].meta.config;
    debug_assert!(
        streams.iter().all(|s| s.meta.config == config),
        "segments of one table must share a histogram config"
    );
    let mut series: Vec<Labels> = Vec::new();
    let mut parts: Vec<Box<dyn Iterator<Item = crate::histogram_stream::HistogramRow> + Send>> =
        Vec::with_capacity(streams.len());
    for stream in streams {
        let remap: Vec<usize> = stream
            .meta
            .series
            .iter()
            .map(|labels| {
                series.iter().position(|s| s == labels).unwrap_or_else(|| {
                    series.push(labels.clone());
                    series.len() - 1
                })
            })
            .collect();
        parts.push(Box::new(stream.rows.map(move |mut row| {
            row.series_idx = remap[row.series_idx];
            row
        })));
    }
    Some(HistogramStream {
        meta: HistogramStreamMeta { config, series },
        rows: Box::new(parts.into_iter().flatten()),
    })
}

impl DataSource for SegmentedSource {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        raw: bool,
    ) -> Option<Counters> {
        let mut series: Vec<Counter> = Vec::new();
        for seg in &self.segments {
            if let Some(chunk) = seg.counters(name, filter, start_ns, end_ns, raw) {
                splice_counters(&mut series, chunk);
            }
        }
        if series.is_empty() {
            None
        } else {
            Some(Counters { series })
        }
    }

    fn gauges(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        raw: bool,
    ) -> Option<Gauges> {
        let mut series: Vec<Gauge> = Vec::new();
        for seg in &self.segments {
            if let Some(chunk) = seg.gauges(name, filter, start_ns, end_ns, raw) {
                splice_gauges(&mut series, chunk);
            }
        }
        if series.is_empty() {
            None
        } else {
            Some(Gauges { series })
        }
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let streams: Vec<HistogramStream> = self
            .segments
            .iter()
            .filter_map(|seg| seg.histogram_stream(name, filter, start_ns, end_ns))
            .collect();
        splice_histogram_streams(streams)
    }

    fn interval(&self) -> f64 {
        self.segments
            .iter()
            .map(|s| s.interval())
            .fold(f64::MAX, f64::min)
    }

    fn time_range(&self) -> Option<(u64, u64)> {
        self.segments
            .iter()
            .filter_map(|s| s.time_range())
            .fold(None, |acc, (lo, hi)| match acc {
                None => Some((lo, hi)),
                Some((alo, ahi)) => Some((alo.min(lo), ahi.max(hi))),
            })
    }

    fn counter_names(&self) -> Vec<String> {
        union_names(self.segments.iter().map(|s| s.counter_names()))
    }

    fn gauge_names(&self) -> Vec<String> {
        union_names(self.segments.iter().map(|s| s.gauge_names()))
    }

    fn histogram_names(&self) -> Vec<String> {
        union_names(self.segments.iter().map(|s| s.histogram_names()))
    }

    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        union_labels(self.segments.iter().map(|s| s.counter_labels(name)))
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        union_labels(self.segments.iter().map(|s| s.gauge_labels(name)))
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        union_labels(self.segments.iter().map(|s| s.histogram_labels(name)))
    }

    fn file_metadata(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for s in &self.segments {
            out.extend(s.file_metadata());
        }
        out
    }

    fn metadata_get(&self, key: &str) -> Option<String> {
        // Last segment wins on collision, matching file_metadata().
        let mut last = None;
        for s in &self.segments {
            if let Some(v) = s.metadata_get(key) {
                last = Some(v);
            }
        }
        last
    }

    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        let mut out: HashMap<String, HashMap<Labels, String>> = HashMap::new();
        for s in &self.segments {
            for (metric, cols) in s.column_map() {
                out.entry(metric).or_default().extend(cols);
            }
        }
        out
    }
}

impl MetricsSource for SegmentedParquetReader {
    fn query_range_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        self.engine
            .query_range_opts(expr, start_s, end_s, step_s, opts.rate_mode)
    }

    fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        self.engine.query(expr, time)
    }

    fn columns(&self, query: &str) -> Result<std::collections::HashSet<String>, QueryError> {
        self.engine.columns(query)
    }

    fn sample_timestamps(&self) -> Vec<u64> {
        // Raw (un-snapped) per-sample timestamps, concatenated in segment
        // order — same splice contract as the query path, no sort/dedup.
        let mut out = Vec::new();
        for s in &self.segments {
            out.extend(s.sample_timestamps());
        }
        out
    }

    fn time_range(&self) -> Option<(f64, f64)> {
        self.time_range()
    }

    fn time_range_ns(&self) -> Option<(u64, u64)> {
        self.time_range_ns()
    }

    fn interval(&self) -> f64 {
        self.interval()
    }

    fn source(&self) -> String {
        self.source()
    }

    fn version(&self) -> String {
        self.version()
    }

    fn filename(&self) -> Option<String> {
        // No single-segment concept of a display name; the caller (rez
        // reader / manifest) owns naming for a segmented table.
        None
    }

    fn metadata_get(&self, key: &str) -> Option<String> {
        self.metadata_get(key)
    }

    fn file_metadata(&self) -> HashMap<String, String> {
        self.file_metadata()
    }

    fn counter_names(&self) -> Vec<String> {
        self.counter_names()
    }

    fn gauge_names(&self) -> Vec<String> {
        self.gauge_names()
    }

    fn histogram_names(&self) -> Vec<String> {
        self.histogram_names()
    }

    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.counter_labels(name)
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.gauge_labels(name)
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.histogram_labels(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use arrow::array::{ArrayRef, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;

    /// Build one parquet segment: a `timestamp` UInt64 column plus one
    /// counter column `name` (UInt64, field metadata `metric`/`metric_type=counter`,
    /// plus any `labels`) with the given (ts, value) rows. Mirrors the schema
    /// conventions used by `parquet.rs`'s test fixtures (see
    /// `build_parquet_with_timestamps` and `fixtures::synthetic::FixtureBuilder`).
    fn segment(name: &str, labels: &[(&str, &str)], rows: &[(u64, u64)]) -> Vec<u8> {
        let mut metadata = HashMap::new();
        metadata.insert("metric".to_string(), name.to_string());
        metadata.insert("metric_type".to_string(), "counter".to_string());
        for (k, v) in labels {
            metadata.insert(k.to_string(), v.to_string());
        }

        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new(name, DataType::UInt64, true).with_metadata(metadata),
        ]));

        let kv = vec![KeyValue {
            key: "sampling_interval_ms".to_string(),
            value: Some("1000".to_string()),
        }];
        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_key_value_metadata(Some(kv))
            .build();

        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();

        let ts: Vec<u64> = rows.iter().map(|(t, _)| *t).collect();
        let vals: Vec<u64> = rows.iter().map(|(_, v)| *v).collect();
        let ts_array = Arc::new(UInt64Array::from(ts)) as ArrayRef;
        let val_array = Arc::new(UInt64Array::from(vals)) as ArrayRef;
        let batch = RecordBatch::try_new(schema, vec![ts_array, val_array]).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        buf
    }

    /// Two-metric variant of [`segment`]: counter `name_a` (UInt64) plus
    /// gauge `name_b` (Int64), sharing one `timestamp` column, with rows
    /// `(ts, counter_value, gauge_value)`.
    fn segment_two(name_a: &str, name_b: &str, rows: &[(u64, u64, i64)]) -> Vec<u8> {
        use arrow::array::Int64Array;

        let meta = |name: &str, kind: &str| {
            let mut m = HashMap::new();
            m.insert("metric".to_string(), name.to_string());
            m.insert("metric_type".to_string(), kind.to_string());
            m
        };

        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new(name_a, DataType::UInt64, true).with_metadata(meta(name_a, "counter")),
            Field::new(name_b, DataType::Int64, true).with_metadata(meta(name_b, "gauge")),
        ]));

        let kv = vec![KeyValue {
            key: "sampling_interval_ms".to_string(),
            value: Some("1000".to_string()),
        }];
        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_key_value_metadata(Some(kv))
            .build();

        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();

        let ts: Vec<u64> = rows.iter().map(|(t, _, _)| *t).collect();
        let a: Vec<u64> = rows.iter().map(|(_, v, _)| *v).collect();
        let b: Vec<i64> = rows.iter().map(|(_, _, v)| *v).collect();
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt64Array::from(ts)) as ArrayRef,
                Arc::new(UInt64Array::from(a)) as ArrayRef,
                Arc::new(Int64Array::from(b)) as ArrayRef,
            ],
        )
        .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        buf
    }

    /// Gauge variant of [`segment`]: one label-free Int64 gauge column.
    fn segment_gauge(name: &str, rows: &[(u64, i64)]) -> Vec<u8> {
        use arrow::array::Int64Array;

        let mut meta = HashMap::new();
        meta.insert("metric".to_string(), name.to_string());
        meta.insert("metric_type".to_string(), "gauge".to_string());

        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new(name, DataType::Int64, true).with_metadata(meta),
        ]));

        let kv = vec![KeyValue {
            key: "sampling_interval_ms".to_string(),
            value: Some("1000".to_string()),
        }];
        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_key_value_metadata(Some(kv))
            .build();

        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();

        let ts: Vec<u64> = rows.iter().map(|(t, _)| *t).collect();
        let vals: Vec<i64> = rows.iter().map(|(_, v)| *v).collect();
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt64Array::from(ts)) as ArrayRef,
                Arc::new(Int64Array::from(vals)) as ArrayRef,
            ],
        )
        .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        buf
    }

    /// Histogram variant of [`segment`]: one `List<UInt64>` bucket column
    /// carrying the histogram config in field metadata. `rows` is
    /// `(ts, buckets)`; every row must have `config.total_buckets()` entries.
    fn segment_histogram(
        name: &str,
        grouping_power: u8,
        max_value_power: u8,
        rows: &[(u64, Vec<u64>)],
    ) -> Vec<u8> {
        use arrow::array::ListArray;
        use arrow::buffer::OffsetBuffer;

        let mut meta = HashMap::new();
        meta.insert("metric".to_string(), name.to_string());
        meta.insert("metric_type".to_string(), "histogram".to_string());
        meta.insert("grouping_power".to_string(), grouping_power.to_string());
        meta.insert("max_value_power".to_string(), max_value_power.to_string());

        let item = Arc::new(Field::new("item", DataType::UInt64, true));
        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new(
                format!("{name}:buckets"),
                DataType::List(item.clone()),
                true,
            )
            .with_metadata(meta),
        ]));

        let kv = vec![KeyValue {
            key: "sampling_interval_ms".to_string(),
            value: Some("1000".to_string()),
        }];
        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_key_value_metadata(Some(kv))
            .build();

        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();

        let ts: Vec<u64> = rows.iter().map(|(t, _)| *t).collect();
        let mut offsets: Vec<i32> = vec![0];
        let mut flat: Vec<u64> = Vec::new();
        for (_, buckets) in rows {
            flat.extend(buckets);
            offsets.push(flat.len() as i32);
        }
        let list = ListArray::new(
            item,
            OffsetBuffer::new(offsets.into()),
            Arc::new(UInt64Array::from(flat)),
            None,
        );
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt64Array::from(ts)) as ArrayRef,
                Arc::new(list) as ArrayRef,
            ],
        )
        .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        buf
    }

    /// Windowed variant of [`segment`]: one counter plus its
    /// `<m>:window_begin` (Int64 offset from the raw timestamp) and
    /// `<m>:window_width` (UInt64 ns) acquisition-window sidecar columns,
    /// with rows `(ts, value, begin_offset, width)`.
    fn segment_windowed(name: &str, rows: &[(u64, u64, i64, u64)]) -> Vec<u8> {
        use arrow::array::Int64Array;

        let mut meta = HashMap::new();
        meta.insert("metric".to_string(), name.to_string());
        meta.insert("metric_type".to_string(), "counter".to_string());

        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new(name, DataType::UInt64, true).with_metadata(meta),
            Field::new(format!("{name}:window_begin"), DataType::Int64, true),
            Field::new(format!("{name}:window_width"), DataType::UInt64, true),
        ]));

        let kv = vec![KeyValue {
            key: "sampling_interval_ms".to_string(),
            value: Some("1000".to_string()),
        }];
        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_key_value_metadata(Some(kv))
            .build();

        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();

        let ts: Vec<u64> = rows.iter().map(|(t, ..)| *t).collect();
        let vals: Vec<u64> = rows.iter().map(|(_, v, ..)| *v).collect();
        let begins: Vec<i64> = rows.iter().map(|(_, _, b, _)| *b).collect();
        let widths: Vec<u64> = rows.iter().map(|(_, _, _, w)| *w).collect();
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt64Array::from(ts)) as ArrayRef,
                Arc::new(UInt64Array::from(vals)) as ArrayRef,
                Arc::new(Int64Array::from(begins)) as ArrayRef,
                Arc::new(UInt64Array::from(widths)) as ArrayRef,
            ],
        )
        .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        buf
    }

    #[test]
    fn union_names_single_series_across_segments() {
        let a = segment(
            "cpu_cycles",
            &[],
            &[(1_000_000_000, 10), (2_000_000_000, 20)],
        );
        let b = segment(
            "cpu_cycles",
            &[],
            &[(3_000_000_000, 35), (4_000_000_000, 50)],
        );
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(vec![a, b], pool).unwrap();
        assert_eq!(r.counter_names(), vec!["cpu_cycles".to_string()]);
        // ONE series, not two (the MultiParquetSource failure mode).
        assert_eq!(r.counter_labels("cpu_cycles").len(), 1);

        // Same metric name, but two DISTINCT label sets across segments:
        // this must union to TWO series, not collapse to one just because
        // the names match.
        let c = segment("cpu_cycles", &[("core", "0")], &[(1_000_000_000, 1)]);
        let d = segment("cpu_cycles", &[("core", "1")], &[(1_000_000_000, 2)]);
        let pool2 = BufferPool::new(64 * 1024 * 1024);
        let r2 = SegmentedParquetReader::open_bytes_with_pool(vec![c, d], pool2).unwrap();
        assert_eq!(r2.counter_labels("cpu_cycles").len(), 2);
    }

    #[test]
    fn open_performs_no_row_group_decode() {
        // BufferPool is a pure cache — it never errors on size, so "open
        // succeeds with a tiny pool" proves nothing. The load-bearing
        // assertion: after open the pool must be completely untouched.
        let a = segment(
            "cpu_cycles",
            &[],
            &[(1_000_000_000, 10), (2_000_000_000, 20)],
        );
        let b = segment("cpu_cycles", &[], &[(3_000_000_000, 35)]);
        let pool = BufferPool::new(64 * 1024 * 1024);
        let _r =
            SegmentedParquetReader::open_bytes_with_pool(vec![a, b], Arc::clone(&pool)).unwrap();
        let stats = pool.stats();
        assert_eq!(stats.misses, 0, "open must not decode row groups");
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.bytes_used, 0);
    }

    #[test]
    fn open_rejects_empty_segments() {
        let pool = BufferPool::new(64 * 1024 * 1024);
        assert!(SegmentedParquetReader::open_bytes_with_pool(vec![], pool).is_err());
    }

    #[test]
    fn query_decodes_only_the_segments_it_touches() {
        // The splice must stay lazy: a query whose window lies wholly inside
        // the last segment must not decode the earlier ones. A decode-all
        // implementation would touch every segment regardless of range.
        let segs = || {
            vec![
                segment("cpu_cycles", &[], &[(1_000_000_000, 10)]),
                segment("cpu_cycles", &[], &[(2_000_000_000, 20)]),
                segment("cpu_cycles", &[], &[(9_000_000_000, 90)]),
            ]
        };

        let narrow_pool = BufferPool::new(64 * 1024 * 1024);
        let r =
            SegmentedParquetReader::open_bytes_with_pool(segs(), Arc::clone(&narrow_pool)).unwrap();
        // Grid mode looks back one step, so 9..10s reaches no further than 8s —
        // still clear of the 1s/2s segments.
        let _ = r.query_range("rate(cpu_cycles[1s])", 9.0, 10.0, 1.0);
        let narrow = narrow_pool.stats();

        let wide_pool = BufferPool::new(64 * 1024 * 1024);
        let r =
            SegmentedParquetReader::open_bytes_with_pool(segs(), Arc::clone(&wide_pool)).unwrap();
        let _ = r.query_range("rate(cpu_cycles[1s])", 1.0, 10.0, 1.0);
        let wide = wide_pool.stats();

        assert!(
            narrow.entries > 0,
            "the narrow query must still decode the segment it does touch \
             (otherwise this test is vacuous): {narrow:?}"
        );
        assert!(
            narrow.entries < wide.entries,
            "narrow query must decode fewer row groups than the full-range one \
             (narrow={narrow:?} wide={wide:?})"
        );
    }

    #[test]
    fn query_range_splices_segments_like_a_single_file() {
        let rows_all = [
            (1_000_000_000u64, 10u64),
            (2_000_000_000, 20),
            (3_000_000_000, 35),
            (4_000_000_000, 50),
        ];
        let single = vec![segment("cpu_cycles", &[], &rows_all)];
        let split = vec![
            segment("cpu_cycles", &[], &rows_all[..2]),
            segment("cpu_cycles", &[], &rows_all[2..]),
        ];
        let pool = BufferPool::new(64 * 1024 * 1024);
        let a = SegmentedParquetReader::open_bytes_with_pool(single, Arc::clone(&pool)).unwrap();
        let b = SegmentedParquetReader::open_bytes_with_pool(split, pool).unwrap();
        // rate() across the segment boundary must be identical to the
        // single-file evaluation, including the boundary window.
        let qa = a
            .query_range("rate(cpu_cycles[2s])", 1.0, 5.0, 1.0)
            .unwrap();
        let qb = b
            .query_range("rate(cpu_cycles[2s])", 1.0, 5.0, 1.0)
            .unwrap();
        assert_eq!(format!("{qa:?}"), format!("{qb:?}"));
        let QueryResult::Matrix { result } = qb else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1, "one spliced series, not one per segment");

        // The boundary step is the load-bearing one: rate at t=3s is computed
        // from the sample at 2s (segment 0) and the sample at 3s (segment 1).
        // Evaluating per segment and concatenating would lose it.
        let at3 = result[0]
            .values
            .iter()
            .find(|(t, _)| (*t - 3.0).abs() < 1e-9)
            .expect("a rate() point at the segment boundary");
        assert!(
            (at3.1 - 15.0).abs() < 1e-9,
            "boundary rate must span segments: {at3:?}"
        );
    }

    #[test]
    fn bare_selector_splices_into_one_series() {
        // A bare vector selector resolves through the gauge path, so this
        // exercises `SegmentedSource::gauges` splicing: one timeline, all
        // samples, identical to the same rows in a single file.
        let rows_all = [
            (1_000_000_000u64, 10i64),
            (2_000_000_000, 20),
            (3_000_000_000, 35),
            (4_000_000_000, 50),
        ];
        let single = vec![segment_gauge("queue_depth", &rows_all)];
        let split = vec![
            segment_gauge("queue_depth", &rows_all[..2]),
            segment_gauge("queue_depth", &rows_all[2..]),
        ];
        let pool = BufferPool::new(64 * 1024 * 1024);
        let a = SegmentedParquetReader::open_bytes_with_pool(single, Arc::clone(&pool)).unwrap();
        let b = SegmentedParquetReader::open_bytes_with_pool(split, pool).unwrap();
        let qa = a.query_range("queue_depth", 1.0, 4.0, 1.0).unwrap();
        let qb = b.query_range("queue_depth", 1.0, 4.0, 1.0).unwrap();
        assert_eq!(format!("{qa:?}"), format!("{qb:?}"));
        let QueryResult::Matrix { result } = qb else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1, "one spliced series, not one per segment");
        assert_eq!(result[0].values.len(), 4);
        assert_eq!(result[0].values.last().unwrap().1, 50.0);
    }

    #[test]
    fn query_range_opts_raw_mode_splices_segments_like_a_single_file() {
        use crate::{QueryOptions, RateMode};

        // Deliberately jittered raw timestamps: Raw mode emits at actual
        // sample times, so a splice bug shows up as different point placement.
        let rows_all = [
            (1_010_000_000u64, 10u64),
            (2_030_000_000, 20),
            (2_990_000_000, 35),
            (4_020_000_000, 50),
        ];
        let single = vec![segment("cpu_cycles", &[], &rows_all)];
        let split = vec![
            segment("cpu_cycles", &[], &rows_all[..2]),
            segment("cpu_cycles", &[], &rows_all[2..]),
        ];
        let pool = BufferPool::new(64 * 1024 * 1024);
        let a = SegmentedParquetReader::open_bytes_with_pool(single, Arc::clone(&pool)).unwrap();
        let b = SegmentedParquetReader::open_bytes_with_pool(split, pool).unwrap();
        let opts = QueryOptions::with_rate_mode(RateMode::Raw);
        let qa = a
            .query_range_opts("rate(cpu_cycles[2s])", 1.0, 5.0, 1.0, &opts)
            .unwrap();
        let qb = b
            .query_range_opts("rate(cpu_cycles[2s])", 1.0, 5.0, 1.0, &opts)
            .unwrap();
        assert_eq!(format!("{qa:?}"), format!("{qb:?}"));
    }

    #[test]
    fn rate_uncertainty_windows_flow_through_splice() {
        // Windowed segments: rate() must carry per-point uncertainty
        // intervals across the boundary, identical to a single file.
        let rows_all = [
            (1_000_000_000u64, 10u64, -5_000_000i64, 10_000_000u64),
            (2_000_000_000, 20, -4_000_000, 8_000_000),
            (3_000_000_000, 35, -6_000_000, 12_000_000),
            (4_000_000_000, 50, -5_000_000, 9_000_000),
        ];
        let single = vec![segment_windowed("cpu_cycles", &rows_all)];
        let split = vec![
            segment_windowed("cpu_cycles", &rows_all[..2]),
            segment_windowed("cpu_cycles", &rows_all[2..]),
        ];
        let pool = BufferPool::new(64 * 1024 * 1024);
        let a = SegmentedParquetReader::open_bytes_with_pool(single, Arc::clone(&pool)).unwrap();
        let b = SegmentedParquetReader::open_bytes_with_pool(split, pool).unwrap();
        let qa = a
            .query_range("rate(cpu_cycles[2s])", 1.0, 5.0, 1.0)
            .unwrap();
        let qb = b
            .query_range("rate(cpu_cycles[2s])", 1.0, 5.0, 1.0)
            .unwrap();
        assert_eq!(format!("{qa:?}"), format!("{qb:?}"));
        let QueryResult::Matrix { result } = qb else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1);
        let intervals = result[0]
            .intervals
            .as_ref()
            .expect("windowed segments must produce rate() uncertainty intervals");
        assert_eq!(intervals.len(), result[0].values.len());

        // The boundary point is the one that matters: its band is derived from
        // the acquisition windows of the sample at 2s (segment 0) and the one
        // at 3s (segment 1). If the splice dropped or misaligned windows this
        // would be None or degenerate.
        let idx = result[0]
            .values
            .iter()
            .position(|(t, _)| (*t - 3.0).abs() < 1e-9)
            .expect("a rate() point at the segment boundary");
        let (lo, hi) = intervals[idx];
        let v = result[0].values[idx].1;
        assert!(
            lo < v && v < hi,
            "boundary band must straddle the value: lo={lo} v={v} hi={hi}"
        );

        // Negative control: the same splice with no window sidecars carries no
        // band at all, so `is_some()` above is load-bearing.
        let plain = vec![
            segment(
                "cpu_cycles",
                &[],
                &[(1_000_000_000, 10), (2_000_000_000, 20)],
            ),
            segment(
                "cpu_cycles",
                &[],
                &[(3_000_000_000, 35), (4_000_000_000, 50)],
            ),
        ];
        let pool = BufferPool::new(64 * 1024 * 1024);
        let c = SegmentedParquetReader::open_bytes_with_pool(plain, pool).unwrap();
        let qc = c
            .query_range("rate(cpu_cycles[2s])", 1.0, 5.0, 1.0)
            .unwrap();
        let QueryResult::Matrix { result } = qc else {
            panic!("expected matrix result");
        };
        assert!(result.iter().all(|s| s.intervals.is_none()));
    }

    #[test]
    fn column_absent_in_earlier_segment_contributes_no_samples() {
        // Segment A has only cpu_cycles; segment B has cpu_cycles + new_metric.
        // Union exposes both; querying new_metric works over B's span.
        let a = segment(
            "cpu_cycles",
            &[],
            &[(1_000_000_000, 10), (2_000_000_000, 20)],
        );
        let b = segment_two(
            "cpu_cycles",
            "new_metric",
            &[(3_000_000_000, 35, 100), (4_000_000_000, 50, 160)],
        );
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(vec![a, b], pool).unwrap();

        // `new_metric` is a gauge in segment B only; the union surfaces it
        // even though segment A's footer never mentions it.
        assert_eq!(r.counter_names(), vec!["cpu_cycles".to_string()]);
        assert_eq!(r.gauge_names(), vec!["new_metric".to_string()]);

        // new_metric only spans segment B; the query must still succeed and
        // return its samples (segment A simply contributes none).
        let q = r.query_range("new_metric", 3.0, 4.0, 1.0).unwrap();
        let QueryResult::Matrix { result } = q else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].values,
            vec![(3.0, 100.0), (4.0, 160.0)],
            "only segment B contributes samples"
        );

        // cpu_cycles still splices across both segments: the rate at the
        // boundary uses segment A's last sample and segment B's first.
        let q = r
            .query_range("rate(cpu_cycles[2s])", 1.0, 5.0, 1.0)
            .unwrap();
        let QueryResult::Matrix { result } = q else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1);
        let at3 = result[0]
            .values
            .iter()
            .find(|(t, _)| (*t - 3.0).abs() < 1e-9)
            .expect("a rate() point at the segment boundary");
        assert!((at3.1 - 15.0).abs() < 1e-9, "{at3:?}");
    }

    #[test]
    fn histogram_stream_splices_segments_like_a_single_file() {
        // Histograms take a different DataSource path (`histogram_stream`),
        // which chains per-segment streams and remaps series indices onto one
        // unified series list. Same rows, split vs single, must agree.
        let config = ::histogram::Config::new(2, 8).unwrap();
        let n = config.total_buckets();
        let row = |t: u64, count: u64| {
            let mut buckets = vec![0u64; n];
            buckets[5] = count;
            (t, buckets)
        };
        let rows_all = [
            row(1_000_000_000, 10),
            row(2_000_000_000, 20),
            row(3_000_000_000, 35),
            row(4_000_000_000, 50),
        ];
        let single = vec![segment_histogram("latency", 2, 8, &rows_all)];
        let split = vec![
            segment_histogram("latency", 2, 8, &rows_all[..2]),
            segment_histogram("latency", 2, 8, &rows_all[2..]),
        ];
        let pool = BufferPool::new(64 * 1024 * 1024);
        let a = SegmentedParquetReader::open_bytes_with_pool(single, Arc::clone(&pool)).unwrap();
        let b = SegmentedParquetReader::open_bytes_with_pool(split, pool).unwrap();

        assert_eq!(b.histogram_names(), vec!["latency".to_string()]);
        assert_eq!(b.histogram_labels("latency").len(), 1, "ONE spliced series");

        let qa = a
            .query_range("histogram_irate(latency)", 1.0, 5.0, 1.0)
            .unwrap();
        let qb = b
            .query_range("histogram_irate(latency)", 1.0, 5.0, 1.0)
            .unwrap();
        assert_eq!(format!("{qa:?}"), format!("{qb:?}"));

        // Load-bearing: the boundary point exists and is non-zero, i.e. it was
        // computed from segment 0's last row and segment 1's first row.
        let QueryResult::Matrix { result } = qb else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1);
        let at3 = result[0]
            .values
            .iter()
            .find(|(t, _)| (*t - 3.0).abs() < 1e-9)
            .expect("a histogram_irate point at the segment boundary");
        assert!(at3.1 > 0.0, "boundary point must span segments: {at3:?}");
    }

    #[test]
    fn columns_resolves_across_segments() {
        // columns("rate(new_metric[2s])") must be non-empty when new_metric
        // exists only in segment B — RezReader routing depends on this.
        let a = segment(
            "cpu_cycles",
            &[],
            &[(1_000_000_000, 10), (2_000_000_000, 20)],
        );
        let b = segment_two(
            "cpu_cycles",
            "new_metric",
            &[(3_000_000_000, 35, 100), (4_000_000_000, 50, 160)],
        );
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(vec![a, b], pool).unwrap();

        let cols = r.columns("rate(new_metric[2s])").unwrap();
        assert!(cols.contains("new_metric"), "cols: {cols:?}");

        let cols = r.columns("rate(cpu_cycles[2s])").unwrap();
        assert!(cols.contains("cpu_cycles"), "cols: {cols:?}");

        // Unknown metric parses but matches nothing.
        let cols = r.columns("rate(no_such_metric[2s])").unwrap();
        assert!(cols.is_empty());
    }

    #[test]
    fn instant_query_reads_latest_spliced_sample() {
        let a = segment_gauge("queue_depth", &[(1_000_000_000, 10), (2_000_000_000, 20)]);
        let b = segment_gauge("queue_depth", &[(3_000_000_000, 35), (4_000_000_000, 50)]);
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(vec![a, b], pool).unwrap();
        let q = r.query("queue_depth", None).unwrap();
        let QueryResult::Vector { result } = q else {
            panic!("expected vector result, got {q:?}");
        };
        assert_eq!(result.len(), 1);
        // Latest sample lives in the LAST segment.
        assert_eq!(result[0].value.1, 50.0);
    }

    #[test]
    fn sample_timestamps_concatenate_in_segment_order() {
        let a = segment(
            "cpu_cycles",
            &[],
            &[(1_000_000_007, 10), (2_000_000_003, 20)],
        );
        let b = segment("cpu_cycles", &[], &[(3_000_000_009, 35)]);
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(vec![a, b], pool).unwrap();
        // Raw (un-snapped) timestamps, segment order, no dedup/sort.
        assert_eq!(
            MetricsSource::sample_timestamps(&r),
            vec![1_000_000_007, 2_000_000_003, 3_000_000_009]
        );
    }
}
