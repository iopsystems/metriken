//! Queries over an archive ([`ArchiveReader`], `metriken-storage`'s): which
//! table answers a query, the evaluation timestamps for a query spanning
//! samplers of different cadence, and the [`MetricsSource`] implementation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use metriken_storage::labeled::{translate_filter, LabeledSource};
use metriken_storage::reader::{SamplerReader, SeriesLabels, TableReader};
use metriken_storage::recordings::RecordingsSource;
use metriken_storage::union::UnionSource;
use metriken_storage::{table_sampler, ArchiveReader, DataSource, UnionChild, UnionError};

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
/// and no other, and a query reading tables of several recordings gets
/// `None`. Querying an [`ArchiveReader`] through [`MetricsSource`] evaluates
/// such a query once per recording, each with its own timestamps.
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
    labels: &'a SeriesLabels,
    /// The metric names the query references that some table of the
    /// archive holds (before pruning).
    held: Vec<String>,
    /// Whether one of `held` is in none of `owners`: a matcher left out
    /// every recording holding it.
    missing: bool,
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
    /// A recording is also left out when no selector naming one of its
    /// metrics can match its labels (`x{source="vllm"}` leaves out the other
    /// recordings), so a label-selected query reads one recording: it opens
    /// no other recording's tables, keeps that recording's cross-cadence
    /// alignment, and is not refused as merging recordings. A recording is
    /// kept or left out whole: one kept for `h` in `g{source="a"} / h` keeps
    /// its `g` too, so it answers that query empty rather than as lacking
    /// `g`. Other label matchers are not consulted: a table holding the
    /// metric with no matching series answers with an empty result, which
    /// is correct, and skipping it here would route on data the catalog
    /// does not carry. When every recording is left out, the first one's
    /// tables are kept, so the query answers empty rather than as naming no
    /// metric.
    fn new(reader: &'a ArchiveReader, query: &str) -> Result<Self, QueryError> {
        // The filter keeps one matcher per label (the last); with recording
        // labels, excluding several sources is an ordinary query, so a
        // repeat is refused rather than half applied, for every label.
        if let Some(label) = crate::promql::repeated_matcher(query)? {
            return Err(QueryError::ParseError(format!(
                "query {query} matches label {label} more than once in one selector, and \
                 only one matcher per label is applied; use one matcher per label (to \
                 exclude several values, {label}!~\"a|b\"; regex matchers take literal \
                 alternatives only)"
            )));
        }
        let referenced: Vec<String> = crate::referenced_metrics(query)?.into_iter().collect();
        let all = reader.owners_of(&referenced);
        let selectors = crate::promql::selectors(query)?;
        let labels = reader.series_labels();
        let mut passing: Vec<&SamplerReader> = Vec::new();
        for t in &all {
            let extra = labels.of(t);
            let pass = selectors.iter().any(|(names, filter)| {
                names.iter().any(|n| t.holds(n)) && translate_filter(extra, filter).is_some()
            });
            if pass && !passing.iter().any(|r| r.same_recording(t)) {
                passing.push(t);
            }
        }
        let keep = |t: &&SamplerReader| match passing.is_empty() {
            false => passing.iter().any(|r| r.same_recording(t)),
            true => all.first().is_some_and(|first| first.same_recording(t)),
        };
        let owners: Vec<&SamplerReader> = all.iter().copied().filter(keep).collect();
        let held: Vec<String> = referenced
            .into_iter()
            .filter(|n| all.iter().any(|t| t.holds(n)))
            .collect();
        let missing = held.iter().any(|n| !owners.iter().any(|t| t.holds(n)));
        Ok(Self {
            owners,
            labels,
            held,
            missing,
        })
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

    /// `opts` with the query's cross-cadence timestamps, when it has any.
    fn opts(&self, opts: &QueryOptions, step_s: f64) -> QueryOptions {
        match self.cross_cadence(step_s, opts.rate_mode) {
            // Clone and set the one field: `QueryOptions` is
            // `#[non_exhaustive]`, so it cannot be built by literal from here
            // — and cloning preserves whatever else the caller set.
            Some(points) => opts.clone().with_eval_timestamps(Some(points)),
            None => opts.clone(),
        }
    }

    /// The cross-cadence timestamps of a query over one recording; `None`
    /// over several (see [`eval_timestamps_for`]).
    fn cross_cadence(&self, step_s: f64, rate_mode: RateMode) -> Option<Arc<[u64]>> {
        let owners = &self.owners;
        if !owners.iter().all(|t| t.same_recording(owners[0])) {
            return None;
        }
        cross_cadence(owners, step_s, rate_mode)
    }
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
///
/// `owners` are tables of one recording.
fn cross_cadence(
    owners: &[&SamplerReader],
    step_s: f64,
    rate_mode: RateMode,
) -> Option<Arc<[u64]>> {
    if matches!(rate_mode, RateMode::Raw) {
        return None;
    }
    if owners.len() < 2 {
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

/// The engine a query is evaluated with: over the one recording it reaches,
/// or over every recording it reaches as one source
/// ([`RecordingsSource`]), each recording's series carrying its labels.
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
/// Across recordings the engine evaluates as it does within one: the same
/// metric in two recordings is two sets of series, told apart by their
/// recording labels, and an aggregation or operator combines them as
/// PromQL does (`sum(x)` sums every recording's series, `g / h` pairs each
/// recording's `g` with its own `h`). Each series is read at its own
/// recording's interval and row times; see [`RecordingsSource`] and the
/// engine's `Join`.
///
/// Refused: a metric name held by two tables of one recording
/// (`gpu_nvidia` and `gpu_amd_smi` both publish `gpu_utilization`), and a
/// name that is a different kind in two recordings the query reaches.
/// Errors too when the query references no known metric.
fn engine(plan: &Plan<'_>, query: &str) -> Result<Option<QueryEngine>, QueryError> {
    if plan.owners.is_empty() {
        return Err(QueryError::ParseError(format!(
            "query references no metric present in this .rez: {query}"
        )));
    }
    // A metric the archive holds that no recording the query reaches holds
    // (`x{source="b"} + y` with `x` only in a) has no series, and neither
    // has anything combined with it, as in Prometheus. Evaluated, it would
    // be "metric not found".
    if plan.missing {
        return Ok(None);
    }
    let recordings = plan.recordings();
    if recordings.len() > 1 {
        refuse_kinds_across_recordings(plan, &recordings, query)?;
    }
    let mut sources: Vec<Arc<dyn DataSource>> = Vec::new();
    for tables in &recordings {
        if let Some(source) = recording_source(tables, query)? {
            let labels = plan.labels.of(tables[0]).clone();
            // The interval measured from the rows of the recording's
            // fastest table the query reads. A segment declares none and
            // reports 1 s, so a 10 s recording's gauge was held 1 s past
            // each row and missing between them.
            let interval = tables
                .iter()
                .map(|t| t.interval())
                .filter(|i| *i > 0.0)
                .min_by(f64::total_cmp);
            let mut labeled = LabeledSource::new(source, labels);
            if let Some(interval) = interval {
                labeled = labeled.with_interval(interval);
            }
            sources.push(Arc::new(labeled));
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
        1 => sources.pop().expect("one"),
        _ => Arc::new(RecordingsSource::new(sources)),
    };
    Ok(Some(QueryEngine::new(source)))
}

/// Refuse a result in which two series share a label set. Two recordings'
/// series can: one recording carries `host` as a recording label and the
/// other on its series. Prometheus refuses a vector holding one label set
/// twice, and a reader would take the two for one.
fn refuse_shared_label_sets<'r>(
    metrics: impl Iterator<Item = &'r HashMap<String, String>>,
) -> Result<(), QueryError> {
    let mut seen: HashSet<Vec<(&String, &String)>> = HashSet::new();
    for metric in metrics {
        let mut key: Vec<(&String, &String)> = metric.iter().collect();
        key.sort();
        if !seen.insert(key) {
            return Err(QueryError::Unsupported(format!(
                "the result holds two series with the labels {metric:?}, from different \
                 recordings; aggregate them (e.g. sum by (...)) or select one with a label \
                 matcher"
            )));
        }
    }
    Ok(())
}

fn checked(result: QueryResult) -> Result<QueryResult, QueryError> {
    match &result {
        QueryResult::Matrix { result } => {
            refuse_shared_label_sets(result.iter().map(|m| &m.metric))?
        }
        QueryResult::Vector { result } => {
            refuse_shared_label_sets(result.iter().map(|m| &m.metric))?
        }
        _ => {}
    }
    Ok(result)
}

fn checked_display(result: crate::DisplayResult) -> Result<crate::DisplayResult, QueryError> {
    use crate::DisplayResult;
    match &result {
        DisplayResult::Series { result, .. } => {
            refuse_shared_label_sets(result.iter().map(|m| &m.metric))?
        }
        DisplayResult::Vector { result } => {
            refuse_shared_label_sets(result.iter().map(|m| &m.metric))?
        }
        _ => {}
    }
    Ok(result)
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

/// Refuse a query naming a metric that is a counter in one recording it
/// reaches and a gauge (or a histogram) in another.
///
/// Each function reads one kind: `rate(x[1m])` reads counters, a bare `x`
/// gauges. Over such recordings it would read the series of the
/// recordings holding that kind and leave the others out without a word.
fn refuse_kinds_across_recordings(
    plan: &Plan<'_>,
    recordings: &[Vec<&SamplerReader>],
    query: &str,
) -> Result<(), QueryError> {
    let kind = |tables: &[&SamplerReader], name: &str| -> Option<&'static str> {
        tables.iter().find_map(|t| {
            if t.holds_counter(name) {
                Some("counter")
            } else if t.holds_gauge(name) {
                Some("gauge")
            } else if t.holds_histogram(name) {
                Some("histogram")
            } else {
                None
            }
        })
    };
    let which = recording_selectors(recordings, plan.labels);
    for name in &plan.held {
        let kinds: Vec<(usize, &'static str)> = recordings
            .iter()
            .enumerate()
            .filter_map(|(i, tables)| kind(tables, name).map(|k| (i, k)))
            .collect();
        let Some(&(first, first_kind)) = kinds.first() else {
            continue;
        };
        if let Some(&(other, other_kind)) = kinds.iter().find(|(_, k)| *k != first_kind) {
            return Err(QueryError::Unsupported(format!(
                "query {query} reads {name}, which is a {first_kind} in {} and a {other_kind} \
                 in {}; select the recordings of one kind with a label matcher",
                which[first], which[other],
            )));
        }
    }
    Ok(())
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
    // ── Query methods: one engine over the recordings the query reaches. ──
    fn query_range_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        let plan = Plan::new(self, expr)?;
        let Some(engine) = engine(&plan, expr)? else {
            return Ok(QueryResult::Matrix { result: Vec::new() });
        };
        checked(engine.query_range_opts(expr, start_s, end_s, step_s, &plan.opts(opts, step_s))?)
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
        let Some(engine) = engine(&plan, expr)? else {
            return Ok(crate::DisplayResult::Series {
                result: Vec::new(),
                budget: opts.budget as u32,
            });
        };
        checked_display(engine.query_range_display_opts(
            expr,
            start_s,
            end_s,
            step_s,
            opts,
            &plan.opts(qopts, step_s),
        )?)
    }
    fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        let plan = Plan::new(self, expr)?;
        // On an archive of several recordings, at the archive's end whichever
        // recordings the query reaches: the engine's own default is the end
        // of what it reads, which would move with a label matcher.
        let time = time.or_else(|| {
            (self.recording_count() > 1)
                .then(|| ArchiveReader::time_range_ns(self).map(|(_, end)| end as f64 / 1e9))
                .flatten()
        });
        let Some(engine) = engine(&plan, expr)? else {
            return Ok(QueryResult::Vector { result: Vec::new() });
        };
        checked(engine.query(expr, time)?)
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
    /// recording labels, which a label matcher selects and an aggregation
    /// combines.
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
        // Grouped by the label that tells the recordings apart, a sum is
        // one per recording.
        assert_eq!(
            series(&reader, "sum by (source) (rate(ops[3s]))"),
            [
                (labels(&[("source", "rezolus")]), 2.0),
                (labels(&[("source", "vllm")]), 3.0)
            ]
        );
        assert_eq!(series(&reader, "sum(rate(ops[3s]))"), [(labels(&[]), 5.0)]);
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
        assert_eq!(series(&reader, "sum(rate(ops[3s]))"), [(labels(&[]), 5.0)]);
        // An operator pairs each twin with itself: `__recording__` is
        // matched on.
        assert_eq!(
            series(&reader, "rate(ops[3s]) / rate(ops[3s])"),
            [
                (
                    labels(&[(RECORDING_LABEL, "0"), ("source", "rezolus")]),
                    1.0
                ),
                (
                    labels(&[(RECORDING_LABEL, "1"), ("source", "rezolus")]),
                    1.0
                ),
            ]
        );
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

    /// A histogram in two recordings is pooled on the slower recording's
    /// rows. The recordings' rows fall half a second apart; pooled by exact
    /// row time, the answer alternated between the recordings row by row.
    /// Their bucket configurations differ too, and are merged under the
    /// coarser.
    #[test]
    fn a_histogram_across_recordings_is_pooled() {
        use crate::segmented::tests::segment_histogram;
        // 10 more observations a second, all in one bucket: `value`.
        let rows = |g: u8, m: u8, value: u64, offset: u64| -> Vec<(u64, Vec<u64>)> {
            let config = histogram::Config::new(g, m).unwrap();
            let mut h = histogram::Histogram::with_config(&config);
            h.increment(value).unwrap();
            let bucket = histogram::SparseHistogram::from(&h).index()[0] as usize;
            (1..=10u64)
                .map(|t| {
                    let mut b = vec![0u64; config.total_buckets()];
                    b[bucket] = t * 10;
                    (t * S + offset, b)
                })
                .collect()
        };
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![("lat", segment_histogram("lat", 3, 10, &rows(3, 10, 100, 0)))],
            ),
            recording(
                &[("source", "b")],
                vec![(
                    "lat",
                    segment_histogram("lat", 7, 20, &rows(7, 20, 5000, S / 2)),
                )],
            ),
        ]);
        let quantile = |query: &str| -> Vec<Vec<(f64, f64)>> {
            let QueryResult::Matrix { result } = reader
                .query_range_opts(query, 2.0, 10.0, 1.0, &QueryOptions::default())
                .unwrap_or_else(|e| panic!("{query}: {e}"))
            else {
                panic!("{query}: not a matrix");
            };
            result.into_iter().map(|m| m.values).collect()
        };
        let only_a = quantile(r#"histogram_quantiles([0.5], lat{source="a"})"#);
        let only_b = quantile(r#"histogram_quantiles([0.5], lat{source="b"})"#);
        let p25 = quantile("histogram_quantiles([0.25], lat)");
        let p75 = quantile("histogram_quantiles([0.75], lat)");
        // Each tick pools ten observations of each: the lower quartile is a's
        // bucket and the upper b's, at every point.
        let (a, b) = (only_a[0][0].1, only_b[0][0].1);
        assert!(a < b, "{a} {b}");
        assert!(
            p25[0].iter().all(|(_, v)| *v <= a * 1.2 && *v >= a * 0.8),
            "{p25:?}"
        );
        assert!(
            p75[0].iter().all(|(_, v)| *v <= b * 1.2 && *v >= b * 0.8),
            "{p75:?}"
        );
        // On b's rows, 3.5 s to 9.5 s; a's row at 10 s is in the interval
        // after b's last row read, which the range cut.
        assert_eq!(p25[0].len(), 7, "{p25:?}");
        let counts = quantile("histogram_count(lat)");
        assert!(counts[0].iter().all(|(_, v)| *v == 20.0), "{counts:?}");
    }

    /// A histogram from a 1 s recording and a 10 s one is pooled on the
    /// 10 s recording's rows: each tick holds ten of the fast recording's
    /// deltas and one of the slow one's. On the fast recording's rows the
    /// slow one's delta landed on every tenth tick.
    #[test]
    fn a_histogram_is_pooled_on_the_slowest_recordings_rows() {
        use crate::segmented::tests::segment_histogram;
        let rows = |every: u64, per_row: u64| -> Vec<(u64, Vec<u64>)> {
            let n = histogram::Config::new(4, 10).unwrap().total_buckets();
            (1..=60 / every)
                .map(|k| {
                    let mut b = vec![0u64; n];
                    b[20] = k * per_row;
                    (k * every * S, b)
                })
                .collect()
        };
        let reader = archive(vec![
            recording(
                &[("source", "fast")],
                vec![("lat", segment_histogram("lat", 4, 10, &rows(1, 1)))],
            ),
            recording(
                &[("source", "slow")],
                vec![("lat", segment_histogram("lat", 4, 10, &rows(10, 10)))],
            ),
        ]);
        let QueryResult::Matrix { result } = reader
            .query_range_opts(
                "histogram_count(lat)",
                1.0,
                60.0,
                1.0,
                &QueryOptions::default(),
            )
            .unwrap()
        else {
            panic!("not a matrix");
        };
        let counts: Vec<(f64, f64)> = result[0].values.clone();
        // The first slow row has no delta; from 20 s on every tick pools
        // both.
        let after: Vec<&(f64, f64)> = counts.iter().filter(|(t, _)| *t >= 20.0).collect();
        assert_eq!(after.len(), 5, "{counts:?}");
        assert!(after.iter().all(|(_, v)| *v == 20.0), "{counts:?}");
    }

    /// A gauge of a recording read every 10 s, in segments that declare no
    /// interval, is held between its rows: the interval is measured from
    /// the rows, not the 1 s a segment reports.
    #[test]
    fn a_slow_recordings_gauge_is_held_between_its_rows() {
        let reader = archive(vec![recording(
            &[("source", "slow")],
            vec![("t", explicit_table("g", "gauge", &gauge_rows(1, 40, 10, 7)))],
        )]);
        let got = values(&reader, "g", RateMode::default()).unwrap();
        // Rows at 1, 11, 21, 31: every second from 1 s to 40 s.
        assert_eq!(got[0].len(), 40, "{got:?}");
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

    /// Across recordings an operator pairs the series of each recording (also
    /// for twins told apart only by `__recording__`), and an aggregation
    /// combines them, in every mode.
    #[test]
    fn combinations_across_recordings_are_answered() {
        let both = |source: &'static str, g: u64, h: u64| {
            recording(
                &[("source", source)],
                vec![
                    ("tg", explicit_table("g", "gauge", &gauge_rows(1, 40, 1, g))),
                    ("th", explicit_table("h", "gauge", &gauge_rows(1, 40, 1, h))),
                ],
            )
        };
        let last = |r: &ArchiveReader, q: &str, mode| -> Vec<f64> {
            let mut v: Vec<f64> = values(r, q, mode)
                .unwrap_or_else(|e| panic!("{q}: {e}"))
                .into_iter()
                .map(|s| s.last().unwrap().1)
                .collect();
            v.sort_by(f64::total_cmp);
            v
        };
        let distinct = archive(vec![both("a", 10, 2), both("b", 100, 4)]);
        let twins = archive(vec![both("same", 10, 2), both("same", 100, 4)]);
        for mode in [RateMode::default(), RateMode::Raw] {
            for reader in [&distinct, &twins] {
                assert_eq!(last(reader, "g / h", mode), [5.0, 25.0]);
                assert_eq!(last(reader, "g * 2", mode), [20.0, 200.0]);
                assert_eq!(last(reader, "sum(g)", mode), [110.0]);
                assert_eq!(last(reader, "max(g) - min(h)", mode), [98.0]);
                assert_eq!(last(reader, "sum(g) / sum(h)", mode), [110.0 / 6.0]);
            }
            assert_eq!(last(&distinct, "sum by (source) (g)", mode), [10.0, 100.0]);
            assert_eq!(
                last(&distinct, "sum by (source) (g) / sum by (source) (h)", mode),
                [5.0, 25.0]
            );
            // Twins differ only in `__recording__`, which `by (source)` drops.
            assert_eq!(last(&twins, "sum by (source) (g)", mode), [110.0]);
            assert_eq!(
                last(&twins, "sum by (__recording__) (g)", mode),
                [10.0, 100.0]
            );
        }
    }

    /// Recordings read at different instants and cadences combine at every
    /// point: a 1 s recording and a 10 s one half a second out of phase sum
    /// to both at every grid point, the slow one held between its rows.
    #[test]
    fn recordings_of_different_phase_and_cadence_sum_at_every_point() {
        let rows = |every: u64, offset: u64, v: u64| -> Vec<(u64, u64)> {
            (0..=40 / every)
                .map(|k| (k * every * S + offset, v))
                .filter(|(t, _)| *t >= S)
                .collect()
        };
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![("t", declared_table("g", &rows(1, 0, 10), 1000))],
            ),
            recording(
                &[("source", "b")],
                vec![("t", declared_table("g", &rows(10, S / 2, 100), 10_000))],
            ),
        ]);
        let sum = values(&reader, "sum(g)", RateMode::default()).unwrap();
        assert_eq!(sum.len(), 1);
        // b's first row is at 10.5 s; from 11 s on, every point has both.
        let after: Vec<f64> = sum[0]
            .iter()
            .filter(|(t, _)| *t >= 11.0)
            .map(|(_, v)| *v)
            .collect();
        assert_eq!(after.len(), 30, "{sum:?}");
        assert!(after.iter().all(|v| *v == 110.0), "{sum:?}");
    }

    /// Each recording is evaluated with its own interval: a gauge of a
    /// recording that declares 10 s stays as fresh beside a 1 s recording
    /// as alone. Joined into one source, it went stale after the fast
    /// recording's 1 s.
    #[test]
    fn each_recording_keeps_its_own_interval() {
        let fast = || {
            recording(
                &[("source", "a")],
                vec![("t", declared_table("g", &gauge_rows(1, 40, 1, 10), 1000))],
            )
        };
        let slow = || {
            recording(
                &[("source", "b")],
                vec![(
                    "t",
                    declared_table("g", &gauge_rows(1, 40, 10, 100), 10_000),
                )],
            )
        };
        let alone = archive(vec![slow()]);
        let joined = archive(vec![fast(), slow()]);
        let points = |r: &ArchiveReader, q: &str| -> usize {
            values(r, q, RateMode::default())
                .unwrap()
                .iter()
                .map(|s| s.len())
                .max()
                .unwrap()
        };
        assert_eq!(points(&joined, r#"g{source="b"}"#), points(&alone, "g"),);
        let QueryResult::Matrix { result } = joined
            .query_range_opts("g", 1.0, 40.0, 1.0, &QueryOptions::default())
            .unwrap()
        else {
            panic!("not a matrix");
        };
        let b = result
            .iter()
            .find(|m| m.metric.get("source").map(String::as_str) == Some("b"))
            .unwrap();
        assert_eq!(b.values.len(), points(&alone, "g"));
        let QueryResult::Vector { result } = joined.query("g", Some(25.0)).unwrap() else {
            panic!("not a vector");
        };
        assert_eq!(result.len(), 2, "{result:?}");
    }

    /// A name that is a gauge in one recording and a counter in another is
    /// an error for the recording that reads it wrong, not a silent drop.
    #[test]
    fn a_name_of_two_kinds_across_recordings_is_an_error() {
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![("t", explicit_table("m", "gauge", &gauge_rows(1, 40, 1, 10)))],
            ),
            recording(
                &[("source", "b")],
                vec![(
                    "t",
                    explicit_table("m", "counter", &gauge_rows(1, 40, 1, 10)),
                )],
            ),
        ]);
        assert!(values(&reader, "m", RateMode::default()).is_err());
        assert!(values(&reader, "rate(m[3s])", RateMode::default()).is_err());
        assert_eq!(
            values(&reader, r#"m{source="a"}"#, RateMode::default())
                .unwrap()
                .len(),
            1
        );
    }

    /// A gauge table declaring its sampling interval.
    fn declared_table(name: &str, rows: &[(u64, u64)], interval_ms: u64) -> Vec<u8> {
        use arrow::array::{ArrayRef, Int64Array, UInt64Array};
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        use parquet::file::metadata::KeyValue;
        use parquet::file::properties::WriterProperties;
        let mut meta = HashMap::new();
        meta.insert("metric".to_string(), name.to_string());
        meta.insert("metric_type".to_string(), "gauge".to_string());
        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new(name, DataType::Int64, true).with_metadata(meta),
        ]));
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(vec![KeyValue {
                key: "sampling_interval_ms".to_string(),
                value: Some(interval_ms.to_string()),
            }]))
            .build();
        let ts: ArrayRef = Arc::new(UInt64Array::from(
            rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        ));
        let values: ArrayRef = Arc::new(Int64Array::from(
            rows.iter().map(|r| r.1 as i64).collect::<Vec<_>>(),
        ));
        let mut buf = Vec::new();
        let mut w = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();
        w.write(&RecordBatch::try_new(schema, vec![ts, values]).unwrap())
            .unwrap();
        w.close().unwrap();
        buf
    }

    fn host(
        source: &'static str,
        host: &'static str,
        tables: Vec<(&str, Vec<u8>)>,
    ) -> InMemorySource {
        recording(&[("source", source), ("host", host)], tables)
    }

    fn gauge(name: &str, v: u64) -> Vec<u8> {
        explicit_table(name, "gauge", &gauge_rows(1, 40, 1, v))
    }

    /// A matcher on one operand narrows that operand; the other recording
    /// is kept whole and answers empty, not "metric not found".
    #[test]
    fn a_matcher_on_one_operand_keeps_recordings_whole() {
        let rec = |source: &'static str, g: u64, h: u64| {
            recording(
                &[("source", source)],
                vec![("tg", gauge("g", g)), ("th", gauge("h", h))],
            )
        };
        let reader = archive(vec![rec("a", 10, 2), rec("b", 100, 4)]);
        for q in [
            r#"g{source="a"} / h"#,
            r#"h / g{source="a"}"#,
            r#"sum by (source) (g{source="a"}) / sum by (source) (h)"#,
        ] {
            let got =
                values(&reader, q, RateMode::default()).unwrap_or_else(|e| panic!("{q}: {e}"));
            assert_eq!(got.len(), 1, "{q}: {got:?}");
        }
    }

    /// A recording lacking an operand answers nothing for the query, as a
    /// pair with no match does; the others answer. When no one recording
    /// holds every metric named, the answer is empty.
    #[test]
    fn a_recording_lacking_an_operand_is_left_out() {
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![("tg", gauge("g", 10)), ("th", gauge("h", 2))],
            ),
            recording(&[("source", "b")], vec![("tg", gauge("g", 100))]),
        ]);
        let got = values(&reader, "g / h", RateMode::default()).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].iter().all(|(_, v)| *v == 5.0), "{got:?}");
        assert_eq!(values(&reader, "g", RateMode::default()).unwrap().len(), 2);

        let split = archive(vec![
            recording(&[("source", "a")], vec![("t", gauge("x", 10))]),
            recording(&[("source", "b")], vec![("t", gauge("y", 2))]),
        ]);
        // No one recording holds both: empty, as Prometheus answers it.
        assert!(values(&split, "x / y", RateMode::default())
            .unwrap()
            .is_empty());
        // A matcher keeping only the recording that lacks `x`: empty too.
        assert!(values(&split, r#"x{source="b"} + y"#, RateMode::default())
            .unwrap()
            .is_empty());
    }

    /// Recordings that differ in more than one label: each grouping keeps
    /// what it names, and `sum` combines them.
    #[test]
    fn recordings_that_differ_in_several_labels_group_by_any() {
        let reader = archive(vec![
            host("agent", "web-01", vec![("t", gauge("g", 10))]),
            host("svc", "db-01", vec![("t", gauge("g", 100))]),
        ]);
        for q in [
            "sum by (source) (g)",
            "sum by (host) (g)",
            "sum without (source) (g)",
        ] {
            assert_eq!(
                values(&reader, q, RateMode::default())
                    .unwrap_or_else(|e| panic!("{q}: {e}"))
                    .len(),
                2,
                "{q}"
            );
        }
        let sum = values(&reader, "sum(g)", RateMode::default()).unwrap();
        assert_eq!(sum.len(), 1);
        assert!(sum[0].iter().all(|(_, v)| *v == 110.0), "{sum:?}");
        // One recording without `host`: `by (source)` still tells them apart.
        let partial = archive(vec![
            host("agent", "web-01", vec![("t", gauge("g", 10))]),
            recording(&[("source", "svc")], vec![("t", gauge("g", 100))]),
        ]);
        assert_eq!(
            values(&partial, "sum by (source) (g)", RateMode::default())
                .unwrap()
                .len(),
            2
        );
    }

    /// An operator between aggregates of different recordings pairs them:
    /// both sides have the empty label set.
    #[test]
    fn an_operator_between_aggregates_of_different_recordings_pairs_them() {
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![("t", counter_table("c", &[(&[], 1)], 10))],
            ),
            recording(
                &[("source", "b")],
                vec![("t", counter_table("c", &[(&[], 2)], 10))],
            ),
        ]);
        assert_eq!(
            series(
                &reader,
                r#"sum(rate(c{source="a"}[3s])) + sum(rate(c{source="b"}[3s]))"#
            ),
            [(labels(&[]), 3.0)]
        );
    }

    /// `without` keeps `__recording__`, so twins stay apart under it.
    #[test]
    fn without_keeps_twins_apart() {
        let twin = |v| {
            recording(
                &[("source", "s")],
                vec![("t", counter_table("c", &[(&[("cpu", "0")], v)], 10))],
            )
        };
        let reader = archive(vec![twin(1), twin(2)]);
        assert_eq!(
            series(&reader, "sum without (cpu) (rate(c[3s]))"),
            [
                (labels(&[(RECORDING_LABEL, "0"), ("source", "s")]), 1.0),
                (labels(&[(RECORDING_LABEL, "1"), ("source", "s")]), 2.0),
            ]
        );
    }

    /// A recording without a label can carry it on its series. Two series
    /// with one label set are refused in a result, as Prometheus refuses
    /// them; aggregated, they combine.
    #[test]
    fn a_label_one_recording_lacks_does_not_tell_it_apart() {
        let reader = archive(vec![
            recording(
                &[("source", "s"), ("host", "x")],
                vec![("t", counter_table("c", &[(&[], 1)], 10))],
            ),
            recording(
                &[("source", "s")],
                vec![("t", counter_table("c", &[(&[("host", "x")], 2)], 10))],
            ),
        ]);
        let err = reader
            .query_range_opts("rate(c[3s])", 1.0, 10.0, 1.0, &QueryOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("two series with the labels"), "{err}");
        assert_eq!(
            series(&reader, "sum by (host) (rate(c[3s]))"),
            [(labels(&[("host", "x")]), 3.0)]
        );
    }

    /// An instant query without a time reads every recording at the
    /// archive's end, not each at its own last row.
    #[test]
    fn an_instant_query_reads_every_recording_at_one_time() {
        let reader = archive(vec![
            recording(&[("source", "a")], vec![("t", gauge("g", 10))]),
            recording(
                &[("source", "b")],
                vec![(
                    "t",
                    explicit_table("g", "gauge", &gauge_rows(1, 20, 1, 100)),
                )],
            ),
        ]);
        let QueryResult::Vector { result } = reader.query("g", None).unwrap() else {
            panic!("not a vector");
        };
        let times: Vec<f64> = result.iter().map(|s| s.value.0).collect();
        assert!(times.windows(2).all(|w| w[0] == w[1]), "{times:?}");
    }

    /// The default instant is the archive's end whether or not a matcher
    /// narrows the query to one recording, so a series does not appear or
    /// vanish with the matcher.
    #[test]
    fn the_default_instant_does_not_depend_on_the_matcher() {
        let reader = archive(vec![
            recording(&[("source", "a")], vec![("t", gauge("g", 100))]),
            recording(
                &[("source", "b")],
                vec![("t", explicit_table("g", "gauge", &gauge_rows(1, 10, 1, 7)))],
            ),
        ]);
        let at = |q: &str| -> Vec<(String, f64)> {
            let QueryResult::Vector { result } = reader.query(q, None).unwrap() else {
                panic!("not a vector");
            };
            result
                .iter()
                .map(|s| (s.metric["source"].clone(), s.value.0))
                .collect()
        };
        // At the archive's end (40 s), recording b (last row at 10 s) is not
        // answered, with or without a matcher narrowing to it.
        assert_eq!(at("g"), [("a".to_string(), 40.0)]);
        assert!(at(r#"g{source="b"}"#).is_empty());
    }

    /// A selector matching one label twice is refused: only one matcher per
    /// label is applied, so `source!="a",source!="b"` let a through.
    #[test]
    fn a_label_matched_twice_in_one_selector_is_refused() {
        let reader = archive(vec![
            recording(&[("source", "a")], vec![("t", gauge("g", 1))]),
            recording(&[("source", "b")], vec![("t", gauge("g", 2))]),
            recording(&[("source", "c")], vec![("t", gauge("g", 3))]),
        ]);
        let err = values(
            &reader,
            r#"g{source!="a",source!="b"}"#,
            RateMode::default(),
        )
        .unwrap_err();
        assert!(err.contains(r#"source!~"a|b""#), "{err}");
        assert_eq!(
            values(&reader, r#"g{source!~"a|b"}"#, RateMode::default())
                .unwrap()
                .len(),
            1
        );
    }
}
