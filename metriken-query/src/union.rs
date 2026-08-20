//! [`UnionMetricsSource`]: presents several readers with DISJOINT
//! metric-name sets as one logical [`MetricsSource`] — e.g. one sampler's
//! several acquisition-group tables, each holding a different subset of
//! that sampler's metrics (rezolus's `.rez` V3 container tables
//! `<sampler>/<group>`, not `<sampler>`, once a sampler is split).
//!
//! This is the opposite problem from [`crate::ParquetBuilder`]'s multi-file
//! union (`MultiParquetSource`): that type unions the SAME identity across
//! files — the same metric, from several hosts/recordings — and
//! deliberately keeps each file's series distinct (labeled/duplicated), not
//! merged. `UnionMetricsSource` instead unions DIFFERENT identities into
//! one namespace: a per-name accessor call dispatches to whichever single
//! child owns that name, so `counters("cpu_cycles", ..)` and
//! `counters("cpu_softirq", ..)` can each resolve from a different child
//! transparently.
//!
//! **No timestamp splicing or join.** Each child keeps its own samples,
//! windows, and cadence exactly as it already reports them. The PromQL
//! engine already aligns two independently-timestamped series onto its
//! evaluation grid whenever a query combines them — `a / b` between two
//! metrics has never required them to share row timestamps, even within
//! one physical table — so a query naming metrics from two different
//! children needs nothing new here beyond routing each name to its owner.
//! One consequence worth being explicit about: each metric's acquisition
//! window still resolves from its OWN child (that child's own table-level
//! or per-metric sidecar), so a `rate()` band is exactly as precise after
//! union as it was before — there is no window fan-out or reconstruction
//! step to lose fidelity in.
//!
//! **Identity must be disjoint across children by construction.** Which
//! readers to compose is a decision the CALLER makes (e.g. rezolus grouping
//! a sampler's group tables), not something derived from untrusted wire
//! bytes the way a single table's own schema is — so a name present in more
//! than one child is a caller/producer bug, not adversarial input this type
//! defends against. See [`build_index`] for exactly what happens if it
//! occurs anyway: deterministic (first child, by construction order, wins),
//! never a panic.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::parquet::ParquetReader;
use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::segmented::SegmentedParquetReader;
use crate::types::{Counters, Gauges};
use crate::{DataSource, MetricsSource, QueryOptions};

/// One child of a [`UnionMetricsSource`].
///
/// Only the two concrete reader types this crate provides are accepted —
/// `MetricsSource` alone isn't enough, since composing at the union layer
/// needs each child's raw [`DataSource`] handle, which is deliberately not
/// part of the public `MetricsSource` surface (see
/// [`ParquetReader::data_source`]/[`SegmentedParquetReader::data_source`]).
/// Built via `From<&ParquetReader>`/`From<&SegmentedParquetReader>`, which
/// borrow — an `Arc` clone under the hood — rather than consume, so the
/// original reader stays usable on its own after contributing to a union.
pub struct UnionChild(Arc<dyn DataSource>);

impl From<&ParquetReader> for UnionChild {
    fn from(reader: &ParquetReader) -> Self {
        UnionChild(reader.data_source())
    }
}

impl From<&SegmentedParquetReader> for UnionChild {
    fn from(reader: &SegmentedParquetReader) -> Self {
        UnionChild(reader.data_source())
    }
}

/// `name -> index into children that owns it`.
type Index = HashMap<String, usize>;

/// Build one metric kind's owner index from each child's own name list
/// (`counter_names()`/`gauge_names()`/`histogram_names()` — footer-only,
/// already-open-time metadata, so this is cheap: no row-group decode).
///
/// A name seen in more than one child keeps its FIRST owner (by `children`
/// order) — see the module docs: disjoint identity is a construction
/// contract, not something derived from untrusted bytes, so a violation
/// degrades to "one of the two answers for it" rather than panicking.
fn build_index(
    children: &[Arc<dyn DataSource>],
    names: impl Fn(&Arc<dyn DataSource>) -> Vec<String>,
) -> Index {
    let mut index = Index::new();
    for (i, child) in children.iter().enumerate() {
        for name in names(child) {
            index.entry(name).or_insert(i);
        }
    }
    index
}

/// Sorted, deduplicated union of metric names across children.
fn union_names<I: IntoIterator<Item = Vec<String>>>(lists: I) -> Vec<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for list in lists {
        names.extend(list);
    }
    names.into_iter().collect()
}

/// The dispatching [`DataSource`] the PromQL engine evaluates over: a
/// per-metric-name lookup into whichever child owns it. See the module docs
/// for the full contract.
struct UnionSource {
    children: Vec<Arc<dyn DataSource>>,
    counter_index: Index,
    gauge_index: Index,
    histogram_index: Index,
}

impl UnionSource {
    fn new(children: Vec<Arc<dyn DataSource>>) -> Self {
        let counter_index = build_index(&children, |c| c.counter_names());
        let gauge_index = build_index(&children, |c| c.gauge_names());
        let histogram_index = build_index(&children, |c| c.histogram_names());
        Self {
            children,
            counter_index,
            gauge_index,
            histogram_index,
        }
    }
}

impl DataSource for UnionSource {
    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        raw: bool,
    ) -> Option<Counters> {
        let i = *self.counter_index.get(name)?;
        self.children[i].counters(name, filter, start_ns, end_ns, raw)
    }

    fn gauges(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        raw: bool,
    ) -> Option<Gauges> {
        let i = *self.gauge_index.get(name)?;
        self.children[i].gauges(name, filter, start_ns, end_ns, raw)
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let i = *self.histogram_index.get(name)?;
        self.children[i].histogram_stream(name, filter, start_ns, end_ns)
    }

    /// The FINEST (minimum) interval across children — the same policy
    /// [`crate::segmented`]'s `SegmentedSource::interval()` uses, and for
    /// the same reason: a group that skipped ticks (window-advance dedup)
    /// has a coarser APPARENT interval than the sampler's true poll cadence
    /// (fewer rows over the same wall-clock span inflates the mean
    /// spacing), and this feeds `rate()`'s default grid step — which should
    /// track the fastest-ticking child, not be dragged wide by a lagging
    /// one that merely skipped some ticks.
    fn interval(&self) -> f64 {
        self.children
            .iter()
            .map(|c| c.interval())
            .fold(f64::MAX, f64::min)
    }

    fn time_range(&self) -> Option<(u64, u64)> {
        self.children
            .iter()
            .filter_map(|c| c.time_range())
            .fold(None, |acc, (lo, hi)| match acc {
                None => Some((lo, hi)),
                Some((alo, ahi)) => Some((alo.min(lo), ahi.max(hi))),
            })
    }

    fn counter_names(&self) -> Vec<String> {
        union_names(self.children.iter().map(|c| c.counter_names()))
    }

    fn gauge_names(&self) -> Vec<String> {
        union_names(self.children.iter().map(|c| c.gauge_names()))
    }

    fn histogram_names(&self) -> Vec<String> {
        union_names(self.children.iter().map(|c| c.histogram_names()))
    }

    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        match self.counter_index.get(name) {
            Some(&i) => self.children[i].counter_labels(name),
            None => Vec::new(),
        }
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        match self.gauge_index.get(name) {
            Some(&i) => self.children[i].gauge_labels(name),
            None => Vec::new(),
        }
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        match self.histogram_index.get(name) {
            Some(&i) => self.children[i].histogram_labels(name),
            None => Vec::new(),
        }
    }

    fn file_metadata(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for c in &self.children {
            out.extend(c.file_metadata());
        }
        out
    }

    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        let mut out: HashMap<String, HashMap<Labels, String>> = HashMap::new();
        for c in &self.children {
            for (metric, cols) in c.column_map() {
                out.entry(metric).or_default().extend(cols);
            }
        }
        out
    }
}

/// A [`MetricsSource`] presenting several readers with disjoint
/// metric-name sets — e.g. one sampler's several acquisition-group tables —
/// as one logical source. See the module docs for the identity/window/
/// interval contract.
pub struct UnionMetricsSource {
    engine: QueryEngine,
}

impl UnionMetricsSource {
    /// Build a union over `children`. Order matters only for the identity
    /// tie-break documented on [`build_index`] (first wins on a name
    /// collision, which correctly-partitioned input never triggers).
    pub fn new(children: Vec<UnionChild>) -> Self {
        let source = UnionSource::new(children.into_iter().map(|c| c.0).collect());
        Self {
            engine: QueryEngine::new(Arc::new(source)),
        }
    }
}

impl MetricsSource for UnionMetricsSource {
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

    fn time_range(&self) -> Option<(f64, f64)> {
        self.engine
            .time_range()
            .map(|(lo, hi)| (lo as f64 / 1e9, hi as f64 / 1e9))
    }

    fn time_range_ns(&self) -> Option<(u64, u64)> {
        self.engine.time_range()
    }

    fn interval(&self) -> f64 {
        self.engine.interval()
    }

    fn source(&self) -> String {
        self.engine.metadata_get("source").unwrap_or_default()
    }

    fn version(&self) -> String {
        self.engine.metadata_get("version").unwrap_or_default()
    }

    fn filename(&self) -> Option<String> {
        // No single-file concept for a union of several tables; the caller
        // (rezolus's `RezReader`) owns naming.
        None
    }

    fn metadata_get(&self, key: &str) -> Option<String> {
        self.engine.metadata_get(key)
    }

    fn file_metadata(&self) -> HashMap<String, String> {
        self.engine.file_metadata()
    }

    fn counter_names(&self) -> Vec<String> {
        self.engine.counter_names()
    }

    fn gauge_names(&self) -> Vec<String> {
        self.engine.gauge_names()
    }

    fn histogram_names(&self) -> Vec<String> {
        self.engine.histogram_names()
    }

    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.engine.counter_labels(name)
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.engine.gauge_labels(name)
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.engine.histogram_labels(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer_pool::BufferPool;

    use arrow::array::{ArrayRef, Int64Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::properties::WriterProperties;

    fn pool() -> Arc<BufferPool> {
        BufferPool::new(64 * 1024 * 1024)
    }

    fn counter_field(name: &str) -> Field {
        Field::new(name, DataType::UInt64, true).with_metadata(HashMap::from([
            ("metric".to_string(), name.to_string()),
            ("metric_type".to_string(), "counter".to_string()),
        ]))
    }

    /// One counter column's per-metric window sidecar: a 50ms window ending
    /// at each row's own timestamp — narrow enough that a `rate()` query
    /// exercises real acquisition-window math.
    fn window_cols(metric: &str, ts: &[u64]) -> Vec<(Field, ArrayRef)> {
        let begin: Vec<i64> = ts.iter().map(|_| -50_000_000i64).collect();
        let width: Vec<u64> = ts.iter().map(|_| 50_000_000u64).collect();
        vec![
            (
                Field::new(format!("{metric}:window_begin"), DataType::Int64, true),
                Arc::new(Int64Array::from(begin)) as ArrayRef,
            ),
            (
                Field::new(format!("{metric}:window_width"), DataType::UInt64, true),
                Arc::new(UInt64Array::from(width)) as ArrayRef,
            ),
        ]
    }

    fn build_table(field_specs: Vec<(Field, ArrayRef)>) -> Vec<u8> {
        let fields: Vec<Field> = field_specs.iter().map(|(f, _)| f.clone()).collect();
        let arrays: Vec<ArrayRef> = field_specs.into_iter().map(|(_, a)| a).collect();
        let schema = Arc::new(Schema::new(fields));
        let props = WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .build();
        let mut buf = Vec::new();
        let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();
        let batch = RecordBatch::try_new(schema, arrays).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        buf
    }

    /// A single-counter table, one row per second, `n` rows, values `0, 10,
    /// 20, ..`, each with its own 50ms window sidecar ending at the tick.
    fn counter_table_bytes(metric: &str, n: u64) -> Vec<u8> {
        let ts: Vec<u64> = (1..=n).map(|i| i * 1_000_000_000).collect();
        let vals: Vec<u64> = (0..n).map(|i| i * 10).collect();
        let mut fields = vec![(
            Field::new("timestamp", DataType::UInt64, false),
            Arc::new(UInt64Array::from(ts.clone())) as ArrayRef,
        )];
        fields.push((
            counter_field(metric),
            Arc::new(UInt64Array::from(vals)) as ArrayRef,
        ));
        fields.extend(window_cols(metric, &ts));
        build_table(fields)
    }

    /// Two counters in ONE table (the shape a V2-style, never-split sampler
    /// would have) — the reference this crate's union must reproduce.
    fn two_counter_table_bytes(a: &str, b: &str, n: u64) -> Vec<u8> {
        // Same value formula `counter_table_bytes` uses for either name, so
        // this table is byte-for-byte "the same data" as the two single-
        // metric tables `counter_table_bytes(a, n)`/`counter_table_bytes(b,
        // n)` would separately produce — that equivalence is the whole
        // point of the test this fixture feeds.
        let ts: Vec<u64> = (1..=n).map(|i| i * 1_000_000_000).collect();
        let a_vals: Vec<u64> = (0..n).map(|i| i * 10).collect();
        let b_vals: Vec<u64> = (0..n).map(|i| i * 10).collect();
        let mut fields = vec![(
            Field::new("timestamp", DataType::UInt64, false),
            Arc::new(UInt64Array::from(ts.clone())) as ArrayRef,
        )];
        fields.push((
            counter_field(a),
            Arc::new(UInt64Array::from(a_vals)) as ArrayRef,
        ));
        fields.extend(window_cols(a, &ts));
        fields.push((
            counter_field(b),
            Arc::new(UInt64Array::from(b_vals)) as ArrayRef,
        ));
        fields.extend(window_cols(b, &ts));
        build_table(fields)
    }

    /// A single-counter table, one row per second, `n` rows, values `0..n`,
    /// each with a 50ms window ending at the tick — narrow enough that a
    /// `rate()` query exercises real acquisition-window math.
    fn counter_table(metric: &str, n: u64) -> ParquetReader {
        ParquetReader::open_bytes_with_pool(counter_table_bytes(metric, n), pool()).unwrap()
    }

    #[test]
    fn per_name_dispatch_resolves_from_the_owning_child_only() {
        let a = counter_table("cpu_cycles", 5);
        let b = counter_table("cpu_softirq", 5);
        let union = UnionMetricsSource::new(vec![UnionChild::from(&a), UnionChild::from(&b)]);

        assert!(union.columns("cpu_cycles").unwrap().contains("cpu_cycles"));
        assert!(union
            .columns("cpu_softirq")
            .unwrap()
            .contains("cpu_softirq"));
        assert!(union.columns("not_a_real_metric").unwrap().is_empty());

        let (start, end) = union.time_range().unwrap();
        assert!(union
            .query_range("rate(cpu_cycles[2s])", start, end, 1.0)
            .is_ok());
        assert!(union
            .query_range("rate(cpu_softirq[2s])", start, end, 1.0)
            .is_ok());
    }

    #[test]
    fn catalog_and_time_range_are_unioned() {
        let a = counter_table("cpu_cycles", 5);
        let b = counter_table("cpu_softirq", 3);
        let union = UnionMetricsSource::new(vec![UnionChild::from(&a), UnionChild::from(&b)]);

        let mut names = union.counter_names();
        names.sort();
        assert_eq!(
            names,
            vec!["cpu_cycles".to_string(), "cpu_softirq".to_string()]
        );

        // The union's time range spans the WIDER child (5 rows), even though
        // it's asking about a name that only the NARROWER child (3 rows) owns
        // — time_range() is a source-level property, not per-metric.
        assert_eq!(union.time_range(), a.time_range());
    }

    #[test]
    fn a_cross_child_series_op_series_expression_matches_one_table_of_the_same_data() {
        // The whole point: `a / b` combining two metrics that live in
        // DIFFERENT children must answer exactly as it would if both had
        // always lived in one table together.
        let a = counter_table("cpu_cycles", 6);
        let b = counter_table("cpu_softirq", 6);
        let union = UnionMetricsSource::new(vec![UnionChild::from(&a), UnionChild::from(&b)]);

        let combined_bytes = two_counter_table_bytes("cpu_cycles", "cpu_softirq", 6);
        let combined = ParquetReader::open_bytes_with_pool(combined_bytes, pool()).unwrap();

        let (start, end) = combined.time_range().unwrap();
        assert_eq!(union.time_range(), combined.time_range());

        for expr in [
            "rate(cpu_cycles[2s]) + rate(cpu_softirq[2s])",
            "rate(cpu_cycles[3s]) / rate(cpu_softirq[3s])",
        ] {
            let ru = union.query_range(expr, start, end, 1.0).unwrap();
            let rc = combined.query_range(expr, start, end, 1.0).unwrap();
            assert_eq!(
                serde_json::to_value(&ru).unwrap(),
                serde_json::to_value(&rc).unwrap(),
                "cross-child union differs from one combined table for {expr}"
            );
        }
    }

    #[test]
    fn bands_are_preserved_per_child_after_union() {
        // Each child's own table-level/per-metric window still resolves a
        // rate() band after union — no fan-out/reconstruction step to lose
        // precision in.
        let a = counter_table("cpu_cycles", 6);
        let b = counter_table("cpu_softirq", 6);
        let union = UnionMetricsSource::new(vec![UnionChild::from(&a), UnionChild::from(&b)]);
        let (start, end) = union.time_range().unwrap();

        for metric in ["cpu_cycles", "cpu_softirq"] {
            let r = union
                .query_range(&format!("rate({metric}[3s])"), start, end, 1.0)
                .unwrap();
            let json = serde_json::to_value(&r).unwrap();
            let intervals = json["result"][0]["intervals"]
                .as_array()
                .unwrap_or_else(|| {
                    panic!("rate({metric}[..]) through the union must still carry bands: {json}")
                });
            assert!(
                intervals.iter().any(|iv| iv.is_array()),
                "{metric}: at least one point must carry a resolved [lo, hi] band: {json}"
            );
        }
    }

    #[test]
    fn a_name_absent_from_every_child_resolves_to_no_columns() {
        let a = counter_table("cpu_cycles", 3);
        let union = UnionMetricsSource::new(vec![UnionChild::from(&a)]);
        assert!(union.columns("nonexistent").unwrap().is_empty());
        assert!(union.query_range("nonexistent", 0.0, 10.0, 1.0).is_err());
    }

    #[test]
    fn a_segmented_child_composes_the_same_as_a_single_segment_child() {
        // `UnionChild::from(&SegmentedParquetReader)` must work identically
        // to the single-segment case — the union only needs a `DataSource`
        // handle, and a segmented table's splice already resolves one below
        // that handle.
        let bytes = counter_table_bytes("cpu_cycles", 4);
        let single = ParquetReader::open_bytes_with_pool(bytes.clone(), pool()).unwrap();
        let segmented =
            SegmentedParquetReader::open_bytes_with_pool(vec![bytes.clone()], pool()).unwrap();
        let other = counter_table("cpu_softirq", 4);

        let union_single =
            UnionMetricsSource::new(vec![UnionChild::from(&single), UnionChild::from(&other)]);
        let union_segmented =
            UnionMetricsSource::new(vec![UnionChild::from(&segmented), UnionChild::from(&other)]);

        let (start, end) = union_single.time_range().unwrap();
        assert_eq!(union_segmented.time_range(), union_single.time_range());
        let expr = "rate(cpu_cycles[2s]) + rate(cpu_softirq[2s])";
        assert_eq!(
            serde_json::to_value(union_single.query_range(expr, start, end, 1.0).unwrap()).unwrap(),
            serde_json::to_value(union_segmented.query_range(expr, start, end, 1.0).unwrap())
                .unwrap(),
        );
    }
}
