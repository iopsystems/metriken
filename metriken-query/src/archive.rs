//! Queries over an archive ([`ArchiveReader`], `metriken-storage`'s): which
//! table answers a query, the evaluation timestamps for a query spanning
//! samplers of different cadence, and the [`MetricsSource`] implementation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use metriken_storage::labeled::{translate_filter, LabeledSource};
use metriken_storage::parquet::MultiParquetSource;
use metriken_storage::reader::{SamplerReader, SeriesLabels, TableReader};
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
/// and no other. A query reading tables of several recordings gets `None`:
/// such a query either evaluates each series on its own, which the uniform
/// grid answers at any cadence, or combines series across recordings,
/// which this reader refuses ([`MetricsSource`] on [`ArchiveReader`]).
pub fn eval_timestamps_for(
    reader: &ArchiveReader,
    query: &str,
    step_s: f64,
    rate_mode: RateMode,
) -> Option<Arc<[u64]>> {
    Plan::new(reader, query)
        .ok()?
        .cross_cadence(step_s, rate_mode)
}

/// What every query entry point works out once: the tables that can answer
/// it and the labels each recording's series carry.
struct Plan<'a> {
    owners: Vec<&'a SamplerReader>,
    labels: SeriesLabels,
}

impl<'a> Plan<'a> {
    /// The tables holding at least one metric the query references.
    ///
    /// Answered from each table's name catalog, so a table the query cannot
    /// touch is never opened. `referenced_metrics` is parse-only — it does
    /// not need a source, which is exactly why routing can use it and
    /// `columns` (which expands selectors through the source's column map)
    /// cannot.
    ///
    /// A table is also left out when no selector naming one of its metrics
    /// can match its recording's labels (`x{source="vllm"}` leaves out the
    /// other recordings), so a label-selected query reads one recording:
    /// it opens no other recording's tables, keeps that recording's
    /// cross-cadence alignment, and is not refused as combining recordings.
    /// Other label matchers are not consulted: a table holding the metric
    /// with no matching series answers with an empty result, which is
    /// correct, and skipping it here would route on data the catalog does
    /// not carry. When every table holding a referenced metric is left out,
    /// the first recording's are kept, so the query answers empty rather
    /// than as naming no metric.
    fn new(reader: &'a ArchiveReader, query: &str) -> Result<Self, QueryError> {
        let referenced: Vec<String> = crate::referenced_metrics(query)?.into_iter().collect();
        let all = reader.owners_of(&referenced);
        let selectors = crate::promql::selectors(query)?;
        let labels = reader.series_labels();
        let pruned: Vec<&SamplerReader> = all
            .iter()
            .copied()
            .filter(|t| {
                let extra = labels.of(t);
                selectors.iter().any(|(names, filter)| {
                    names.iter().any(|n| t.holds(n)) && translate_filter(extra, filter).is_some()
                })
            })
            .collect();
        let owners = if !pruned.is_empty() {
            pruned
        } else {
            match all.first() {
                Some(first) => all
                    .iter()
                    .copied()
                    .filter(|t| t.same_recording(first))
                    .collect(),
                None => all,
            }
        };
        Ok(Self { owners, labels })
    }

    /// The owning tables grouped by recording, in first-table order.
    fn recordings(&self) -> Vec<Vec<&'a SamplerReader>> {
        let mut out: Vec<Vec<&SamplerReader>> = Vec::new();
        for t in &self.owners {
            match out.iter_mut().find(|r| r[0].same_recording(t)) {
                Some(r) => r.push(t),
                None => out.push(vec![t]),
            }
        }
        out
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
    fn cross_cadence(&self, step_s: f64, rate_mode: RateMode) -> Option<Arc<[u64]>> {
        if matches!(rate_mode, RateMode::Raw) {
            return None;
        }
        let owners = &self.owners;
        if owners.len() < 2 {
            return None;
        }
        // The rows of one recording only (see `eval_timestamps_for`).
        if !owners.iter().all(|t| t.same_recording(owners[0])) {
            return None;
        }

        // Cadence comes from the ROWS, not from `interval()`: that reports
        // the recording's nominal interval, which every table in an archive
        // shares — on a real recording a 1 s sampler and a 30 s one both
        // answered 1.0, so asking it can never detect a cadence difference.
        // These are the instants the query path reads, nothing having
        // rounded them.
        //
        // Cadence is a property of the SAMPLER, not of a table. Two group
        // tables of one sampler are read together on one schedule; a group
        // that dedups or skips ticks is sparse WITHIN that cadence, not a
        // second cadence, and relocating a query onto its rows would
        // silently change the answer for a query that merely named a
        // sibling group's metric.
        //
        // So a sampler's cadence is the spacing of its DENSEST
        // participating table — the one that shows the underlying read
        // schedule.
        let mut by_sampler: BTreeMap<&str, (u64, &[u64])> = BTreeMap::new();
        for t in owners {
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
        // Deliberately a ratio, not equality: gaps measured from real rows
        // are never exactly equal, so "different cadence" has to mean
        // *materially* different. A sampler read at least twice as far
        // apart as another is a different cadence in any sense that matters
        // here.
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
/// recording labels, so `rate(x[1m])` over an archive holding several
/// producers of one run returns each producer's series, and
/// `x{source="vllm"}` selects one. A query that combines series of
/// different recordings is refused ([`refuse_combining_across_recordings`]).
///
/// Still refused, within one recording: a metric name held by two tables of
/// the query (`gpu_nvidia` and `gpu_amd_smi` both publish
/// `gpu_utilization`). Errors too when the query references no known metric.
fn route(plan: &Plan<'_>, query: &str) -> Result<QueryEngine, QueryError> {
    if plan.owners.is_empty() {
        return Err(QueryError::ParseError(format!(
            "query references no metric present in this .rez: {query}"
        )));
    }
    let recordings = plan.recordings();
    if recordings.len() > 1 {
        refuse_histograms_across_recordings(&recordings, &plan.labels, query)?;
        refuse_combining_across_recordings(plan, &recordings, query)?;
    }
    let mut sources: Vec<Arc<dyn DataSource>> = Vec::new();
    for tables in &recordings {
        if let Some(source) = recording_source(tables, query)? {
            sources.push(Arc::new(LabeledSource::new(
                source,
                plan.labels.of(tables[0]).clone(),
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

/// The recordings' label sets, as selectors, for an error message.
fn recording_selectors(recordings: &[Vec<&SamplerReader>], labels: &SeriesLabels) -> Vec<String> {
    recordings
        .iter()
        .map(|tables| {
            let set: Vec<String> = labels
                .of(tables[0])
                .inner
                .iter()
                .map(|(k, v)| format!("{k}=\"{v}\""))
                .collect();
            format!("{{{}}}", set.join(", "))
        })
        .collect()
}

/// Refuse a query that combines series of different recordings: an
/// aggregation, or an operator with a selector on both sides, whose
/// selectors reach more than one recording's tables.
///
/// The engine was written for series that share one table's rows, and
/// combining across recordings breaks that in several ways, each seen on a
/// test archive: two recordings' rows fall at different instants, so in
/// `RateMode::Raw` `sum(g)` alternated between them and at the range edges a
/// grid `sum` read one recording alone; different cadences across
/// recordings are not aligned (the cross-cadence timestamps answer for one
/// recording); binary operators match on visible labels only, so twin
/// recordings (told apart by the internal `__recording__`) paired one's
/// `g` with the other's `h`; and a lone unmatched series on the right of an
/// operator is paired with every unmatched series on the left whatever its
/// recording. Answering these is engine work (recording-aware matching,
/// alignment per recording, trimmed edges), so until then such a query is
/// refused, naming the recordings and a matcher that selects one. Each
/// series on its own (`rate(x[1m])`) is answered across recordings, and
/// any query narrowed to one recording is answered whole.
fn refuse_combining_across_recordings(
    plan: &Plan<'_>,
    recordings: &[Vec<&SamplerReader>],
    query: &str,
) -> Result<(), QueryError> {
    let combined = crate::promql::combination_sites(query)?
        .iter()
        .any(|selectors| recordings_selected(plan, selectors) > 1);
    if !combined {
        return Ok(());
    }
    let which = recording_selectors(recordings, &plan.labels);
    Err(QueryError::ParseError(format!(
        "query {query} combines series from {} recordings ({}); aggregations and \
         operators across recordings are not answered yet. Select one recording with \
         a label matcher (e.g. {}), or query the series without combining them",
        recordings.len(),
        which.join(", "),
        which[0],
    )))
}

/// How many recordings' tables `selectors` can read: tables of the plan
/// holding one of a selector's names under recording labels its matchers
/// accept.
fn recordings_selected(plan: &Plan<'_>, selectors: &[crate::promql::Selector]) -> usize {
    let mut seen: Vec<&SamplerReader> = Vec::new();
    for t in &plan.owners {
        let hit = selectors.iter().any(|(names, filter)| {
            names.iter().any(|n| t.holds(n))
                && translate_filter(plan.labels.of(t), filter).is_some()
        });
        if hit && !seen.iter().any(|r| r.same_recording(t)) {
            seen.push(t);
        }
    }
    seen.len()
}

/// Refuse a query that reads one histogram from several recordings.
///
/// The histogram functions pool, per row, the buckets of every series they
/// select, and two recordings' rows fall at different instants (two
/// producers, or two runs). Pooled, the answer alternates between the
/// recordings row by row instead of describing either. A label matcher
/// that selects one recording answers. The recordings' bucket
/// configurations may also differ, which pooling cannot reconcile.
fn refuse_histograms_across_recordings(
    recordings: &[Vec<&SamplerReader>],
    labels: &SeriesLabels,
    query: &str,
) -> Result<(), QueryError> {
    let referenced = crate::referenced_metrics(query)?;
    let mut names: Vec<&String> = referenced
        .iter()
        .filter(|name| {
            recordings
                .iter()
                .filter(|tables| tables.iter().any(|t| t.holds_histogram(name)))
                .count()
                > 1
        })
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    names.sort();
    let which = recording_selectors(recordings, labels);
    Err(QueryError::ParseError(format!(
        "query {query} reads histogram {} from {} recordings ({}); a histogram function \
         pools each row of the series it selects, and these recordings' rows fall at \
         different times, so select one recording with a label matcher, e.g. {}{}",
        names
            .iter()
            .map(|n| n.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        recordings.len(),
        which.join(", "),
        names[0],
        which[0],
    )))
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
        let plan = Plan::new(self, expr)?;
        let aligned;
        let opts = match plan.cross_cadence(step_s, opts.rate_mode) {
            Some(points) => {
                // Clone and set the one field: `QueryOptions` is
                // `#[non_exhaustive]`, so it cannot be built by literal from
                // here — and cloning preserves whatever else the caller set.
                aligned = opts.clone().with_eval_timestamps(Some(points));
                &aligned
            }
            None => opts,
        };
        route(&plan, expr)?.query_range_opts(expr, start_s, end_s, step_s, opts)
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
        let plan = Plan::new(self, expr)?;
        let aligned;
        let qopts = match plan.cross_cadence(step_s, qopts.rate_mode) {
            Some(points) => {
                aligned = qopts.clone().with_eval_timestamps(Some(points));
                &aligned
            }
            None => qopts,
        };
        route(&plan, expr)?.query_range_display_opts(expr, start_s, end_s, step_s, opts, qopts)
    }
    fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        route(&Plan::new(self, expr)?, expr)?.query(expr, time)
    }
    fn columns(&self, query: &str) -> Result<HashSet<String>, QueryError> {
        // Through the same labelled sources as a query, so a selector
        // naming a recording label resolves to that recording's columns.
        let plan = Plan::new(self, query)?;
        let mut out = HashSet::new();
        for t in &plan.owners {
            let Some(r) = t.reader() else {
                continue;
            };
            let source = LabeledSource::new(r.source(), plan.labels.of(t).clone());
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
    /// recording labels. This was refused as cross-recording. Combining
    /// them is refused until the engine aligns and matches across
    /// recordings; one recording selected by label is answered whole.
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
        // The filter's regex form is literals and alternations.
        assert_eq!(
            series(&reader, r#"rate(ops{source=~"rezolus|other"}[3s])"#).len(),
            1
        );
        assert_eq!(
            series(&reader, r#"sum(rate(ops{source="vllm"}[3s]))"#),
            [(labels(&[]), 3.0)]
        );
        for combining in ["sum(rate(ops[3s]))", "sum by (source) (rate(ops[3s]))"] {
            let err = reader
                .query_range_opts(combining, 1.0, 10.0, 1.0, &QueryOptions::default())
                .unwrap_err()
                .to_string();
            assert!(err.contains("combines series from 2 recordings"), "{err}");
            assert!(err.contains(r#"{source="rezolus"}"#), "{err}");
        }
    }

    /// A series' own label under a recording key keeps its value under
    /// `exported_<key>`, equal values included, so the two series stay apart.
    #[test]
    fn a_clashing_series_label_moves_to_exported() {
        let reader = archive(vec![recording(
            &[("source", "svc"), ("host", "lb")],
            vec![(
                "prometheus/scrape",
                counter_table(
                    "requests",
                    &[
                        (&[("host", "backend-1")], 1),
                        (&[("host", "lb")], 2),
                        (&[], 4),
                    ],
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
                (
                    labels(&[(&exported, "lb"), ("host", "lb"), ("source", "svc")]),
                    2.0
                ),
                (labels(&[("host", "lb"), ("source", "svc")]), 4.0),
            ]
        );
        assert_eq!(
            series(&reader, r#"rate(requests{exported_host="backend-1"}[3s])"#).len(),
            1
        );
        // The series without its own host has no exported_host, so a
        // negative matcher keeps it.
        assert_eq!(
            series(&reader, r#"rate(requests{exported_host!="backend-1"}[3s])"#).len(),
            2
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
        assert_eq!(
            series(&reader, r#"sum(rate(ops{__recording__="1"}[3s]))"#),
            [(labels(&[]), 3.0)]
        );
        assert!(reader
            .query_range_opts(
                "sum(rate(ops[3s]))",
                1.0,
                10.0,
                1.0,
                &QueryOptions::default()
            )
            .is_err());
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

    /// The cross-cadence timestamps answer for one recording: a query
    /// reading two gets the uniform grid.
    #[test]
    fn cross_cadence_timestamps_come_from_one_recording() {
        let one = archive(vec![recording(
            &[("source", "a")],
            vec![
                ("fast", counter_table("fast_ops", &[(&[], 1)], 50)),
                ("slow", sparse_counter("slow_ops")),
            ],
        )]);
        let at = |r: &ArchiveReader| {
            eval_timestamps_for(r, "fast_ops + slow_ops", 1.0, RateMode::default())
        };
        assert!(at(&one).is_some());

        let two = archive(vec![
            recording(
                &[("source", "a")],
                vec![("fast", counter_table("fast_ops", &[(&[], 1)], 50))],
            ),
            recording(
                &[("source", "b")],
                vec![("slow", sparse_counter("slow_ops"))],
            ),
        ]);
        assert!(at(&two).is_none());
    }

    /// Rows 10 s apart, a day after the others: another run.
    fn late_rows() -> Vec<(u64, u64)> {
        (0..5).map(|i| ((86_400 + i * 10) * S, i)).collect()
    }

    /// A table of one counter or gauge `name` at the given `(ts, value)` rows.
    fn explicit_table(name: &str, kind: &str, rows: &[(u64, u64)]) -> Vec<u8> {
        use arrow::array::{ArrayRef, Int64Array, UInt64Array};
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        let mut meta = HashMap::new();
        meta.insert("metric".to_string(), name.to_string());
        meta.insert("metric_type".to_string(), kind.to_string());
        let gauge = kind == "gauge";
        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new(
                name,
                if gauge {
                    DataType::Int64
                } else {
                    DataType::UInt64
                },
                true,
            )
            .with_metadata(meta),
        ]));
        let ts: ArrayRef = Arc::new(UInt64Array::from(
            rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        ));
        let values: ArrayRef = if gauge {
            Arc::new(Int64Array::from(
                rows.iter().map(|r| r.1 as i64).collect::<Vec<_>>(),
            ))
        } else {
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.1).collect::<Vec<_>>(),
            ))
        };
        let mut buf = Vec::new();
        let mut w = ArrowWriter::try_new(&mut buf, schema.clone(), None).unwrap();
        w.write(&RecordBatch::try_new(schema, vec![ts, values]).unwrap())
            .unwrap();
        w.close().unwrap();
        buf
    }

    fn values(
        reader: &ArchiveReader,
        query: &str,
        mode: RateMode,
    ) -> Result<Vec<Vec<(f64, f64)>>, String> {
        let r = reader
            .query_range_opts(query, 1.0, 40.0, 1.0, &QueryOptions::with_rate_mode(mode))
            .map_err(|e| e.to_string())?;
        let QueryResult::Matrix { result } = r else {
            panic!("{query}: not a matrix");
        };
        Ok(result.into_iter().map(|m| m.values).collect())
    }

    /// A counter table with rows 10 s apart.
    fn sparse_counter(name: &str) -> Vec<u8> {
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
    }

    /// A label-selected query reads one recording, so it keeps that
    /// recording's cross-cadence alignment even beside a recording it shares
    /// no instants with.
    #[test]
    fn a_label_selected_query_keeps_its_recordings_alignment() {
        // Recording b is another run (a day later), so the two share no
        // instants and an unqualified query stays on the grid.
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![
                    ("fast", counter_table("fast_ops", &[(&[], 1)], 50)),
                    ("slow", sparse_counter("slow_ops")),
                ],
            ),
            recording(
                &[("source", "b")],
                vec![
                    ("fast", explicit_table("fast_ops", "counter", &late_rows())),
                    ("slow", explicit_table("slow_ops", "counter", &late_rows())),
                ],
            ),
        ]);
        let at = |q: &str| eval_timestamps_for(&reader, q, 1.0, RateMode::default());
        assert!(at("fast_ops + slow_ops").is_none());
        assert!(at(r#"fast_ops{source="a"} + slow_ops{source="a"}"#).is_some());
        // A selector that matches no recording still answers, empty.
        assert!(series(&reader, r#"rate(fast_ops{source="c"}[3s])"#).is_empty());
    }

    /// A histogram in two recordings: an unqualified query is refused,
    /// because the histogram functions pool each row and the recordings'
    /// rows fall at different times (here half a second apart, which made
    /// the answer alternate between the recordings row by row); a matcher
    /// selecting one recording answers. The recordings' bucket
    /// configurations also differ here, which pooling could not reconcile.
    #[test]
    fn a_histogram_across_recordings_is_refused() {
        use crate::segmented::tests::segment_histogram;
        let rows = |n: usize, offset: u64| -> Vec<(u64, Vec<u64>)> {
            (1..=10u64)
                .map(|t| {
                    let mut b = vec![0u64; n];
                    b[n / 2] = t * 10;
                    (t * S + offset, b)
                })
                .collect()
        };
        let a = histogram::Config::new(3, 10).unwrap().total_buckets();
        let b = histogram::Config::new(7, 20).unwrap().total_buckets();
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![("lat", segment_histogram("lat", 3, 10, &rows(a, 0)))],
            ),
            recording(
                &[("source", "b")],
                vec![("lat", segment_histogram("lat", 7, 20, &rows(b, S / 2)))],
            ),
        ]);
        let p50 =
            |query: &str| reader.query_range_opts(query, 2.0, 10.0, 1.0, &QueryOptions::default());
        let err = p50("histogram_quantiles([0.5], lat)")
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 recordings"), "{err}");
        assert!(err.contains(r#"lat{source="a"}"#), "{err}");
        let one = |query: &str| -> Vec<(f64, f64)> {
            let QueryResult::Matrix { result } = p50(query).unwrap_or_else(|e| panic!("{e}"))
            else {
                panic!("{query}: not a matrix");
            };
            assert_eq!(result.len(), 1, "{query}");
            result[0].values.clone()
        };
        let only_a = one(r#"histogram_quantiles([0.5], lat{source="a"})"#);
        let only_b = one(r#"histogram_quantiles([0.5], lat{source="b"})"#);
        // Each on its own recording's rows, one value throughout.
        assert!(only_a.windows(2).all(|w| w[0].1 == w[1].1), "{only_a:?}");
        assert!(only_b.windows(2).all(|w| w[0].1 == w[1].1), "{only_b:?}");
        assert_ne!(only_a[0].1, only_b[0].1);
    }

    /// A twin without tables still marks the other: the label does not come
    /// and go as a recording gains or loses tables.
    #[test]
    fn a_twin_without_tables_still_counts() {
        let reader = archive(vec![
            recording(&[("source", "rezolus")], vec![]),
            recording(
                &[("source", "rezolus")],
                vec![("cpu_usage", counter_table("ops", &[(&[], 2)], 10))],
            ),
        ]);
        let got: Vec<String> = series(&reader, "rate(ops[3s])")
            .into_iter()
            .map(|(l, _)| l[RECORDING_LABEL].clone())
            .collect();
        assert_eq!(got, ["1"]);
    }

    /// Readers of different archives joined by `flatten` repeat catalog
    /// indices; a group of twins that repeats one is numbered by position,
    /// so no two of its recordings share a value.
    #[test]
    fn flattened_twins_never_share_a_recording_label() {
        let one = || {
            recording(
                &[("source", "x")],
                vec![("cpu_usage", counter_table("ops", &[(&[], 2)], 10))],
            )
        };
        let reader = ArchiveReader::flatten(
            vec![archive(vec![one()]), archive(vec![one(), one()])],
            None,
        );
        let mut got: Vec<String> = series(&reader, "rate(ops[3s])")
            .into_iter()
            .map(|(l, _)| l[RECORDING_LABEL].clone())
            .collect();
        got.sort();
        assert_eq!(got, ["0", "1", "2"]);
    }

    fn gauge_rows(from: u64, to: u64, every: u64, v: u64) -> Vec<(u64, u64)> {
        (from..=to)
            .step_by(every as usize)
            .map(|t| (t * S, v))
            .collect()
    }

    /// A recording at a slower cadence, starting halfway through, does not
    /// thin the others: each series is read on the grid at full resolution.
    #[test]
    fn a_partly_overlapping_recording_does_not_thin_the_others() {
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![(
                    "fast",
                    explicit_table("g", "gauge", &gauge_rows(1, 40, 1, 10)),
                )],
            ),
            recording(
                &[("source", "b")],
                vec![(
                    "slow",
                    explicit_table("g", "gauge", &gauge_rows(21, 40, 10, 100)),
                )],
            ),
        ]);
        assert!(eval_timestamps_for(&reader, "g", 1.0, RateMode::default()).is_none());
        let per_series = values(&reader, r#"g"#, RateMode::default()).unwrap();
        assert!(per_series.iter().any(|s| s.len() == 40), "{per_series:?}");
    }

    /// Combining series of different recordings is refused in every mode:
    /// aggregations, and operators with a selector on both sides, including
    /// over twin recordings, where the engine's matching (visible labels
    /// only) paired one recording's `g` with the other's `h`. A query
    /// narrowed to one recording, and each series on its own, are answered.
    #[test]
    fn combining_across_recordings_is_refused_in_every_mode() {
        let both = |source: &'static str, g: u64, h: u64| {
            recording(
                &[("source", source)],
                vec![
                    ("tg", explicit_table("g", "gauge", &gauge_rows(1, 40, 1, g))),
                    ("th", explicit_table("h", "gauge", &gauge_rows(1, 40, 1, h))),
                ],
            )
        };
        for reader in [
            archive(vec![both("a", 10, 2), both("b", 100, 4)]),
            archive(vec![both("same", 10, 2), both("same", 100, 4)]),
        ] {
            for mode in [RateMode::default(), RateMode::Raw] {
                for q in ["sum(g)", "g / h", "g / on() h", "max(g) - min(h)"] {
                    let err = values(&reader, q, mode).unwrap_err();
                    assert!(
                        err.contains("combines series from 2 recordings"),
                        "{q}: {err}"
                    );
                }
                assert_eq!(values(&reader, "g", mode).unwrap().len(), 2);
                assert_eq!(values(&reader, "g * 2", mode).unwrap().len(), 2);
            }
        }
        let reader = archive(vec![both("a", 10, 2), both("b", 100, 4)]);
        let ratio = values(
            &reader,
            r#"g{source="b"} / h{source="b"}"#,
            RateMode::default(),
        )
        .unwrap();
        assert_eq!(ratio.len(), 1);
        assert!(ratio[0].iter().all(|(_, v)| *v == 25.0), "{ratio:?}");
    }
}
