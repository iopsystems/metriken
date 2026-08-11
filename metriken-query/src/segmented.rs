//! SegmentedParquetReader: presents an ordered list of parquet byte blobs
//! (segments of one logical table) as a single MetricsSource. Open is
//! footer-only per segment; queries decode only the row groups they touch,
//! spliced in segment order. Same-identity columns across segments are ONE
//! series (unlike MultiParquetSource, which duplicates).

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::sync::Arc;

use crate::{BufferPool, MetricsSource, ParquetReader, QueryError, QueryResult};

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
/// Query splicing across segments (`query_range` / `query` / `columns`) is
/// not implemented yet; those methods return `Err(QueryError::Unsupported)`.
/// This type currently exposes only the footer-derived identity surface:
/// metric names, labels, time range, and file metadata.
pub struct SegmentedParquetReader {
    /// Segments in logical (time) order. Each is opened footer-only and all
    /// share `pool`'s decode-cache budget.
    segments: Vec<ParquetReader>,
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
        Ok(Self { segments })
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

impl MetricsSource for SegmentedParquetReader {
    fn query_range(
        &self,
        _expr: &str,
        _start_s: f64,
        _end_s: f64,
        _step_s: f64,
    ) -> Result<QueryResult, QueryError> {
        // A3: splice per-segment row groups in segment order for the touched
        // time range. Not implemented yet.
        Err(QueryError::Unsupported(
            "SegmentedParquetReader: query splicing across segments not yet implemented".into(),
        ))
    }

    fn query(&self, _expr: &str, _time: Option<f64>) -> Result<QueryResult, QueryError> {
        // A3: same splicing dependency as query_range.
        Err(QueryError::Unsupported(
            "SegmentedParquetReader: query splicing across segments not yet implemented".into(),
        ))
    }

    fn columns(&self, _query: &str) -> Result<std::collections::HashSet<String>, QueryError> {
        // A3: resolving a query to physical columns needs the same
        // cross-segment query machinery as query_range/query.
        Err(QueryError::Unsupported(
            "SegmentedParquetReader: query splicing across segments not yet implemented".into(),
        ))
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
}
