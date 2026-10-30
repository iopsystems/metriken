//! Queries over an archive ([`ArchiveReader`], `metriken-storage`'s): which
//! table answers a query, the evaluation timestamps for a query spanning
//! samplers of different cadence, and the [`MetricsSource`] implementation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use metriken_storage::labeled::{
    translate_filter_with, LabeledSource, Precedence, RECORDING_LABEL,
};
use metriken_storage::reader::{SamplerReader, SeriesLabels, TableReader};
use metriken_storage::recordings::RecordingsSource;
use metriken_storage::union::UnionSource;
use metriken_storage::{table_sampler, ArchiveReader, DataSource, UnionChild, UnionError};

use crate::promql::streaming::conflicts::{Recorded, SourceName};
use crate::promql::streaming::per_source::{GroupSource, Member, Part, SourceGroups, Tap};
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
/// `None`: it is evaluated on the uniform grid, each series read at its own
/// recording's rows and interval.
///
/// It plans as a query without [`QueryOptions::series_labels`]: a matcher on
/// a recording label selects the recording.
pub fn eval_timestamps_for(
    reader: &ArchiveReader,
    query: &str,
    step_s: f64,
    rate_mode: RateMode,
) -> Option<Arc<[u64]>> {
    Plan::new(reader, query, None, Precedence::Recording)
        .ok()?
        .cross_cadence(step_s, rate_mode)
}

/// What every query entry point works out once: the tables that can answer
/// it and the labels each recording's series carry.
struct Plan<'a> {
    reader: &'a ArchiveReader,
    owners: Vec<&'a SamplerReader>,
    labels: &'a SeriesLabels,
    /// The metric names the query references that some table of the
    /// archive holds (before pruning).
    held: Vec<String>,
    /// Whether one of `held` is in none of `owners`: a matcher or the
    /// scope left out every recording holding it.
    missing: bool,
    /// The query's scope, which `owners` already keeps to its sources.
    scope: Option<&'a crate::Scope>,
    /// How a recording's labels combine with its series' own
    /// ([`QueryOptions::series_labels`]).
    precedence: Precedence,
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
    ///
    /// With a scope, only the tables of the sources it names are owners, and
    /// a metric only other sources hold is missing. Under
    /// [`Precedence::Series`] a matcher on a recording label leaves out no
    /// recording, since a series' own value of the key is matched instead;
    /// a label-selected query then reads every recording, without the
    /// one-recording evaluation above.
    fn new(
        reader: &'a ArchiveReader,
        query: &str,
        scope: Option<&'a crate::Scope>,
        precedence: Precedence,
    ) -> Result<Self, QueryError> {
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
        let unscoped = reader.owners_of(&referenced);
        let all: Vec<&SamplerReader> = match scope {
            None => unscoped.clone(),
            Some(scope) => {
                let mut kept = Vec::new();
                for t in &unscoped {
                    if scope_span(reader, scope, t)?.is_some() {
                        kept.push(*t);
                    }
                }
                kept
            }
        };
        let selectors = crate::promql::selectors(query)?;
        let labels = reader.series_labels();
        let mut passing: Vec<&SamplerReader> = Vec::new();
        for t in &all {
            let extra = labels.of(t);
            let pass = selectors.iter().any(|(names, filter)| {
                names.iter().any(|n| t.holds(n))
                    && translate_filter_with(extra, filter, precedence).is_some()
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
            .filter(|n| unscoped.iter().any(|t| t.holds(n)))
            .collect();
        let missing = held.iter().any(|n| !owners.iter().any(|t| t.holds(n)));
        Ok(Self {
            reader,
            owners,
            labels,
            held,
            missing,
            scope,
            precedence,
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

    /// `opts` with the query's cross-cadence timestamps, when it has any,
    /// and without its scope, which the plan has applied.
    fn opts(&self, opts: &QueryOptions, step_s: f64) -> QueryOptions {
        // Clone and set fields: `QueryOptions` is `#[non_exhaustive]`, so it
        // cannot be built by literal from here — and cloning preserves
        // whatever else the caller set.
        let opts = opts.clone().with_scope(None).with_series_labels(false);
        match self.cross_cadence(step_s, opts.rate_mode) {
            Some(points) => opts.with_eval_timestamps(Some(points)),
            None => opts,
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

/// The first row of any source of read group `group` other than the one at
/// `position`, the time from which that source's rows can conflict with
/// another's (S4). A source without a known span counts from 0. The end of
/// the other sources' spans does not bound the rows kept: a live archive
/// can serve rows past the span the reader opened with.
fn others_from(reader: &ArchiveReader, group: Option<usize>, position: Option<usize>) -> u64 {
    let Some(g) = group.and_then(|g| reader.read_groups().get(g)) else {
        return 0;
    };
    g.sources
        .iter()
        .filter(|&&p| Some(p) != position)
        .map(|&p| reader.sources()[p].span_ns.map_or(0, |(first, _)| first))
        .min()
        .unwrap_or(0)
}

/// Under [`Precedence::Series`], the `__recording__` value every series of
/// `table`'s read group carries when the query can read several read groups
/// (its scope's, else the archive's); `None` otherwise. One numbering over
/// the reader's read groups: the catalog index of each group's first
/// source, or its position among the reader's sources when two groups'
/// first sources share a catalog index (readers joined by
/// `ArchiveReader::flatten`).
fn mark(plan: &Plan<'_>, table: &SamplerReader) -> Option<String> {
    if plan.precedence != Precedence::Series {
        return None;
    }
    let reader = plan.reader;
    let groups = reader.read_groups();
    let readable = match plan.scope {
        None => groups.len(),
        Some(scope) => groups
            .iter()
            .filter(|g| {
                g.sources.iter().any(|&p| {
                    let uuid = reader.sources()[p].uuid.as_deref();
                    scope.span_of(p, uuid).ok().flatten().is_some()
                })
            })
            .count(),
    };
    if readable < 2 {
        return None;
    }
    let group = reader.read_group_of(table)?;
    let firsts: Vec<usize> = groups.iter().map(|g| g.sources[0]).collect();
    let catalog: Vec<usize> = firsts
        .iter()
        .map(|&p| reader.sources()[p].catalog_index)
        .collect();
    let mut distinct = catalog.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let value = match distinct.len() == catalog.len() {
        true => catalog[group],
        false => firsts[group],
    };
    Some(value.to_string())
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
/// The sources of one process group (`ArchiveReader::read_groups`: copies,
/// a recording and a dump of it, two recorders of one agent) are one
/// recording to a query, and their series carry one label set. A leaf reads
/// each of them alone, and each producer epoch of a source alone, and
/// combines the results per series (`streaming::per_source`, S9), so a
/// process counts once and no value is a difference between rows of two
/// sources or across a restart. A source a query's scope reads over a span
/// is read the same way, as a group of its own whose members' rows are
/// clipped to the span: its points end one interval after its last row in
/// the span, and a gap of more than twice its median row spacing has no
/// points (S5). A query reaching no such group and no span is evaluated as
/// before, as is an unspanned source of one epoch.
///
/// Refused: a metric name held by two tables of one recording
/// (`gpu_nvidia` and `gpu_amd_smi` both publish `gpu_utilization`), and a
/// name that is a different kind in two recordings the query reaches.
/// Errors too when the query references no known metric.
fn engine(plan: &Plan<'_>, query: &str) -> Result<Option<QueryEngine>, QueryError> {
    // A metric the archive holds that no recording the query reaches holds
    // (`x{source="b"} + y` with `x` only in a, or a scope leaving out a) has
    // no series, and neither has anything combined with it, as in
    // Prometheus. Evaluated, it would be "metric not found".
    if plan.missing {
        return Ok(None);
    }
    if plan.owners.is_empty() {
        return Err(QueryError::ParseError(format!(
            "query references no metric present in this .rez: {query}"
        )));
    }
    let recordings = plan.recordings();
    if recordings.len() > 1 {
        refuse_kinds_across_recordings(plan, &recordings, query)?;
    }
    // The recordings by read group, in first-recording order.
    let mut units: Vec<(Option<usize>, Vec<&Vec<&SamplerReader>>)> = Vec::new();
    for tables in &recordings {
        let group = plan.reader.read_group_of(tables[0]);
        match units
            .iter_mut()
            .find(|(g, _)| group.is_some() && *g == group)
        {
            Some((_, unit)) => unit.push(tables),
            None => units.push((group, vec![tables])),
        }
    }
    // The recordings in catalog order: a source alone, or a group's members
    // and its sources ranked.
    let mut parts: Vec<Result<Arc<dyn DataSource>, Grouped>> = Vec::new();
    // The rows the query reads of its groups' sources, and the conflicts
    // found in them (S4).
    let conflicts: Arc<crate::promql::streaming::conflicts::Conflicts> = Default::default();
    for (group, unit) in units {
        // A source whose process group has other sources is read by the
        // group's rules even where it alone holds the metric, so a series
        // reads the same whichever of the group's sources hold it.
        let grouped = group
            .and_then(|g| plan.reader.read_groups().get(g))
            .is_some_and(|g| g.sources.len() > 1);
        let mut members: Vec<Member> = Vec::new();
        let mut whole: Vec<Ranked> = Vec::new();
        // Whether a source of the unit is read over a scope's span: it is
        // then read as a group. Each member reads through a `Tap` clipped to
        // its `[lo, hi]`, and the combine keeps a point only where the
        // member's rows hold it (S3: to one interval past its last row, and
        // not in a hole).
        let mut spanned = false;
        for tables in unit {
            let Some(source) = recording_source(tables, query)? else {
                continue;
            };
            let mut labels = plan.labels.of(tables[0]).clone();
            let labeled = match mark(plan, tables[0]) {
                Some(mark) => LabeledSource::new(source, labels).marking(RECORDING_LABEL, &mark),
                None => {
                    // With one read group readable under `Series`, no
                    // series carries `__recording__`: remove a twin's.
                    if plan.precedence == Precedence::Series {
                        labels.inner.remove(RECORDING_LABEL);
                    }
                    LabeledSource::new(source, labels).with_precedence(plan.precedence)
                }
            };
            let source: Arc<dyn DataSource> = Arc::new(labeled);
            // A scope's span: its members' spans within it, which the
            // per-source combine clips each member's reads to.
            let span = match plan.scope {
                Some(scope) => scope_span(plan.reader, scope, tables[0])?.flatten(),
                None => None,
            };
            spanned |= span.is_some();
            let (span_lo, span_hi) = span.unwrap_or((0, u64::MAX));
            let position = plan.reader.source_position(tables[0]);
            let info = position.map(|p| &plan.reader.sources()[p]);
            // A source of a process group of several keeps the rows the
            // query reads of it, which are compared for conflicts (S4):
            // within the scope's span, from the first row of any other
            // source of the group.
            let raw = Arc::clone(&source);
            let source: Arc<dyn DataSource> = match grouped {
                true => Arc::new(
                    Recorded::new(
                        source,
                        SourceName {
                            position: position.unwrap_or(0),
                            name: info
                                .and_then(|i| i.uuid.clone())
                                .unwrap_or_else(|| format!("source {}", position.unwrap_or(0))),
                        },
                        Arc::clone(&conflicts),
                    )
                    .keeping(
                        others_from(plan.reader, group, position).max(span_lo),
                        span_hi,
                    ),
                ),
                false => source,
            };
            let member = |lo: u64, hi: u64| Member {
                source: Arc::clone(&source),
                raw: Arc::clone(&raw),
                lo: lo.max(span_lo),
                hi: hi.min(span_hi),
                uuid: info.and_then(|i| i.uuid.clone()),
                catalog_index: info.map_or(0, |i| i.catalog_index),
                position: position.unwrap_or(0),
                first_ns: info
                    .and_then(|i| i.span_ns)
                    .map_or(u64::MAX, |(first, _)| first),
            };
            // One member per producer epoch: from its first row (the
            // epoch's `from_ts`) to the next epoch's, so no pair of rows
            // spans a restart.
            let epochs = info.map_or(&[][..], |i| i.epochs.as_slice());
            for k in 0..epochs.len().max(1) {
                let lo = if k == 0 { 0 } else { epochs[k].first_ns };
                let hi = match epochs.get(k + 1) {
                    Some(next) => next.first_ns.saturating_sub(1),
                    None => u64::MAX,
                };
                // An epoch outside the scope's span holds no row read.
                if lo.max(span_lo) <= hi.min(span_hi) {
                    members.push(member(lo, hi));
                }
            }
            let finest = tables
                .iter()
                .map(|t| t.interval())
                .filter(|i| *i > 0.0)
                .fold(f64::INFINITY, f64::min);
            // The source the group's histogram reads go to, clipped to the
            // span.
            let clipped: Arc<dyn DataSource> = match span {
                Some((lo, hi)) => Arc::new(Tap::new(Arc::clone(&source), lo, hi)),
                None => Arc::clone(&source),
            };
            whole.push((finest, clipped, member(0, u64::MAX)));
        }
        match members.len() {
            0 => {}
            1 if !grouped && !spanned => parts.push(Ok(members.pop().expect("one").source)),
            _ => parts.push(Err((members, whole))),
        }
    }
    if parts.iter().all(|p| p.is_ok()) {
        let mut singles: Vec<Arc<dyn DataSource>> = parts
            .into_iter()
            .map(|p| p.ok().expect("all single"))
            .collect();
        let source: Arc<dyn DataSource> = match singles.len() {
            // A table whose segments have gone since the probe is absent, so
            // its metrics are too: the same error a query naming a metric
            // this archive never held gets.
            0 => {
                return Err(QueryError::ParseError(format!(
                    "query references {query}, whose tables have all been evicted since this \
                     archive was opened"
                )))
            }
            1 => singles.pop().expect("one"),
            _ => Arc::new(RecordingsSource::new(singles)),
        };
        return Ok(Some(QueryEngine::new(source)));
    }
    let mut children: Vec<Arc<dyn DataSource>> = Vec::with_capacity(parts.len());
    let mut ordered: Vec<Part> = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            Ok(source) => {
                children.push(Arc::clone(&source));
                ordered.push(Part::Single(source));
            }
            Err((members, mut whole)) => {
                // A group as the whole sees it, its sources finest first
                // (S3's tie rule after the interval).
                whole
                    .sort_by(|(ia, _, a), (ib, _, b)| ia.total_cmp(ib).then_with(|| a.cmp_ties(b)));
                children.push(Arc::new(GroupSource::new(
                    whole.into_iter().map(|(_, s, _)| s).collect(),
                    members.clone(),
                    Arc::clone(&conflicts),
                )));
                ordered.push(Part::Group(members));
            }
        }
    }
    let source: Arc<dyn DataSource> = match children.len() {
        1 => children.pop().expect("one"),
        _ => Arc::new(RecordingsSource::new(children)),
    };
    Ok(Some(QueryEngine::with_groups(
        source,
        SourceGroups {
            parts: ordered,
            conflicts,
        },
    )))
}

/// The span `scope` reads of `table`'s source: `None` when the scope does
/// not name the source, `Some(None)` for every row. An error when the scope
/// names the source twice.
fn scope_span(
    reader: &ArchiveReader,
    scope: &crate::Scope,
    table: &SamplerReader,
) -> Result<Option<Option<(u64, u64)>>, QueryError> {
    let Some(position) = reader.source_position(table) else {
        return Ok(None);
    };
    let uuid = reader.sources()[position].uuid.as_deref();
    scope
        .span_of(position, uuid)
        .map_err(QueryError::Unsupported)
}

/// A source of a process group with what ranks it: its finest table
/// interval and its tie-breaking facts.
type Ranked = (f64, Arc<dyn DataSource>, Member);

/// A process group's members, and its sources ranked.
type Grouped = (Vec<Member>, Vec<Ranked>);

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
                "the result holds two series with the labels {metric:?}; aggregate them \
                 (e.g. sum by (...)) or select one with a label matcher"
            )));
        }
    }
    Ok(())
}

fn checked(result: QueryResult) -> Result<QueryResult, QueryError> {
    match &result {
        QueryResult::Matrix { result, .. } => {
            refuse_shared_label_sets(result.iter().map(|m| &m.metric))?
        }
        QueryResult::Vector { result, .. } => {
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
        DisplayResult::Vector { result, .. } => {
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
/// recordings holding that kind and leave the others out silently.
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
            } else if t.holds_gauge_histogram(name) {
                Some("gauge histogram")
            } else {
                None
            }
        })
    };
    let which = recording_selectors(recordings, plan.labels);
    let selectors = crate::promql::selectors(query)?;
    for name in &plan.held {
        // The recordings a selector naming `name` can read: `m{source="a"} +
        // y` reads `m` in recording a only, whatever `y` reaches.
        let reads = |tables: &[&SamplerReader]| {
            selectors.iter().any(|(names, filter)| {
                names.contains(name)
                    && translate_filter_with(plan.labels.of(tables[0]), filter, plan.precedence)
                        .is_some()
            })
        };
        let kinds: Vec<(usize, &'static str)> = recordings
            .iter()
            .enumerate()
            .filter(|(_, tables)| reads(tables))
            .filter_map(|(i, tables)| kind(tables, name).map(|k| (i, k)))
            .collect();
        let Some(&(first, first_kind)) = kinds.first() else {
            continue;
        };
        if let Some(&(other, other_kind)) = kinds.iter().find(|(_, k)| *k != first_kind) {
            // Under the series' labels a matcher on a recording label
            // selects series, not recordings; only the scope does.
            let how = match plan.precedence {
                Precedence::Recording => "a label matcher",
                Precedence::Series => "the query's scope",
            };
            return Err(QueryError::Unsupported(format!(
                "query {query} reads {name}, which is a {first_kind} in {} and a {other_kind} \
                 in {}; select the recordings of one kind with {how}",
                which[first], which[other],
            )));
        }
    }
    Ok(())
}

/// `reader`'s source, reporting the interval measured from `table`'s rows
/// and holding its gauges for it, or `fallback` for a table whose spacing
/// was not measured (one row). A segment that declares no interval reports
/// 1 s, which would hold a 10 s table's gauge 1 s past each row.
fn timed(
    table: &SamplerReader,
    reader: &TableReader,
    fallback: Option<f64>,
) -> Arc<dyn DataSource> {
    let source = LabeledSource::new(reader.source(), Default::default());
    match Some(table.interval()).filter(|i| *i > 0.0).or(fallback) {
        Some(i) => Arc::new(source.with_interval(i)),
        None => Arc::new(source),
    }
}

/// One recording's tables that hold a metric of `query`, as one source: the
/// table itself, or a union of them. `None` when every one of them has been
/// evicted since the archive was opened.
fn recording_source(
    tables: &[&SamplerReader],
    query: &str,
) -> Result<Option<Arc<dyn DataSource>>, QueryError> {
    // The finest measured interval among the recording's tables.
    let fallback = tables
        .iter()
        .map(|t| t.interval())
        .filter(|i| *i > 0.0)
        .min_by(f64::total_cmp);
    if let [one] = tables {
        return Ok(one.reader().map(|r| timed(one, r, fallback)));
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
        .filter_map(|t| {
            t.reader()
                .map(|r| UnionChild::from_source(timed(t, r, fallback)))
        })
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

/// The precedence `opts` asks for ([`QueryOptions::series_labels`]).
fn precedence(opts: &QueryOptions) -> Precedence {
    match opts.series_labels {
        true => Precedence::Series,
        false => Precedence::Recording,
    }
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
        let plan = Plan::new(self, expr, opts.scope.as_deref(), precedence(opts))?;
        let Some(engine) = engine(&plan, expr)? else {
            return Ok(QueryResult::Matrix {
                result: Vec::new(),
                notices: Vec::new(),
            });
        };
        let mut result = checked(engine.query_range_opts(
            expr,
            start_s,
            end_s,
            step_s,
            &plan.opts(opts, step_s),
        )?)?;
        result.notices_mut().extend(engine.take_notices());
        Ok(result)
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
        let plan = Plan::new(self, expr, qopts.scope.as_deref(), precedence(qopts))?;
        let Some(engine) = engine(&plan, expr)? else {
            return Ok(crate::DisplayResult::Series {
                result: Vec::new(),
                budget: opts.budget as u32,
                notices: Vec::new(),
            });
        };
        let mut result = checked_display(engine.query_range_display_opts(
            expr,
            start_s,
            end_s,
            step_s,
            opts,
            &plan.opts(qopts, step_s),
        )?)?;
        result.notices_mut().extend(engine.take_notices());
        Ok(result)
    }
    fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        let plan = Plan::new(self, expr, None, Precedence::Recording)?;
        // On an archive of several recordings, at the archive's end whichever
        // recordings the query reaches: the engine's own default is the end
        // of what it reads, which would move with a label matcher.
        let time = time.or_else(|| {
            (self.recording_count() > 1)
                .then(|| ArchiveReader::time_range_ns(self).map(|(_, end)| end as f64 / 1e9))
                .flatten()
        });
        let Some(engine) = engine(&plan, expr)? else {
            return Ok(QueryResult::Vector {
                result: Vec::new(),
                notices: Vec::new(),
            });
        };
        let mut result = checked(engine.query(expr, time)?)?;
        result.notices_mut().extend(engine.take_notices());
        Ok(result)
    }
    fn columns(&self, query: &str) -> Result<HashSet<String>, QueryError> {
        // Through the same labelled sources as a query, so a selector
        // naming a recording label resolves to that recording's columns.
        let plan = Plan::new(self, query, None, Precedence::Recording)?;
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
    fn gauge_histogram_names(&self) -> Vec<String> {
        ArchiveReader::gauge_histogram_names(self)
    }
    fn gauge_histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        ArchiveReader::gauge_histogram_labels(self, name)
    }
    fn counter_labels_matching(
        &self,
        name: &str,
        filter: &metriken_storage::Labels,
    ) -> Vec<BTreeMap<String, String>> {
        ArchiveReader::counter_labels_matching(self, name, filter)
    }
    fn gauge_labels_matching(
        &self,
        name: &str,
        filter: &metriken_storage::Labels,
    ) -> Vec<BTreeMap<String, String>> {
        ArchiveReader::gauge_labels_matching(self, name, filter)
    }
    fn histogram_labels_matching(
        &self,
        name: &str,
        filter: &metriken_storage::Labels,
    ) -> Vec<BTreeMap<String, String>> {
        ArchiveReader::histogram_labels_matching(self, name, filter)
    }
    fn gauge_histogram_labels_matching(
        &self,
        name: &str,
        filter: &metriken_storage::Labels,
    ) -> Vec<BTreeMap<String, String>> {
        ArchiveReader::gauge_histogram_labels_matching(self, name, filter)
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
    /// The instants at which the first source with rows, or a copy of it,
    /// holds a row in any of its tables, ascending, each instant once.
    /// Sources are tried in order: one that recorded nothing owns no table,
    /// and one whose rows retention evicted after the reader opened reads
    /// none. Sources are tried in the order [`ArchiveReader::sources`] lists
    /// them.
    ///
    /// An archive has a table per sampler or acquisition group, and the list
    /// is the union of their row timestamps, so an instant at which only a
    /// slower table was read is listed. A copy of the source (a source with
    /// its uuid, in its read group) can hold fewer or more of the
    /// recording's rows than the source; the list is their union. Another
    /// source is not listed, even one in the same read group: a second
    /// recorder of one agent samples on its own schedule. A reader of
    /// several read groups lists the group of its first source with rows;
    /// open each read group as its own reader (`ArchiveReader::open_read_groups`) for its list. A
    /// second recorder within one read group has no list of its own. Rows of
    /// a live archive's WAL tail are included, as a query reads them.
    ///
    /// Built from each table's [`SamplerReader::row_timestamps`]: the first
    /// call reads them, and the reader keeps them.
    fn sample_timestamps(&self) -> Vec<u64> {
        let names = MetricsSource::all_names(self);
        let tables: Vec<(usize, &SamplerReader)> = self
            .owners_of(&names)
            .into_iter()
            .filter_map(|t| Some((self.source_position(t)?, t)))
            .collect();
        let sources = self.sources();
        let mut leads: Vec<usize> = tables.iter().map(|(p, _)| *p).collect();
        leads.sort_unstable();
        leads.dedup();
        for lead in leads {
            let uuid = &sources[lead].uuid;
            let group = tables
                .iter()
                .find(|(p, _)| *p == lead)
                .and_then(|(_, t)| self.read_group_of(t));
            let mut out: Vec<u64> = Vec::new();
            for (p, t) in &tables {
                let copy =
                    uuid.is_some() && sources[*p].uuid == *uuid && self.read_group_of(t) == group;
                if *p != lead && !copy {
                    continue;
                }
                let mut rows = t.row_timestamps().to_vec();
                rows.sort_unstable();
                rows.dedup();
                out = merge_sorted_unique(out, rows);
            }
            if !out.is_empty() {
                return out;
            }
        }
        Vec::new()
    }
}

/// The ascending union of two ascending, duplicate-free lists, each value
/// once.
fn merge_sorted_unique(a: Vec<u64>, b: Vec<u64>) -> Vec<u64> {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let mut out = Vec::with_capacity(a.len().max(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
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
        typed_table(name, "counter", series, rows)
    }

    /// [`counter_table`] with columns of `metric_type`.
    fn typed_table(
        name: &str,
        metric_type: &str,
        series: &[(&[(&str, &str)], u64)],
        rows: u64,
    ) -> Vec<u8> {
        use arrow::array::{ArrayRef, Int64Array, UInt64Array};
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
            meta.insert("metric_type".to_string(), metric_type.to_string());
            for (k, v) in *labels {
                meta.insert(k.to_string(), v.to_string());
            }
            // A gauge column is signed.
            let gauge = metric_type == "gauge";
            let data_type = match gauge {
                true => DataType::Int64,
                false => DataType::UInt64,
            };
            fields.push(Field::new(format!("{name}__{i}"), data_type, true).with_metadata(meta));
            let values = (1..=rows).map(|r| r * per_second);
            columns.push(match gauge {
                true => Arc::new(Int64Array::from(
                    values.map(|v| v as i64).collect::<Vec<_>>(),
                )),
                false => Arc::new(UInt64Array::from(values.collect::<Vec<_>>())),
            });
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
        let QueryResult::Matrix { result, .. } = result else {
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

    /// A filtered listing lists the series a query's selector with the same
    /// matchers selects, and reads only the recordings whose labels can
    /// match it.
    #[test]
    fn a_filtered_listing_lists_what_a_selector_selects() {
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![(
                    "t",
                    counter_table("x", &[(&[("cpu", "0")], 1), (&[("cpu", "1")], 2)], 5),
                )],
            ),
            recording(
                &[("source", "b")],
                vec![(
                    "t",
                    counter_table(
                        "x",
                        &[
                            (&[("cpu", "1")], 3),
                            (&[("cpu", "2"), ("source", "inner")], 4),
                        ],
                        5,
                    ),
                )],
            ),
        ]);
        for selector in [
            r#"{source="a"}"#,
            r#"{source!="a"}"#,
            r#"{cpu="1"}"#,
            r#"{cpu=~"1|2"}"#,
            r#"{exported_source="inner"}"#,
            r#"{source="c"}"#,
        ] {
            let filter = crate::selector_filter(selector).unwrap();
            let mut listed = MetricsSource::counter_labels_matching(&reader, "x", &filter);
            listed.sort();
            let selected: Vec<_> = series(&reader, &format!("rate(x{selector}[2s])"))
                .into_iter()
                .map(|(labels, _)| labels)
                .collect();
            assert_eq!(listed, selected, "{selector}");
        }
        let values = MetricsSource::label_values_matching(
            &reader,
            "x",
            "cpu",
            &crate::selector_filter(r#"{source="b"}"#).unwrap(),
        );
        assert_eq!(
            values,
            ["1", "2"]
                .into_iter()
                .map(String::from)
                .collect::<HashSet<_>>()
        );

        // A filter on a recording label reads only that recording's table.
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![("t", counter_table("x", &[(&[("cpu", "0")], 1)], 5))],
            ),
            recording(
                &[("source", "b")],
                vec![("t", counter_table("x", &[(&[("cpu", "0")], 1)], 5))],
            ),
        ]);
        let listed = MetricsSource::counter_labels_matching(
            &reader,
            "x",
            &crate::selector_filter(r#"{source="b"}"#).unwrap(),
        );
        assert_eq!(listed, vec![labels(&[("cpu", "0"), ("source", "b")])]);
        assert_eq!(reader.opened_tables().len(), 1);
        let _ = reader.counter_labels("x");
        assert_eq!(reader.opened_tables().len(), 2);

        // A gauge lists by its own kind, and only matching recordings.
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![
                    ("t", counter_table("x", &[(&[("cpu", "0")], 1)], 5)),
                    ("u", typed_table("g", "gauge", &[(&[("cpu", "1")], 1)], 5)),
                ],
            ),
            recording(
                &[("source", "b")],
                vec![("u", typed_table("g", "gauge", &[(&[("cpu", "2")], 1)], 5))],
            ),
        ]);
        let filter = crate::selector_filter(r#"{source="a"}"#).unwrap();
        assert_eq!(
            MetricsSource::gauge_labels_matching(&reader, "g", &filter),
            vec![labels(&[("cpu", "1"), ("source", "a")])]
        );
        assert_eq!(reader.opened_tables(), vec!["u".to_string()]);
        assert!(MetricsSource::gauge_labels_matching(&reader, "x", &filter).is_empty());
    }

    /// `selector_filter` takes one vector selector's matchers, without its
    /// metric name.
    #[test]
    fn selector_filter_takes_one_selectors_matchers() {
        let filter = crate::selector_filter(r#"x{a="1", b=~"2|3", c!="4"}"#).unwrap();
        assert_eq!(
            filter.inner,
            labels(&[("a", "1"), ("b", "~2|3"), ("c", "!4")])
        );
        assert!(crate::selector_filter(r#"rate(x[1m])"#).is_err());
        assert!(crate::selector_filter(r#"{a="1", a="2"}"#).is_err());
        assert_eq!(
            crate::selector_filter(r#"{a!="1"}"#).unwrap().inner,
            labels(&[("a", "!1")])
        );
        assert_eq!(crate::selector_filter("{}").unwrap().inner, labels(&[]));
        assert_eq!(
            crate::selector_filter(r#"{__name__="x", a="1"}"#)
                .unwrap()
                .inner,
            labels(&[("a", "1")])
        );
    }

    /// A scope keeps a query to the sources it names, each within its span;
    /// a source other than an archive refuses a scope.
    #[test]
    fn a_scope_keeps_a_query_to_its_sources() {
        use crate::{Scope, ScopedSource, SourceRef};
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![("t", counter_table("x", &[(&[("cpu", "0")], 1)], 10))],
            ),
            recording(
                &[("source", "b")],
                vec![("t", counter_table("x", &[(&[("cpu", "0")], 2)], 10))],
            ),
        ]);
        let run = |entries: Vec<ScopedSource>| {
            let opts =
                QueryOptions::default().with_scope(Some(Arc::new(Scope::new(entries).unwrap())));
            let QueryResult::Matrix { result, .. } = reader
                .query_range_opts("rate(x[2s])", 1.0, 10.0, 1.0, &opts)
                .unwrap()
            else {
                panic!("not a matrix");
            };
            result
        };
        let only_b = run(vec![ScopedSource {
            source: SourceRef::Position(1),
            span: None,
        }]);
        assert_eq!(only_b.len(), 1);
        assert_eq!(
            only_b[0].metric.get("source").map(String::as_str),
            Some("b")
        );
        let clipped = run(vec![ScopedSource {
            source: SourceRef::Position(0),
            span: Some((3 * S, 6 * S)),
        }]);
        assert_eq!(clipped.len(), 1);
        assert!(!clipped[0].values.is_empty());
        assert!(
            clipped[0]
                .values
                .iter()
                .all(|(t, _)| (3.0..=6.0).contains(t)),
            "{:?}",
            clipped[0].values
        );

        // A windowed gauge function stops at the span's end too: its window
        // is clipped to the source's span, as within a process group.
        let reader_g = archive(vec![recording(
            &[("source", "a")],
            vec![("u", typed_table("g", "gauge", &[(&[("cpu", "0")], 1)], 10))],
        )]);
        let opts = QueryOptions::default().with_scope(Some(Arc::new(
            Scope::new(vec![ScopedSource::new(
                SourceRef::Position(0),
                Some((3 * S, 6 * S)),
            )])
            .unwrap(),
        )));
        let QueryResult::Matrix { result, .. } = reader_g
            .query_range_opts("avg_over_time(g[3s])", 1.0, 10.0, 1.0, &opts)
            .unwrap()
        else {
            panic!("not a matrix");
        };
        assert!(!result.is_empty() && !result[0].values.is_empty());
        assert!(
            result[0]
                .values
                .iter()
                .all(|(t, _)| (3.0..=7.0).contains(t)),
            "{:?}",
            result[0].values
        );

        // A span reads a source by the process-group rules even alone: a
        // gap of more than twice its median row spacing has no points (S5),
        // where the unscoped read interpolates across it.
        let rows: Vec<(u64, u64)> = (1..=20u64)
            .filter(|t| !(9..=10).contains(t))
            .map(|t| (t * S, t))
            .collect();
        let gappy = archive(vec![recording(
            &[("source", "a")],
            vec![("u", explicit_table("g", "gauge", &rows))],
        )]);
        let points = |scope: Option<Scope>| -> Vec<f64> {
            let opts = QueryOptions::default().with_scope(scope.map(Arc::new));
            let QueryResult::Matrix { result, .. } = gappy
                .query_range_opts("avg_over_time(g[3s])", 1.0, 20.0, 1.0, &opts)
                .unwrap()
            else {
                panic!("not a matrix");
            };
            result[0].values.iter().map(|(t, _)| *t).collect()
        };
        let unscoped = points(None);
        let spanned = points(Some(
            Scope::new(vec![ScopedSource::new(
                SourceRef::Position(0),
                Some((0, 100 * S)),
            )])
            .unwrap(),
        ));
        assert!(unscoped.contains(&10.0), "{unscoped:?}");
        assert!(!spanned.contains(&10.0), "{spanned:?}");
        assert!(
            spanned.contains(&5.0) && spanned.contains(&15.0),
            "{spanned:?}"
        );

        let store = crate::MemoryStore::builder().build();
        let opts = QueryOptions::default().with_scope(Some(Arc::new(Scope::default())));
        assert!(matches!(
            store.query_range_opts("x", 1.0, 2.0, 1.0, &opts),
            Err(QueryError::Unsupported(_))
        ));
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

    /// Under `series_labels` a series keeps its own value of a key its
    /// recording also carries, and a matcher on the key matches it there;
    /// a series without the key carries and matches the recording's value.
    /// The scope, not a matcher, chooses the recording.
    #[test]
    fn series_labels_match_a_series_own_value() {
        let table = |n: u64| {
            counter_table(
                "requests",
                &[(&[("instance", "api:9090")], n), (&[], 4 * n)],
                10,
            )
        };
        let gauge = |n: u64| typed_table("level", "gauge", &[(&[("instance", "api:9090")], n)], 10);
        let free = |n: u64| typed_table("free", "gauge", &[(&[], n)], 10);
        let reader = archive(vec![
            recording(
                &[("source", "svc"), ("instance", "rec-a")],
                vec![
                    ("prometheus/scrape", table(1)),
                    ("prometheus/levels", gauge(1)),
                    ("prometheus/free", free(1)),
                ],
            ),
            recording(
                &[("source", "svc"), ("instance", "rec-b")],
                vec![
                    ("prometheus/scrape", table(2)),
                    ("prometheus/levels", gauge(2)),
                    ("prometheus/free", free(2)),
                ],
            ),
        ]);
        let run = |query: &str, opts: &QueryOptions| -> Vec<(BTreeMap<String, String>, f64)> {
            let result = reader
                .query_range_opts(query, 1.0, 10.0, 1.0, opts)
                .unwrap_or_else(|e| panic!("{query}: {e}"));
            let QueryResult::Matrix { result, .. } = result else {
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
            out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
            out
        };
        let scoped = |position: usize| {
            QueryOptions::default()
                .with_series_labels(true)
                .with_scope(Some(Arc::new(
                    crate::Scope::new(vec![crate::ScopedSource::new(
                        crate::SourceRef::Position(position),
                        None,
                    )])
                    .unwrap(),
                )))
        };
        let own = labels(&[("instance", "api:9090"), ("source", "svc")]);
        assert_eq!(
            run(r#"rate(requests{instance="api:9090"}[3s])"#, &scoped(0)),
            [(own.clone(), 1.0)]
        );
        assert_eq!(
            run(r#"rate(requests{instance="api:9090"}[3s])"#, &scoped(1)),
            [(own.clone(), 2.0)]
        );
        assert_eq!(
            run("rate(requests[3s])", &scoped(0)),
            [
                (own.clone(), 1.0),
                (labels(&[("instance", "rec-a"), ("source", "svc")]), 4.0),
            ]
        );
        assert_eq!(
            run(r#"rate(requests{instance="rec-a"}[3s])"#, &scoped(0)),
            [(labels(&[("instance", "rec-a"), ("source", "svc")]), 4.0)]
        );
        // Without a scope, both recordings' series match.
        let unscoped = QueryOptions::default().with_series_labels(true);
        assert_eq!(
            run(r#"sum(rate(requests{instance="api:9090"}[3s]))"#, &unscoped),
            [(labels(&[]), 3.0)]
        );
        // Their series that keep their own `instance` would have one label
        // set; over two read groups every series carries its group's
        // `__recording__`, which a matcher selects on.
        let marked = |r: &str| {
            labels(&[
                (RECORDING_LABEL, r),
                ("instance", "api:9090"),
                ("source", "svc"),
            ])
        };
        assert_eq!(
            run(r#"rate(requests{instance="api:9090"}[3s])"#, &unscoped),
            [(marked("0"), 1.0), (marked("1"), 2.0)]
        );
        assert_eq!(
            run(r#"rate(requests{__recording__="1"}[3s])"#, &unscoped),
            [
                (marked("1"), 2.0),
                (
                    labels(&[
                        (RECORDING_LABEL, "1"),
                        ("instance", "rec-b"),
                        ("source", "svc")
                    ]),
                    8.0
                ),
            ]
        );
        assert_eq!(
            run(r#"rate(requests{instance="rec-b"}[3s])"#, &unscoped),
            [(
                labels(&[
                    (RECORDING_LABEL, "1"),
                    ("instance", "rec-b"),
                    ("source", "svc")
                ]),
                8.0
            )]
        );
        // An operator pairs a series that kept its own `instance` with its
        // recording's series: scoped, and over both recordings by the mark.
        let paired = |r: Vec<(BTreeMap<String, String>, f64)>| -> Vec<BTreeMap<String, String>> {
            r.into_iter().map(|(l, _)| l).collect()
        };
        assert_eq!(
            paired(run(
                r#"rate(requests{instance="api:9090"}[3s]) / level"#,
                &scoped(0)
            )),
            std::slice::from_ref(&own)
        );
        assert_eq!(
            run("sum(rate(requests[3s])) / sum(level)", &scoped(1)).len(),
            1
        );
        assert_eq!(
            paired(run(
                r#"rate(requests{instance="api:9090"}[3s]) / level"#,
                &unscoped
            )),
            [marked("0"), marked("1")]
        );
        // `free` carries no `instance` of its own, so it reads the
        // recording's; the recording's keys are not recording identity
        // under the flag, so the two still pair as one recording's.
        assert_eq!(
            run(
                r#"rate(requests{instance="api:9090"}[3s]) / free"#,
                &scoped(0)
            )
            .len(),
            1
        );
        // The display form reads the same way.
        let display = reader
            .query_range_display_opts(
                r#"rate(requests{instance="api:9090"}[3s])"#,
                1.0,
                10.0,
                1.0,
                &crate::DisplayOptions::default(),
                &scoped(0),
            )
            .unwrap();
        let crate::DisplayResult::Series { result, .. } = display else {
            panic!("not series");
        };
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].metric.get("instance").map(String::as_str),
            Some("api:9090")
        );
        // A gauge reads the same way.
        let gauges = run(r#"level{instance="api:9090"}"#, &scoped(1));
        assert_eq!(gauges.len(), 1, "{gauges:?}");
        assert_eq!(gauges[0].0, own);
        // Without `series_labels` the recording's value is matched, and
        // the series' own moves to `exported_instance`.
        assert!(run(
            r#"rate(requests{instance="api:9090"}[3s])"#,
            &QueryOptions::default()
        )
        .is_empty());
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
        let QueryResult::Matrix { result, .. } = r else {
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
            let QueryResult::Matrix { result, .. } = reader
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
        // On b's rows, 2.5 s to 9.5 s, the first with b's row at 1.5 s
        // before the range; a's row at 10 s is in the interval after b's
        // last row read, which the range cut.
        assert_eq!(p25[0].len(), 8, "{p25:?}");
        let counts = quantile("histogram_count(lat)");
        assert!(counts[0].iter().all(|(_, v)| *v == 20.0), "{counts:?}");
    }

    /// A `sum(...)` around a histogram function answers as the bare
    /// function does on an archive, with and without a grouping clause.
    #[test]
    fn a_sum_around_a_histogram_function_routes() {
        use crate::segmented::tests::segment_histogram;
        let n = histogram::Config::new(4, 10).unwrap().total_buckets();
        let rows = |per_row: u64| -> Vec<(u64, Vec<u64>)> {
            (1..=10u64)
                .map(|k| {
                    let mut b = vec![0u64; n];
                    b[20] = k * per_row;
                    (k * S, b)
                })
                .collect()
        };
        let reader = archive(vec![
            recording(
                &[("source", "a")],
                vec![("lat", segment_histogram("lat", 4, 10, &rows(5)))],
            ),
            recording(
                &[("source", "b")],
                vec![("lat", segment_histogram("lat", 4, 10, &rows(7)))],
            ),
        ]);
        let values = |query: &str| -> Vec<Vec<(f64, f64)>> {
            let QueryResult::Matrix { result, .. } = reader
                .query_range_opts(query, 1.0, 10.0, 1.0, &QueryOptions::default())
                .unwrap_or_else(|e| panic!("{query}: {e}"))
            else {
                panic!("{query}: not a matrix");
            };
            result.into_iter().map(|m| m.values).collect()
        };
        for func in [
            "histogram_irate",
            "histogram_mean",
            "histogram_count",
            "histogram_sum",
        ] {
            let bare = values(&format!("{func}(lat)"));
            assert!(!bare.is_empty() && !bare[0].is_empty(), "{func}: {bare:?}");
            assert_eq!(values(&format!("sum({func}(lat))")), bare, "{func}");
            let only_a = values(&format!(r#"sum({func}(lat{{source="a"}}))"#));
            assert_eq!(only_a, values(&format!(r#"{func}(lat{{source="a"}})"#)));
            if func != "histogram_mean" {
                assert_ne!(only_a, bare, "{func}: the matcher prunes recording b");
            }
            for grouping in ["by (source)", "without (source)"] {
                assert_eq!(
                    values(&format!("sum {grouping} ({func}(lat))")),
                    values(&format!("{func} {grouping} (lat)")),
                    "{func} {grouping}"
                );
            }
            assert_eq!(
                reader
                    .columns(&format!(r#"sum({func}(lat{{source="a"}}))"#))
                    .unwrap(),
                reader
                    .columns(&format!(r#"{func}(lat{{source="a"}})"#))
                    .unwrap(),
                "{func}"
            );
            assert!(
                crate::referenced_metrics(&format!("sum by (x) ({func}(lat))"))
                    .unwrap()
                    .contains("lat"),
                "{func}"
            );
            // A grouping on both is refused rather than answered as either.
            for query in [
                format!("sum({func} by (source) (lat))"),
                format!("sum({func} by\n(source) (lat))"),
                format!("sum({func} without (source) (lat))"),
            ] {
                let err = reader
                    .query_range_opts(&query, 1.0, 10.0, 1.0, &QueryOptions::default())
                    .expect_err(&query);
                assert!(
                    err.to_string().contains("takes no `by`/`without` clause"),
                    "{query:?}: {err}"
                );
            }
        }
    }

    /// A selector named by a `__name__` matcher routes on an archive as the
    /// bare name does.
    #[test]
    fn a_name_matcher_routes() {
        use crate::segmented::tests::segment_histogram;
        let n = histogram::Config::new(4, 10).unwrap().total_buckets();
        let rows: Vec<(u64, Vec<u64>)> = (1..=10u64)
            .map(|k| {
                let mut b = vec![0u64; n];
                b[20] = k * 5;
                (k * S, b)
            })
            .collect();
        let reader = archive(vec![recording(
            &[("source", "a")],
            vec![
                ("svc/ops", counter_table("svc/ops", &[(&[], 7)], 10)),
                ("lat", segment_histogram("lat", 4, 10, &rows)),
            ],
        )]);
        let values = |query: &str| -> Vec<Vec<(f64, f64)>> {
            let QueryResult::Matrix { result, .. } = reader
                .query_range_opts(query, 2.0, 10.0, 1.0, &QueryOptions::default())
                .unwrap_or_else(|e| panic!("{query}: {e}"))
            else {
                panic!("{query}: not a matrix");
            };
            result.into_iter().map(|m| m.values).collect()
        };
        let named = values(r#"sum(irate({__name__="svc/ops"}[5s]))"#);
        assert!(!named.is_empty() && !named[0].is_empty(), "{named:?}");
        assert_eq!(
            values(r#"sum(irate({__name__="svc/ops", source="a"}[5s]))"#),
            named
        );
        // The histogram functions, parsed apart from PromQL, read it too.
        for (by_matcher, by_name) in [
            (r#"histogram_mean({__name__="lat"})"#, "histogram_mean(lat)"),
            (
                r#"sum(histogram_irate({__name__="lat", source="a"}))"#,
                r#"sum(histogram_irate(lat{source="a"}))"#,
            ),
            (
                r#"histogram_quantiles([0.5], {__name__="lat"})"#,
                "histogram_quantiles([0.5], lat)",
            ),
        ] {
            let got = values(by_matcher);
            assert!(!got.is_empty() && !got[0].is_empty(), "{by_matcher}");
            assert_eq!(got, values(by_name), "{by_matcher}");
        }
        for query in [
            r#"histogram_mean({__name__!="lat"})"#,
            r#"histogram_mean({__name__=~"lat"})"#,
            r#"histogram_mean(lat{__name__="lat"})"#,
            r#"histogram_mean({__name__="lat", __name__="lat"})"#,
            r#"histogram_mean({__name__="", __name__="lat"})"#,
            r#"histogram_mean({__name__="lat,x"})"#,
            r#"histogram_mean({__name__=""})"#,
        ] {
            assert!(
                reader
                    .query_range_opts(query, 2.0, 10.0, 1.0, &QueryOptions::default())
                    .is_err(),
                "{query}"
            );
        }
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
        let QueryResult::Matrix { result, .. } = reader
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

    /// A table of counter `name` in segments of `rows` each: series `cpu=i`
    /// grows by `i + 1` a second, rows one second apart from `offset`.
    fn counter_segments(
        name: &str,
        cpus: u64,
        offset: u64,
        segments: u64,
        rows: u64,
    ) -> Vec<Vec<u8>> {
        use arrow::array::{ArrayRef, UInt64Array};
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        (0..segments)
            .map(|seg| {
                let ts: Vec<u64> = (seg * rows..(seg + 1) * rows)
                    .map(|k| (k + 1) * S + offset)
                    .collect();
                let mut fields = vec![Field::new("timestamp", DataType::UInt64, false)];
                let mut columns: Vec<ArrayRef> = vec![Arc::new(UInt64Array::from(ts.clone()))];
                for cpu in 0..cpus {
                    let mut meta = HashMap::new();
                    meta.insert("metric".to_string(), name.to_string());
                    meta.insert("metric_type".to_string(), "counter".to_string());
                    meta.insert("cpu".to_string(), cpu.to_string());
                    fields.push(
                        Field::new(format!("{name}__{cpu}"), DataType::UInt64, true)
                            .with_metadata(meta),
                    );
                    columns.push(Arc::new(UInt64Array::from(
                        ts.iter().map(|t| t / S * (cpu + 1)).collect::<Vec<_>>(),
                    )));
                }
                let schema = Arc::new(Schema::new(fields));
                let mut buf = Vec::new();
                let mut w = ArrowWriter::try_new(&mut buf, schema.clone(), None).unwrap();
                w.write(&RecordBatch::try_new(schema, columns).unwrap())
                    .unwrap();
                w.close().unwrap();
                buf
            })
            .collect()
    }

    /// Rates across recordings are computed in one pass over every
    /// recording's segments, as within one, and agree with the per-series
    /// path; so does a display read.
    #[test]
    fn rates_across_recordings_take_the_batch_path() {
        let rec = |source: &str, offset: u64| InMemorySource {
            name: source.to_string(),
            metadata: BTreeMap::new(),
            labels: labels(&[("source", source)]),
            complete: true,
            tables: vec![("t".to_string(), counter_segments("c", 4, offset, 3, 10))],
        };
        let reader = archive(vec![rec("a", 0), rec("b", S / 2)]);
        let batched = || crate::promql::streaming::dispatch::BATCH_RATES.with(|n| n.get());
        let displayed = || crate::promql::streaming::dispatch::BATCH_DISPLAYS.with(|n| n.get());
        let per_series = QueryOptions::default().with_per_series_rates(true);
        for q in [
            "rate(c[3s])",
            "sum(rate(c[3s]))",
            "sum by (cpu) (rate(c[3s]))",
            "max by (source) (rate(c[3s]))",
        ] {
            let before = batched();
            let fast = reader
                .query_range_opts(q, 2.0, 30.0, 1.0, &QueryOptions::default())
                .unwrap();
            assert_eq!(batched(), before + 1, "{q}: the batch path");
            let slow = reader
                .query_range_opts(q, 2.0, 30.0, 1.0, &per_series)
                .unwrap();
            let (
                QueryResult::Matrix {
                    result: mut fast, ..
                },
                QueryResult::Matrix {
                    result: mut slow, ..
                },
            ) = (fast, slow)
            else {
                panic!("{q}: not matrices");
            };
            let key = |m: &crate::MatrixSample| {
                let mut l: Vec<_> = m.metric.iter().collect();
                l.sort();
                format!("{l:?}")
            };
            fast.sort_by_key(key);
            slow.sort_by_key(key);
            assert_eq!(fast.len(), slow.len(), "{q}");
            assert!(!fast.is_empty(), "{q}");
            for (f, s) in fast.iter().zip(&slow) {
                assert_eq!(f.metric, s.metric, "{q}");
                assert_eq!(f.values.len(), s.values.len(), "{q}");
                for (a, b) in f.values.iter().zip(&s.values) {
                    assert_eq!(a.0, b.0, "{q}");
                    assert!(
                        (a.1 - b.1).abs() <= 1e-9 * a.1.abs().max(1.0),
                        "{q}: {a:?} {b:?}"
                    );
                }
            }
            let display = crate::DisplayOptions::default();
            let before = displayed();
            let shown = reader
                .query_range_display_opts(q, 2.0, 30.0, 1.0, &display, &QueryOptions::default())
                .unwrap();
            assert_eq!(displayed(), before + 1, "{q}: the batch display path");
            let crate::DisplayResult::Series { result, .. } = shown else {
                panic!("{q}: not series");
            };
            assert_eq!(result.len(), fast.len(), "{q}");
        }
        // Both recordings, every point: four series growing 1 to 4 a second,
        // in each.
        let sum = series(&reader, "sum(rate(c[3s]))");
        assert_eq!(sum, [(labels(&[]), 20.0)]);

        // A recording that holds matched series it cannot scan sends the
        // query to the per-series path rather than being left out of it.
        // Here b's series carry their own `source`, under `exported_source`
        // beside the recording's, and a filter on `exported_source` is
        // decided per series (`LabeledSource::counter_scan`).
        let own = InMemorySource {
            tables: vec![("t".to_string(), own_source_segments())],
            ..rec("b", 0)
        };
        let mixed = archive(vec![rec("a", 0), own]);
        let before = batched();
        // a's four series (1 + 2 + 3 + 4) and b's series whose own source is
        // not "y" (growing 5 a second).
        assert_eq!(
            series(&mixed, r#"sum(rate(c{exported_source!="y"}[3s]))"#),
            [(labels(&[]), 15.0)]
        );
        assert_eq!(batched(), before, "the per-series path");
    }

    /// Two segments of counter `c` with two series carrying their own
    /// `source`: `x` growing 5 a second, `y` 7.
    fn own_source_segments() -> Vec<Vec<u8>> {
        use arrow::array::{ArrayRef, UInt64Array};
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        (0..2u64)
            .map(|seg| {
                let ts: Vec<u64> = (seg * 15..(seg + 1) * 15).map(|k| (k + 1) * S).collect();
                let mut fields = vec![Field::new("timestamp", DataType::UInt64, false)];
                let mut columns: Vec<ArrayRef> = vec![Arc::new(UInt64Array::from(ts.clone()))];
                for (own, per_second) in [("x", 5u64), ("y", 7)] {
                    let mut meta = HashMap::new();
                    meta.insert("metric".to_string(), "c".to_string());
                    meta.insert("metric_type".to_string(), "counter".to_string());
                    meta.insert("source".to_string(), own.to_string());
                    fields.push(
                        Field::new(format!("c__{own}"), DataType::UInt64, true).with_metadata(meta),
                    );
                    columns.push(Arc::new(UInt64Array::from(
                        ts.iter().map(|t| t / S * per_second).collect::<Vec<_>>(),
                    )));
                }
                let schema = Arc::new(Schema::new(fields));
                let mut buf = Vec::new();
                let mut w = ArrowWriter::try_new(&mut buf, schema.clone(), None).unwrap();
                w.write(&RecordBatch::try_new(schema, columns).unwrap())
                    .unwrap();
                w.close().unwrap();
                buf
            })
            .collect()
    }

    /// An archive opened from bytes reports a table's declared interval, and
    /// for a table declaring none the median gap of its rows, so a hole does
    /// not lengthen it.
    #[test]
    fn an_in_memory_table_reports_its_declared_or_median_interval() {
        let declared = archive(vec![recording(
            &[("source", "a")],
            vec![("t", declared_table("g", &gauge_rows(1, 40, 1, 1), 5000))],
        )]);
        assert_eq!(ArchiveReader::interval(&declared), 5.0);
        let mut rows = gauge_rows(1, 100, 10, 1);
        rows.extend(gauge_rows(500, 600, 10, 1));
        let holed = archive(vec![recording(
            &[("source", "a")],
            vec![("t", explicit_table("g", "gauge", &rows))],
        )]);
        assert_eq!(ArchiveReader::interval(&holed), 10.0);
        // A first segment of one row: the next segment's rows give the gap.
        let short = archive(vec![InMemorySource {
            tables: vec![(
                "t".to_string(),
                vec![
                    explicit_table("g", "gauge", &gauge_rows(1, 1, 10, 1)),
                    explicit_table("g", "gauge", &gauge_rows(11, 100, 10, 1)),
                ],
            )],
            ..recording(&[("source", "a")], vec![])
        }]);
        assert_eq!(ArchiveReader::interval(&short), 10.0);
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

    /// Under `series_labels`, readers joined by `flatten` whose groups'
    /// first sources share a catalog index are marked by position.
    #[test]
    fn flattened_marks_never_collide() {
        let one = |instance: &str| {
            recording(
                &[("source", "svc"), ("instance", instance)],
                vec![(
                    "prometheus/scrape",
                    counter_table("requests", &[(&[("instance", "api:9090")], 1)], 10),
                )],
            )
        };
        // The second archive's two recordings are twins (equal labels),
        // numbered 0 and 1 within it; the mark numbers all three groups by
        // position instead, and replaces the twins' own `__recording__`.
        let reader = ArchiveReader::flatten(
            vec![
                archive(vec![one("rec-a")]),
                archive(vec![one("rec-b"), one("rec-b")]),
            ],
            None,
        );
        let result = reader
            .query_range_opts(
                "rate(requests[3s])",
                1.0,
                10.0,
                1.0,
                &QueryOptions::default().with_series_labels(true),
            )
            .unwrap();
        let QueryResult::Matrix { result, .. } = result else {
            panic!("not a matrix");
        };
        let mut got: Vec<String> = result
            .iter()
            .map(|m| m.metric[RECORDING_LABEL].clone())
            .collect();
        got.sort();
        assert_eq!(got, ["0", "1", "2"]);
        // A scope naming one of the twins reads one read group: no
        // `__recording__`, the twin's own included.
        let one_twin = QueryOptions::default()
            .with_series_labels(true)
            .with_scope(Some(Arc::new(
                crate::Scope::new(vec![crate::ScopedSource::new(
                    crate::SourceRef::Position(2),
                    None,
                )])
                .unwrap(),
            )));
        let result = reader
            .query_range_opts("rate(requests[3s])", 1.0, 10.0, 1.0, &one_twin)
            .unwrap();
        let QueryResult::Matrix { result, .. } = result else {
            panic!("not a matrix");
        };
        assert_eq!(result.len(), 1);
        assert!(
            !result[0].metric.contains_key(RECORDING_LABEL),
            "{:?}",
            result[0].metric
        );
    }

    /// A source other than an archive reader refuses `series_labels`, as it
    /// refuses a scope.
    #[test]
    fn only_an_archive_applies_series_labels() {
        let mut memory = metriken_storage::memory::Memory::new(1000);
        memory.push_counter_series(
            "c",
            metriken_storage::types::Counter {
                labels: metriken_storage::Labels::default(),
                timestamps: vec![S, 2 * S, 3 * S],
                values: vec![1, 2, 3],
                windows: None,
            },
        );
        let engine = QueryEngine::new(Arc::new(memory));
        let flagged = QueryOptions::default().with_series_labels(true);
        let err = engine
            .query_range_opts("rate(c[2s])", 1.0, 3.0, 1.0, &flagged)
            .unwrap_err()
            .to_string();
        assert!(err.contains("series_labels"), "{err}");
        assert!(engine
            .query_range_opts("rate(c[2s])", 1.0, 3.0, 1.0, &QueryOptions::default())
            .is_ok());
        let err = engine
            .query_range_display_opts(
                "rate(c[2s])",
                1.0,
                3.0,
                1.0,
                &crate::DisplayOptions::default(),
                &flagged,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("series_labels"), "{err}");
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

    /// Matching is one-to-one across recordings as within one: `g / on() h`
    /// with an `h` in each recording is many-to-many and refused in every
    /// mode; `on(source)` makes it one-to-one.
    #[test]
    fn a_many_to_many_match_across_recordings_is_refused() {
        let both = |source: &'static str, g: u64, h: u64| {
            recording(
                &[("source", source)],
                vec![
                    ("tg", explicit_table("g", "gauge", &gauge_rows(1, 40, 1, g))),
                    ("th", explicit_table("h", "gauge", &gauge_rows(1, 40, 1, h))),
                ],
            )
        };
        let reader = archive(vec![both("a", 10, 2), both("b", 100, 4)]);
        for mode in [RateMode::default(), RateMode::Raw] {
            for q in ["g / on() h", "sum(g) / on() h"] {
                let err = values(&reader, q, mode).unwrap_err();
                assert!(
                    err.to_string()
                        .contains("many-to-many matching not allowed"),
                    "{q}: {err}"
                );
            }
            let err = values(&reader, "g / on() sum(h)", mode).unwrap_err();
            assert!(err.to_string().contains("multiple matches"), "{err}");
            let mut v: Vec<f64> = values(&reader, "g / on(source) h", mode)
                .unwrap()
                .into_iter()
                .map(|s| s.last().unwrap().1)
                .collect();
            v.sort_by(f64::total_cmp);
            assert_eq!(v, [5.0, 25.0]);
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
        let QueryResult::Matrix { result, .. } = joined
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
        let QueryResult::Vector { result, .. } = joined.query("g", Some(25.0)).unwrap() else {
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
        // Under `series_labels` a label matcher does not select a
        // recording, so the refusal names the scope, and the scope answers.
        let flagged = QueryOptions::default().with_series_labels(true);
        let err = reader
            .query_range_opts(r#"m{source="a"}"#, 1.0, 10.0, 1.0, &flagged)
            .unwrap_err()
            .to_string();
        assert!(err.contains("with the query's scope"), "{err}");
        let scoped = flagged.with_scope(Some(Arc::new(
            crate::Scope::new(vec![crate::ScopedSource::new(
                crate::SourceRef::Position(0),
                None,
            )])
            .unwrap(),
        )));
        assert!(reader
            .query_range_opts(r#"m{source="a"}"#, 1.0, 10.0, 1.0, &scoped)
            .is_ok());
        // A matcher selecting one kind answers, though another operand
        // reaches the other recording.
        let with_y = archive(vec![
            recording(
                &[("source", "a")],
                vec![
                    ("t", explicit_table("m", "gauge", &gauge_rows(1, 40, 1, 10))),
                    ("ty", gauge("y", 1)),
                ],
            ),
            recording(
                &[("source", "b")],
                vec![
                    (
                        "t",
                        explicit_table("m", "counter", &gauge_rows(1, 40, 1, 10)),
                    ),
                    ("ty", gauge("y", 2)),
                ],
            ),
        ]);
        for q in [r#"m{source="a"} + y"#, r#"sum(m{source="a"}) + sum(y)"#] {
            values(&with_y, q, RateMode::default()).unwrap_or_else(|e| panic!("{q}: {e}"));
        }
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

    /// Recordings whose labels differ only in `__producer__` (an endpoint's
    /// recordings before and after a producer change) are twins: they carry
    /// `__recording__`, so `without` and binary matching keep them apart.
    #[test]
    fn recordings_differing_only_in_producer_stay_apart() {
        let rec = |producer: &'static str, v| {
            recording(
                &[("source", "s"), ("__producer__", producer)],
                vec![("t", counter_table("c", &[(&[("cpu", "0")], v)], 10))],
            )
        };
        let reader = archive(vec![rec("pa", 1), rec("pb", 2)]);
        for q in [
            "sum without (cpu) (rate(c[3s]))",
            "rate(c[3s]) / rate(c[3s])",
        ] {
            match reader.query_range_opts(q, 1.0, 10.0, 1.0, &QueryOptions::default()) {
                Ok(QueryResult::Matrix { result, .. }) => assert_eq!(result.len(), 2, "{q}"),
                other => panic!("{q}: {other:?}"),
            }
        }
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
        let QueryResult::Vector { result, .. } = reader.query("g", None).unwrap() else {
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
            let QueryResult::Vector { result, .. } = reader.query(q, None).unwrap() else {
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
