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
        check_histogram_configs(&segments)?;

        // Open-time, footer-only identity indexes (see `SeriesIdentity` /
        // `HistogramRunIndex` docs): a single schema pass per segment per
        // metric kind, reused by every query so splicing never re-derives
        // series identity by linear scan.
        let counter_identity = SeriesIdentity::build(&segments, ParquetReader::counter_columns);
        let gauge_identity = SeriesIdentity::build(&segments, ParquetReader::gauge_columns);
        let histogram_identity = SeriesIdentity::build(&segments, ParquetReader::histogram_columns);
        let histogram_runs = HistogramRunIndex::build(&segments);

        let source = SegmentedSource {
            segments: segments.iter().map(ParquetReader::data_source).collect(),
            counter_identity,
            gauge_identity,
            histogram_identity,
            histogram_runs,
        };
        let engine = QueryEngine::new(Arc::new(source));
        Ok(Self { segments, engine })
    }

    /// Number of segments backing this reader.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    // Introspection routes through `self.engine` — the same `QueryEngine` the
    // queries use, over the same [`SegmentedSource`]. `ParquetReader` does the
    // same (see `parquet.rs`). Re-deriving the union here from `self.segments`
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
    /// in `segments` order, wins on key collision).
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
}

/// Reject a segment whose OWN schema carries two histogram columns for the
/// same metric name under different `grouping_power`/`max_value_power`
/// configs.
///
/// This is a WITHIN-segment check only. `ParquetSource::histogram_stream`
/// resolves a metric purely by name (`c.name == name`) and decodes every
/// matching column under the FIRST one's config — so if one segment's schema
/// holds two differently-configured columns for the same name (label
/// metadata can't rescue this: an unqualified query like
/// `histogram_mean(latency)` has an empty label filter and matches both),
/// their buckets can never be decoded separately. That's the one conflict
/// shape this reader cannot split into distinct series, so it's rejected at
/// open rather than silently misread.
///
/// A DIFFERENT config for the same name in a LATER segment is not an error
/// here — a `.rez` agent restart can retune a sampler's histogram mid
/// recording, and each segment decodes fine under its own config. That case
/// is handled by [`HistogramRunIndex`], which splits it into distinct
/// `__run__`-labeled series instead of rejecting the whole archive.
///
/// Reads parquet field metadata only (via
/// [`ParquetReader::histogram_config_variants`]) — no row-group decode.
fn check_histogram_configs(segments: &[ParquetReader]) -> Result<(), Box<dyn Error>> {
    for (idx, segment) in segments.iter().enumerate() {
        for (name, configs) in segment.histogram_config_variants() {
            if configs.len() > 1 {
                let detail = configs
                    .iter()
                    .map(|(gp, mvp)| format!("grouping_power={gp}, max_value_power={mvp}"))
                    .collect::<Vec<_>>()
                    .join(" and ");
                return Err(format!(
                    "segment {idx} has multiple histogram columns for metric '{name}' under \
                     different configs ({detail}); ParquetSource::histogram_stream decodes \
                     every column for a metric name under ONE shared config, so these buckets \
                     cannot be separated within a single segment"
                )
                .into());
            }
        }
    }
    Ok(())
}

/// `(name, labels) -> position` for one metric kind (counter, gauge, or raw
/// per-column histogram identity), built ONCE at open from footer-only
/// column lists (see [`ParquetReader::counter_columns`] and its gauge/
/// histogram twins) — no row-group decode.
///
/// Splicing used to find a series' accumulator by scanning the
/// already-spliced `Vec` (`acc.iter_mut().find(...)` / a `Vec::position`
/// equivalent for histograms) for every incoming sample — O(segments ×
/// series) per query, which becomes tens of millions of `Labels`
/// comparisons on a wide archive (many series, many segments). This index
/// turns that into an O(1) lookup: [`SegmentedSource::counters`] /
/// `::gauges` size their accumulator once from [`Self::order`] and use
/// [`Self::position`] to place each incoming sample directly;
/// [`splice_histogram_streams`] does the same for histogram series.
///
/// Position order is "first appearance across segments, in segment order" —
/// the same ordering contract splicing has always promised
/// (`two_label_sets_splice_independently_across_three_segments` is the
/// regression test). Built from RAW schema order
/// (`counter_columns`/`gauge_columns`/`histogram_columns`), not the sorted
/// order `counter_labels`/etc. return for display.
#[derive(Default)]
struct SeriesIdentity {
    /// name -> distinct label sets, index == position.
    order: HashMap<String, Vec<Labels>>,
    /// name -> (labels -> position), mirrors `order`.
    pos: HashMap<String, HashMap<Labels, usize>>,
}

impl SeriesIdentity {
    /// Build from one schema pass per segment (`columns_for`), independent
    /// of metric name — avoids re-scanning the schema once per name.
    fn build(
        segments: &[ParquetReader],
        columns_for: impl Fn(&ParquetReader) -> Vec<(String, Labels)>,
    ) -> Self {
        let mut order: HashMap<String, Vec<Labels>> = HashMap::new();
        let mut pos: HashMap<String, HashMap<Labels, usize>> = HashMap::new();
        for seg in segments {
            for (name, labels) in columns_for(seg) {
                let list = order.entry(name.clone()).or_default();
                let p = pos.entry(name).or_default();
                if !p.contains_key(&labels) {
                    p.insert(labels.clone(), list.len());
                    list.push(labels);
                }
            }
        }
        Self { order, pos }
    }

    /// Ordered distinct label sets for `name` (empty if `name` is unknown).
    fn order(&self, name: &str) -> &[Labels] {
        self.order.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// O(1) position of `labels` within `name`'s series, if known.
    fn position(&self, name: &str, labels: &Labels) -> Option<usize> {
        self.pos.get(name)?.get(labels).copied()
    }
}

/// Per-histogram-metric run assignment, built once at open from field
/// metadata only (via [`ParquetReader::histogram_configs`], itself
/// footer-only).
///
/// A `.rez` agent restart can remap a numeric column id to a histogram with
/// different `grouping_power`/`max_value_power` mid-recording.
/// [`splice_histogram_streams`] cannot decode two configs as one series, so
/// each DISTINCT config observed for a name becomes its own "run", numbered
/// by first appearance across segments (in segment order). A name with only
/// one distinct config has a single run (`run_count() == 1`) and is spliced
/// exactly as before — no `__run__` label, no behavior change.
///
/// Safe to key runs purely by config (ignoring per-column labels): this
/// runs AFTER [`check_histogram_configs`], which has already rejected any
/// WITHIN-segment conflict — so within one segment, a metric name has at
/// most one histogram config.
#[derive(Default)]
struct HistogramRunIndex {
    /// name -> distinct configs, in first-appearance order (index == run).
    runs: HashMap<String, Vec<(u8, u8)>>,
    /// name -> (segment index -> run index).
    segment_run: HashMap<String, HashMap<usize, usize>>,
}

impl HistogramRunIndex {
    fn build(segments: &[ParquetReader]) -> Self {
        let mut runs: HashMap<String, Vec<(u8, u8)>> = HashMap::new();
        let mut segment_run: HashMap<String, HashMap<usize, usize>> = HashMap::new();
        for (idx, seg) in segments.iter().enumerate() {
            for (name, config) in seg.histogram_configs() {
                let list = runs.entry(name.clone()).or_default();
                let run = list.iter().position(|c| *c == config).unwrap_or_else(|| {
                    list.push(config);
                    list.len() - 1
                });
                segment_run.entry(name).or_default().insert(idx, run);
            }
        }
        // Split, warn, never coerce: log the conflict ONCE per open (not
        // once per query) for every name that resolved to more than one
        // run, naming the metric and each run's config so an operator can
        // tell a genuine agent restart from a misconfigured recorder.
        for (name, configs) in &runs {
            if configs.len() > 1 {
                let detail = configs
                    .iter()
                    .enumerate()
                    .map(|(run, (gp, mvp))| format!("run {run} = gp={gp}/mvp={mvp}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::warn!(
                    metric = name,
                    runs = configs.len(),
                    "histogram '{name}' has {} distinct bucket configs across segments \
                     (agent restart mid-recording?); splitting into __run__ series: {detail}",
                    configs.len(),
                );
            }
        }
        Self { runs, segment_run }
    }

    /// Number of distinct runs for `name`. `1` (or `0` for an unknown name)
    /// means no conflict: splice all segments as one series, unlabeled.
    fn run_count(&self, name: &str) -> usize {
        self.runs.get(name).map(Vec::len).unwrap_or(1)
    }

    /// Which run segment `idx` belongs to for `name`, if it carries that
    /// metric at all.
    fn segment_run(&self, name: &str, idx: usize) -> Option<usize> {
        self.segment_run.get(name)?.get(&idx).copied()
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
    /// Open-time identity indexes (see [`SeriesIdentity`]) used to splice
    /// counters/gauges/histograms in O(1) per sample instead of scanning
    /// the already-spliced accumulator.
    counter_identity: SeriesIdentity,
    gauge_identity: SeriesIdentity,
    histogram_identity: SeriesIdentity,
    /// Cross-segment histogram config-conflict resolution (see
    /// [`HistogramRunIndex`]).
    histogram_runs: HistogramRunIndex,
}

/// Merge `c`'s samples into the already-accumulated series `a` (same
/// identity; concatenate in arrival order).
///
/// Windows policy: per-point acquisition windows concatenate only when every
/// contributing chunk of a series carries them; a mixed series drops to
/// `None` (no uncertainty band) rather than risk misaligned windows.
///
/// Used by the O(1)-indexed splice path in [`SegmentedSource::counters`];
/// probed directly by [`tests::merge_counter_drops_windows_on_mixed_coverage`].
fn merge_counter(a: &mut Counter, c: Counter) {
    a.timestamps.extend(c.timestamps);
    a.values.extend(c.values);
    a.windows = match (a.windows.take(), c.windows) {
        (Some(mut aw), Some(cw)) => {
            aw.extend(cw);
            Some(aw)
        }
        _ => None,
    };
}

/// Gauge twin of [`merge_counter`].
fn merge_gauge(a: &mut Gauge, g: Gauge) {
    a.timestamps.extend(g.timestamps);
    a.values.extend(g.values);
    a.windows = match (a.windows.take(), g.windows) {
        (Some(mut aw), Some(gw)) => {
            aw.extend(gw);
            Some(aw)
        }
        _ => None,
    };
}

/// Add a `__run__` label to every series in `stream`, disambiguating a
/// histogram identity that [`HistogramRunIndex`] found split across
/// incompatible configs.
///
/// `__run__` is therefore a RESERVED label name (mirroring how
/// `:window_begin`/`:window_width` are reserved column-name suffixes, see
/// parquet.rs) — a real user label with that name (e.g. from `record
/// --label __run__=x`) would otherwise be silently overwritten here, which
/// is exactly the silent-coercion class this feature exists to prevent.
fn relabel_with_run(mut stream: HistogramStream, run: usize) -> HistogramStream {
    for labels in &mut stream.meta.series {
        debug_assert!(
            !labels.inner.contains_key("__run__"),
            "'__run__' is a reserved label name; a real label with this name would be \
             silently overwritten by the run-disambiguation policy"
        );
        labels.inner.insert("__run__".to_string(), run.to_string());
    }
    stream
}

/// Chain per-segment histogram streams (all belonging to the same run — see
/// [`HistogramRunIndex`]) in segment order, remapping each stream's series
/// indices onto a unified series list so the same labels are ONE series
/// across segments. Unlike [`HistogramStream::merge`] (a k-way sort-merge
/// for independent files), this concatenates — preserving single-file
/// row-order semantics for segments of one table.
///
/// `identity` (built once at open, see [`SeriesIdentity`]) gives each
/// label set's position in O(1); the loop below still only materializes
/// entries for labels actually present in `streams` (a label-filtered query
/// may touch only a subset of `identity`'s full order), so it never invents
/// phantom empty series for labels the caller filtered out.
fn splice_histogram_streams(
    name: &str,
    identity: &SeriesIdentity,
    streams: Vec<HistogramStream>,
) -> Option<HistogramStream> {
    if streams.len() <= 1 {
        return streams.into_iter().next();
    }
    let config = streams[0].meta.config;
    // `check_histogram_configs` + `HistogramRunIndex` keep same-run streams
    // config-uniform, so this is belt-and-braces for a source composed some
    // other way. Log in release too: the failure mode is silently wrong
    // bucket boundaries, not a crash.
    if streams.iter().any(|s| s.meta.config != config) {
        tracing::error!(
            metric = name,
            "histogram configs differ across segments within one run; decoding every \
             segment under the first segment's config will produce wrong bucket \
             boundaries (this should have been rejected at open or split into runs)"
        );
    }
    debug_assert!(
        streams.iter().all(|s| s.meta.config == config),
        "segments spliced together must share a histogram config"
    );
    let mut series: Vec<Labels> = Vec::new();
    let mut local_of_global: HashMap<usize, usize> = HashMap::new();
    let mut parts: Vec<Box<dyn Iterator<Item = crate::histogram_stream::HistogramRow> + Send>> =
        Vec::with_capacity(streams.len());
    for stream in streams {
        let remap: Vec<usize> = stream
            .meta
            .series
            .iter()
            .map(|labels| match identity.position(name, labels) {
                Some(global) => *local_of_global.entry(global).or_insert_with(|| {
                    series.push(labels.clone());
                    series.len() - 1
                }),
                None => {
                    // Shouldn't happen: `identity` was built from the union
                    // of these same segments' histogram columns. Defensive
                    // fallback so a mismatch degrades to an extra series,
                    // not a panic.
                    tracing::warn!(
                        metric = name,
                        ?labels,
                        "histogram series missing from open-time identity index"
                    );
                    series.push(labels.clone());
                    series.len() - 1
                }
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
        // Slots sized/positioned from the open-time identity index: O(1)
        // per incoming sample instead of the O(series) `Vec` scan splicing
        // used to do (see `SeriesIdentity`). A label-filtered query simply
        // leaves the non-matching slots `None`, dropped by `flatten()`
        // below — same outcome as before, just not by linear search.
        let order = self.counter_identity.order(name);
        if order.is_empty() {
            return None;
        }
        let mut slots: Vec<Option<Counter>> = (0..order.len()).map(|_| None).collect();
        for seg in &self.segments {
            let Some(chunk) = seg.counters(name, filter, start_ns, end_ns, raw) else {
                continue;
            };
            for c in chunk.series {
                match self.counter_identity.position(name, &c.labels) {
                    Some(pos) => match &mut slots[pos] {
                        Some(a) => merge_counter(a, c),
                        slot => *slot = Some(c),
                    },
                    None => {
                        tracing::warn!(
                            metric = name,
                            labels = ?c.labels,
                            "counter series missing from open-time identity index; dropping"
                        );
                    }
                }
            }
        }
        let series: Vec<Counter> = slots.into_iter().flatten().collect();
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
        let order = self.gauge_identity.order(name);
        if order.is_empty() {
            return None;
        }
        let mut slots: Vec<Option<Gauge>> = (0..order.len()).map(|_| None).collect();
        for seg in &self.segments {
            let Some(chunk) = seg.gauges(name, filter, start_ns, end_ns, raw) else {
                continue;
            };
            for g in chunk.series {
                match self.gauge_identity.position(name, &g.labels) {
                    Some(pos) => match &mut slots[pos] {
                        Some(a) => merge_gauge(a, g),
                        slot => *slot = Some(g),
                    },
                    None => {
                        tracing::warn!(
                            metric = name,
                            labels = ?g.labels,
                            "gauge series missing from open-time identity index; dropping"
                        );
                    }
                }
            }
        }
        let series: Vec<Gauge> = slots.into_iter().flatten().collect();
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
        if self.histogram_runs.run_count(name) <= 1 {
            let streams: Vec<HistogramStream> = self
                .segments
                .iter()
                .filter_map(|seg| seg.histogram_stream(name, filter, start_ns, end_ns))
                .collect();
            return splice_histogram_streams(name, &self.histogram_identity, streams);
        }

        // Cross-segment histogram config conflict: `name` carries more than
        // one distinct (grouping_power, max_value_power) across segments
        // (a `.rez` agent restart retuned the sampler mid-recording). Each
        // config is its own run, disambiguated by a `__run__` label; an
        // unqualified selector (no explicit `__run__`) resolves to the
        // FIRST run rather than mixing configs — split, warn, never coerce.
        let (want_run, inner_filter) = match filter.inner.get("__run__") {
            Some(v) => {
                let want: usize = v.parse().ok()?;
                let mut f = filter.clone();
                f.inner.remove("__run__");
                (want, f)
            }
            None => (0usize, filter.clone()),
        };

        let streams: Vec<HistogramStream> = self
            .segments
            .iter()
            .enumerate()
            .filter(|&(idx, _)| self.histogram_runs.segment_run(name, idx) == Some(want_run))
            .filter_map(|(_, seg)| seg.histogram_stream(name, &inner_filter, start_ns, end_ns))
            .collect();

        let spliced = splice_histogram_streams(name, &self.histogram_identity, streams)?;
        Some(relabel_with_run(spliced, want_run))
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
        if self.histogram_runs.run_count(name) <= 1 {
            return union_labels(self.segments.iter().map(|s| s.histogram_labels(name)));
        }
        // Conflict: surface each run's label sets tagged with `__run__` so
        // the split is addressable (`histogram_mean(latency{__run__="1"})`).
        // `__run__` is a RESERVED label name (see `relabel_with_run`) — a
        // real user label with this name would otherwise be silently
        // overwritten below.
        let mut sets: Vec<BTreeMap<String, String>> = Vec::new();
        for (idx, seg) in self.segments.iter().enumerate() {
            let Some(run) = self.histogram_runs.segment_run(name, idx) else {
                continue;
            };
            for mut labels in seg.histogram_labels(name) {
                debug_assert!(
                    !labels.contains_key("__run__"),
                    "'__run__' is a reserved label name; a real label with this name would be \
                     silently overwritten by the run-disambiguation policy"
                );
                labels.insert("__run__".to_string(), run.to_string());
                sets.push(labels);
            }
        }
        sets.sort();
        sets.dedup();
        sets
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

    /// Direct unit test of the windows policy in [`merge_counter`] /
    /// [`merge_gauge`] — the merge step the O(1)-indexed splice path in
    /// [`SegmentedSource::counters`]/`::gauges` uses for a repeat identity.
    /// The end-to-end query path cannot discriminate here:
    /// `collect_to_matrix` reports `intervals: None` unless every point has a
    /// band, so a splice that kept one segment's (now short and misattributed)
    /// windows looks identical from outside. These assertions read the merged
    /// `windows` field itself. (Every case here operates on a single series
    /// at position 0 — this test is about the windows-merge policy, not
    /// identity matching across multiple series; that's covered by
    /// `two_label_sets_splice_independently_across_three_segments`.)
    #[test]
    fn merge_counter_drops_windows_on_mixed_coverage() {
        let counter = |windows: Option<Vec<(u64, u64)>>| Counter {
            labels: Labels::default(),
            timestamps: vec![1, 2],
            values: vec![10, 20],
            windows,
        };
        let with = || Some(vec![(0u64, 1u64), (1, 2)]);

        // Some then None -> None.
        let mut a = counter(with());
        merge_counter(&mut a, counter(None));
        assert!(
            a.windows.is_none(),
            "a later segment without windows must drop the band, not leave a \
             short window vector misaligned with {} timestamps",
            a.timestamps.len()
        );

        // None then Some -> None (windows must never attach to the wrong samples).
        let mut a = counter(None);
        merge_counter(&mut a, counter(with()));
        assert!(a.windows.is_none());

        // Some then Some -> concatenated, one entry per timestamp.
        let mut a = counter(with());
        merge_counter(&mut a, counter(with()));
        assert_eq!(a.windows.as_ref().map(Vec::len), Some(4));
        assert_eq!(a.timestamps.len(), 4);

        // Gauges follow the same policy.
        let gauge = |windows: Option<Vec<(u64, u64)>>| Gauge {
            labels: Labels::default(),
            timestamps: vec![1, 2],
            values: vec![10, 20],
            windows,
        };
        let mut a = gauge(with());
        merge_gauge(&mut a, gauge(None));
        assert!(a.windows.is_none());

        let mut a = gauge(None);
        merge_gauge(&mut a, gauge(with()));
        assert!(a.windows.is_none());

        let mut a = gauge(with());
        merge_gauge(&mut a, gauge(with()));
        assert_eq!(a.windows.as_ref().map(Vec::len), Some(4));
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

    #[test]
    fn histogram_run_index_flags_cross_segment_config_drift() {
        // Direct unit test of the split-detection step `histogram_power_drift_splits_series`
        // exercises end-to-end: two segments with different powers for the
        // same name must resolve to two runs, each segment assigned to its
        // own run in first-appearance order. This is also where
        // `HistogramRunIndex::build` logs the "splitting into __run__
        // series" warning (see its doc comment) — once per open, not once
        // per query.
        let n2 = ::histogram::Config::new(2, 8).unwrap().total_buckets();
        let n3 = ::histogram::Config::new(3, 8).unwrap().total_buckets();
        let seg_a = ParquetReader::open_bytes_with_pool(
            segment_histogram("latency", 2, 8, &[(1_000_000_000, vec![0u64; n2])]),
            BufferPool::new(64 * 1024 * 1024),
        )
        .unwrap();
        let seg_b = ParquetReader::open_bytes_with_pool(
            segment_histogram("latency", 3, 8, &[(2_000_000_000, vec![0u64; n3])]),
            BufferPool::new(64 * 1024 * 1024),
        )
        .unwrap();

        let index = HistogramRunIndex::build(&[seg_a, seg_b]);
        assert_eq!(
            index.run_count("latency"),
            2,
            "two distinct configs must resolve to two runs"
        );
        assert_eq!(index.segment_run("latency", 0), Some(0));
        assert_eq!(index.segment_run("latency", 1), Some(1));

        // A name with a single config anywhere is not a conflict.
        assert_eq!(index.run_count("no_such_metric"), 1);
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
}
