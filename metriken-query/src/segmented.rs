//! SegmentedParquetReader: presents an ordered list of parquet byte blobs
//! (segments of one logical table) as a single MetricsSource. Open is
//! footer-only per segment; queries decode only the row groups they touch,
//! spliced in segment order. Same-identity columns across segments are ONE
//! series (unlike MultiParquetSource, which duplicates).

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::sync::Arc;

use crate::promql::QueryEngine;
use crate::{BufferPool, DataSource, MetricsSource, QueryError, QueryOptions, QueryResult};

pub use metriken_storage::segmented::*;

/// Where a segmented table's bytes come from, fetched on demand.
///
/// A reader used to be handed every segment's bytes up front and keep them
/// for its life, plus a parsed footer for each. On a long recording that is
/// the whole table resident before a query has asked for anything: measured
/// at 4.1 GB after building the dashboard of a 1.3 GB, ten-hour archive, of
/// which the segment bytes were 1.27 GB and parsed footers most of the rest.
/// With a store, open reads each segment once to build its catalog and
/// identity indexes and then lets it go; a query fetches the segments its
/// time range touches, through a bounded cache.
///
/// Segments are in logical (time) order, so `idx` is a position, not an id.
/// Reads an ordered list of parquet segments that together form one logical
/// per-sampler table, and presents them as a single [`MetricsSource`] with a
/// unioned identity surface: the same `(name, labels)` pair appearing in
/// more than one segment is ONE series, not one per segment (unlike
/// `MultiParquetSource`, which duplicates same-identity series across files).
///
/// Open reads each segment's footer once, through the [`SegmentStore`], to
/// build the identity indexes and a per-segment catalog (time span,
/// interval), and keeps neither the bytes nor the parsed footer. A query
/// fetches only the segments whose span it touches, and each of those
/// decodes only the row groups the query's time range touches; opened
/// segments are held in a cache bounded by the pool's byte budget.
///
/// Queries (`query_range` / `query` / `columns`) evaluate over a
/// [`DataSource`] that splices raw per-series samples across segments in
/// segment order, *below* PromQL evaluation — so range functions like
/// `rate()` see one continuous timeline and boundary-spanning windows are
/// computed on complete data.
pub struct SegmentedParquetReader {
    source: Arc<SegmentedSource>,
    /// PromQL engine over the splicing [`SegmentedSource`].
    engine: QueryEngine,
}

impl SegmentedParquetReader {
    /// A reader over an already-open segmented source, such as one table of
    /// an archive.
    pub fn from_source(source: Arc<SegmentedSource>) -> Self {
        let engine = QueryEngine::new(Arc::clone(&source) as Arc<dyn DataSource>);
        Self { source, engine }
    }

    /// Open `segments` (raw parquet bytes, in logical/time order) that are
    /// already in memory. The bytes stay resident for the reader's life; see
    /// [`open_with_pool`](Self::open_with_pool) for the on-demand form.
    pub fn open_bytes_with_pool(
        segments: Vec<Vec<u8>>,
        pool: Arc<BufferPool>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::open_with_pool(Arc::new(InMemorySegments::new(segments)), pool)
    }

    /// Open a table whose segments `store` supplies on demand.
    ///
    /// Reads every segment once here, footer-only, and keeps only what
    /// answers questions without the bytes: which series exist (per metric
    /// kind), each segment's time span and interval, the histogram run
    /// index, the merged file metadata and the column map. Nothing is
    /// decoded. The cache of opened segments is bounded by
    /// `pool.max_bytes()`, counted in segment bytes; it always holds at
    /// least the segment most recently opened.
    pub fn open_with_pool(
        store: Arc<dyn SegmentStore>,
        pool: Arc<BufferPool>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::open(store, pool, None, None, false)
    }

    /// [`open_with_pool`](Self::open_with_pool) with a [`ColumnRelabel`]:
    /// the identity indexes are built from what each column can present as,
    /// and every query cuts its samples by occupant before splicing.
    pub fn open_relabeled_with_pool(
        store: Arc<dyn SegmentStore>,
        pool: Arc<BufferPool>,
        relabel: Arc<dyn ColumnRelabel>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::open(store, pool, Some(relabel), None, false)
    }

    /// [`open_with_pool`](Self::open_with_pool) or, with a relabel,
    /// [`open_relabeled_with_pool`](Self::open_relabeled_with_pool), for a
    /// table that is reopened as new rows arrive. The reader saves the state
    /// a later reader of the table can start from (see
    /// [`handover`](Self::handover)), and starts from `previous`, the
    /// handover of an earlier reader of the table, when there is one.
    ///
    /// The leading segments whose [`SegmentStore::key`] matches the earlier
    /// reader's are not read again: the new reader starts from the identity
    /// indexes and catalog the earlier one had after them, and takes over
    /// the segments it had open when both use the same `pool`. The rest are
    /// read as [`open_with_pool`](Self::open_with_pool) reads them. Reuse
    /// needs a relabel whose identities are fixed
    /// ([`ColumnRelabel::identities_are_fixed`]) or none, and the earlier
    /// reader opened with a relabel exactly when this open has one.
    ///
    /// A handover from another table, or a store whose keys repeat for
    /// different bytes, gives wrong answers without an error.
    pub fn open_after(
        store: Arc<dyn SegmentStore>,
        pool: Arc<BufferPool>,
        relabel: Option<Arc<dyn ColumnRelabel>>,
        previous: Option<&Handover>,
    ) -> Result<Self, Box<dyn Error>> {
        Self::open(store, pool, relabel, previous, true)
    }

    /// What a reader opened after this one over the same table can start
    /// from, with [`open_after`](Self::open_after): the open state after
    /// the leading keyed segments, and the segments among them this reader
    /// has open. `None` when this reader saved no such state: it was not
    /// opened with [`open_after`](Self::open_after), its store keys no
    /// segment, its relabel's identities are not fixed, or a column named an
    /// occupant the relabel did not describe. A handover holds no
    /// [`SegmentStore`].
    pub fn handover(&self) -> Option<Handover> {
        self.source.handover()
    }

    fn open(
        store: Arc<dyn SegmentStore>,
        pool: Arc<BufferPool>,
        relabel: Option<Arc<dyn ColumnRelabel>>,
        previous: Option<&Handover>,
        keep: bool,
    ) -> Result<Self, Box<dyn Error>> {
        let source = SegmentedSource::open(store, pool, relabel, previous, keep)?;
        let engine = QueryEngine::new(Arc::clone(&source) as Arc<dyn DataSource>);
        Ok(Self { source, engine })
    }

    /// Number of segments backing this reader, gone ones included.
    pub fn segment_count(&self) -> usize {
        self.source.segment_count()
    }

    /// The reader's spliced [`DataSource`] (the same [`SegmentedSource`]
    /// queries evaluate against), for composition by
    /// [`crate::UnionMetricsSource`] — mirrors
    /// [`crate::ParquetReader::data_source`], the equivalent accessor for a
    /// single-segment table.
    pub(crate) fn data_source(&self) -> Arc<dyn DataSource> {
        self.engine.data_source()
    }

    // Introspection routes through `self.engine` — the same `QueryEngine` the
    // queries use, over the same [`SegmentedSource`]. `ParquetReader` does the
    // same (see `parquet.rs`). Re-deriving the union here from the store
    // would be a second implementation of the same semantics that nothing
    // keeps in step with the one queries actually see.

    /// Names of all counter metrics across every segment (sorted, deduplicated union).
    pub fn counter_names(&self) -> Vec<String> {
        self.engine.counter_names()
    }

    /// Names of all gauge metrics across every segment (sorted, deduplicated union).
    pub fn gauge_names(&self) -> Vec<String> {
        self.engine.gauge_names()
    }

    /// Names of all histogram metrics across every segment (sorted, deduplicated union).
    pub fn histogram_names(&self) -> Vec<String> {
        self.engine.histogram_names()
    }

    /// All label combinations for the named counter metric, unioned (and
    /// deduplicated) across every segment.
    pub fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.engine.counter_labels(name)
    }

    /// All label combinations for the named gauge metric, unioned (and
    /// deduplicated) across every segment.
    pub fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.engine.gauge_labels(name)
    }

    /// All label combinations for the named histogram metric, unioned (and
    /// deduplicated) across every segment.
    pub fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.engine.histogram_labels(name)
    }

    /// Full time extent across all segments in nanoseconds, or `None` if empty.
    pub fn time_range_ns(&self) -> Option<(u64, u64)> {
        self.engine.time_range()
    }

    /// Full time extent across all segments in seconds, or `None` if empty.
    pub fn time_range(&self) -> Option<(f64, f64)> {
        self.time_range_ns()
            .map(|(lo, hi)| (lo as f64 / 1e9, hi as f64 / 1e9))
    }

    /// Sampling interval in seconds; the finest across all segments.
    pub fn interval(&self) -> f64 {
        self.engine.interval()
    }

    /// Key-value metadata merged across all segment footers (last segment,
    /// in store order, wins on key collision).
    pub fn file_metadata(&self) -> HashMap<String, String> {
        self.engine.file_metadata()
    }

    /// Look up a single metadata value by key without cloning the full map.
    /// Last segment wins on collision (matches [`file_metadata`](Self::file_metadata)).
    pub fn metadata_get(&self, key: &str) -> Option<String> {
        self.engine.metadata_get(key)
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

    /// How many segments the cache currently holds open, and their bytes.
    /// For tests and diagnostics.
    pub fn cached_segments(&self) -> (usize, usize) {
        self.source.cached_segments()
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
            .query_range_opts(expr, start_s, end_s, step_s, opts)
    }

    fn query_range_display_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &crate::DisplayOptions,
        qopts: &QueryOptions,
    ) -> Result<crate::DisplayResult, QueryError> {
        self.engine
            .query_range_display_opts(expr, start_s, end_s, step_s, opts, qopts)
    }

    fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        self.engine.query(expr, time)
    }

    fn columns(&self, query: &str) -> Result<std::collections::HashSet<String>, QueryError> {
        self.engine.columns(query)
    }

    fn sample_timestamps(&self) -> Vec<u64> {
        DataSource::sample_timestamps(&*self.source)
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
    use crate::labels::Labels;
    use bytes::Bytes;
    use std::sync::{Arc, Mutex};

    use crate::ParquetReader;
    use metriken_storage::parquet::MultiParquetSource;

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

    /// Multi-series variant of [`segment`]: one counter `name` carried by
    /// several label-differentiated columns sharing one `timestamp` column.
    /// `series` is `(label_value, values_aligned_to_ts)`, and its ORDER is the
    /// schema order — which is the order `parse_schema` (and therefore
    /// `read_counters`) yields the series in. Column names are unique but
    /// arbitrary; identity comes from the `metric` + label field metadata.
    fn segment_labeled(name: &str, key: &str, ts: &[u64], series: &[(&str, Vec<u64>)]) -> Vec<u8> {
        let mut fields = vec![Field::new("timestamp", DataType::UInt64, false)];
        for (i, (value, values)) in series.iter().enumerate() {
            assert_eq!(values.len(), ts.len(), "series must align to timestamps");
            let mut meta = HashMap::new();
            meta.insert("metric".to_string(), name.to_string());
            meta.insert("metric_type".to_string(), "counter".to_string());
            meta.insert(key.to_string(), value.to_string());
            fields.push(
                Field::new(format!("{name}__{i}"), DataType::UInt64, true).with_metadata(meta),
            );
        }
        let schema = Arc::new(Schema::new(fields));

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

        let mut columns: Vec<ArrayRef> = vec![Arc::new(UInt64Array::from(ts.to_vec())) as ArrayRef];
        for (_, values) in series {
            columns.push(Arc::new(UInt64Array::from(values.clone())) as ArrayRef);
        }
        let batch = RecordBatch::try_new(schema, columns).unwrap();
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
        segment_histogram_labeled(name, grouping_power, max_value_power, &[], rows)
    }

    /// [`segment_histogram`] with labels on the column.
    fn segment_histogram_labeled(
        name: &str,
        grouping_power: u8,
        max_value_power: u8,
        labels: &[(&str, &str)],
        rows: &[(u64, Vec<u64>)],
    ) -> Vec<u8> {
        let rows: Vec<(u64, Option<Vec<u64>>)> =
            rows.iter().map(|(t, b)| (*t, Some(b.clone()))).collect();
        segment_histogram_nullable(name, grouping_power, max_value_power, labels, &rows)
    }

    /// [`segment_histogram_labeled`] where a row's cell may be null: a
    /// member absent that tick, as a wide table stores it.
    fn segment_histogram_nullable(
        name: &str,
        grouping_power: u8,
        max_value_power: u8,
        labels: &[(&str, &str)],
        rows: &[(u64, Option<Vec<u64>>)],
    ) -> Vec<u8> {
        use arrow::array::ListArray;
        use arrow::buffer::NullBuffer;
        use arrow::buffer::OffsetBuffer;

        let mut meta = HashMap::new();
        meta.insert("metric".to_string(), name.to_string());
        meta.insert("metric_type".to_string(), "histogram".to_string());
        meta.insert("grouping_power".to_string(), grouping_power.to_string());
        meta.insert("max_value_power".to_string(), max_value_power.to_string());
        for (k, v) in labels {
            meta.insert(k.to_string(), v.to_string());
        }

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
            flat.extend(buckets.iter().flatten());
            offsets.push(flat.len() as i32);
        }
        let valid = NullBuffer::from_iter(rows.iter().map(|(_, b)| b.is_some()));
        let list = ListArray::new(
            item,
            OffsetBuffer::new(offsets.into()),
            Arc::new(UInt64Array::from(flat)),
            Some(valid),
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

    /// Histogram variant carrying the acquisition-window sidecars exactly the
    /// way the `.rez` writer emits them (`src/recorder/rez.rs`): the value
    /// column is `<name>:buckets` but the sidecars are named after the METRIC
    /// (`<name>:window_begin` / `<name>:window_width`), not after the bucket
    /// column. Used to pin that those sidecars stay unattached — see
    /// [`counter_to_histogram_flip_splits_series`].
    fn segment_histogram_windowed(
        name: &str,
        grouping_power: u8,
        max_value_power: u8,
        rows: &[(u64, Vec<u64>)],
    ) -> Vec<u8> {
        use arrow::array::{Int64Array, ListArray};
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
        let begins: Vec<i64> = rows.iter().map(|_| -5_000_000i64).collect();
        let widths: Vec<u64> = rows.iter().map(|_| 10_000_000u64).collect();
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt64Array::from(ts)) as ArrayRef,
                Arc::new(list) as ArrayRef,
                Arc::new(Int64Array::from(begins)) as ArrayRef,
                Arc::new(UInt64Array::from(widths)) as ArrayRef,
            ],
        )
        .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        buf
    }

    /// One segment carrying TWO histogram columns for the SAME metric name
    /// under DIFFERENT configs (label-differentiated). The `.rez` writer cannot
    /// produce this (one column per metric id per table), but the reader must
    /// not decode both under one config — see
    /// [`open_rejects_unsplicable_histogram_within_one_segment`].
    fn segment_two_histograms(name: &str, a: (u8, u8), b: (u8, u8), ts: u64) -> Vec<u8> {
        use arrow::array::ListArray;
        use arrow::buffer::OffsetBuffer;

        let field = |col: &str, core: &str, (gp, mvp): (u8, u8), item: Arc<Field>| {
            let mut meta = HashMap::new();
            meta.insert("metric".to_string(), name.to_string());
            meta.insert("metric_type".to_string(), "histogram".to_string());
            meta.insert("grouping_power".to_string(), gp.to_string());
            meta.insert("max_value_power".to_string(), mvp.to_string());
            meta.insert("core".to_string(), core.to_string());
            Field::new(col.to_string(), DataType::List(item), true).with_metadata(meta)
        };

        let item = Arc::new(Field::new("item", DataType::UInt64, true));
        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            field(&format!("{name}:buckets"), "0", a, item.clone()),
            field(&format!("{name}__1:buckets"), "1", b, item.clone()),
        ]));

        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_key_value_metadata(Some(vec![KeyValue {
                key: "sampling_interval_ms".to_string(),
                value: Some("1000".to_string()),
            }]))
            .build();

        let list = |(gp, mvp): (u8, u8)| {
            let n = ::histogram::Config::new(gp, mvp).unwrap().total_buckets();
            let mut buckets = vec![0u64; n];
            buckets[5] = 1;
            ListArray::new(
                item.clone(),
                OffsetBuffer::new(vec![0i32, n as i32].into()),
                Arc::new(UInt64Array::from(buckets)),
                None,
            )
        };

        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt64Array::from(vec![ts])) as ArrayRef,
                Arc::new(list(a)) as ArrayRef,
                Arc::new(list(b)) as ArrayRef,
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

    /// Table-level variant of [`segment_windowed`]: TWO counters sharing one
    /// bare `:window_begin`/`:window_width` pair (no per-metric sidecars),
    /// with rows `(ts, value_a, value_b, begin_offset, width)`.
    fn segment_table_windowed(rows: &[(u64, u64, u64, i64, u64)]) -> Vec<u8> {
        use arrow::array::Int64Array;

        let counter_meta = |name: &str| {
            let mut meta = HashMap::new();
            meta.insert("metric".to_string(), name.to_string());
            meta.insert("metric_type".to_string(), "counter".to_string());
            meta
        };

        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new("cpu_cycles", DataType::UInt64, true)
                .with_metadata(counter_meta("cpu_cycles")),
            Field::new("cpu_instructions", DataType::UInt64, true)
                .with_metadata(counter_meta("cpu_instructions")),
            Field::new(":window_begin", DataType::Int64, true),
            Field::new(":window_width", DataType::UInt64, true),
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
        let a_vals: Vec<u64> = rows.iter().map(|(_, a, ..)| *a).collect();
        let b_vals: Vec<u64> = rows.iter().map(|(_, _, b, _, _)| *b).collect();
        let begins: Vec<i64> = rows.iter().map(|(_, _, _, b, _)| *b).collect();
        let widths: Vec<u64> = rows.iter().map(|(_, _, _, _, w)| *w).collect();
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt64Array::from(ts)) as ArrayRef,
                Arc::new(UInt64Array::from(a_vals)) as ArrayRef,
                Arc::new(UInt64Array::from(b_vals)) as ArrayRef,
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

    /// A query whose range ends at the last sample's time, given in `f64`
    /// seconds, still sees that sample. The timestamps are a real recording's:
    /// `1790832064.022395` seconds converts back to 120 ns before the last
    /// sample, which dropped it, and with it the grid point at 064.0 that it
    /// brackets; `rate()` then returned nothing at a 1 s step.
    #[test]
    fn a_range_ending_at_the_last_sample_includes_it() {
        use crate::MetricsSource;
        let first: u64 = 1_790_832_062_063_539_000;
        let last: u64 = 1_790_832_064_022_395_000;
        let mut rows: Vec<(u64, u64)> =
            (0..19).map(|k| (first + k * 103_000_000, 10 * k)).collect();
        rows.push((last, 190));
        let round_trip = ((last as f64 / 1e9) * 1e9) as u64;
        assert!(round_trip < last, "the fixture reproduces the rounding");
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(
            vec![segment("cpu_cycles", &[], &rows)],
            pool,
        )
        .unwrap();
        let result = r
            .query_range(
                "rate(cpu_cycles[1s])",
                first as f64 / 1e9,
                last as f64 / 1e9,
                1.0,
            )
            .unwrap();
        let crate::QueryResult::Matrix { result } = result else {
            panic!("a range query gives a matrix");
        };
        let times: Vec<f64> = result[0].values.iter().map(|(t, _)| *t).collect();
        assert!(
            times.contains(&1_790_832_064.0),
            "the point at 064.0 is emitted: {times:?}"
        );
    }

    /// A histogram query whose range starts at the first sample's time, given
    /// in `f64` seconds, still reads that sample. `1790832062.002000539`
    /// seconds converts back to 101 ns after the sample; without the read
    /// slack the sample was left out.
    #[test]
    fn a_histogram_range_starting_at_the_first_sample_includes_it() {
        use crate::MetricsSource;
        let first: u64 = 1_790_832_062_002_000_539;
        assert!(
            ((first as f64 / 1e9) * 1e9).round() as u64 > first,
            "the fixture reproduces the rounding"
        );
        let rows: Vec<(u64, Vec<u64>)> = (0..3u64)
            .map(|k| (first + k * 1_000_000_000, vec![(k + 1) * 10; 16]))
            .collect();
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(
            vec![segment_histogram("latency", 2, 4, &rows)],
            pool,
        )
        .unwrap();
        let query = |start: u64| {
            r.query_range(
                "histogram_irate(latency)",
                start as f64 / 1e9,
                (first + 2_000_000_000) as f64 / 1e9,
                1.0,
            )
            .map(|res| format!("{res:?}"))
            .map_err(|e| e.to_string())
        };
        assert_eq!(
            query(first),
            query(first - 500_000_000),
            "a range starting at the first sample reads it, as one starting earlier does"
        );
    }

    /// A reader reopened over the same segments, on the same pool, reads the
    /// blocks the first one decoded: the blocks are keyed by the segment's
    /// bytes, not by which open read them. Segments with different bytes
    /// share nothing.
    #[test]
    fn a_reopened_reader_reuses_the_pools_decoded_blocks() {
        use crate::MetricsSource;
        let segs = || {
            vec![
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
            ]
        };
        // The per-series path reads through the pool; a batch read does not.
        let opts = crate::QueryOptions::default().with_per_series_rates(true);
        let query = |r: &SegmentedParquetReader| {
            r.query_range_opts("rate(cpu_cycles[2s])", 1.0, 4.0, 1.0, &opts)
                .unwrap();
        };
        let pool = BufferPool::new(64 * 1024 * 1024);

        let first =
            SegmentedParquetReader::open_bytes_with_pool(segs(), Arc::clone(&pool)).unwrap();
        query(&first);
        let after_first = pool.stats();
        assert!(after_first.misses > 0, "the first reader decodes");

        let second =
            SegmentedParquetReader::open_bytes_with_pool(segs(), Arc::clone(&pool)).unwrap();
        query(&second);
        let after_second = pool.stats();
        assert_eq!(
            after_second.misses, after_first.misses,
            "the reopened reader decodes nothing new"
        );
        assert!(after_second.hits > after_first.hits);

        let other = SegmentedParquetReader::open_bytes_with_pool(
            vec![segment(
                "cpu_cycles",
                &[],
                &[(1_000_000_000, 11), (2_000_000_000, 21)],
            )],
            Arc::clone(&pool),
        )
        .unwrap();
        query(&other);
        assert!(
            pool.stats().misses > after_second.misses,
            "different bytes are decoded, not served from the first segments' blocks"
        );
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
        // still clear of the 1s/2s segments. Pool entries count decodes on the
        // per-series path, which is the one read through the pool.
        let opts = crate::QueryOptions::default().with_per_series_rates(true);
        let _ = r.query_range_opts("rate(cpu_cycles[1s])", 9.0, 10.0, 1.0, &opts);
        let narrow = narrow_pool.stats();

        let wide_pool = BufferPool::new(64 * 1024 * 1024);
        let r =
            SegmentedParquetReader::open_bytes_with_pool(segs(), Arc::clone(&wide_pool)).unwrap();
        let _ = r.query_range_opts("rate(cpu_cycles[1s])", 1.0, 10.0, 1.0, &opts);
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

    /// Table-level windows (bare `:window_begin`/`:window_width`, shared by
    /// every metric in the table) must splice across segments exactly like
    /// per-metric sidecars do: identical output to a single unsplit table,
    /// and both metrics sharing the table get a boundary-spanning band.
    #[test]
    fn table_level_windows_splice_across_segments() {
        let rows_all = [
            (
                1_000_000_000u64,
                10u64,
                100u64,
                -5_000_000i64,
                10_000_000u64,
            ),
            (2_000_000_000, 20, 200, -4_000_000, 8_000_000),
            (3_000_000_000, 35, 350, -6_000_000, 12_000_000),
            (4_000_000_000, 50, 500, -5_000_000, 9_000_000),
        ];
        let single = vec![segment_table_windowed(&rows_all)];
        let split = vec![
            segment_table_windowed(&rows_all[..2]),
            segment_table_windowed(&rows_all[2..]),
        ];
        let pool = BufferPool::new(64 * 1024 * 1024);
        let a = SegmentedParquetReader::open_bytes_with_pool(single, Arc::clone(&pool)).unwrap();
        let b = SegmentedParquetReader::open_bytes_with_pool(split, pool).unwrap();

        // The cross-segment identity index (built from `counter_columns`,
        // which reuses `parse_schema`) must not surface the bare
        // `:window_begin`/`:window_width` pair as phantom series.
        assert_eq!(
            b.counter_names(),
            vec!["cpu_cycles".to_string(), "cpu_instructions".to_string()],
            "table-level window columns must not become phantom series in the \
             segmented identity index: {:?}",
            b.counter_names()
        );

        for expr in ["rate(cpu_cycles[2s])", "rate(cpu_instructions[2s])"] {
            let qa = a.query_range(expr, 1.0, 5.0, 1.0).unwrap();
            let qb = b.query_range(expr, 1.0, 5.0, 1.0).unwrap();
            assert_eq!(
                format!("{qa:?}"),
                format!("{qb:?}"),
                "split-segment table-level windows must match a single unsplit table for {expr}"
            );
            let QueryResult::Matrix { result } = qb else {
                panic!("expected matrix result");
            };
            assert_eq!(result.len(), 1, "one spliced series, not one per segment");

            let intervals = result[0]
                .intervals
                .as_ref()
                .expect("table-level windows must produce rate() uncertainty intervals");
            let idx = result[0]
                .values
                .iter()
                .position(|(t, _)| (*t - 3.0).abs() < 1e-9)
                .expect("a rate() point at the segment boundary");
            let (lo, hi) = intervals[idx];
            let v = result[0].values[idx].1;
            assert!(
                lo < v && v < hi,
                "boundary band (from the table-level window) must straddle the value \
                 for {expr}: lo={lo} v={v} hi={hi}"
            );
        }
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
    fn two_label_sets_splice_independently_across_three_segments() {
        // Every other splice test uses ONE series, so the identity-matching
        // loop in `splice_counters` is barely exercised. Two label sets across
        // three segments pin down both halves of the contract:
        //   - each label set accumulates its OWN samples (no cross-talk), and
        //   - "first appearance fixes series order" (segmented.rs docs).
        //
        // Segment 1 deliberately lists the columns in the OPPOSITE schema
        // order, so a splice that matched positionally instead of by label
        // would swap core=1's samples onto core=0. Segment 2 restores the
        // original order.
        let s0 = segment_labeled(
            "cpu_cycles",
            "core",
            &[1_000_000_000, 2_000_000_000],
            &[("0", vec![10, 20]), ("1", vec![100, 200])],
        );
        let s1 = segment_labeled(
            "cpu_cycles",
            "core",
            &[3_000_000_000, 4_000_000_000],
            &[("1", vec![300, 400]), ("0", vec![30, 40])],
        );
        let s2 = segment_labeled(
            "cpu_cycles",
            "core",
            &[5_000_000_000],
            &[("0", vec![50]), ("1", vec![500])],
        );
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(vec![s0, s1, s2], pool).unwrap();

        assert_eq!(r.counter_labels("cpu_cycles").len(), 2);

        // irate in Raw mode reports pairwise deltas at the real sample times,
        // so each point is directly attributable to two adjacent raw samples.
        let opts = QueryOptions::with_rate_mode(crate::RateMode::Raw);
        let q = r
            .query_range_opts("irate(cpu_cycles[1s])", 1.0, 5.0, 1.0, &opts)
            .unwrap();
        let QueryResult::Matrix { result } = q else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 2, "two label sets must stay two series");

        // Order: core=0 appeared first in segment 0, so it stays first even
        // though segment 1 lists core=1 first.
        assert_eq!(result[0].metric.get("core").map(String::as_str), Some("0"));
        assert_eq!(result[1].metric.get("core").map(String::as_str), Some("1"));

        // core=0 climbs by 10 per second across every boundary; core=1 by 100.
        // A cross-talk bug (samples landing on the wrong series) would show up
        // here as a huge spike at a segment boundary.
        let values = |i: usize| -> Vec<f64> { result[i].values.iter().map(|(_, v)| *v).collect() };
        assert_eq!(values(0), vec![10.0, 10.0, 10.0, 10.0]);
        assert_eq!(values(1), vec![100.0, 100.0, 100.0, 100.0]);
    }

    #[test]
    fn mixed_window_coverage_drops_the_band_for_that_series() {
        // Windows policy (`splice_counters` / `splice_gauges`): per-point
        // acquisition windows concatenate only when EVERY contributing segment
        // carries them. A series covered by a windowed segment and a plain one
        // drops to `None` rather than emit a band over misaligned windows.
        let early_windowed = || {
            segment_windowed(
                "cpu_cycles",
                &[
                    (1_000_000_000, 10, -5_000_000, 10_000_000),
                    (2_000_000_000, 20, -4_000_000, 8_000_000),
                ],
            )
        };
        let late_windowed = || {
            segment_windowed(
                "cpu_cycles",
                &[
                    (3_000_000_000, 35, -6_000_000, 12_000_000),
                    (4_000_000_000, 50, -5_000_000, 9_000_000),
                ],
            )
        };
        let early_plain = || {
            segment(
                "cpu_cycles",
                &[],
                &[(1_000_000_000, 10), (2_000_000_000, 20)],
            )
        };
        let late_plain = || {
            segment(
                "cpu_cycles",
                &[],
                &[(3_000_000_000, 35), (4_000_000_000, 50)],
            )
        };

        // Both mixing directions. Segments always stay in TIME order (that is
        // the type's contract); what varies is which one carries the windows,
        // so this covers both the (Some, None) and (None, Some) arms of the
        // `_ => None` match.
        for segments in [
            vec![early_windowed(), late_plain()],
            vec![early_plain(), late_windowed()],
        ] {
            let pool = BufferPool::new(64 * 1024 * 1024);
            let r = SegmentedParquetReader::open_bytes_with_pool(segments, pool).unwrap();
            let q = r
                .query_range("rate(cpu_cycles[2s])", 1.0, 5.0, 1.0)
                .unwrap();
            let QueryResult::Matrix { result } = q else {
                panic!("expected matrix result");
            };
            assert_eq!(result.len(), 1);
            assert!(
                result[0].intervals.is_none(),
                "a series with mixed window coverage must carry no band"
            );
        }

        // NOTE: this end-to-end assertion pins the user-visible outcome, but it
        // is NOT a tight probe of the policy branch: `collect_to_matrix` only
        // emits `intervals` when EVERY point carries a band, so a splice that
        // wrongly kept one segment's windows (leaving them short and
        // misattributed) would also surface as `None` here. The branch itself
        // is pinned directly by `splice_window_policy_drops_mixed_coverage`.

        // Control: all-windowed segments DO produce a band, so the assertion
        // above is about mixing, not about windows never surviving a splice.
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(
            vec![early_windowed(), late_windowed()],
            pool,
        )
        .unwrap();
        let q = r
            .query_range("rate(cpu_cycles[2s])", 1.0, 5.0, 1.0)
            .unwrap();
        let QueryResult::Matrix { result } = q else {
            panic!("expected matrix result");
        };
        assert!(result[0].intervals.is_some());
    }

    // ─── Cross-segment identity conflicts ────────────────────────────────────
    //
    // Column names in `.rez` tables are the snapshot's numeric-id names ("5",
    // "5x3") and those ids are per-agent-process: an agent restart mid-recording
    // remaps id → metric arbitrarily, so the SAME name can carry a DIFFERENT
    // metric in a later segment. Identity here is (name, value shape, and for
    // histograms the H2 powers); on conflict the runs stay DISTINCT series —
    // never a hard error, never a silent coercion. (These fixtures use
    // `id_5`-style names so PromQL doesn't parse the selector as a number; the
    // policy is about the name, not its spelling.)

    #[test]
    fn type_flip_across_segments_splits_series() {
        // "id_5" is a counter in segment A and a gauge in segment B.
        let a = segment("id_5", &[], &[(1_000_000_000, 10), (2_000_000_000, 20)]);
        let b = segment_gauge("id_5", &[(3_000_000_000, 35), (4_000_000_000, 50)]);
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(vec![a, b], pool).unwrap();

        // Both runs survive the union, each under its own value shape.
        assert_eq!(r.counter_names(), vec!["id_5".to_string()]);
        assert_eq!(r.gauge_names(), vec!["id_5".to_string()]);

        // The counter run reads as a counter: rate over segment A only.
        let q = r.query_range("rate(id_5[2s])", 1.0, 5.0, 1.0).unwrap();
        let QueryResult::Matrix { result } = q else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1);
        let at2 = result[0]
            .values
            .iter()
            .find(|(t, _)| (*t - 2.0).abs() < 1e-9)
            .expect("a rate() point inside the counter run");
        assert!((at2.1 - 10.0).abs() < 1e-9, "{at2:?}");
        // …and it does NOT run past the flip: segment B's 35/50 are a different
        // metric, so counting them as the same counter would show a jump here.
        assert!(
            result[0].values.iter().all(|(t, _)| *t <= 2.0),
            "the gauge run must not be spliced onto the counter: {:?}",
            result[0].values
        );

        // The gauge run reads as a gauge, with its own values.
        let q = r.query_range("id_5", 3.0, 4.0, 1.0).unwrap();
        let QueryResult::Matrix { result } = q else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].values, vec![(3.0, 35.0), (4.0, 50.0)]);
    }

    #[test]
    fn a_null_histogram_cell_is_no_sample() {
        // A member that is absent for the first two rows of a segment, then
        // reads 5, 6 and 8. Its first reading has nothing before it, so the
        // answer is the same as when the segment starts at that reading.
        let n = ::histogram::Config::new(2, 8).unwrap().total_buckets();
        let cell = |count: u64| {
            let mut buckets = vec![0u64; n];
            buckets[5] = count;
            Some(buckets)
        };
        let deltas = |rows: &[(u64, Option<Vec<u64>>)]| {
            let seg = segment_histogram_nullable("latency", 2, 8, &[], rows);
            let pool = BufferPool::new(64 * 1024 * 1024);
            let r = SegmentedParquetReader::open_bytes_with_pool(vec![seg], pool).unwrap();
            let QueryResult::Matrix { result } = r
                .query_range("histogram_irate(latency)", 1.0, 5.0, 1.0)
                .unwrap()
            else {
                panic!("not a matrix");
            };
            result[0].values.clone()
        };
        let with_nulls = deltas(&[
            (1_000_000_000, None),
            (2_000_000_000, None),
            (3_000_000_000, cell(5)),
            (4_000_000_000, cell(6)),
            (5_000_000_000, cell(8)),
        ]);
        let without = deltas(&[
            (3_000_000_000, cell(5)),
            (4_000_000_000, cell(6)),
            (5_000_000_000, cell(8)),
        ]);
        assert_eq!(with_nulls, without);
    }

    #[test]
    fn counter_to_histogram_flip_splits_series() {
        // Same name arrives as a counter (with acquisition-window sidecars) in
        // segment A and as a histogram in segment B. Segment B is written the
        // way the `.rez` writer writes histograms: value column `id_5:buckets`,
        // sidecars named after the METRIC (`id_5:window_begin`) — the shape the
        // design flags as the silent-success case for a naive schema union.
        let n = ::histogram::Config::new(2, 8).unwrap().total_buckets();
        let hrow = |t: u64, count: u64| {
            let mut buckets = vec![0u64; n];
            buckets[5] = count;
            (t, buckets)
        };
        let a = segment_windowed(
            "id_5",
            &[
                (1_000_000_000, 10, -5_000_000, 10_000_000),
                (2_000_000_000, 20, -4_000_000, 8_000_000),
            ],
        );
        // `histogram_irate` needs two deltas (three samples) before it emits
        // anything — its first observed delta is always null, exactly like
        // counter irate/rate needing a prior sample (see
        // `test_histogram_irate_first_step_is_null` in promql/tests.rs).
        // Two rows alone would make this probe vacuous regardless of the
        // conflict policy, so segment B carries three.
        let b = segment_histogram_windowed(
            "id_5",
            2,
            8,
            &[
                hrow(3_000_000_000, 10),
                hrow(4_000_000_000, 20),
                hrow(5_000_000_000, 40),
            ],
        );
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(vec![a, b], pool).unwrap();

        // Both runs survive, each under its own value shape.
        assert_eq!(r.counter_names(), vec!["id_5".to_string()]);
        assert_eq!(r.histogram_names(), vec!["id_5".to_string()]);
        // Segment B's `id_5:window_begin` is an Int64 column; if the reserved
        // suffix were not honoured it would surface as a phantom gauge here and
        // its offsets would read as metric values.
        assert!(r.gauge_names().is_empty(), "{:?}", r.gauge_names());

        // The counter run keeps ITS windows (the histogram segment contributes
        // no counter samples, so coverage is uniform and the band survives).
        let q = r.query_range("rate(id_5[2s])", 1.0, 5.0, 1.0).unwrap();
        let QueryResult::Matrix { result } = q else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1);
        assert!(
            result[0].intervals.is_some(),
            "the counter run's own :window_* sidecars must still produce a band"
        );
        assert!(result[0].values.iter().all(|(t, _)| *t <= 2.0));

        // The histogram run decodes as a histogram over segment B.
        let q = r
            .query_range("histogram_irate(id_5)", 3.0, 5.0, 1.0)
            .unwrap();
        let QueryResult::Matrix { result } = q else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1);
        assert!(result[0].values.iter().any(|(_, v)| *v > 0.0));
    }

    /// The parquet loader's twin of `ingest_does_not_turn_histogram_configuration_into_labels`:
    /// a histogram column whose field metadata carries the storage keys yields
    /// a label set with none of them. Both loaders derive labels from
    /// `Labels::from_metadata`, so this and its twin are what notice a
    /// pre-filter added to one loader and not the other — the shape the
    /// drift had before the shared list existed.
    #[test]
    fn parquet_does_not_turn_histogram_configuration_into_labels() {
        let n = ::histogram::Config::new(3, 8).unwrap().total_buckets();
        let seg = segment_histogram("latency", 3, 8, &[(1_000_000_000, vec![1; n])]);
        let pool = BufferPool::new(64 * 1024 * 1024);

        let single = ParquetReader::open_bytes_with_pool(seg.clone(), Arc::clone(&pool)).unwrap();
        let labels = single.histogram_labels("latency");
        assert_eq!(
            labels,
            vec![BTreeMap::new()],
            "single-file reader: {labels:?}"
        );

        let segmented =
            SegmentedParquetReader::open_bytes_with_pool(vec![seg], Arc::clone(&pool)).unwrap();
        let labels = segmented.histogram_labels("latency");
        assert_eq!(
            labels,
            vec![BTreeMap::new()],
            "segmented reader: {labels:?}"
        );
    }

    #[test]
    fn histogram_power_drift_splits_series() {
        // Same histogram name, different H2 powers across segments. The powers
        // are part of identity, so these are two runs — each decoded with its
        // OWN powers, addressable as distinct series. Opening must NOT fail:
        // one remapped id cannot cost the whole archive.
        let buckets = |gp: u8, mvp: u8, idx: usize, count: u64| {
            let n = ::histogram::Config::new(gp, mvp).unwrap().total_buckets();
            let mut b = vec![0u64; n];
            b[idx] = count;
            b
        };
        // Bucket 20 resolves to a very different value under gp=2 vs gp=3, so a
        // run decoded under the wrong config reads as a different latency.
        let seg_a = || {
            segment_histogram(
                "latency",
                2,
                8,
                &[
                    (1_000_000_000, buckets(2, 8, 20, 10)),
                    (2_000_000_000, buckets(2, 8, 20, 20)),
                ],
            )
        };
        let seg_b = || {
            segment_histogram(
                "latency",
                3,
                8,
                &[
                    (3_000_000_000, buckets(3, 8, 20, 10)),
                    (4_000_000_000, buckets(3, 8, 20, 20)),
                ],
            )
        };

        let pool = BufferPool::new(64 * 1024 * 1024);
        let r =
            SegmentedParquetReader::open_bytes_with_pool(vec![seg_a(), seg_b()], Arc::clone(&pool))
                .expect("a mid-recording powers change must not fail the open");
        // Still footer-only: the conflict is decided from field metadata.
        let stats = pool.stats();
        assert_eq!(
            stats.misses, 0,
            "conflict policy must not decode row groups"
        );
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.bytes_used, 0);

        // Two distinct series, disambiguated by run.
        let labels = r.histogram_labels("latency");
        assert_eq!(labels.len(), 2, "{labels:?}");
        assert_eq!(
            labels
                .iter()
                .filter_map(|l| l.get("__run__").cloned())
                .collect::<Vec<_>>(),
            vec!["0".to_string(), "1".to_string()],
        );

        // Each run must decode with its own powers: compare against a
        // single-segment reader, which has no conflict and no run label.
        let mean = |r: &SegmentedParquetReader, expr: &str, at: f64| -> f64 {
            let q = r.query_range(expr, 1.0, 5.0, 1.0).unwrap();
            let QueryResult::Matrix { result } = q else {
                panic!("expected matrix result for {expr}");
            };
            assert_eq!(result.len(), 1, "{expr}: {result:?}");
            result[0]
                .values
                .iter()
                .find(|(t, _)| (*t - at).abs() < 1e-9)
                .unwrap_or_else(|| panic!("{expr}: no point at t={at}: {:?}", result[0].values))
                .1
        };
        let pool = BufferPool::new(64 * 1024 * 1024);
        let only_a =
            SegmentedParquetReader::open_bytes_with_pool(vec![seg_a()], Arc::clone(&pool)).unwrap();
        let only_b = SegmentedParquetReader::open_bytes_with_pool(vec![seg_b()], pool).unwrap();

        let run0 = mean(&r, "histogram_mean(latency{__run__=\"0\"})", 2.0);
        let run1 = mean(&r, "histogram_mean(latency{__run__=\"1\"})", 4.0);
        assert_eq!(run0, mean(&only_a, "histogram_mean(latency)", 2.0));
        assert_eq!(run1, mean(&only_b, "histogram_mean(latency)", 4.0));
        assert!(
            (run0 - run1).abs() > 1.0,
            "bucket 20 must resolve differently under gp=2 ({run0}) and gp=3 ({run1}); \
             equal values would make the per-run decode assertions vacuous"
        );

        // An unqualified query resolves to the FIRST run and never mixes the
        // two: samples from the later run would be decoded under the wrong
        // powers, which is exactly the coercion the policy forbids.
        let q = r
            .query_range("histogram_mean(latency)", 1.0, 5.0, 1.0)
            .unwrap();
        let QueryResult::Matrix { result } = q else {
            panic!("expected matrix result");
        };
        assert_eq!(result.len(), 1);
        assert!(
            result[0].values.iter().all(|(t, _)| *t <= 2.0),
            "the drifted run must not be spliced into the first: {:?}",
            result[0].values
        );

        // Matching configs are not a conflict: one series, no run label.
        let pool = BufferPool::new(64 * 1024 * 1024);
        let plain =
            SegmentedParquetReader::open_bytes_with_pool(vec![seg_a(), seg_a()], pool).unwrap();
        let labels = plain.histogram_labels("latency");
        assert_eq!(labels.len(), 1, "{labels:?}");
        assert!(labels[0].is_empty(), "{labels:?}");
    }

    /// Bucket vector for `(gp, mvp)` with a single non-zero bucket.
    fn run_test_buckets(gp: u8, mvp: u8, idx: usize, count: u64) -> Vec<u64> {
        let n = ::histogram::Config::new(gp, mvp).unwrap().total_buckets();
        let mut b = vec![0u64; n];
        b[idx] = count;
        b
    }

    // The conflict policy advertises `latency{__run__="1"}` as the way to reach
    // a drifted run, but the only production consumer (rezolus' `RezReader`)
    // routes EVERY query through `columns()` first. A run-qualified selector
    // that resolves to no columns is therefore rejected before it can reach
    // `query_range` — "query references no metric present in this .rez" — which
    // makes runs >= 1 unreachable data through the front door even though the
    // `query_range` unit tests (which bypass `columns()`) pass.
    #[test]
    fn run_qualified_queries_resolve_through_columns() {
        let seg_a = segment_histogram(
            "latency",
            2,
            8,
            &[(1_000_000_000, run_test_buckets(2, 8, 20, 10))],
        );
        let seg_b = segment_histogram(
            "latency",
            3,
            8,
            &[(2_000_000_000, run_test_buckets(3, 8, 20, 10))],
        );
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(vec![seg_a, seg_b], pool).unwrap();
        assert_eq!(r.histogram_labels("latency").len(), 2, "drift expected");

        let plain = r.columns("histogram_mean(latency)").unwrap();
        assert!(plain.contains("latency:buckets"), "cols: {plain:?}");
        for run in ["0", "1"] {
            let cols = r
                .columns(&format!("histogram_mean(latency{{__run__=\"{run}\"}})"))
                .unwrap();
            assert_eq!(
                cols, plain,
                "run {run} must resolve to the same physical column"
            );
        }

        // A run that does not exist still resolves to nothing.
        let cols = r.columns("histogram_mean(latency{__run__=\"7\"})").unwrap();
        assert!(cols.is_empty(), "cols: {cols:?}");
    }

    // A dashboard can pin `__run__="0"` so one query works across an A/B pair
    // where only one side's histogram config drifted. On the side that did NOT
    // drift the segments carry no `__run__` at all, so the filter used to fail
    // closed: `columns()` returned nothing and the stream reported the metric
    // missing. Run 0 IS the single run here, so it must resolve.
    #[test]
    fn run_zero_is_accepted_when_nothing_drifted() {
        // Two rows per segment with a rising count: a histogram scalar is
        // delta-based, so a single row per segment yields no points at all.
        let seg = |t0: u64, c0: u64| {
            segment_histogram(
                "latency",
                2,
                8,
                &[
                    (t0, run_test_buckets(2, 8, 20, c0)),
                    (t0 + 1_000_000_000, run_test_buckets(2, 8, 20, c0 + 10)),
                ],
            )
        };
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(
            vec![seg(1_000_000_000, 10), seg(3_000_000_000, 30)],
            pool,
        )
        .unwrap();
        assert_eq!(r.histogram_labels("latency").len(), 1, "no drift here");

        let cols = r.columns("histogram_mean(latency{__run__=\"0\"})").unwrap();
        assert!(cols.contains("latency:buckets"), "cols: {cols:?}");

        let pinned = r
            .query_range("histogram_mean(latency{__run__=\"0\"})", 1.0, 4.0, 1.0)
            .expect("__run__=\"0\" must resolve on a single-run histogram");
        let unpinned = r
            .query_range("histogram_mean(latency)", 1.0, 4.0, 1.0)
            .unwrap();
        assert_eq!(
            format!("{pinned:?}"),
            format!("{unpinned:?}"),
            "pinning run 0 must not change the result, nor add a __run__ label"
        );

        // Any other run genuinely has no data, at both entry points.
        assert!(r
            .columns("histogram_mean(latency{__run__=\"1\"})")
            .unwrap()
            .is_empty());
        assert!(matches!(
            r.query_range("histogram_mean(latency{__run__=\"1\"})", 1.0, 4.0, 1.0),
            Ok(crate::QueryResult::Matrix { result }) if result.is_empty()
        ));
    }

    #[test]
    fn open_rejects_unsplicable_histogram_within_one_segment() {
        // The one case that cannot be split into runs: TWO histogram columns
        // for the same metric name under different configs inside ONE segment.
        // `ParquetSource::histogram_stream` decodes every matching column under
        // the first column's config, so these buckets cannot be separated —
        // an error beats silently wrong latency numbers.
        let a = segment_two_histograms("latency", (2, 8), (3, 8), 1_000_000_000);
        let pool = BufferPool::new(64 * 1024 * 1024);
        let Err(err) = SegmentedParquetReader::open_bytes_with_pool(vec![a], Arc::clone(&pool))
        else {
            panic!("a segment-internal histogram config conflict must be rejected");
        };
        let msg = err.to_string();
        assert!(msg.contains("latency"), "{msg}");
        assert!(msg.contains("grouping_power=2"), "{msg}");
        assert!(msg.contains("grouping_power=3"), "{msg}");
        // Footer-only, even when rejecting.
        let stats = pool.stats();
        assert_eq!(stats.misses, 0, "config check must not decode row groups");
        assert_eq!(stats.entries, 0);
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

    /// A segmented source must compose into `ParquetBuilder` alongside an
    /// ordinary single-file reader, each carrying its own injected labels.
    ///
    /// This is the composition systemslab's job-spanning queries rely on: N
    /// artifacts merged into one reader, each tagged so a single PromQL query
    /// can slice by job. Before `source_labeled`, `MultiParquetSource` held
    /// `Vec<(Arc<ParquetSource>, Labels)>`, so a `.rez` table -- which is a
    /// segmented source whenever its writer sealed more than once -- could not
    /// enter the builder at all.
    ///
    /// `source_labeled` takes an opaque [`CompositionSource`] rather than a
    /// bare `Arc<dyn DataSource>`: `DataSource`'s methods return `Counters`,
    /// `Gauges` and `HistogramStream`, so making the trait itself public would
    /// drag the crate's internal row representations into the public API.
    #[test]
    fn builder_composes_a_segmented_source_alongside_a_plain_one() {
        let plain = segment(
            "cpu_cycles",
            &[],
            &[(1_000_000_000, 10), (2_000_000_000, 20)],
        );
        let seg_a = segment(
            "cpu_cycles",
            &[],
            &[(1_000_000_000, 10), (2_000_000_000, 20)],
        );
        let seg_b = segment(
            "cpu_cycles",
            &[],
            &[(3_000_000_000, 35), (4_000_000_000, 50)],
        );

        let pool = BufferPool::new(64 * 1024 * 1024);
        let segmented =
            SegmentedParquetReader::open_bytes_with_pool(vec![seg_a, seg_b], Arc::clone(&pool))
                .unwrap();
        let single = Arc::new(ParquetReader::open_bytes(plain).unwrap());

        let combined = ParquetReader::builder()
            .pool(pool)
            .reader_labeled(single, [("job", "single")])
            .source_labeled(&segmented, [("job", "segmented")])
            .build()
            .unwrap();

        // Both children contribute a series, kept distinct by the injected
        // label -- not merged, and not silently dropped.
        let mut jobs: Vec<String> = combined
            .counter_labels("cpu_cycles")
            .into_iter()
            .filter_map(|l| l.get("job").cloned())
            .collect();
        jobs.sort();
        assert_eq!(jobs, vec!["segmented".to_string(), "single".to_string()]);
    }

    /// A store that counts fetches, and can be told to forget a segment.
    struct CountingStore {
        segments: Vec<Vec<u8>>,
        fetched: Mutex<Vec<usize>>,
        gone: Mutex<std::collections::HashSet<usize>>,
    }

    impl CountingStore {
        fn new(segments: Vec<Vec<u8>>) -> Arc<Self> {
            Arc::new(Self {
                segments,
                fetched: Mutex::new(Vec::new()),
                gone: Mutex::new(Default::default()),
            })
        }

        fn fetched(&self) -> Vec<usize> {
            std::mem::take(&mut *self.fetched.lock().unwrap())
        }
    }

    impl SegmentStore for CountingStore {
        fn len(&self) -> usize {
            self.segments.len()
        }

        fn bytes(&self, idx: usize) -> SegmentBytes {
            self.fetched.lock().unwrap().push(idx);
            if self.gone.lock().unwrap().contains(&idx) {
                return Ok(None);
            }
            Ok(self.segments.get(idx).map(|b| Bytes::from(b.clone())))
        }
    }

    /// Three one-second segments at 1s, 3s and 5s, one series.
    fn three_segments() -> Vec<Vec<u8>> {
        vec![
            segment("c", &[], &[(1_000_000_000, 10)]),
            segment("c", &[], &[(3_000_000_000, 30)]),
            segment("c", &[], &[(5_000_000_000, 50)]),
        ]
    }

    /// Open reads every segment exactly once and keeps none of them; a
    /// query fetches only the segments whose span it touches, and the
    /// listing surface answers from what open kept.
    #[test]
    fn open_reads_each_segment_once_and_a_query_fetches_only_what_it_touches() {
        let store = CountingStore::new(three_segments());
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_with_pool(store.clone(), pool).unwrap();
        assert_eq!(store.fetched(), vec![0, 1, 2], "open: each once, in order");
        assert_eq!(r.cached_segments().0, 0, "open keeps no segment");

        assert_eq!(r.counter_names(), vec!["c".to_string()]);
        assert_eq!(r.counter_labels("c").len(), 1);
        assert_eq!(r.time_range_ns(), Some((1_000_000_000, 5_000_000_000)));
        assert_eq!(
            store.fetched(),
            Vec::<usize>::new(),
            "listing needs no bytes"
        );

        // A range over the middle second touches the middle segment only.
        let opts = QueryOptions::with_rate_mode(crate::RateMode::Raw);
        let _ = r.query_range_opts("irate(c[1s])", 2.5, 3.5, 1.0, &opts);
        assert_eq!(store.fetched(), vec![1]);
        assert_eq!(r.cached_segments().0, 1);

        // Again: served from the cache.
        let _ = r.query_range_opts("irate(c[1s])", 2.5, 3.5, 1.0, &opts);
        assert_eq!(store.fetched(), Vec::<usize>::new());

        // The whole range: the two not yet cached.
        let _ = r.query_range_opts("irate(c[1s])", 0.0, 6.0, 1.0, &opts);
        assert_eq!(store.fetched(), vec![0, 2]);
        assert_eq!(r.cached_segments().0, 3);
    }

    /// The cache is bounded by segment bytes and keeps the most recently
    /// used, never fewer than one.
    #[test]
    fn the_segment_cache_evicts_least_recently_used_within_its_byte_budget() {
        let segments = three_segments();
        // What one opened segment is charged: its bytes plus the footer
        // estimate, the same figure the cache uses.
        let one =
            MultiParquetSource::open_bytes_with_pool(segments[0].clone(), BufferPool::new(1 << 20))
                .unwrap()
                .resident_estimate();
        let store = CountingStore::new(segments);
        // Room for two segments, not three.
        let pool = BufferPool::new(2 * one + one / 2);
        let r = SegmentedParquetReader::open_with_pool(store.clone(), pool).unwrap();
        let opts = QueryOptions::with_rate_mode(crate::RateMode::Raw);
        let _ = r.query_range_opts("irate(c[1s])", 0.0, 6.0, 1.0, &opts);
        store.fetched();
        let (n, bytes) = r.cached_segments();
        assert_eq!(n, 2, "two fit, the first opened was evicted");
        assert!(bytes <= 2 * one + one / 2);

        // The last two opened are the ones held: a query touching only the
        // last segment fetches nothing, one touching the first fetches it.
        let _ = r.query_range_opts("irate(c[1s])", 4.5, 5.5, 1.0, &opts);
        assert_eq!(store.fetched(), Vec::<usize>::new());
        let _ = r.query_range_opts("irate(c[1s])", 0.5, 1.5, 1.0, &opts);
        assert_eq!(store.fetched(), vec![0]);

        // A budget smaller than one segment still opens one at a time.
        let store = CountingStore::new(three_segments());
        let r = SegmentedParquetReader::open_with_pool(store.clone(), BufferPool::new(1)).unwrap();
        let _ = r.query_range_opts("irate(c[1s])", 0.0, 6.0, 1.0, &opts);
        assert_eq!(r.cached_segments().0, 1);
    }

    /// A segment the store has lost since open — a rolling buffer's
    /// retention — is skipped, and the rest still answer.
    #[test]
    fn a_segment_lost_after_open_is_skipped() {
        let store = CountingStore::new(three_segments());
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_with_pool(store.clone(), pool).unwrap();
        store.gone.lock().unwrap().insert(0);
        let opts = QueryOptions::with_rate_mode(crate::RateMode::Raw);
        let QueryResult::Matrix { result } = r
            .query_range_opts("irate(c[1s])", 0.0, 6.0, 1.0, &opts)
            .unwrap()
        else {
            panic!("matrix");
        };
        // Samples at 3s and 5s remain: one pairwise rate, at 5s. With the
        // lost segment there would be a second, at 3s.
        let points: Vec<(f64, f64)> = result[0].values.clone();
        assert_eq!(
            points,
            vec![(5.0, 10.0)],
            "the lost segment's sample is gone: {points:?}"
        );
        assert_eq!(r.segment_count(), 3, "the catalog still counts it");
    }

    /// The counter scan skips a segment lost after open, and the batched
    /// rate path then gives what the per-series path gives.
    #[test]
    fn the_counter_scan_skips_a_segment_lost_after_open() {
        let store = CountingStore::new(three_segments());
        let r = SegmentedParquetReader::open_with_pool(store.clone(), BufferPool::new(1 << 26))
            .unwrap();
        store.gone.lock().unwrap().insert(0);
        let mut scan = r
            .source
            .counter_scan("c", &Labels::default(), 0, 6_000_000_000)
            .expect("c scans");
        let mut read = 0;
        while let Some(chunk) = scan.next_chunk(1).expect("no read error") {
            read += chunk.segments.len();
        }
        assert_eq!(read, 2, "the lost segment is skipped, the others read");

        let range = |opts: &QueryOptions| {
            let QueryResult::Matrix { result } = r
                .query_range_opts("irate(c[2s])", 0.0, 6.0, 1.0, opts)
                .unwrap()
            else {
                panic!("matrix");
            };
            result[0].values.clone()
        };
        let rates = || crate::promql::streaming::dispatch::BATCH_RATES.with(|n| n.get());
        let before = rates();
        let batched = range(&QueryOptions::default());
        assert_eq!(rates(), before + 1, "the batched path answered");
        let per_series = range(&QueryOptions::default().with_per_series_rates(true));
        assert_eq!(batched, per_series);
    }

    /// A store that has lost a segment BEFORE open still opens; one that has
    /// lost every segment does not.
    #[test]
    fn open_tolerates_a_gone_segment_but_not_all_of_them() {
        let store = CountingStore::new(three_segments());
        store.gone.lock().unwrap().insert(1);
        let r = SegmentedParquetReader::open_with_pool(store.clone(), BufferPool::new(1 << 20))
            .unwrap();
        assert_eq!(r.time_range_ns(), Some((1_000_000_000, 5_000_000_000)));
        let opts = QueryOptions::with_rate_mode(crate::RateMode::Raw);
        let _ = r.query_range_opts("irate(c[1s])", 0.0, 6.0, 1.0, &opts);
        assert_eq!(
            store.fetched().iter().filter(|i| **i == 1).count(),
            1,
            "only open asked for the gone segment; the query never did"
        );

        let store = CountingStore::new(three_segments());
        for i in 0..3 {
            store.gone.lock().unwrap().insert(i);
        }
        assert!(SegmentedParquetReader::open_with_pool(store, BufferPool::new(1 << 20)).is_err());
    }

    /// A relabelling that says slot columns (`slot=<n>`) were held by
    /// `who=a` until `cut` and `who=b` from then on, and knows the filter
    /// key it supplies.
    struct HandOver {
        cut: u64,
    }

    impl HandOver {
        fn who(&self, ts: u64) -> &'static str {
            if ts < self.cut {
                "a"
            } else {
                "b"
            }
        }

        fn with(labels: &Labels, who: &str) -> Labels {
            let mut l = labels.clone();
            l.inner.insert("who".to_string(), who.to_string());
            l
        }
    }

    impl ColumnRelabel for HandOver {
        fn identities(&self, _name: &str, labels: &Labels) -> Option<Vec<Labels>> {
            labels
                .inner
                .contains_key("slot")
                .then(|| vec![Self::with(labels, "a"), Self::with(labels, "b")])
        }

        fn split(&self, _name: &str, labels: &Labels, timestamps: &[u64]) -> Option<Vec<Run>> {
            if !labels.inner.contains_key("slot") {
                return None;
            }
            let first_b = timestamps.partition_point(|ts| *ts < self.cut);
            let mut runs = Vec::new();
            if first_b > 0 {
                runs.push((Self::with(labels, "a"), 0..first_b));
            }
            if first_b < timestamps.len() {
                runs.push((Self::with(labels, "b"), first_b..timestamps.len()));
            }
            Some(runs)
        }

        fn at(&self, _name: &str, labels: &Labels, timestamp: u64) -> Option<Labels> {
            labels
                .inner
                .contains_key("slot")
                .then(|| Self::with(labels, self.who(timestamp)))
        }

        fn segment_filter(&self, _name: &str, filter: &Labels) -> Labels {
            // `who` is ours; the columns cannot answer it. Everything else
            // passes through.
            let mut f = filter.clone();
            f.inner.remove("who");
            f
        }
    }

    /// A relabelled column lists every identity it can present as, and a
    /// query cuts its samples at the handover: each occupant's series has
    /// only its own samples, spliced across segments.
    #[test]
    fn a_relabelled_column_splits_by_occupant_across_segments() {
        let s0 = segment_labeled(
            "c",
            "slot",
            &[1_000_000_000, 2_000_000_000],
            &[("7", vec![10, 20])],
        );
        let s1 = segment_labeled(
            "c",
            "slot",
            &[3_000_000_000, 4_000_000_000],
            &[("7", vec![30, 40])],
        );
        let r = SegmentedParquetReader::open_relabeled_with_pool(
            Arc::new(InMemorySegments::new(vec![s0, s1])),
            BufferPool::new(1 << 20),
            Arc::new(HandOver { cut: 2_500_000_000 }),
        )
        .unwrap();

        let labels = r.counter_labels("c");
        assert_eq!(labels.len(), 2, "one column, two occupants: {labels:?}");
        assert!(labels.iter().all(|l| l["slot"] == "7"));
        assert_eq!(
            labels.iter().map(|l| l["who"].as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );

        let opts = QueryOptions::with_rate_mode(crate::RateMode::Raw);
        let QueryResult::Matrix { result } = r
            .query_range_opts("irate(c[1s])", 0.0, 5.0, 1.0, &opts)
            .unwrap()
        else {
            panic!("matrix");
        };
        let mut by_who: Vec<(String, Vec<(f64, f64)>)> = result
            .into_iter()
            .map(|s| (s.metric["who"].clone(), s.values))
            .collect();
        by_who.sort_by(|a, b| a.0.cmp(&b.0));
        // a holds 1s and 2s: one pairwise rate at 2s. b holds 3s and 4s:
        // one at 4s. Nothing crosses the handover.
        assert_eq!(
            by_who,
            vec![
                ("a".to_string(), vec![(2.0, 10.0)]),
                ("b".to_string(), vec![(4.0, 10.0)]),
            ]
        );
    }

    /// A filter on a key the relabelling supplies is answered after the
    /// split, with the segment asked a filter it can answer.
    #[test]
    fn a_filter_on_a_relabelled_key_selects_the_occupant() {
        let s0 = segment_labeled(
            "c",
            "slot",
            &[1_000_000_000, 2_000_000_000],
            &[("7", vec![10, 20])],
        );
        let s1 = segment_labeled(
            "c",
            "slot",
            &[3_000_000_000, 4_000_000_000],
            &[("7", vec![30, 40])],
        );
        let r = SegmentedParquetReader::open_relabeled_with_pool(
            Arc::new(InMemorySegments::new(vec![s0, s1])),
            BufferPool::new(1 << 20),
            Arc::new(HandOver { cut: 2_500_000_000 }),
        )
        .unwrap();
        let opts = QueryOptions::with_rate_mode(crate::RateMode::Raw);
        let QueryResult::Matrix { result } = r
            .query_range_opts("irate(c{who=\"b\"}[1s])", 0.0, 5.0, 1.0, &opts)
            .unwrap()
        else {
            panic!("matrix");
        };
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].metric["who"], "b");
        assert_eq!(result[0].values, vec![(4.0, 10.0)]);

        // A key the columns do carry still narrows inside the segment.
        assert!(
            r.query_range_opts("irate(c{slot=\"8\"}[1s])", 0.0, 5.0, 1.0, &opts)
                .map(|q| matches!(q, QueryResult::Matrix { result } if result.is_empty()))
                .unwrap_or(true),
            "no such slot"
        );
    }

    /// Histogram rows are relabelled one at a time by their timestamp, and
    /// the two occupants read as two series.
    #[test]
    fn histogram_rows_are_relabelled_by_timestamp() {
        // Cumulative snapshots growing by 5 a second. The count reducer
        // emits each series' delta between consecutive rows, so each
        // occupant's two rows yield one point of 5 — and a row that crossed
        // the handover would show as a delta of 10 on the wrong side.
        let n = ::histogram::Config::new(2, 8).unwrap().total_buckets();
        let at = |k: u64| {
            let mut b = vec![0u64; n];
            b[3] = 5 * k;
            b
        };
        let seg = segment_histogram_labeled(
            "latency",
            2,
            8,
            &[("slot", "7")],
            &[
                (1_000_000_000, at(1)),
                (2_000_000_000, at(2)),
                (3_000_000_000, at(3)),
                (4_000_000_000, at(4)),
            ],
        );
        let r = SegmentedParquetReader::open_relabeled_with_pool(
            Arc::new(InMemorySegments::new(vec![seg])),
            BufferPool::new(1 << 20),
            Arc::new(HandOver { cut: 2_500_000_000 }),
        )
        .unwrap();
        let labels = r.histogram_labels("latency");
        assert_eq!(labels.len(), 2, "{labels:?}");

        let QueryResult::Matrix { result } = r
            .query_range("histogram_count by (who) (latency)", 0.0, 5.0, 1.0)
            .unwrap()
        else {
            panic!("matrix");
        };
        let mut whos: Vec<String> = result.iter().map(|s| s.metric["who"].clone()).collect();
        whos.sort();
        assert_eq!(whos, vec!["a".to_string(), "b".to_string()]);
        for s in &result {
            let pts: Vec<f64> = s.values.iter().map(|(_, v)| *v).collect();
            assert!(
                !pts.is_empty() && pts.iter().all(|v| *v == 5.0),
                "each occupant sees only its own rows, one delta of 5: {} -> {pts:?}",
                s.metric["who"]
            );
        }
    }

    /// The streams a segmented source hands out carry the same samples the
    /// materialized read does, series for series, relabelling included —
    /// and a grid `rate()` over them, which is what queries use now,
    /// matches the vector form point for point.
    #[test]
    fn counter_streams_match_the_materialized_read() {
        let s0 = segment_labeled(
            "c",
            "slot",
            &[1_000_000_000, 2_000_000_000],
            &[("7", vec![10, 20]), ("8", vec![5, 6])],
        );
        let s1 = segment_labeled(
            "c",
            "slot",
            &[3_000_000_000, 4_000_000_000],
            &[("7", vec![30, 40]), ("8", vec![7, 8])],
        );
        let r = SegmentedParquetReader::open_relabeled_with_pool(
            Arc::new(InMemorySegments::new(vec![s0, s1])),
            BufferPool::new(1 << 20),
            Arc::new(HandOver { cut: 2_500_000_000 }),
        )
        .unwrap();
        let source = r.data_source();
        let whole = source
            .counters("c", &Labels::default(), 0, u64::MAX)
            .unwrap();
        let streams = source
            .counter_streams("c", &Labels::default(), 0, u64::MAX)
            .unwrap();
        assert_eq!(streams.len(), whole.series.len(), "one stream per series");
        assert_eq!(streams.len(), 4, "two slots, two occupants each");
        for (stream, series) in streams.into_iter().zip(whole.series) {
            assert_eq!(stream.labels, series.labels);
            assert_eq!(stream.windowed, series.windows.is_some());
            let samples: Vec<crate::CounterSample> = stream.samples.collect();
            assert_eq!(
                samples.iter().map(|s| (s.ts, s.value)).collect::<Vec<_>>(),
                series
                    .timestamps
                    .iter()
                    .copied()
                    .zip(series.values.iter().copied())
                    .collect::<Vec<_>>(),
                "{:?}",
                series.labels
            );
        }

        // And through the engine: grid rate over the streams.
        let QueryResult::Matrix { result } = r.query_range("rate(c[1s])", 0.0, 5.0, 1.0).unwrap()
        else {
            panic!("matrix");
        };
        type Row = (String, String, Vec<(f64, f64)>);
        let mut got: Vec<Row> = result
            .into_iter()
            .map(|s| (s.metric["slot"].clone(), s.metric["who"].clone(), s.values))
            .collect();
        got.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        assert_eq!(
            got,
            vec![
                ("7".to_string(), "a".to_string(), vec![(2.0, 10.0)]),
                ("7".to_string(), "b".to_string(), vec![(4.0, 10.0)]),
                ("8".to_string(), "a".to_string(), vec![(2.0, 1.0)]),
                ("8".to_string(), "b".to_string(), vec![(4.0, 1.0)]),
            ]
        );
    }

    fn lb(pairs: &[(&str, &str)]) -> Labels {
        Labels {
            inner: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// A stream reads only the segments its series has columns in and the
    /// query's range touches: a series absent from a segment costs no
    /// fetch, and a range over one segment fetches that one.
    #[test]
    fn a_stream_fetches_only_the_segments_that_feed_it() {
        let s0 = segment_labeled("c", "k", &[1_000_000_000], &[("x", vec![1])]);
        let s1 = segment_labeled("c", "k", &[2_000_000_000], &[("y", vec![1])]);
        let s2 = segment_labeled("c", "k", &[3_000_000_000], &[("x", vec![2])]);
        let store = CountingStore::new(vec![s0, s1, s2]);
        let r = SegmentedParquetReader::open_with_pool(store.clone(), BufferPool::new(1 << 20))
            .unwrap();
        store.fetched();
        let source = r.data_source();
        let mut x = source
            .counter_streams("c", &lb(&[("k", "x")]), 0, u64::MAX)
            .unwrap();
        let samples: Vec<_> = x.remove(0).samples.collect();
        assert_eq!(samples.len(), 2);
        assert_eq!(
            store.fetched(),
            vec![0, 2],
            "the segment without x is never read"
        );
        let mut y = source
            .counter_streams("c", &lb(&[("k", "y")]), 0, 1_500_000_000)
            .unwrap();
        assert_eq!(y.remove(0).samples.count(), 0);
        assert_eq!(
            store.fetched(),
            Vec::<usize>::new(),
            "out of range: nothing fetched"
        );
    }

    /// A store whose leading segments carry keys, counting the fetches of
    /// each segment.
    struct KeyedSegments {
        segments: Vec<(Option<u64>, Bytes)>,
        fetched: Mutex<Vec<usize>>,
    }

    impl KeyedSegments {
        fn new(segments: Vec<(Option<u64>, Vec<u8>)>) -> Arc<Self> {
            Arc::new(Self {
                segments: segments
                    .into_iter()
                    .map(|(k, b)| (k, Bytes::from(b)))
                    .collect(),
                fetched: Mutex::new(Vec::new()),
            })
        }

        fn fetched(&self) -> Vec<usize> {
            let mut f = self.fetched.lock().unwrap().clone();
            f.sort_unstable();
            f.dedup();
            f
        }
    }

    impl SegmentStore for KeyedSegments {
        fn len(&self) -> usize {
            self.segments.len()
        }

        fn bytes(&self, idx: usize) -> SegmentBytes {
            self.fetched.lock().unwrap().push(idx);
            Ok(self.segments.get(idx).map(|(_, b)| b.clone()))
        }

        fn key(&self, idx: usize) -> Option<u64> {
            self.segments.get(idx).and_then(|(k, _)| *k)
        }
    }

    fn rows(from: u64, n: u64) -> Vec<(u64, u64)> {
        (from..from + n)
            .map(|s| (s * 1_000_000_000, s * 10))
            .collect()
    }

    /// The answer to a fixed query, with each series' labels sorted.
    fn answer(r: &SegmentedParquetReader) -> String {
        let QueryResult::Matrix { result } = r
            .query_range("rate(cpu_cycles[2s])", 1.0, 20.0, 1.0)
            .unwrap()
        else {
            panic!("not a matrix");
        };
        let mut out: Vec<String> = result
            .into_iter()
            .map(|m| {
                let labels: BTreeMap<_, _> = m.metric.into_iter().collect();
                format!("{labels:?} {:?}", m.values)
            })
            .collect();
        out.sort();
        out.join("\n")
    }

    /// A reader opened after another over the same table reads only the
    /// segments sealed since, and answers as a fresh open does.
    #[test]
    fn open_after_reads_only_segments_sealed_since() {
        let seg = |from| segment("cpu_cycles", &[], &rows(from, 4));
        let pool = BufferPool::new(64 * 1024 * 1024);
        let first = SegmentedParquetReader::open_after(
            KeyedSegments::new(vec![(Some(1), seg(1)), (Some(2), seg(5)), (None, seg(9))]),
            Arc::clone(&pool),
            None,
            None,
        )
        .unwrap();
        answer(&first);

        let grown = || {
            vec![
                (Some(1), seg(1)),
                (Some(2), seg(5)),
                (Some(3), seg(9)),
                (None, seg(13)),
            ]
        };
        let store = KeyedSegments::new(grown());
        let after = SegmentedParquetReader::open_after(
            Arc::clone(&store) as Arc<dyn SegmentStore>,
            Arc::clone(&pool),
            None,
            Some(&first.handover().unwrap()),
        )
        .unwrap();
        assert_eq!(
            store.fetched(),
            vec![2, 3],
            "only the new segments are read at open"
        );
        let fresh =
            SegmentedParquetReader::open_with_pool(KeyedSegments::new(grown()), Arc::clone(&pool))
                .unwrap();
        assert_eq!(answer(&after), answer(&fresh));
        assert_eq!(
            store.fetched(),
            vec![2, 3],
            "the segments the first reader had open are taken over, not fetched"
        );

        // With no new sealed segment, a third reader starts from the same
        // state and reads only the tail.
        let store = KeyedSegments::new(grown());
        let third = SegmentedParquetReader::open_after(
            Arc::clone(&store) as Arc<dyn SegmentStore>,
            Arc::clone(&pool),
            None,
            Some(&after.handover().unwrap()),
        )
        .unwrap();
        assert_eq!(store.fetched(), vec![3]);
        assert_eq!(answer(&third), answer(&fresh));
    }

    /// When the leading keys differ (retention evicted the first segment),
    /// nothing is reused.
    #[test]
    fn open_after_with_a_different_prefix_reads_everything() {
        let seg = |from| segment("cpu_cycles", &[], &rows(from, 4));
        let pool = BufferPool::new(64 * 1024 * 1024);
        let first = SegmentedParquetReader::open_after(
            KeyedSegments::new(vec![(Some(1), seg(1)), (Some(2), seg(5))]),
            Arc::clone(&pool),
            None,
            None,
        )
        .unwrap();
        answer(&first);
        let store = KeyedSegments::new(vec![(Some(2), seg(5)), (Some(3), seg(9))]);
        let after = SegmentedParquetReader::open_after(
            Arc::clone(&store) as Arc<dyn SegmentStore>,
            Arc::clone(&pool),
            None,
            Some(&first.handover().unwrap()),
        )
        .unwrap();
        assert_eq!(store.fetched(), vec![0, 1]);
        let fresh = SegmentedParquetReader::open_with_pool(
            KeyedSegments::new(vec![(Some(2), seg(5)), (Some(3), seg(9))]),
            Arc::clone(&pool),
        )
        .unwrap();
        assert_eq!(answer(&after), answer(&fresh));
    }

    /// A reader with a relabel whose identities can change hands nothing on.
    #[test]
    fn a_changing_relabel_hands_nothing_on() {
        let seg = |from| segment("cpu_cycles", &[("slot", "0")], &rows(from, 4));
        let pool = BufferPool::new(64 * 1024 * 1024);
        let relabel = || Some(Arc::new(HandOver { cut: 6_000_000_000 }) as Arc<dyn ColumnRelabel>);
        let first = SegmentedParquetReader::open_after(
            KeyedSegments::new(vec![(Some(1), seg(1)), (Some(2), seg(5))]),
            Arc::clone(&pool),
            relabel(),
            None,
        )
        .unwrap();
        assert!(first.handover().is_none());
    }

    /// Presents every `slot` column as `who="a"` throughout, and says its
    /// identities are fixed.
    struct AllA;

    impl ColumnRelabel for AllA {
        fn identities(&self, _name: &str, labels: &Labels) -> Option<Vec<Labels>> {
            labels
                .inner
                .contains_key("slot")
                .then(|| vec![HandOver::with(labels, "a")])
        }

        fn split(&self, _name: &str, labels: &Labels, timestamps: &[u64]) -> Option<Vec<Run>> {
            labels
                .inner
                .contains_key("slot")
                .then(|| vec![(HandOver::with(labels, "a"), 0..timestamps.len())])
        }

        fn at(&self, _name: &str, labels: &Labels, _timestamp: u64) -> Option<Labels> {
            labels
                .inner
                .contains_key("slot")
                .then(|| HandOver::with(labels, "a"))
        }

        fn segment_filter(&self, _name: &str, filter: &Labels) -> Labels {
            let mut f = filter.clone();
            f.inner.remove("who");
            f
        }

        fn identities_are_fixed(&self) -> bool {
            true
        }
    }

    /// [`AllA`], counting the calls to `identities`.
    struct Counted(std::sync::atomic::AtomicUsize);

    impl ColumnRelabel for Counted {
        fn identities(&self, name: &str, labels: &Labels) -> Option<Vec<Labels>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            AllA.identities(name, labels)
        }

        fn split(&self, name: &str, labels: &Labels, timestamps: &[u64]) -> Option<Vec<Run>> {
            AllA.split(name, labels, timestamps)
        }

        fn at(&self, name: &str, labels: &Labels, timestamp: u64) -> Option<Labels> {
            AllA.at(name, labels, timestamp)
        }

        fn segment_filter(&self, name: &str, filter: &Labels) -> Labels {
            AllA.segment_filter(name, filter)
        }

        fn identities_are_fixed(&self) -> bool {
            true
        }
    }

    /// An open asks the relabel for a column's identities once, however
    /// many segments hold the column.
    #[test]
    fn an_open_asks_for_each_columns_identities_once() {
        let seg = |from| segment("cpu_cycles", &[("slot", "0")], &rows(from, 4));
        let relabel = Arc::new(Counted(std::sync::atomic::AtomicUsize::new(0)));
        let r = SegmentedParquetReader::open_after(
            KeyedSegments::new(vec![
                (Some(1), seg(1)),
                (Some(2), seg(5)),
                (Some(3), seg(9)),
            ]),
            BufferPool::new(64 * 1024 * 1024),
            Some(Arc::clone(&relabel) as Arc<dyn ColumnRelabel>),
            None,
        )
        .unwrap();
        assert_eq!(relabel.0.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            r.counter_labels("cpu_cycles"),
            vec![BTreeMap::from([
                ("slot".to_string(), "0".to_string()),
                ("who".to_string(), "a".to_string())
            ])]
        );
    }

    /// A reader opened without a relabel is not reused by an open with one,
    /// whose identities for the same columns differ.
    #[test]
    fn open_after_a_reader_without_a_relabel_reads_everything() {
        let seg = |from| segment("cpu_cycles", &[("slot", "0")], &rows(from, 4));
        let pool = BufferPool::new(64 * 1024 * 1024);
        let segments = || vec![(Some(1), seg(1)), (Some(2), seg(5)), (None, seg(9))];
        let first = SegmentedParquetReader::open_after(
            KeyedSegments::new(segments()),
            Arc::clone(&pool),
            None,
            None,
        )
        .unwrap();
        answer(&first);
        let store = KeyedSegments::new(segments());
        let after = SegmentedParquetReader::open_after(
            Arc::clone(&store) as Arc<dyn SegmentStore>,
            Arc::clone(&pool),
            Some(Arc::new(AllA)),
            Some(&first.handover().unwrap()),
        )
        .unwrap();
        assert_eq!(store.fetched(), vec![0, 1, 2]);
        let fresh = SegmentedParquetReader::open_relabeled_with_pool(
            KeyedSegments::new(segments()),
            Arc::clone(&pool),
            Arc::new(AllA),
        )
        .unwrap();
        assert_eq!(
            after.counter_labels("cpu_cycles"),
            fresh.counter_labels("cpu_cycles")
        );
        assert_eq!(answer(&after), answer(&fresh));
    }

    /// A column naming an occupant the relabel does not describe leaves a
    /// reader with no handover; once described, it has one.
    #[test]
    fn an_undescribed_occupant_hands_nothing_on() {
        use crate::long::{OccupantLabels, OCCUPANT_LABEL};
        use metriken_storage::occupants::Occupant;
        let seg = |from| segment("cpu_cycles", &[(OCCUPANT_LABEL, "5")], &rows(from, 4));
        let pool = BufferPool::new(64 * 1024 * 1024);
        let segments = || vec![(Some(1), seg(1)), (Some(2), seg(5)), (None, seg(9))];
        let open = |rows: Vec<Occupant>| {
            SegmentedParquetReader::open_after(
                KeyedSegments::new(segments()),
                Arc::clone(&pool),
                Some(Arc::new(OccupantLabels::new(rows))),
                None,
            )
            .unwrap()
        };
        assert!(open(Vec::new()).handover().is_none());
        let described = Occupant {
            occupant: 5,
            labels: [("comm".to_string(), "w".to_string())].into(),
        };
        assert!(open(vec![described]).handover().is_some());
    }

    /// A batch read fetches only the segments its range touches.
    #[test]
    fn a_batch_read_fetches_only_the_segments_it_touches() {
        let store = KeyedSegments::new(vec![
            (Some(1), segment("cpu_cycles", &[], &[(1_000_000_000, 10)])),
            (Some(2), segment("cpu_cycles", &[], &[(2_000_000_000, 20)])),
            (
                Some(3),
                segment(
                    "cpu_cycles",
                    &[],
                    &[(9_000_000_000, 90), (10_000_000_000, 100)],
                ),
            ),
        ]);
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_with_pool(
            Arc::clone(&store) as Arc<dyn SegmentStore>,
            Arc::clone(&pool),
        )
        .unwrap();
        let opened = store.fetched();
        store.fetched.lock().unwrap().clear();
        let result = r
            .query_range("sum(rate(cpu_cycles[1s]))", 9.0, 10.0, 1.0)
            .unwrap();
        assert_eq!(pool.stats().misses, 0, "the batch path ran");
        assert_eq!(store.fetched(), vec![2], "opened {opened:?}");
        let QueryResult::Matrix { result } = result else {
            panic!("a matrix");
        };
        assert_eq!(result[0].values, vec![(10.0, 10.0)]);
    }

    /// One counter with a `duration` column and no window columns: rows
    /// `(timestamp, value, duration)`.
    fn segment_with_duration(rows: &[(u64, u64, u64)]) -> Vec<u8> {
        let mut metadata = HashMap::new();
        metadata.insert("metric".to_string(), "cpu_cycles".to_string());
        metadata.insert("metric_type".to_string(), "counter".to_string());
        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new("duration", DataType::UInt64, true),
            Field::new("cpu_cycles", DataType::UInt64, true).with_metadata(metadata),
        ]));
        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), None).unwrap();
        let col = |f: fn(&(u64, u64, u64)) -> u64| {
            Arc::new(UInt64Array::from(rows.iter().map(f).collect::<Vec<_>>())) as ArrayRef
        };
        let batch =
            RecordBatch::try_new(schema, vec![col(|r| r.0), col(|r| r.2), col(|r| r.1)]).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        buf
    }

    /// With windows from a `duration` column, a hole and a reset, the batch
    /// path gives what the per-series path gives.
    #[test]
    fn a_batch_read_with_a_duration_column_matches_the_per_series_path() {
        let s = 1_000_000_000u64;
        let segs = vec![
            segment_with_duration(&[(s, 10, 300), (2 * s, 20, 310), (3 * s, 35, 290)]),
            segment_with_duration(&[(8 * s, 90, 305), (9 * s, 4, 300), (10 * s, 30, 300)]),
        ];
        let per_series = crate::QueryOptions::default().with_per_series_rates(true);
        for q in ["rate(cpu_cycles[1s])", "sum(irate(cpu_cycles[1s]))"] {
            for step in [1.0, 0.5] {
                let pool = BufferPool::new(64 * 1024 * 1024);
                let r =
                    SegmentedParquetReader::open_bytes_with_pool(segs.clone(), Arc::clone(&pool))
                        .unwrap();
                let batch = r.query_range(q, 1.0, 10.0, step).unwrap();
                assert_eq!(pool.stats().misses, 0, "{q}: the batch path ran");
                let streams = r.query_range_opts(q, 1.0, 10.0, step, &per_series).unwrap();
                assert_eq!(
                    format!("{batch:?}"),
                    format!("{streams:?}"),
                    "{q} step {step}"
                );
            }
        }
    }

    /// A row of [`segment_with_windows`]: timestamp, value, duration,
    /// window begin offset and width.
    type WindowRow = (u64, u64, u64, Option<i64>, Option<u64>);

    /// A counter with `duration` and, when `windowed`, nullable
    /// `:window_begin`/`:window_width` columns: rows `(timestamp, value,
    /// duration, begin, width)`.
    fn segment_with_windows(rows: &[WindowRow], windowed: bool) -> Vec<u8> {
        let mut metadata = HashMap::new();
        metadata.insert("metric".to_string(), "cpu_cycles".to_string());
        metadata.insert("metric_type".to_string(), "counter".to_string());
        let mut fields = vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new("duration", DataType::UInt64, true),
        ];
        let mut cols: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.0).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.2).collect::<Vec<_>>(),
            )),
        ];
        if windowed {
            fields.push(Field::new(":window_begin", DataType::Int64, true));
            fields.push(Field::new(":window_width", DataType::UInt64, true));
            cols.push(Arc::new(arrow::array::Int64Array::from(
                rows.iter().map(|r| r.3).collect::<Vec<_>>(),
            )));
            cols.push(Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.4).collect::<Vec<_>>(),
            )));
        }
        fields.push(Field::new("cpu_cycles", DataType::UInt64, true).with_metadata(metadata));
        cols.push(Arc::new(UInt64Array::from(
            rows.iter().map(|r| r.1).collect::<Vec<_>>(),
        )));
        let schema = Arc::new(Schema::new(fields));
        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), None).unwrap();
        writer
            .write(&RecordBatch::try_new(schema, cols).unwrap())
            .unwrap();
        writer.close().unwrap();
        buf
    }

    /// Where a row's window columns are null, and in a later segment with
    /// only a duration column, the window comes from the duration on both
    /// paths, and the bands agree.
    #[test]
    fn a_batch_read_with_mixed_windows_matches_the_per_series_path() {
        let s = 1_000_000_000u64;
        let segs = vec![
            segment_with_windows(
                &[
                    (s, 10, 300_000, Some(-5), Some(100)),
                    (2 * s, 20, 310_000, None, None),
                    (3 * s, 35, 290_000, Some(-7), Some(120)),
                    (4 * s, 50, 300_000, Some(-3), Some(90)),
                ],
                true,
            ),
            segment_with_windows(
                &[
                    (5 * s, 60, 305_000, None, None),
                    (6 * s, 75, 300_000, None, None),
                    (7 * s, 90, 300_000, None, None),
                ],
                false,
            ),
        ];
        let per_series = crate::QueryOptions::default().with_per_series_rates(true);
        for q in ["rate(cpu_cycles[1s])", "sum(irate(cpu_cycles[1s]))"] {
            for step in [1.0, 0.5] {
                let pool = BufferPool::new(64 * 1024 * 1024);
                let r =
                    SegmentedParquetReader::open_bytes_with_pool(segs.clone(), Arc::clone(&pool))
                        .unwrap();
                let batch = r.query_range(q, 1.0, 7.0, step).unwrap();
                assert_eq!(pool.stats().misses, 0, "{q}: the batch path ran");
                let streams = r.query_range_opts(q, 1.0, 7.0, step, &per_series).unwrap();
                assert_eq!(
                    format!("{batch:?}"),
                    format!("{streams:?}"),
                    "{q} step {step}"
                );
            }
        }
    }

    /// Samples outside the range a query reads do not make edges outside
    /// it observed: with samples half a second off the grid, a query over
    /// 10-15 s has points at 11-14 s on both paths.
    #[test]
    fn a_batch_read_keeps_to_the_range() {
        let rows: Vec<(u64, u64)> = (0..30u64)
            .map(|k| (k * 1_000_000_000 + 500_000_000, k * 10))
            .collect();
        let r = SegmentedParquetReader::open_bytes_with_pool(
            vec![segment("cpu_cycles", &[], &rows)],
            BufferPool::new(64 * 1024 * 1024),
        )
        .unwrap();
        let per_series = crate::QueryOptions::default().with_per_series_rates(true);
        let batch = r
            .query_range("rate(cpu_cycles[1s])", 10.0, 15.0, 1.0)
            .unwrap();
        let streams = r
            .query_range_opts("rate(cpu_cycles[1s])", 10.0, 15.0, 1.0, &per_series)
            .unwrap();
        assert_eq!(format!("{batch:?}"), format!("{streams:?}"));
        let QueryResult::Matrix { result } = batch else {
            panic!("a matrix");
        };
        let times: Vec<f64> = result[0].values.iter().map(|(t, _)| *t).collect();
        assert_eq!(times, vec![11.0, 12.0, 13.0, 14.0]);
    }

    /// A store whose segments become unreadable after open.
    struct Failing {
        segments: Vec<Bytes>,
        fail: std::sync::atomic::AtomicBool,
    }

    impl SegmentStore for Failing {
        fn len(&self) -> usize {
            self.segments.len()
        }

        fn bytes(&self, idx: usize) -> SegmentBytes {
            if idx == 1 && self.fail.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("unreadable".into());
            }
            Ok(self.segments.get(idx).cloned())
        }
    }

    /// A segment that cannot be read ends the batch read: the query takes
    /// the per-series path and answers as it does, rather than losing that
    /// segment's rows to the batch path alone.
    #[test]
    fn an_unreadable_segment_falls_back_to_the_per_series_path() {
        let s = 1_000_000_000u64;
        let store = Arc::new(Failing {
            segments: vec![
                Bytes::from(segment("cpu_cycles", &[], &[(s, 10), (2 * s, 20)])),
                Bytes::from(segment("cpu_cycles", &[], &[(3 * s, 35), (4 * s, 50)])),
                Bytes::from(segment("cpu_cycles", &[], &[(5 * s, 70), (6 * s, 80)])),
            ],
            fail: std::sync::atomic::AtomicBool::new(false),
        });
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_with_pool(
            Arc::clone(&store) as Arc<dyn SegmentStore>,
            Arc::clone(&pool),
        )
        .unwrap();
        store.fail.store(true, std::sync::atomic::Ordering::Relaxed);
        let batch = r
            .query_range("sum(rate(cpu_cycles[1s]))", 1.0, 6.0, 1.0)
            .unwrap();
        assert!(pool.stats().misses > 0, "the per-series path ran");
        let per_series = crate::QueryOptions::default().with_per_series_rates(true);
        let streams = r
            .query_range_opts("sum(rate(cpu_cycles[1s]))", 1.0, 6.0, 1.0, &per_series)
            .unwrap();
        assert_eq!(format!("{batch:?}"), format!("{streams:?}"));
    }

    /// An expression reading two tables through a union takes the batch path
    /// on each, and answers as the per-series path does.
    #[test]
    fn a_union_of_two_tables_takes_the_batch_path() {
        let s = 1_000_000_000u64;
        let rows_a = [(s, 10), (2 * s, 20), (3 * s, 35), (4 * s, 50)];
        let rows_b = [(s, 1), (2 * s, 3), (3 * s, 4), (4 * s, 9)];
        let pool_a = BufferPool::new(64 * 1024 * 1024);
        let pool_b = BufferPool::new(64 * 1024 * 1024);
        let a = SegmentedParquetReader::open_bytes_with_pool(
            vec![segment("cpu_cycles", &[], &rows_a)],
            Arc::clone(&pool_a),
        )
        .unwrap();
        let b = SegmentedParquetReader::open_bytes_with_pool(
            vec![segment("cpu_instructions", &[], &rows_b)],
            Arc::clone(&pool_b),
        )
        .unwrap();
        let union = crate::UnionMetricsSource::try_new(vec![
            crate::UnionChild::from(&a),
            crate::UnionChild::from(&b),
        ])
        .unwrap();
        let q = "sum(irate(cpu_cycles[1s])) / sum(irate(cpu_instructions[1s]))";
        let batch = union.query_range(q, 2.0, 4.0, 1.0).unwrap();
        assert_eq!(pool_a.stats().misses, 0, "cpu_cycles took the batch path");
        assert_eq!(
            pool_b.stats().misses,
            0,
            "cpu_instructions took the batch path"
        );
        let per_series = crate::QueryOptions::default().with_per_series_rates(true);
        let streams = union
            .query_range_opts(q, 2.0, 4.0, 1.0, &per_series)
            .unwrap();
        assert!(pool_a.stats().misses > 0, "the per-series path ran");
        assert_eq!(format!("{batch:?}"), format!("{streams:?}"));
        let QueryResult::Matrix { result } = batch else {
            panic!("a matrix");
        };
        assert_eq!(result[0].values, vec![(2.0, 5.0), (3.0, 15.0), (4.0, 3.0)]);
    }

    /// A display query reduces each series as the engine produces it, and
    /// gives what reducing the full matrix gives.
    #[test]
    fn a_display_query_matches_reducing_the_matrix() {
        use crate::{DisplayOptions, MetricsSource};
        let s = 1_000_000_000u64;
        let rows = |k: u64| -> Vec<(u64, u64)> {
            (1..=40)
                .filter(|t| *t != 17)
                .map(|t| (t * s, t * t * k))
                .collect()
        };
        let pool = BufferPool::new(64 * 1024 * 1024);
        let r = SegmentedParquetReader::open_bytes_with_pool(
            vec![
                segment("cpu_cycles", &[("id", "0")], &rows(1)[..20]),
                segment("cpu_cycles", &[("id", "1")], &rows(3)[..20]),
                segment("cpu_cycles", &[("id", "0")], &rows(1)[20..]),
                segment("cpu_cycles", &[("id", "1")], &rows(3)[20..]),
            ],
            Arc::clone(&pool),
        )
        .unwrap();
        let windows: Vec<WindowRow> = (1..=40u64)
            .filter(|t| !(15..=20).contains(t))
            .map(|t| {
                let w = (t != 9).then_some(100_000_000);
                (t * s, t * t, 300, w.map(|_| -50_000_000), w)
            })
            .collect();
        let windowed_pool = BufferPool::new(64 * 1024 * 1024);
        let windowed = SegmentedParquetReader::open_bytes_with_pool(
            vec![
                segment_with_windows(&windows[..17], true),
                segment_with_windows(&windows[17..], true),
            ],
            Arc::clone(&windowed_pool),
        )
        .unwrap();
        // Flat from 10 s to 20 s: a zero rate, which `1 / rate` drops.
        let flat_rows: Vec<(u64, u64)> = (1..=40u64)
            .map(|t| (t * s, t.clamp(10, 20) * 7 + t.saturating_sub(20) * 3))
            .collect();
        let flat = SegmentedParquetReader::open_bytes_with_pool(
            vec![segment("cpu_cycles", &[("id", "0")], &flat_rows)],
            BufferPool::new(64 * 1024 * 1024),
        )
        .unwrap();
        // Enough series that one group is spread across partitions.
        let many = SegmentedParquetReader::open_bytes_with_pool(
            (0..20u64)
                .map(|id| {
                    let id_s = id.to_string();
                    segment("cpu_cycles", &[("id", id_s.as_str())], &rows(id + 1))
                })
                .collect(),
            BufferPool::new(64 * 1024 * 1024),
        )
        .unwrap();
        let options = [
            crate::QueryOptions::default(),
            crate::QueryOptions::default().with_per_series_rates(true),
            crate::QueryOptions::with_rate_mode(crate::RateMode::Raw),
            crate::QueryOptions::default()
                .with_eval_timestamps(Some((1..=13u64).map(|k| k * 3 * s).collect())),
        ];
        let displays = || crate::promql::streaming::dispatch::BATCH_DISPLAYS.with(|n| n.get());
        // The per-series path gives `by` groups in no fixed order, so the
        // series are compared as a set.
        let sorted = |d: &crate::DisplayResult| {
            let mut v = serde_json::to_value(d).unwrap();
            if let Some(series) = v["result"].as_array_mut() {
                series.sort_by_key(|s| s["metric"].to_string());
            }
            v
        };
        // Whether the source computes the display query as it reads, with
        // the default options.
        for (r, q, as_read) in [
            (&r, "rate(cpu_cycles[1s])", true),
            (&r, "sum(irate(cpu_cycles[1s]))", true),
            (&windowed, "rate(cpu_cycles[1s])", true),
            (&windowed, "irate(cpu_cycles[1s]) / 1000", true),
            (&windowed, "2 * (rate(cpu_cycles[1s]) - 3)", true),
            (&windowed, "1 / rate(cpu_cycles[1s])", true),
            (&windowed, "sum(irate(cpu_cycles[1s])) * 2", true),
            (&r, "sum by (id) (rate(cpu_cycles[2s]))", true),
            (&r, "irate(cpu_cycles[1s]) - 40", true),
            (&r, "5 - irate(cpu_cycles[1s])", true),
            (&r, "irate(cpu_cycles[1s]) * 2", true),
            (&windowed, "avg(irate(cpu_cycles[1s]))", true),
            (&r, "min(irate(cpu_cycles[1s]))", true),
            (&r, "max by (id) (irate(cpu_cycles[1s]))", true),
            (&r, "count(irate(cpu_cycles[1s]))", true),
            (&r, "sum without (id) (irate(cpu_cycles[1s])) / 1000", true),
            (&r, "sum(irate(cpu_cycles[1s]) * 2)", false),
            (&flat, "1 / rate(cpu_cycles[1s])", true),
            (&flat, "sum(1 / rate(cpu_cycles[1s]))", false),
            (&many, "sum(irate(cpu_cycles[1s]))", true),
            (&many, "avg by (id) (irate(cpu_cycles[1s]))", true),
            (&r, "rate(not_a_metric[1s])", false),
        ] {
            for budget in [0, 5, 50] {
                let display = DisplayOptions {
                    budget,
                    ..Default::default()
                };
                for (o, qopts) in options.iter().enumerate() {
                    let before = displays();
                    let streamed = r.query_range_display_opts(q, 1.0, 40.0, 1.0, &display, qopts);
                    assert_eq!(
                        displays() > before,
                        as_read && o == 0,
                        "{q} options {o}: computed as read"
                    );
                    let reduced = r
                        .query_range_opts(q, 1.0, 40.0, 1.0, qopts)
                        .map(|m| crate::display::display_from_result(m, 1.0, 40.0, 1.0, &display));
                    match (streamed, reduced) {
                        (Ok(a), Ok(b)) => {
                            assert_eq!(sorted(&a), sorted(&b), "{q} options {o} budget {budget}")
                        }
                        (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{q}"),
                        (a, b) => panic!("{q} options {o}: {a:?} against {b:?}"),
                    }
                }
            }
        }
    }

    /// The shape of [`grouped_windows`]' recording.
    #[derive(Clone, Copy, Default)]
    struct Windows {
        /// Series 2 comes back in the twenty-sixth window after stopping.
        resume: bool,
        /// Each series' samples are offset by a fraction of a second and
        /// the series of a group are weighted 1, 1e6 and 1e12, so the order
        /// in which a group's points are summed shows in the low bits.
        offsets: bool,
        /// Series 5 starts at 2001 s, before the end of the second block of
        /// grid points, and is alone in group `d`, so only the bound for a
        /// series with no sample yet holds that block back.
        late: bool,
        /// A seventh series, alone in group `c`, has five samples.
        short: bool,
        /// Seconds between samples.
        every: u64,
    }

    /// Thirty windows of 100 s of six series in two groups, in time order;
    /// series 2 stops after the ninth window.
    fn grouped_windows(w: Windows) -> SegmentedParquetReader {
        let s = 1_000_000_000u64;
        let every = w.every.max(1);
        let mut segments = Vec::new();
        for win in 0..30u64 {
            for id in 0..7u64 {
                let stopped = id == 2 && win >= 9 && !(w.resume && win >= 25);
                let early = id == 5 && w.late && win < 20;
                let short = id == 6 && !(w.short && win == 9);
                if stopped || early || short {
                    continue;
                }
                let offset = if w.offsets { (id + 1) * 137_000_000 } else { 0 };
                let weight = if w.offsets {
                    1_000_000u64.pow((id % 3) as u32)
                } else {
                    1
                };
                let ticks: Vec<u64> = if id == 6 {
                    (950..955).collect()
                } else {
                    (win * 100 + 1..=win * 100 + 100)
                        .filter(|t| t % every == 0)
                        .collect()
                };
                let rows: Vec<(u64, u64)> = ticks
                    .into_iter()
                    .map(|t| {
                        let v = t * (id + 1) * 3 + (t * 7919 * (id + 1)) % 3;
                        (t * s + offset, v * weight)
                    })
                    .collect();
                let id_s = id.to_string();
                let g = match id {
                    0..3 => "a",
                    5 if w.late => "d",
                    3..6 => "b",
                    _ => "c",
                };
                segments.push(segment(
                    "cpu_cycles",
                    &[("id", id_s.as_str()), ("g", g)],
                    &rows,
                ));
            }
        }
        SegmentedParquetReader::open_bytes_with_pool(segments, BufferPool::new(64 * 1024 * 1024))
            .unwrap()
    }

    /// Each chunk's `rest_start` is the catalog start of the first segment
    /// after it, and `None` after the last, at any chunk size.
    #[test]
    fn rest_start_is_the_start_of_the_next_unread_segment() {
        let r = grouped_windows(Windows::default());
        let starts: Vec<u64> = r
            .source
            .segment_spans()
            .into_iter()
            .map(|span| span.expect("every segment has a span").0)
            .collect();
        for n in [1, 3, 8] {
            let mut scan = r
                .source
                .counter_scan("cpu_cycles", &Labels::default(), 0, u64::MAX)
                .expect("cpu_cycles scans");
            let mut read = 0;
            while let Some(chunk) = scan.next_chunk(n).expect("no read error") {
                read += chunk.segments.len();
                let next = starts[read..].iter().min().copied();
                assert_eq!(chunk.rest_start, next, "chunk size {n}, {read} read");
            }
            assert_eq!(read, starts.len(), "chunk size {n}");
        }
    }

    #[test]
    fn a_grouped_display_query_feeds_final_points_before_the_end() {
        use crate::{DisplayOptions, MetricsSource};
        let displays = || crate::promql::streaming::dispatch::BATCH_DISPLAYS.with(|n| n.get());
        let fed = || crate::batch_rate::FED_BEFORE_END.with(|n| n.get());
        let base = Windows::default();
        // Each shape, and whether a stopped series comes back.
        for w in [
            base,
            Windows {
                resume: true,
                ..base
            },
            Windows {
                offsets: true,
                late: true,
                ..base
            },
            Windows {
                short: true,
                ..base
            },
            Windows { every: 15, ..base },
        ] {
            let r = grouped_windows(w);
            for q in [
                "sum by (g) (irate(cpu_cycles[1s]))",
                "avg(rate(cpu_cycles[1s])) / 2",
                "max by (g) (irate(cpu_cycles[1s]))",
            ] {
                for budget in [0, 50] {
                    let display = DisplayOptions {
                        budget,
                        ..Default::default()
                    };
                    let qopts = crate::QueryOptions::default();
                    let before = displays();
                    crate::batch_rate::FED_BEFORE_END.with(|n| n.set(0));
                    let streamed = r
                        .query_range_display_opts(q, 1.0, 3000.0, 1.0, &display, &qopts)
                        .unwrap();
                    assert_eq!(displays() > before, !w.resume, "{q}");
                    // Three blocks of grid points per group. The first two
                    // are final before the last chunk is read, in group `a`
                    // only once series 2 is ended early, and in group `c`
                    // once its short series is.
                    if !w.resume {
                        let crate::DisplayResult::Series { result, .. } = &streamed else {
                            panic!("{q}: series");
                        };
                        assert_eq!(fed(), 2 * result.len(), "{q}: blocks fed before the end");
                    }
                    let matrix = r.query_range_opts(q, 1.0, 3000.0, 1.0, &qopts).unwrap();
                    let reduced =
                        crate::display::display_from_result(matrix, 1.0, 3000.0, 1.0, &display);
                    assert_eq!(
                        serde_json::to_value(&streamed).unwrap(),
                        serde_json::to_value(&reduced).unwrap(),
                        "{q} budget {budget}"
                    );
                }
            }
        }
    }
}
