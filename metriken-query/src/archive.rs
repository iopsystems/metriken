//! Queries over an archive ([`ArchiveReader`], `metriken-storage`'s): which
//! table answers a query, the evaluation timestamps for a query spanning
//! samplers of different cadence, and the [`MetricsSource`] implementation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use metriken_storage::labeled::LabeledSource;
use metriken_storage::parquet::MultiParquetSource;
use metriken_storage::reader::{SamplerReader, TableReader};
use metriken_storage::union::UnionSource;
use metriken_storage::{table_sampler, ArchiveReader, DataSource, Labels, UnionChild, UnionError};

use crate::promql::QueryEngine;
use crate::{MetricsSource, QueryError, QueryOptions, QueryResult, RateMode};

/// The evaluation timestamps a composed query needs to stay faithful to
/// this recording's cadences, or `None` when it does not need any.
///
/// Querying an [`ArchiveReader`] through [`MetricsSource`] applies the
/// cross-cadence policy itself: when a query spans samplers recording at
/// different rates, the points are moved onto the SLOW table's own rows, so
/// every point lands where both operands genuinely have data. A caller that
/// instead composes `ArchiveReader::composition_sources` into
/// a labelled multi-source queries that composed reader, never this one, and
/// so silently loses the policy — the composed reader falls back to the
/// uniform grid and holds the slow operand's value forward between its real
/// readings.
///
/// Pass the result to `QueryOptions::with_eval_timestamps` on the composed
/// query to restore it:
///
/// ```rust,ignore
/// let mut opts = QueryOptions::default();
/// if let Some(points) = eval_timestamps_for(&reader, query, step_s, opts.rate_mode) {
///     opts = opts.with_eval_timestamps(Some(points));
/// }
/// composed.query_range_opts(query, start_s, end_s, step_s, &opts)
/// ```
///
/// `None` means the composed query needs no adjustment: the query touches
/// one cadence (the overwhelming majority), or `rate_mode` is
/// [`RateMode::Raw`], which places points at real un-snapped sample times
/// by contract and must not have them relocated.
///
/// # This answers for ONE recording
///
/// The timestamps are absolute, so they describe this recording's timeline
/// and no other. A caller composing SEVERAL recordings cannot simply pick
/// one: two jobs that ran at different wall-clock times share no instants,
/// and imposing one's rows on the other puts every point where the other
/// has no data — the very fault this exists to avoid. Apply this when
/// exactly one recording is in the composition, or when every recording
/// answers with the same timestamps; otherwise there is no well-defined
/// alignment and the uniform grid is the honest fallback.
pub fn eval_timestamps_for(
    reader: &ArchiveReader,
    query: &str,
    step_s: f64,
    rate_mode: RateMode,
) -> Option<Arc<[u64]>> {
    cross_cadence_eval_timestamps(reader, query, step_s, rate_mode)
}

/// Sub-readers that hold at least one metric the query references.
///
/// Answered from each table's name catalog, so a table the query cannot
/// touch is never opened. `referenced_metrics` is parse-only — it does not
/// need a source, which is exactly why routing can use it and `columns`
/// (which expands selectors through the source's column map) cannot.
///
/// Label matchers are not consulted: a table holding the metric with no
/// matching series answers with an empty result, which is correct, and
/// skipping it here would route on data the catalog does not carry.
fn owners<'a>(
    reader: &'a ArchiveReader,
    query: &str,
) -> Result<Vec<&'a SamplerReader>, QueryError> {
    let referenced: Vec<String> = crate::referenced_metrics(query)?.into_iter().collect();
    Ok(reader.owners_of(&referenced))
}

/// The engine that answers a query: over the tables that hold a metric it
/// references, grouped by recording, each recording's series carrying that
/// recording's labels ([`ArchiveReader::series_labels`]).
///
/// Within one recording, tables of different samplers union freely. They
/// did not always: a query spanning two samplers used to be refused as
/// "cross-timeline", because there was no way to say what treating two
/// separately-read values as simultaneous costs. The query engine now
/// prices that itself — operands whose acquisition edges differ have their
/// bands widened to the union of both spans — so the refusal has nothing
/// left to protect. Measured, the join costs 1.5–3.0 ms on a 32-core host,
/// under 1% of a 200 ms interval.
///
/// Across recordings, the recordings' sources are joined side by side: the
/// same metric in two recordings is two sets of series, told apart by their
/// recording labels, so `sum by (source) (...)` and `x{source="vllm"}` work
/// over an archive holding several producers of one run. This used to be
/// refused, because without the labels a union of two recordings answered
/// from one of them.
///
/// Still refused, within one recording: a metric name held by two tables of
/// the query (`gpu_nvidia` and `gpu_amd_smi` both publish
/// `gpu_utilization`). Errors too when the query references no known metric.
fn route(reader: &ArchiveReader, query: &str) -> Result<QueryEngine, QueryError> {
    let owners = owners(reader, query)?;
    if owners.is_empty() {
        return Err(QueryError::ParseError(format!(
            "query references no metric present in this .rez: {query}"
        )));
    }
    let mut recordings: Vec<Vec<&SamplerReader>> = Vec::new();
    for t in owners {
        match recordings.iter_mut().find(|r| r[0].same_recording(t)) {
            Some(r) => r.push(t),
            None => recordings.push(vec![t]),
        }
    }
    let labels = reader.series_labels();
    let mut sources: Vec<Arc<dyn DataSource>> = Vec::new();
    for tables in &recordings {
        if let Some(source) = recording_source(tables, query)? {
            sources.push(Arc::new(LabeledSource::new(
                source,
                labels.of(tables[0]).clone(),
            )));
        }
    }
    let source: Arc<dyn DataSource> = match sources.len() {
        // A table whose segments have gone since the probe is absent, so its
        // metrics are too: the same error a query naming a metric this
        // archive never held gets.
        0 => {
            return Err(QueryError::ParseError(format!(
                "query references {query}, whose tables have all been evicted since this \
                 archive was opened"
            )))
        }
        1 => sources.pop().expect("one source"),
        _ => Arc::new(MultiParquetSource {
            files: sources
                .into_iter()
                .map(|s| (s, Labels::default()))
                .collect(),
        }),
    };
    Ok(QueryEngine::new(source))
}

/// One recording's tables that hold a metric of `query`, as one source: the
/// table itself, or a union of them. `None` when every one of them has been
/// evicted since the archive was opened.
fn recording_source(
    tables: &[&SamplerReader],
    query: &str,
) -> Result<Option<Arc<dyn DataSource>>, QueryError> {
    if let [one] = tables {
        return Ok(one.reader().map(TableReader::source));
    }
    // Building the union only touches each table's already-open,
    // footer-level name catalog (no row-group decode), so a fresh one per
    // query is cheap enough not to need caching.
    //
    // `try_new`, not `new`: this composition set is derived from archive
    // bytes (table schemas plus parsed table keys), not hand-picked by
    // trusted code, so a producer/archive bug that put the same metric name
    // in two "disjoint" group tables of this sampler must be a loud error,
    // not `UnionSource`'s silent first-wins. `filter_map`: on a live archive
    // one of several tables can have been evicted between the probe and
    // here, and the rest still answer.
    let children: Vec<UnionChild> = tables
        .iter()
        .filter_map(|t| t.reader().map(TableReader::union_child))
        .collect();
    if children.is_empty() {
        return Ok(None);
    }
    UnionSource::try_new(children)
        .map(|u| Some(Arc::new(u) as Arc<dyn DataSource>))
        .map_err(|e| match e {
            UnionError::NonDisjoint { duplicates } => {
                // Two very different situations produce this, and conflating
                // them tells the operator their archive is corrupt when it is
                // not. Distinct SAMPLERS sharing a metric name is legitimate
                // and shipped: `gpu_amd_smi` and `gpu_nvidia` both publish the
                // vendor-neutral `gpu_utilization`, `gpu_temperature` and six
                // more, because only one of them ever populates on a given
                // host. Two group tables of ONE sampler sharing a name is a
                // real archive defect.
                let mut samplers: Vec<&str> =
                    tables.iter().map(|t| table_sampler(t.sampler())).collect();
                samplers.sort();
                samplers.dedup();
                if samplers.len() > 1 {
                    QueryError::ParseError(format!(
                        "query {query} references metric name(s) published by more than one \
                         sampler ({}), so it is ambiguous which is meant: {}. This is not an \
                         archive fault — those samplers deliberately share vendor-neutral \
                         names. Query one of them at a time.",
                        samplers.join(", "),
                        duplicates.join(", ")
                    ))
                } else {
                    QueryError::ParseError(format!(
                        "query {query} references metric name(s) present in more than one \
                         acquisition-group table of the same sampler — the archive's own \
                         tables are not disjoint, which should never happen: {}",
                        duplicates.join(", ")
                    ))
                }
            }
            UnionError::Empty => unreachable!("`children` is not empty"),
        })
}

/// The typical spacing between consecutive rows, or `None` for fewer than two
/// rows (no gap to measure).
///
/// The median, not the mean: a sampler's rows are irregular — 30 s then 60 s
/// apart on a real recording — and a mean is dragged around by the long gaps
/// and by any restart-sized hole in the middle of a recording. The median
/// answers "how often does this table usually produce a row", which is the
/// question being asked.
fn typical_gap_ns(timestamps: &[u64]) -> Option<u64> {
    if timestamps.len() < 2 {
        return None;
    }
    let mut gaps: Vec<u64> = timestamps
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .collect();
    gaps.sort_unstable();
    Some(gaps[gaps.len() / 2]).filter(|g| *g > 0)
}

/// The timestamps a query should be evaluated at, when it spans samplers of
/// different cadence — `None` when it does not and the uniform grid is
/// right.
///
/// The grid walks `start + k·step`. A query combining a fast sampler with a
/// slow one therefore produces most of its points where the slow sampler
/// has no reading at all: that value is held forward and combined with the fast
/// operand as if the two were simultaneous.
///
/// The grid cannot be tuned out of this. A slow sampler's rows are not
/// evenly spaced — measured on a real recording, one sampler's readings
/// fell 30 s apart and then 60 s apart — so no step and no phase puts a
/// uniform grid on them. Two earlier attempts are worth recording:
/// coarsening the STEP relocated the grid and made the combined band
/// explode (0.85% wide before, 6.7x after), and widening only the averaging
/// SPAN left the points on the grid, still between the slow sampler's real
/// readings.
///
/// So hand the engine the slow sampler's own row timestamps. Every point then
/// lands where both operands genuinely have data, and each rate averages
/// over the gap it actually spans.
///
/// Returns `None` unless the query really touches more than one cadence, so
/// single-sampler queries — the overwhelming majority — are untouched.
///
/// Also `None` under [`RateMode::Raw`], which already answers this question
/// its own way: Raw places points at the real, un-snapped sample
/// timestamps. Relocating them would contradict that contract, and would
/// break the query outright — Raw's counter producer reads sample pairs
/// and ignores supplied points, while the gauge producers honour them, so
/// a counter-and-gauge expression would have its two sides land on
/// different instants and intersect nowhere.
fn cross_cadence_eval_timestamps(
    reader: &ArchiveReader,
    query: &str,
    step_s: f64,
    rate_mode: RateMode,
) -> Option<Arc<[u64]>> {
    if matches!(rate_mode, RateMode::Raw) {
        return None;
    }

    let owners = owners(reader, query).ok()?;
    if owners.len() < 2 {
        return None;
    }
    // The rows of one recording; recordings of one run share a wall clock
    // but not row instants, and two runs share neither, so a query over
    // several stays on the uniform grid (see `eval_timestamps_for`).
    if !owners.iter().all(|t| t.same_recording(owners[0])) {
        return None;
    }

    // Cadence comes from the ROWS, not from `interval()`: that reports the
    // recording's nominal interval, which every table in an archive shares
    // — on a real recording a 1 s sampler and a 30 s one both answered 1.0,
    // so asking it can never detect a cadence difference. These are the
    // instants the query path reads, nothing having rounded them.
    //
    // Cadence is a property of the SAMPLER, not of a table. Two group
    // tables of one sampler are read together on one schedule; a group that
    // dedups or skips ticks is sparse WITHIN that cadence, not a second
    // cadence, and relocating a query onto its rows would silently change
    // the answer for a query that merely named a sibling group's metric.
    //
    // So a sampler's cadence is the spacing of its DENSEST participating
    // table — the one that shows the underlying read schedule.
    let mut by_sampler: BTreeMap<&str, (u64, &[u64])> = BTreeMap::new();
    for t in &owners {
        let ts = t.row_timestamps();
        let Some(gap) = typical_gap_ns(ts) else {
            continue;
        };
        by_sampler
            .entry(table_sampler(t.sampler()))
            .and_modify(|slot| {
                if gap < slot.0 {
                    *slot = (gap, ts);
                }
            })
            .or_insert((gap, ts));
    }
    if by_sampler.len() < 2 {
        return None;
    }

    let fastest = by_sampler.values().map(|(gap, _)| *gap).min()?;
    let (slowest, timestamps) = by_sampler.into_values().max_by_key(|(gap, _)| *gap)?;
    // Deliberately a ratio, not equality: gaps measured from real rows are
    // never exactly equal, so "different cadence" has to mean *materially*
    // different. A sampler read at least twice as far apart as another is a
    // different cadence in any sense that matters here.
    if slowest < fastest.saturating_mul(2) {
        return None;
    }
    // A slow sampler finer than the step is already oversampled by the
    // grid; moving off it would only lose points.
    if (slowest as f64) <= step_s * 1e9 {
        return None;
    }
    Some(timestamps.into())
}

impl MetricsSource for ArchiveReader {
    // ── Query methods: route to the sub-reader owning the referenced metrics. ──
    fn query_range_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        let aligned;
        let opts = match cross_cadence_eval_timestamps(self, expr, step_s, opts.rate_mode) {
            Some(points) => {
                // Clone and set the one field: `QueryOptions` is
                // `#[non_exhaustive]`, so it cannot be built by literal from
                // here — and cloning preserves whatever else the caller set.
                aligned = opts.clone().with_eval_timestamps(Some(points));
                &aligned
            }
            None => opts,
        };
        route(self, expr)?.query_range_opts(expr, start_s, end_s, step_s, opts)
    }
    /// Routed as [`query_range_opts`](Self::query_range_opts) is, with the
    /// same evaluation timestamps.
    fn query_range_display_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &crate::DisplayOptions,
        qopts: &QueryOptions,
    ) -> Result<crate::DisplayResult, QueryError> {
        let aligned;
        let qopts = match cross_cadence_eval_timestamps(self, expr, step_s, qopts.rate_mode) {
            Some(points) => {
                aligned = qopts.clone().with_eval_timestamps(Some(points));
                &aligned
            }
            None => qopts,
        };
        route(self, expr)?.query_range_display_opts(expr, start_s, end_s, step_s, opts, qopts)
    }
    fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        route(self, expr)?.query(expr, time)
    }
    fn columns(&self, query: &str) -> Result<HashSet<String>, QueryError> {
        // Through the same labelled sources as a query, so a selector
        // naming a recording label resolves to that recording's columns.
        let labels = self.series_labels();
        let mut out = HashSet::new();
        for t in owners(self, query)? {
            let Some(r) = t.reader() else {
                continue;
            };
            let source = LabeledSource::new(r.source(), labels.of(t).clone());
            out.extend(QueryEngine::new(Arc::new(source)).columns(query)?);
        }
        Ok(out)
    }

    // ── Metadata: from the archive's probed catalog. ──
    fn counter_names(&self) -> Vec<String> {
        ArchiveReader::counter_names(self)
    }
    fn gauge_names(&self) -> Vec<String> {
        ArchiveReader::gauge_names(self)
    }
    fn histogram_names(&self) -> Vec<String> {
        ArchiveReader::histogram_names(self)
    }
    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        ArchiveReader::counter_labels(self, name)
    }
    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        ArchiveReader::gauge_labels(self, name)
    }
    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        ArchiveReader::histogram_labels(self, name)
    }
    fn time_range(&self) -> Option<(f64, f64)> {
        ArchiveReader::time_range_ns(self).map(|(b, e)| (b as f64 / 1e9, e as f64 / 1e9))
    }
    fn time_range_ns(&self) -> Option<(u64, u64)> {
        ArchiveReader::time_range_ns(self)
    }
    fn interval(&self) -> f64 {
        ArchiveReader::interval(self)
    }
    fn source(&self) -> String {
        self.metadata().get("source").cloned().unwrap_or_default()
    }
    fn version(&self) -> String {
        self.metadata().get("version").cloned().unwrap_or_default()
    }
    fn filename(&self) -> Option<String> {
        ArchiveReader::filename(self)
    }
    fn metadata_get(&self, key: &str) -> Option<String> {
        self.metadata().get(key).cloned()
    }
    fn file_metadata(&self) -> HashMap<String, String> {
        self.metadata()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The typical gap is the MEDIAN, so one long hole (a restart, a missed
    /// poll) does not masquerade as the table's cadence. Moved from rezolus.
    #[test]
    fn typical_gap_is_robust_to_a_single_long_hole() {
        const S: u64 = 1_000_000_000;
        let steady: Vec<u64> = (0..10).map(|i| i * S).collect();
        assert_eq!(typical_gap_ns(&steady), Some(S));

        let mut holed = steady.clone();
        holed.extend((0..10).map(|i| 110 * S + i * S));
        assert_eq!(
            typical_gap_ns(&holed),
            Some(S),
            "a mean would be dragged upward by the hole; the median must not be"
        );

        assert_eq!(typical_gap_ns(&[]), None);
        assert_eq!(typical_gap_ns(&[42]), None);
    }

    // ── Recording labels on series ──

    use metriken_storage::labeled::{EXPORTED_PREFIX, RECORDING_LABEL};
    use metriken_storage::{BufferPool, InMemorySource};

    const S: u64 = 1_000_000_000;

    /// A table of counter `name`: one column per series, each with its
    /// labels, sharing a `timestamp` column of `rows` rows one second apart.
    fn counter_table(name: &str, series: &[(&[(&str, &str)], u64)], rows: u64) -> Vec<u8> {
        use arrow::array::{ArrayRef, UInt64Array};
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        use parquet::file::properties::WriterProperties;

        let mut fields = vec![Field::new("timestamp", DataType::UInt64, false)];
        let mut columns: Vec<ArrayRef> = vec![Arc::new(UInt64Array::from(
            (1..=rows).map(|i| i * S).collect::<Vec<_>>(),
        ))];
        for (i, (labels, per_second)) in series.iter().enumerate() {
            let mut meta = HashMap::new();
            meta.insert("metric".to_string(), name.to_string());
            meta.insert("metric_type".to_string(), "counter".to_string());
            for (k, v) in *labels {
                meta.insert(k.to_string(), v.to_string());
            }
            fields.push(
                Field::new(format!("{name}__{i}"), DataType::UInt64, true).with_metadata(meta),
            );
            columns.push(Arc::new(UInt64Array::from(
                (1..=rows).map(|r| r * per_second).collect::<Vec<_>>(),
            )));
        }
        let schema = Arc::new(Schema::new(fields));
        let mut buf = Vec::new();
        let mut writer =
            ArrowWriter::try_new(&mut buf, schema.clone(), Some(WriterProperties::default()))
                .unwrap();
        writer
            .write(&RecordBatch::try_new(schema, columns).unwrap())
            .unwrap();
        writer.close().unwrap();
        buf
    }

    fn recording(labels: &[(&str, &str)], tables: Vec<(&str, Vec<u8>)>) -> InMemorySource {
        let labels: BTreeMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        InMemorySource {
            name: labels.get("source").cloned().unwrap_or_default(),
            metadata: labels.clone(),
            labels,
            complete: true,
            tables: tables
                .into_iter()
                .map(|(t, bytes)| (t.to_string(), vec![bytes]))
                .collect(),
        }
    }

    fn archive(recordings: Vec<InMemorySource>) -> ArchiveReader {
        ArchiveReader::from_in_memory(recordings, None, BufferPool::new(16 << 20)).unwrap()
    }

    /// The label sets (without `__name__`) of `query`'s series, with their
    /// last value.
    fn series(reader: &ArchiveReader, query: &str) -> Vec<(BTreeMap<String, String>, f64)> {
        let result = reader
            .query_range_opts(query, 1.0, 10.0, 1.0, &QueryOptions::default())
            .unwrap_or_else(|e| panic!("{query}: {e}"));
        let QueryResult::Matrix { result } = result else {
            panic!("{query}: not a matrix");
        };
        let mut out: Vec<_> = result
            .into_iter()
            .map(|m| {
                let labels = m
                    .metric
                    .into_iter()
                    .filter(|(k, _)| k != "__name__")
                    .collect::<BTreeMap<_, _>>();
                (labels, m.values.last().map(|v| v.1).unwrap_or(f64::NAN))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_single_recording_carries_its_labels() {
        let reader = archive(vec![recording(
            &[("source", "rezolus"), ("host", "web-01")],
            vec![(
                "cpu_usage",
                counter_table("cpu_cycles", &[(&[("id", "0")], 5)], 10),
            )],
        )]);
        assert_eq!(
            series(&reader, "rate(cpu_cycles[3s])"),
            [(
                labels(&[("host", "web-01"), ("id", "0"), ("source", "rezolus")]),
                5.0
            )]
        );
        assert_eq!(
            series(&reader, r#"rate(cpu_cycles{source="rezolus"}[3s])"#).len(),
            1
        );
        assert!(series(&reader, r#"rate(cpu_cycles{source="vllm"}[3s])"#).is_empty());
        assert_eq!(
            reader.counter_labels("cpu_cycles"),
            [labels(&[
                ("host", "web-01"),
                ("id", "0"),
                ("source", "rezolus")
            ])]
        );
    }

    /// The same metric in two recordings: two sets of series, told apart by
    /// recording labels. This was refused as cross-recording.
    #[test]
    fn recordings_of_one_run_are_queried_together() {
        let reader = archive(vec![
            recording(
                &[("source", "rezolus")],
                vec![("cpu_usage", counter_table("ops", &[(&[], 2)], 10))],
            ),
            recording(
                &[("source", "vllm")],
                vec![("prometheus/scrape", counter_table("ops", &[(&[], 3)], 10))],
            ),
        ]);
        assert_eq!(
            series(&reader, "rate(ops[3s])"),
            [
                (labels(&[("source", "rezolus")]), 2.0),
                (labels(&[("source", "vllm")]), 3.0)
            ]
        );
        assert_eq!(
            series(&reader, r#"rate(ops{source="vllm"}[3s])"#),
            [(labels(&[("source", "vllm")]), 3.0)]
        );
        assert_eq!(
            series(&reader, r#"rate(ops{source!="vllm"}[3s])"#),
            [(labels(&[("source", "rezolus")]), 2.0)]
        );
        assert_eq!(series(&reader, "sum(rate(ops[3s]))"), [(labels(&[]), 5.0)]);
        assert_eq!(series(&reader, "sum by (source) (rate(ops[3s]))").len(), 2);
        // The filter's regex form is literals and alternations.
        assert_eq!(
            series(&reader, r#"rate(ops{source=~"rezolus|other"}[3s])"#).len(),
            1
        );
    }

    /// A series' own label under a recording key keeps its value under
    /// `exported_<key>`, unless it equals the recording's.
    #[test]
    fn a_clashing_series_label_moves_to_exported() {
        let reader = archive(vec![recording(
            &[("source", "svc"), ("host", "lb")],
            vec![(
                "prometheus/scrape",
                counter_table(
                    "requests",
                    &[(&[("host", "backend-1")], 1), (&[("host", "lb")], 2)],
                    10,
                ),
            )],
        )]);
        let exported = format!("{EXPORTED_PREFIX}host");
        assert_eq!(
            series(&reader, "rate(requests[3s])"),
            [
                (
                    labels(&[(&exported, "backend-1"), ("host", "lb"), ("source", "svc")]),
                    1.0
                ),
                (labels(&[("host", "lb"), ("source", "svc")]), 2.0),
            ]
        );
        assert_eq!(
            series(&reader, r#"rate(requests{exported_host="backend-1"}[3s])"#).len(),
            1
        );
        // The series whose own host equals the recording's has no
        // exported_host, so a negative matcher keeps it.
        assert_eq!(
            series(&reader, r#"rate(requests{exported_host!="backend-1"}[3s])"#),
            [(labels(&[("host", "lb"), ("source", "svc")]), 2.0)]
        );
        assert!(series(&reader, r#"rate(requests{host="backend-1"}[3s])"#).is_empty());
    }

    /// Two recordings with one label set stay two sets of series.
    #[test]
    fn identical_label_sets_are_kept_apart() {
        let reader = archive(vec![
            recording(
                &[("source", "rezolus")],
                vec![("cpu_usage", counter_table("ops", &[(&[], 2)], 10))],
            ),
            recording(
                &[("source", "rezolus")],
                vec![("cpu_usage", counter_table("ops", &[(&[], 3)], 10))],
            ),
        ]);
        assert_eq!(
            series(&reader, "rate(ops[3s])"),
            [
                (
                    labels(&[(RECORDING_LABEL, "0"), ("source", "rezolus")]),
                    2.0
                ),
                (
                    labels(&[(RECORDING_LABEL, "1"), ("source", "rezolus")]),
                    3.0
                ),
            ]
        );
        assert_eq!(series(&reader, "sum(rate(ops[3s]))"), [(labels(&[]), 5.0)]);
    }

    /// Within one recording a name two samplers share is still refused.
    #[test]
    fn a_name_two_samplers_share_is_still_refused_within_a_recording() {
        let reader = archive(vec![recording(
            &[("source", "rezolus")],
            vec![
                ("gpu_nvidia", counter_table("gpu_ops", &[(&[], 1)], 10)),
                ("gpu_amd_smi", counter_table("gpu_ops", &[(&[], 2)], 10)),
            ],
        )]);
        let err = reader
            .query_range_opts("gpu_ops", 1.0, 10.0, 1.0, &QueryOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than one sampler"), "{err}");
    }

    #[test]
    fn columns_resolve_through_recording_labels() {
        let reader = archive(vec![
            recording(
                &[("source", "rezolus")],
                vec![("cpu_usage", counter_table("ops", &[(&[], 2)], 10))],
            ),
            recording(
                &[("source", "vllm")],
                vec![("prometheus/scrape", counter_table("ops", &[(&[], 3)], 10))],
            ),
        ]);
        assert!(!reader.columns(r#"ops{source="vllm"}"#).unwrap().is_empty());
        assert!(reader.columns(r#"ops{source="other"}"#).unwrap().is_empty());
    }

    /// Recordings do not share row instants, so a query over two keeps the
    /// uniform grid.
    #[test]
    fn cross_cadence_timestamps_come_from_one_recording_only() {
        let slow = |name: &str| {
            use arrow::array::{ArrayRef, UInt64Array};
            use arrow::datatypes::{DataType, Field, Schema};
            use arrow::record_batch::RecordBatch;
            use parquet::arrow::ArrowWriter;
            let mut meta = HashMap::new();
            meta.insert("metric".to_string(), name.to_string());
            meta.insert("metric_type".to_string(), "counter".to_string());
            let schema = Arc::new(Schema::new(vec![
                Field::new("timestamp", DataType::UInt64, false),
                Field::new(name, DataType::UInt64, true).with_metadata(meta),
            ]));
            let ts: Vec<u64> = (0..5).map(|i| (i * 10 + 1) * S).collect();
            let vals: Vec<u64> = (0..5).collect();
            let mut buf = Vec::new();
            let mut w = ArrowWriter::try_new(&mut buf, schema.clone(), None).unwrap();
            w.write(
                &RecordBatch::try_new(
                    schema,
                    vec![
                        Arc::new(UInt64Array::from(ts)) as ArrayRef,
                        Arc::new(UInt64Array::from(vals)) as ArrayRef,
                    ],
                )
                .unwrap(),
            )
            .unwrap();
            w.close().unwrap();
            buf
        };
        let one = archive(vec![recording(
            &[("source", "a")],
            vec![
                ("fast", counter_table("fast_ops", &[(&[], 1)], 50)),
                ("slow", slow("slow_ops")),
            ],
        )]);
        assert!(
            eval_timestamps_for(&one, "fast_ops + slow_ops", 1.0, RateMode::default()).is_some()
        );

        let two = archive(vec![
            recording(
                &[("source", "a")],
                vec![("fast", counter_table("fast_ops", &[(&[], 1)], 50))],
            ),
            recording(&[("source", "b")], vec![("slow", slow("slow_ops"))]),
        ]);
        assert!(
            eval_timestamps_for(&two, "fast_ops + slow_ops", 1.0, RateMode::default()).is_none()
        );
    }
}
