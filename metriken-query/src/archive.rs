//! Queries over an archive ([`ArchiveReader`], `metriken-storage`'s): which
//! table answers a query, the evaluation timestamps for a query spanning
//! samplers of different cadence, and the [`MetricsSource`] implementation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use metriken_storage::reader::{SamplerReader, TableReader};
use metriken_storage::{table_sampler, ArchiveReader};

use crate::{
    MetricsSource, QueryError, QueryOptions, QueryResult, RateMode, SegmentedParquetReader,
    UnionChild, UnionError, UnionMetricsSource,
};

/// The evaluation timestamps a composed query needs to stay faithful to
/// this recording's cadences, or `None` when it does not need any.
///
/// Querying this reader directly applies the cross-cadence policy itself
/// (see `query_range_opts`): when a query spans samplers recording at
/// different rates, the points are moved onto the SLOW table's own rows, so
/// every point lands where both operands genuinely have data. A caller that
/// instead composes [`composition_sources`](Self::composition_sources) into
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
/// if let Some(points) = reader.eval_timestamps_for(query, step_s, opts.rate_mode) {
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

/// Resolve the reader that answers every metric a query references: the
/// single owning table directly, or — when every owner lives in ONE
/// RECORDING — a fresh [`UnionMetricsSource`] over exactly those tables.
///
/// Tables of different samplers union freely within a recording. They did
/// not always: a query spanning two samplers used to be refused as
/// "cross-timeline", because there was no way to say what treating two
/// separately-read values as simultaneous costs. The query engine now
/// prices that itself — operands whose acquisition edges differ have their
/// bands widened to the union of both spans — so the refusal has nothing
/// left to protect. Measured, the join costs 1.5–3.0 ms on a 32-core host,
/// under 1% of a 200 ms interval.
///
/// Still refused: the SAME sampler across two DIFFERENT recordings of a
/// multi-recording (A/B) archive. Those are genuinely different timelines
/// — different agents, hosts or arms — and unioning them would let
/// first-wins silently answer from one recording. Errors too when the
/// query references no known metric.
fn route(reader: &ArchiveReader, query: &str) -> Result<Routed, QueryError> {
    let owners = owners(reader, query)?;
    match owners.as_slice() {
        [] => Err(QueryError::ParseError(format!(
            "query references no metric present in this .rez: {query}"
        ))),
        // A table whose segments have gone since the probe is absent, so
        // its metrics are too: the same error a query naming a metric this
        // archive never held gets.
        [one] => one
            .reader()
            .map(|r| {
                Routed::Direct(SegmentedParquetReader::from_source(Arc::clone(
                    r.segmented(),
                )))
            })
            .ok_or_else(|| {
                QueryError::ParseError(format!(
                    "query references {query}, whose table ({}) has been evicted since \
                 this archive was opened",
                    one.sampler()
                ))
            }),
        many => {
            // Group owners by (RECORDING, SAMPLER), not by sampler alone:
            // two group tables of one sampler are a same-timeline union
            // ONLY within one recording. `from_recordings` flattens every
            // recording's tables into one `tables` vec, so a metric
            // present in every recording of a multi-recording (A/B)
            // archive — e.g. `cpu_cycles` in each side's `cpu_usage`
            // table — would otherwise look exactly like two group tables
            // of one sampler, and unioning them would let
            // `UnionSource`'s first-wins silently answer from ONE
            // recording instead of refusing (see the module docs).
            // `rez::table_sampler` is the identity function for every V2
            // (or unsplit V3) table, so within one recording this
            // reduces to today's behavior whenever nothing actually
            // split.
            let mut groups: Vec<usize> = many.iter().map(|t| t.recording()).collect();
            groups.sort();
            groups.dedup();
            match groups.as_slice() {
                [_] => {
                    // Building the union only touches each table's
                    // already-open, footer-level name catalog (no
                    // row-group decode), so a fresh one per query is
                    // cheap enough not to need caching.
                    //
                    // `try_new`, not `new`: this composition set is
                    // derived from archive bytes (table schemas plus
                    // parsed table keys), not hand-picked by trusted
                    // code, so a producer/archive bug that put the same
                    // metric name in two "disjoint" group tables of this
                    // sampler must be a loud error, not `UnionSource`'s
                    // silent first-wins.
                    // `filter_map`, not `map`: on a live archive one of
                    // several owners can have been evicted between the
                    // probe and here, and the rest still answer.
                    let children: Vec<UnionChild> = many
                        .iter()
                        .filter_map(|t| t.reader().map(TableReader::union_child))
                        .collect();
                    if children.is_empty() {
                        return Err(QueryError::ParseError(format!(
                            "query references {query}, whose tables have all been \
                             evicted since this archive was opened"
                        )));
                    }
                    UnionMetricsSource::try_new(children)
                        .map(Routed::Union)
                        .map_err(|e| match e {
                            UnionError::NonDisjoint { duplicates } => {
                                // Two very different situations produce
                                // this, and conflating them tells the
                                // operator their archive is corrupt when
                                // it is not. Distinct SAMPLERS sharing a
                                // metric name is legitimate and shipped:
                                // `gpu_amd_smi` and `gpu_nvidia` both
                                // publish the vendor-neutral
                                // `gpu_utilization`, `gpu_temperature` and
                                // six more, because only one of them ever
                                // populates on a given host. Two group
                                // tables of ONE sampler sharing a name is
                                // a real archive defect.
                                let mut samplers: Vec<&str> =
                                    many.iter().map(|t| table_sampler(t.sampler())).collect();
                                samplers.sort();
                                samplers.dedup();
                                if samplers.len() > 1 {
                                    QueryError::ParseError(format!(
                                        "query {query} references metric name(s) published \
                                         by more than one sampler ({}), so it is ambiguous \
                                         which is meant: {}. This is not an archive fault — \
                                         those samplers deliberately share vendor-neutral \
                                         names. Query one of them at a time.",
                                        samplers.join(", "),
                                        duplicates.join(", ")
                                    ))
                                } else {
                                    QueryError::ParseError(format!(
                                        "query {query} references metric name(s) present in \
                                         more than one acquisition-group table of the same \
                                         sampler — the archive's own tables are not \
                                         disjoint, which should never happen: {}",
                                        duplicates.join(", ")
                                    ))
                                }
                            }
                            UnionError::Empty => {
                                unreachable!("the `many` arm always has at least 2 owners")
                            }
                        })
                }
                _ => {
                    // Only one case reaches here now: metrics drawn from
                    // more than one RECORDING of a multi-recording
                    // archive. Those are different agents, hosts or arms
                    // on genuinely different timelines, and the widened
                    // band does not make them comparable — unioning them
                    // would let first-wins silently answer from one side.
                    let mut samplers: Vec<&str> =
                        many.iter().map(|t| table_sampler(t.sampler())).collect();
                    samplers.sort();
                    samplers.dedup();
                    Err(QueryError::ParseError(format!(
                        "query {query} references metrics ({}) from {} different \
                         recordings of this multi-recording .rez — cross-recording \
                         queries are not supported; query one recording at a time \
                         (see `ArchiveReader::open_recordings`)",
                        samplers.join(", "),
                        groups.len()
                    )))
                }
            }
        }
    }
}

/// What `route()` resolves a query to: the one owning table's reader, or a
/// same-timeline union built fresh for this one query.
enum Routed {
    Direct(SegmentedParquetReader),
    Union(UnionMetricsSource),
}

impl Routed {
    fn as_dyn(&self) -> &dyn MetricsSource {
        match self {
            Routed::Direct(r) => r,
            Routed::Union(u) => u,
        }
    }
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
        route(self, expr)?
            .as_dyn()
            .query_range_opts(expr, start_s, end_s, step_s, opts)
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
        route(self, expr)?
            .as_dyn()
            .query_range_display_opts(expr, start_s, end_s, step_s, opts, qopts)
    }
    fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        route(self, expr)?.as_dyn().query(expr, time)
    }
    fn columns(&self, query: &str) -> Result<HashSet<String>, QueryError> {
        // columns() is answerable as the union — it never crosses timelines.
        let mut out = HashSet::new();
        for t in owners(self, query)? {
            let Some(r) = t.reader() else {
                continue;
            };
            out.extend(
                SegmentedParquetReader::from_source(Arc::clone(r.segmented())).columns(query)?,
            );
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
}
