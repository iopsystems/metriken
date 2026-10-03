//! The long segment layout, read. The format is `metriken-segment`'s
//! ([`metriken_segment::long`]), re-exported here so existing paths keep
//! working; this module adds the reader's side of an occupant's labels,
//! [`OccupantLabels`].

pub use metriken_segment::long::*;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::labels::Labels;
use crate::segmented::{ColumnRelabel, Run};

/// Puts each occupant's labels on its series: a long table's series carry
/// only [`OCCUPANT_LABEL`] and the metric column's fixed labels, and this is
/// metriken-query's hook for the rest.
///
/// An occupant never changes labels, so a series is one run. The occupant
/// number stays on the series: two occupants can share every other label
/// (a recording with no `__uid__`), and it keeps them apart. It is internal
/// by the `__` rule, so listings and legends hide it.
pub struct OccupantLabels {
    labels: HashMap<u64, BTreeMap<String, String>>,
    /// Every key an occupant supplies. A query filter on one of these cannot
    /// be answered by the columns, so it is taken off the segment filter and
    /// applied to the relabelled series.
    keys: BTreeSet<String>,
}

impl OccupantLabels {
    /// From an occupant stream's rows. An occupant restated with the same
    /// labels is one entry; restated with different ones is a defect of the
    /// writer, and the first labels win.
    pub fn new(rows: impl IntoIterator<Item = metriken_segment::occupants::Occupant>) -> Self {
        let mut labels: HashMap<u64, BTreeMap<String, String>> = HashMap::new();
        let mut keys = BTreeSet::new();
        for o in rows {
            keys.extend(o.labels.keys().cloned());
            labels.entry(o.occupant).or_insert(o.labels);
        }
        Self { labels, keys }
    }

    pub fn len(&self) -> usize {
        self.labels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }

    fn with(&self, labels: &Labels) -> Option<Labels> {
        let occ: u64 = labels.inner.get(OCCUPANT_LABEL)?.parse().ok()?;
        let extra = self.labels.get(&occ)?;
        let mut out = labels.clone();
        for (k, v) in extra {
            out.inner.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Some(out)
    }
}

impl ColumnRelabel for OccupantLabels {
    /// Returns true: an occupant's labels do not change, and each occupant
    /// column presents as one label set for all its samples. A segment
    /// naming an occupant the stream does not describe saves no state
    /// (see [`SegmentedParquetReader::handover`](crate::SegmentedParquetReader::handover)).
    fn identities_are_fixed(&self) -> bool {
        true
    }

    fn identities(&self, _name: &str, labels: &Labels) -> Option<Vec<Labels>> {
        self.with(labels).map(|l| vec![l])
    }

    fn split(&self, _name: &str, labels: &Labels, timestamps: &[u64]) -> Option<Vec<Run>> {
        self.with(labels).map(|l| vec![(l, 0..timestamps.len())])
    }

    fn at(&self, _name: &str, labels: &Labels, _timestamp: u64) -> Option<Labels> {
        self.with(labels)
    }

    fn segment_filter(&self, _name: &str, filter: &Labels) -> Labels {
        let mut f = filter.clone();
        f.inner.retain(|k, _| !self.keys.contains(k));
        f
    }
}

#[cfg(test)]
mod relabel_tests {
    use super::*;
    use metriken_segment::occupants::Occupant;

    fn occ(n: u64, pairs: &[(&str, &str)]) -> Occupant {
        Occupant {
            occupant: n,
            labels: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn the_relabel_adds_an_occupants_labels_and_keeps_its_number() {
        let r = OccupantLabels::new([occ(5, &[("comm", "w"), ("id", "3")])]);
        let col = Labels::from([(OCCUPANT_LABEL, "5"), ("state", "user")]);
        let got = r.identities("cpu", &col).unwrap();
        assert_eq!(
            got,
            vec![Labels::from([
                (OCCUPANT_LABEL, "5"),
                ("state", "user"),
                ("comm", "w"),
                ("id", "3")
            ])]
        );
        // An occupant the stream does not know is left alone.
        assert!(r
            .identities("cpu", &Labels::from([(OCCUPANT_LABEL, "6")]))
            .is_none());
        // Filters on occupant keys come off the segment filter.
        let f = r.segment_filter("cpu", &Labels::from([("comm", "w"), ("state", "user")]));
        assert_eq!(f, Labels::from([("state", "user")]));
    }
}

/// The reader over long segments: every query a long table answers must
/// equal what the wide table it replaces answers, when the wide columns
/// carry the occupant as a label.
#[cfg(test)]
mod reader_tests {
    use std::collections::{BTreeSet, HashMap};
    use std::sync::Arc;

    use arrow::array::{ArrayRef, Int64Array, ListArray, UInt64Array};
    use arrow::buffer::OffsetBuffer;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;

    use super::*;
    use crate::segmented::{ColumnRelabel, InMemorySegments, Run, SegmentedParquetReader};
    use crate::{BufferPool, Labels, MetricsSource};

    const GP: u8 = 2;
    const MVP: u8 = 8;
    const BUCKETS: usize = 28;

    /// One observation: an occupant's values at a tick. The table-level
    /// window is derived from the tick so every occupant at a tick shares it.
    #[derive(Clone)]
    struct Obs {
        ts: u64,
        occ: u64,
        cpu: Option<u64>,
        depth: Option<i64>,
        lat: Option<Vec<u64>>,
    }

    fn window(ts: u64) -> (i64, u64) {
        (-((ts / 1_000_000) as i64 % 7), 1_000 + ts % 5)
    }

    fn meta(metric: &str, kind: &str, extra: &[(&str, String)]) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("metric".to_string(), metric.to_string());
        m.insert("metric_type".to_string(), kind.to_string());
        if kind == "histogram" {
            m.insert("grouping_power".to_string(), GP.to_string());
            m.insert("max_value_power".to_string(), MVP.to_string());
        }
        for (k, v) in extra {
            m.insert(k.to_string(), v.clone());
        }
        m
    }

    fn list(values: &[Option<Vec<u64>>]) -> ArrayRef {
        let item = Arc::new(Field::new("item", DataType::UInt64, true));
        let mut offsets = vec![0i32];
        let mut flat: Vec<u64> = Vec::new();
        let mut valid = Vec::new();
        for v in values {
            if let Some(b) = v {
                flat.extend(b);
            }
            valid.push(v.is_some());
            offsets.push(flat.len() as i32);
        }
        Arc::new(ListArray::new(
            item,
            OffsetBuffer::new(offsets.into()),
            Arc::new(UInt64Array::from(flat)),
            Some(valid.into()),
        ))
    }

    fn list_field(name: &str, m: HashMap<String, String>) -> Field {
        let item = Arc::new(Field::new("item", DataType::UInt64, true));
        Field::new(name, DataType::List(item), true).with_metadata(m)
    }

    fn write(fields: Vec<Field>, cols: Vec<ArrayRef>, kv: Vec<(&str, String)>) -> Vec<u8> {
        write_paged(fields, cols, kv, None)
    }

    /// [`write`], with data pages of at most `page_rows` rows when given.
    fn write_paged(
        fields: Vec<Field>,
        cols: Vec<ArrayRef>,
        kv: Vec<(&str, String)>,
        page_rows: Option<usize>,
    ) -> Vec<u8> {
        let schema = Arc::new(Schema::new(fields));
        let mut kv: Vec<KeyValue> = kv
            .into_iter()
            .map(|(k, v)| KeyValue {
                key: k.to_string(),
                value: Some(v),
            })
            .collect();
        kv.push(KeyValue {
            key: "sampling_interval_ms".to_string(),
            value: Some("1000".to_string()),
        });
        let mut props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_key_value_metadata(Some(kv));
        if let Some(n) = page_rows {
            props = props
                .set_data_page_row_count_limit(n)
                .set_write_batch_size(n);
        }
        let props = props.build();
        let mut buf = Vec::new();
        let mut w = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();
        w.write(&RecordBatch::try_new(schema, cols).unwrap())
            .unwrap();
        w.close().unwrap();
        buf
    }

    /// A long segment of `rows`, in the given order. `occupants` overrides
    /// the footer's occupant list.
    fn long(rows: &[Obs], occupants: Option<&str>) -> Vec<u8> {
        long_paged(rows, occupants, None)
    }

    fn long_paged(rows: &[Obs], occupants: Option<&str>, page_rows: Option<usize>) -> Vec<u8> {
        let list_value = occupants
            .map(str::to_string)
            .unwrap_or_else(|| encode_occupant_ranges(rows.iter().map(|r| r.occ)));
        write_paged(
            vec![
                Field::new("timestamp", DataType::UInt64, false),
                Field::new(":window_begin", DataType::Int64, true),
                Field::new(":window_width", DataType::UInt64, true),
                Field::new(OCCUPANT_COLUMN, DataType::UInt64, false),
                Field::new("1", DataType::UInt64, true).with_metadata(meta("cpu", "counter", &[])),
                Field::new("2", DataType::Int64, true).with_metadata(meta("depth", "gauge", &[])),
                list_field("3:buckets", meta("lat", "histogram", &[])),
            ],
            vec![
                Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.ts))),
                Arc::new(Int64Array::from_iter_values(
                    rows.iter().map(|r| window(r.ts).0),
                )),
                Arc::new(UInt64Array::from_iter_values(
                    rows.iter().map(|r| window(r.ts).1),
                )),
                Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.occ))),
                Arc::new(UInt64Array::from_iter(rows.iter().map(|r| r.cpu))),
                Arc::new(Int64Array::from_iter(rows.iter().map(|r| r.depth))),
                list(&rows.iter().map(|r| r.lat.clone()).collect::<Vec<_>>()),
            ],
            vec![
                (LAYOUT_KEY, LAYOUT_LONG.to_string()),
                (OCCUPANTS_KEY, list_value),
            ],
            page_rows,
        )
    }

    /// The same observations as a wide segment: a column per metric and
    /// occupant, labelled with the occupant, null where it has no value.
    fn wide(rows: &[Obs]) -> Vec<u8> {
        let ticks: Vec<u64> = rows
            .iter()
            .map(|r| r.ts)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let occs: Vec<u64> = rows
            .iter()
            .map(|r| r.occ)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let at = |t: u64, o: u64| rows.iter().find(|r| r.ts == t && r.occ == o);
        let mut fields = vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new(":window_begin", DataType::Int64, true),
            Field::new(":window_width", DataType::UInt64, true),
        ];
        let mut cols: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(ticks.clone())),
            Arc::new(Int64Array::from_iter_values(
                ticks.iter().map(|t| window(*t).0),
            )),
            Arc::new(UInt64Array::from_iter_values(
                ticks.iter().map(|t| window(*t).1),
            )),
        ];
        for &o in &occs {
            let label = [(OCCUPANT_LABEL, o.to_string())];
            fields.push(
                Field::new(format!("1x{o}"), DataType::UInt64, true)
                    .with_metadata(meta("cpu", "counter", &label)),
            );
            cols.push(Arc::new(UInt64Array::from_iter(
                ticks.iter().map(|t| at(*t, o).and_then(|r| r.cpu)),
            )));
            fields.push(
                Field::new(format!("2x{o}"), DataType::Int64, true)
                    .with_metadata(meta("depth", "gauge", &label)),
            );
            cols.push(Arc::new(Int64Array::from_iter(
                ticks.iter().map(|t| at(*t, o).and_then(|r| r.depth)),
            )));
            fields.push(list_field(
                &format!("3x{o}:buckets"),
                meta("lat", "histogram", &label),
            ));
            cols.push(list(
                &ticks
                    .iter()
                    .map(|t| at(*t, o).and_then(|r| r.lat.clone()))
                    .collect::<Vec<_>>(),
            ));
        }
        write(fields, cols, vec![])
    }

    fn buckets(n: u64) -> Vec<u64> {
        let mut b = vec![0u64; BUCKETS];
        b[(n % BUCKETS as u64) as usize] = n;
        b[3] += 1;
        b
    }

    /// Three occupants over six ticks: 0 lives throughout, 1 leaves after
    /// tick 3, 2 arrives at tick 3; 0 has no gauge at tick 4. Histograms are
    /// present wherever an occupant is, since a wide table's null list cell
    /// and a long table's absent row are not the same thing to a stream.
    fn observations() -> Vec<Obs> {
        let mut rows = Vec::new();
        for tick in 1..=6u64 {
            let ts = tick * 1_000_000_000;
            for occ in 0..3u64 {
                let alive = match occ {
                    0 => true,
                    1 => tick <= 3,
                    _ => tick >= 3,
                };
                if !alive {
                    continue;
                }
                rows.push(Obs {
                    ts,
                    occ,
                    cpu: Some(tick * (occ + 1) * 10 + occ),
                    depth: (!(occ == 0 && tick == 4)).then_some(tick as i64 - occ as i64),
                    lat: Some(buckets(tick + occ)),
                });
            }
        }
        rows
    }

    fn open(segments: Vec<Vec<u8>>) -> SegmentedParquetReader {
        SegmentedParquetReader::open_bytes_with_pool(segments, BufferPool::new(64 << 20)).unwrap()
    }

    /// Split at tick 3.5 into two segments, the way a writer seals.
    fn halves(rows: &[Obs]) -> (Vec<Obs>, Vec<Obs>) {
        rows.iter().cloned().partition(|r| r.ts < 3_500_000_000)
    }

    // No bare counter selector: the engine answers counters through rate()
    // only, whatever the layout.
    const QUERIES: &[&str] = &[
        "rate(cpu[2s])",
        "sum(rate(cpu[2s]))",
        "irate(cpu{__occupant__=\"1\"}[2s])",
        "depth",
        "max(depth)",
        // Only the occupant present at every tick: a wide file decodes a null
        // histogram cell as an empty snapshot and emits it as a row, so an
        // occupant absent from some ticks reads differently there. In a long
        // segment an absent observation has no row.
        "histogram_mean(lat{__occupant__=\"0\"})",
        "histogram_count(lat{__occupant__=\"0\"})",
    ];

    /// A result as JSON, whose maps are ordered, so two results compare
    /// equal when they hold the same series whatever order their labels
    /// were inserted in.
    fn canonical(r: &crate::QueryResult) -> serde_json::Value {
        serde_json::to_value(r).unwrap()
    }

    fn assert_same(a: &SegmentedParquetReader, b: &SegmentedParquetReader) {
        for q in QUERIES {
            let ra = a.query_range(q, 1.0, 6.0, 1.0).unwrap();
            let rb = b.query_range(q, 1.0, 6.0, 1.0).unwrap();
            assert_eq!(canonical(&ra), canonical(&rb), "range query {q}");
            // Instant queries: the same answer, or the same refusal.
            let ia = a
                .query(q, Some(5.0))
                .map(|r| canonical(&r))
                .map_err(|e| format!("{e:?}"));
            let ib = b
                .query(q, Some(5.0))
                .map(|r| canonical(&r))
                .map_err(|e| format!("{e:?}"));
            assert_eq!(ia, ib, "instant query {q}");
        }
        for name in ["cpu", "depth", "lat"] {
            let mut la = a.counter_labels(name);
            la.extend(a.gauge_labels(name));
            la.extend(a.histogram_labels(name));
            let mut lb = b.counter_labels(name);
            lb.extend(b.gauge_labels(name));
            lb.extend(b.histogram_labels(name));
            la.sort();
            lb.sort();
            assert_eq!(la, lb, "labels of {name}");
        }
        assert_eq!(
            MetricsSource::sample_timestamps(a),
            MetricsSource::sample_timestamps(b)
        );
    }

    #[test]
    fn a_long_table_reads_as_the_wide_table_it_replaces() {
        let rows = observations();
        let (a, b) = halves(&rows);
        let wide = open(vec![wide(&a), wide(&b)]);
        let long = open(vec![long(&a, None), long(&b, None)]);
        assert_eq!(wide.counter_labels("cpu").len(), 3);
        assert_same(&wide, &long);
    }

    /// A counter series as its sorted labels and `(ts, value, window)` samples.
    type Streamed = (Vec<(String, String)>, Vec<(u64, u64, Option<(u64, u64)>)>);

    /// Every counter series, read the way a union reads a segmented child:
    /// stream by stream, one column position at a time.
    fn streamed(r: &SegmentedParquetReader) -> Vec<Streamed> {
        let source = r.data_source();
        let mut out: Vec<_> = source
            .counter_streams("cpu", &Labels::default(), 0, u64::MAX)
            .unwrap()
            .into_iter()
            .map(|s| {
                let mut labels: Vec<(String, String)> =
                    s.labels.inner.clone().into_iter().collect();
                labels.sort();
                let samples = s.samples.map(|x| (x.ts, x.value, x.window)).collect();
                (labels, samples)
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn counter_streams_read_one_occupant_per_stream() {
        let rows = observations();
        let (a, b) = halves(&rows);
        let wide = open(vec![wide(&a), wide(&b)]);
        let long = open(vec![long(&a, None), long(&b, None)]);
        let w = streamed(&wide);
        assert_eq!(w.len(), 3);
        assert_eq!(w, streamed(&long));
    }

    #[test]
    fn row_order_within_a_long_segment_does_not_matter() {
        let rows = observations();
        let (mut a, mut b) = halves(&rows);
        let arrival = open(vec![long(&a, None), long(&b, None)]);
        a.sort_by_key(|r| (r.occ, r.ts));
        b.sort_by_key(|r| (std::cmp::Reverse(r.occ), r.ts));
        let sorted = open(vec![long(&a, None), long(&b, None)]);
        assert_same(&arrival, &sorted);
    }

    #[test]
    fn opening_a_long_segment_decodes_no_row_group() {
        let rows = observations();
        let pool = BufferPool::new(64 << 20);
        let r = SegmentedParquetReader::open_bytes_with_pool(
            vec![long(&rows, None)],
            Arc::clone(&pool),
        )
        .unwrap();
        assert_eq!(r.counter_labels("cpu").len(), 3);
        let stats = pool.stats();
        assert_eq!(stats.misses, 0, "open must not decode row groups");
        assert_eq!(stats.entries, 0);
    }

    #[test]
    fn a_malformed_occupant_list_presents_no_series() {
        let rows = observations();
        // More occupants than the segment has rows.
        let r = open(vec![long(&rows, Some("0-100000"))]);
        assert!(r.counter_labels("cpu").is_empty());
        // Unparseable.
        let r = open(vec![long(&rows, Some("zero"))]);
        assert!(r.counter_labels("cpu").is_empty());
    }

    #[test]
    fn a_column_named_occupant_in_a_wide_file_is_a_metric() {
        // Without the layout key, `occupant` is an ordinary column.
        let seg = write(
            vec![
                Field::new("timestamp", DataType::UInt64, false),
                Field::new(OCCUPANT_COLUMN, DataType::UInt64, true).with_metadata(meta(
                    "occupant",
                    "counter",
                    &[],
                )),
            ],
            vec![
                Arc::new(UInt64Array::from(vec![1_000_000_000u64, 2_000_000_000])),
                Arc::new(UInt64Array::from(vec![1u64, 2])),
            ],
            vec![],
        );
        let r = open(vec![seg]);
        assert_eq!(r.counter_labels("occupant").len(), 1);
    }

    /// A hundred occupants over ten ticks, every occupant at every tick:
    /// more than a read prunes for (`PRUNE_MAX_OCCUPANTS`).
    fn many() -> Vec<Obs> {
        let mut rows = Vec::new();
        for tick in 1..=10u64 {
            for occ in 0..100u64 {
                rows.push(Obs {
                    ts: tick * 1_000_000_000,
                    occ,
                    cpu: Some(tick * (occ + 1)),
                    depth: Some((tick + occ) as i64),
                    lat: Some(buckets(tick)),
                });
            }
        }
        rows
    }

    /// Sorted by occupant, eight rows to a page: an occupant's ten rows
    /// span two or three of the 125 pages.
    fn paged(rows: &[Obs]) -> Vec<u8> {
        let mut rows = rows.to_vec();
        rows.sort_by_key(|r| (r.occ, r.ts));
        long_paged(&rows, None, Some(8))
    }

    #[test]
    fn a_single_series_read_decodes_only_its_pages() {
        let rows = many();
        let whole = open(vec![wide(&rows)]);
        let pool = BufferPool::new(64 << 20);
        let long =
            SegmentedParquetReader::open_bytes_with_pool(vec![paged(&rows)], Arc::clone(&pool))
                .unwrap();
        for q in [
            "rate(cpu{__occupant__=\"17\"}[2s])",
            "depth{__occupant__=\"17\"}",
            "sum(rate(cpu{__occupant__=\"11\"}[2s]))",
            "histogram_mean(lat{__occupant__=\"17\"})",
        ] {
            let a = whole.query_range(q, 1.0, 10.0, 1.0).unwrap();
            let b = long.query_range(q, 1.0, 10.0, 1.0).unwrap();
            assert_eq!(canonical(&a), canonical(&b), "{q}");
        }
        // A pruned read decodes into its own arrays, not the pool's
        // whole-column cache: nothing went through the pool.
        let stats = pool.stats();
        assert_eq!(
            stats.misses, 0,
            "pruned reads must not decode whole columns"
        );
        assert_eq!(stats.entries, 0);
    }

    #[test]
    fn an_all_series_query_falls_back_to_one_shared_decode() {
        let rows = many();
        let whole = open(vec![wide(&rows)]);
        let pool = BufferPool::new(64 << 20);
        let long =
            SegmentedParquetReader::open_bytes_with_pool(vec![paged(&rows)], Arc::clone(&pool))
                .unwrap();
        for q in ["sum(rate(cpu[2s]))", "rate(cpu[2s])"] {
            let a = whole.query_range(q, 1.0, 10.0, 1.0).unwrap();
            let b = long.query_range(q, 1.0, 10.0, 1.0).unwrap();
            assert_eq!(canonical(&a), canonical(&b), "{q}");
        }
        // A hundred counter streams read one row group: too many to prune
        // for, so it is decoded once for all of them, outside the pool, and
        // each stream takes its own rows.
        assert_eq!(pool.stats().misses, 0, "the shared read bypasses the pool");
        let a = whole.query_range("max(depth)", 1.0, 10.0, 1.0).unwrap();
        let b = long.query_range("max(depth)", 1.0, 10.0, 1.0).unwrap();
        assert_eq!(canonical(&a), canonical(&b), "max(depth)");
    }

    /// An occupant absent from some ticks has no rows there, so its
    /// histogram reads as a series of only the ticks it was present at: the
    /// same as a file holding just those ticks.
    #[test]
    fn a_histogram_with_gaps_reads_only_its_observations() {
        let rows = observations();
        let late: Vec<Obs> = rows.iter().filter(|r| r.occ == 2).cloned().collect();
        let (a, b) = halves(&rows);
        let long = open(vec![long(&a, None), long(&b, None)]);
        let (la, lb) = halves(&late);
        let only = open(vec![wide(&la), wide(&lb)]);
        for q in [
            "histogram_mean(lat{__occupant__=\"2\"})",
            "histogram_count(lat{__occupant__=\"2\"})",
            "histogram_sum(lat{__occupant__=\"2\"})",
        ] {
            let x = long.query_range(q, 1.0, 6.0, 1.0).unwrap();
            let y = only.query_range(q, 1.0, 6.0, 1.0).unwrap();
            assert_eq!(canonical(&x), canonical(&y), "{q}");
        }
    }

    /// Occupant labels from outside the segment, as an archive supplies
    /// them: `__occupant__` n is the thread named `comm`.
    struct Occupants(HashMap<String, &'static str>);

    impl Occupants {
        fn labels(&self, labels: &Labels) -> Option<Labels> {
            let occ = labels.inner.get(OCCUPANT_LABEL)?;
            let mut l = labels.clone();
            l.inner
                .insert("comm".to_string(), self.0.get(occ)?.to_string());
            Some(l)
        }
    }

    impl ColumnRelabel for Occupants {
        fn identities(&self, _name: &str, labels: &Labels) -> Option<Vec<Labels>> {
            self.labels(labels).map(|l| vec![l])
        }

        fn split(&self, _name: &str, labels: &Labels, timestamps: &[u64]) -> Option<Vec<Run>> {
            self.labels(labels).map(|l| vec![(l, 0..timestamps.len())])
        }

        fn at(&self, _name: &str, labels: &Labels, _timestamp: u64) -> Option<Labels> {
            self.labels(labels)
        }

        fn segment_filter(&self, _name: &str, filter: &Labels) -> Labels {
            let mut f = filter.clone();
            f.inner.remove("comm");
            f
        }
    }

    #[test]
    fn occupant_labels_come_from_the_relabel() {
        let rows = observations();
        let (a, b) = halves(&rows);
        let names = Occupants(HashMap::from([
            ("0".to_string(), "main"),
            ("1".to_string(), "worker"),
            ("2".to_string(), "worker"),
        ]));
        let r = SegmentedParquetReader::open_relabeled_with_pool(
            Arc::new(InMemorySegments::new(vec![long(&a, None), long(&b, None)])),
            BufferPool::new(64 << 20),
            Arc::new(names),
        )
        .unwrap();
        let comms: BTreeSet<String> = r
            .counter_labels("cpu")
            .into_iter()
            .map(|l| l["comm"].clone())
            .collect();
        assert_eq!(
            comms,
            BTreeSet::from(["main".to_string(), "worker".to_string()])
        );

        // Two worker threads, one after the other, sum under one comm.
        let q = r
            .query("sum by (comm) (rate(cpu{comm=\"worker\"}[2s]))", Some(5.0))
            .unwrap();
        let crate::QueryResult::Vector { result } = q else {
            panic!("expected a vector, got {q:?}");
        };
        assert_eq!(result.len(), 1);
        // At 5 s only occupant 2 is alive: cpu = tick * 3 * 10 + 2, so its
        // rate is 30/s.
        assert!((result[0].value.1 - 30.0).abs() < 1e-9, "{result:?}");
    }

    /// Occupant 0 is `early` before 3.5 s and `late` after, so its column
    /// presents as two label sets: its identities are not fixed.
    struct Shift;

    impl Shift {
        fn with(labels: &Labels, comm: &str) -> Labels {
            let mut l = labels.clone();
            l.inner.insert("comm".to_string(), comm.to_string());
            l
        }

        fn ours(labels: &Labels) -> bool {
            labels.inner.get(OCCUPANT_LABEL).map(String::as_str) == Some("0")
        }
    }

    impl ColumnRelabel for Shift {
        fn identities(&self, _name: &str, labels: &Labels) -> Option<Vec<Labels>> {
            Self::ours(labels)
                .then(|| vec![Self::with(labels, "early"), Self::with(labels, "late")])
        }

        fn split(&self, _name: &str, labels: &Labels, timestamps: &[u64]) -> Option<Vec<Run>> {
            if !Self::ours(labels) {
                return None;
            }
            let cut = timestamps.partition_point(|ts| *ts < 3_500_000_000);
            Some(vec![
                (Self::with(labels, "early"), 0..cut),
                (Self::with(labels, "late"), cut..timestamps.len()),
            ])
        }

        fn at(&self, _name: &str, labels: &Labels, timestamp: u64) -> Option<Labels> {
            Self::ours(labels).then(|| {
                Self::with(
                    labels,
                    if timestamp < 3_500_000_000 {
                        "early"
                    } else {
                        "late"
                    },
                )
            })
        }

        fn segment_filter(&self, _name: &str, filter: &Labels) -> Labels {
            let mut f = filter.clone();
            f.inner.remove("comm");
            f
        }
    }

    /// A one-occupant read under a relabel whose identities are not fixed is
    /// still cut by occupant: each label set gets only its own samples.
    #[test]
    fn a_relabel_that_is_not_fixed_cuts_a_one_occupant_read() {
        let rows = observations();
        let (a, b) = halves(&rows);
        let r = SegmentedParquetReader::open_relabeled_with_pool(
            Arc::new(InMemorySegments::new(vec![long(&a, None), long(&b, None)])),
            BufferPool::new(64 << 20),
            Arc::new(Shift),
        )
        .unwrap();
        // When each `comm` has points, over the six ticks.
        let times = |comm: &str| {
            let q = r
                .query_range(
                    &format!("rate(cpu{{comm=\"{comm}\"}}[1500ms])"),
                    1.0,
                    6.0,
                    1.0,
                )
                .unwrap();
            let crate::QueryResult::Matrix { result } = q else {
                panic!("expected a matrix, got {q:?}");
            };
            assert_eq!(result.len(), 1, "{comm}: {result:?}");
            result[0]
                .values
                .iter()
                .map(|(t, _)| *t)
                .collect::<Vec<f64>>()
        };
        let (early, late) = (times("early"), times("late"));
        assert!(
            !early.is_empty() && early.iter().all(|t| *t < 3.5),
            "{early:?}"
        );
        assert!(
            !late.is_empty() && late.iter().all(|t| *t > 3.5),
            "{late:?}"
        );
    }

    /// Two hundred occupants over sixty ticks in twelve segments, each living
    /// a different span, some with one tick in a segment.
    fn churn() -> Vec<Obs> {
        let mut rows = Vec::new();
        for tick in 1..=60u64 {
            for occ in 0..200u64 {
                let (born, dies) = (occ % 50, occ % 50 + 3 + (occ * 7) % 40);
                if tick < born || tick > dies {
                    continue;
                }
                rows.push(Obs {
                    ts: tick * 1_000_000_000,
                    occ,
                    cpu: Some(tick * (occ + 1) * 3),
                    depth: Some((tick + occ) as i64),
                    lat: Some(buckets(tick)),
                });
            }
        }
        rows
    }

    /// An all-series read of a long table answers as the wide table does,
    /// whatever the pool holds.
    #[test]
    fn an_all_series_read_across_segments_matches_the_wide_table() {
        let rows = churn();
        let by_segment = |tick_span: u64| -> Vec<Vec<Obs>> {
            let mut segs: Vec<Vec<Obs>> = Vec::new();
            for r in &rows {
                let i = ((r.ts / 1_000_000_000 - 1) / tick_span) as usize;
                if segs.len() <= i {
                    segs.resize_with(i + 1, Vec::new);
                }
                segs[i].push(r.clone());
            }
            segs
        };
        let wide_segs: Vec<Vec<u8>> = by_segment(5).iter().map(|s| wide(s)).collect();
        let long_segs: Vec<Vec<u8>> = by_segment(5).iter().map(|s| long(s, None)).collect();
        let whole =
            SegmentedParquetReader::open_bytes_with_pool(wide_segs, BufferPool::new(64 << 20))
                .unwrap();
        for pool in [64 << 20, 4 << 10] {
            let longr = SegmentedParquetReader::open_bytes_with_pool(
                long_segs.clone(),
                BufferPool::new(pool),
            )
            .unwrap();
            for q in ["sum(rate(cpu[3s]))", "rate(cpu[3s])", "sum(irate(cpu[3s]))"] {
                let a = whole.query_range(q, 1.0, 60.0, 1.0).unwrap();
                let b = longr.query_range(q, 1.0, 60.0, 1.0).unwrap();
                assert_eq!(canonical(&a), canonical(&b), "{q}, pool {pool}");
            }
        }
    }
}
