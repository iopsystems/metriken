//! SegmentedParquetReader: presents an ordered list of parquet byte blobs
//! (segments of one logical table) as a single MetricsSource. Open is
//! footer-only per segment; queries decode only the row groups they touch,
//! spliced in segment order. Same-identity columns across segments are ONE
//! series (unlike MultiParquetSource, which duplicates).

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use lru::LruCache;

use crate::histogram_stream::{HistogramStream, HistogramStreamMeta};
use crate::labels::Labels;
use crate::parquet::MultiParquetSource;

use crate::types::{Counter, Counters, Gauge, Gauges};
use crate::{BufferPool, DataSource};

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
pub trait SegmentStore: Send + Sync {
    /// How many segments the table has.
    fn len(&self) -> usize;

    /// The bytes of segment `idx`.
    ///
    /// `Ok(None)` means the segment no longer exists — a rolling buffer's
    /// retention took it since the store was built — and the reader skips
    /// it. `Err` is a read failure and is reported.
    fn bytes(&self, idx: usize) -> SegmentBytes;

    /// An id for segment `idx` that differs whenever the segment's bytes do,
    /// among this table's segments as the store presents them over time.
    /// `None` (the default) for a segment that can change under one id, such
    /// as one built from a live tail. A reader reopened with
    /// `metriken_query::SegmentedParquetReader::open_after` reuses its predecessor's work
    /// for the leading segments whose ids match; only the segments before
    /// the first `None` are reused.
    fn key(&self, idx: usize) -> Option<u64> {
        let _ = idx;
        None
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What [`SegmentStore::bytes`] returns: the segment, `None` if it is gone,
/// or the read failure.
pub type SegmentBytes = Result<Option<Bytes>, Box<dyn Error + Send + Sync>>;

/// Identity that varies with time: what a column's samples are attributed
/// to depends on when they were taken.
///
/// A table's column is a slot, and a slot changes hands — a task exits and
/// another lands in its place. The column's own labels say what the slot
/// meant when the segment was written, or, once identity leaves column
/// metadata, only which slot it is. The archive knows who held the slot and
/// when; this is how it tells the reader. The reader asks at open what
/// label sets each column can present as (for the identity indexes and the
/// listings), and at query time how to cut a column's samples into runs by
/// occupant. It never sees the index itself.
///
/// The filter question is the subtle one. A query's label filter is applied
/// inside a segment against column labels, and a key the relabelling
/// supplies (`comm`, say) is not on the column, so the segment would match
/// nothing. [`segment_filter`](Self::segment_filter) is where the
/// implementation turns such a filter into one the columns can answer, and
/// the reader applies the original filter to the relabelled runs afterwards.
pub trait ColumnRelabel: Send + Sync {
    /// Every label set a column of metric `name` with `labels` presents as
    /// over the recording, in first-appearance order. `None` leaves the
    /// column as it is.
    fn identities(&self, name: &str, labels: &Labels) -> Option<Vec<Labels>>;

    /// Cut a column's samples, taken at ascending `timestamps`, into runs
    /// that each present as one label set. The runs cover every sample, in
    /// order. `None` leaves the column as it is.
    fn split(&self, name: &str, labels: &Labels, timestamps: &[u64]) -> Option<Vec<Run>>;

    /// What one sample of the column presents as. `None` leaves it as it is.
    fn at(&self, name: &str, labels: &Labels, timestamp: u64) -> Option<Labels>;

    /// Whether each column presents as exactly one label set for every one
    /// of its samples, and every instance built over the same table returns
    /// that same set for the columns of a segment with a
    /// [`SegmentStore::key`]. A reader then reads one occupant of a long
    /// column without relabelling it, and a reader reopened with
    /// `metriken_query::SegmentedParquetReader::open_after` keeps its predecessor's
    /// identities. Default: false.
    fn identities_are_fixed(&self) -> bool {
        false
    }

    /// The filter to ask a segment with, given the query's. Default: the
    /// query's own.
    fn segment_filter(&self, name: &str, filter: &Labels) -> Labels {
        let _ = name;
        filter.clone()
    }
}

/// One run of a relabelled column: the labels its samples present as, and
/// which samples (a range of indices into the column's own).
pub type Run = (Labels, Range<usize>);

/// A store over bytes already in memory: the tar archive, the tests, and
/// any caller of the original `metriken_query::SegmentedParquetReader::open_bytes_with_pool`.
pub struct InMemorySegments(Vec<Bytes>);

impl InMemorySegments {
    pub fn new(segments: Vec<Vec<u8>>) -> Self {
        Self(segments.into_iter().map(Bytes::from).collect())
    }
}

impl SegmentStore for InMemorySegments {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn bytes(&self, idx: usize) -> SegmentBytes {
        Ok(self.0.get(idx).cloned())
    }
}

/// What one open asks of a relabel and of the identity indexes, kept so
/// each is done once per column rather than once per segment: a relabel's
/// answer depends on the metric and the column's labels, not the segment.
#[derive(Default)]
struct IdentityMemo {
    /// metric -> column labels -> the sets the column presents as.
    sets: HashMap<String, HashMap<Labels, Presented>>,
    /// metric -> column labels -> counter positions of its sets, once they
    /// are registered.
    positions: HashMap<String, HashMap<Labels, Positions>>,
    /// Per [`Kind`], metric -> the column labels already registered.
    registered: [HashMap<String, std::collections::HashSet<Labels>>; 3],
}

/// The counter positions of a column's sets; `None` for a set the index
/// does not hold.
type Positions = Arc<[Option<usize>]>;

/// The label sets a column presents as.
#[derive(Clone)]
enum Presented {
    /// Its own labels.
    Own,
    /// The relabel's.
    Sets(Arc<[Labels]>),
}

impl Presented {
    /// The sets, given the column's own labels.
    fn of<'a>(&'a self, own: &'a Labels) -> &'a [Labels] {
        match self {
            Presented::Own => std::slice::from_ref(own),
            Presented::Sets(sets) => sets,
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Counter = 0,
    Gauge = 1,
    ColumnMap = 2,
}

impl IdentityMemo {
    /// The label sets a column of `name` with `labels` presents as: its
    /// own, unless the relabel says otherwise. Sets `unresolved` for a
    /// column naming an occupant the relabel did not describe.
    fn presents(
        &mut self,
        relabel: Option<&Arc<dyn ColumnRelabel>>,
        name: &str,
        labels: &Labels,
        unresolved: &mut bool,
    ) -> Presented {
        let Some(r) = relabel else {
            return Presented::Own;
        };
        if let Some(sets) = self.sets.get(name).and_then(|m| m.get(labels)) {
            return sets.clone();
        }
        let sets = match r.identities(name, labels) {
            Some(sets) => Presented::Sets(sets.into()),
            None => {
                if labels.inner.contains_key(crate::long::OCCUPANT_LABEL) {
                    *unresolved = true;
                }
                Presented::Own
            }
        };
        self.sets
            .entry(name.to_string())
            .or_default()
            .insert(labels.clone(), sets.clone());
        sets
    }

    /// Whether this is the first time `(kind, name, labels)` is seen.
    fn register(&mut self, kind: Kind, name: &str, labels: &Labels) -> bool {
        let seen = &mut self.registered[kind as usize];
        if seen.get(name).is_some_and(|m| m.contains(labels)) {
            return false;
        }
        seen.entry(name.to_string())
            .or_default()
            .insert(labels.clone());
        true
    }

    /// The positions in `identity` of the sets a counter column presents
    /// as. Ask after registering the column: the answer is kept, and a set
    /// not yet registered has no position.
    fn positions(
        &mut self,
        relabel: Option<&Arc<dyn ColumnRelabel>>,
        identity: &SeriesIdentity,
        name: &str,
        labels: &Labels,
        unresolved: &mut bool,
    ) -> Positions {
        if let Some(p) = self.positions.get(name).and_then(|m| m.get(labels)) {
            return Arc::clone(p);
        }
        let sets = self.presents(relabel, name, labels, unresolved);
        let p: Positions = sets
            .of(labels)
            .iter()
            .map(|l| identity.position(name, l))
            .collect();
        self.positions
            .entry(name.to_string())
            .or_default()
            .insert(labels.clone(), Arc::clone(&p));
        p
    }
}

/// What `metriken_query::SegmentedParquetReader`'s open builds from a table's segments:
/// the identity indexes, catalog and column locations. Cloned so a reader
/// reopened over the same table starts from the state its predecessor had
/// after the segments both share.
#[derive(Clone, Default)]
struct OpenState {
    /// One entry per segment, in store order.
    catalog: Vec<SegmentCatalog>,
    /// Open-time identity indexes (see [`SeriesIdentity`]) used to splice
    /// counters/gauges/histograms in O(1) per sample instead of scanning
    /// the already-spliced accumulator.
    counter_identity: SeriesIdentity,
    gauge_identity: SeriesIdentity,
    histogram_identity: SeriesIdentity,
    /// Cross-segment histogram config-conflict resolution (see
    /// [`HistogramRunIndex`]).
    histogram_runs: HistogramRunIndex,
    /// Merged across segments at open, last wins.
    file_metadata: HashMap<String, String>,
    /// Built at open from every segment's columns; see `column_map`.
    column_map: HashMap<String, HashMap<Labels, String>>,
    /// name -> per series position, the `(segment, column)` pairs its
    /// samples come from, in segment order. What `counter_streams` reads.
    counter_columns: CounterColumns,
    /// Segments folded in that were present (not gone).
    opened: usize,
    /// Whether a column named an occupant the relabel did not describe.
    unresolved: bool,
}

impl OpenState {
    /// Fold segment `idx`, opened footer-only as `seg`, into the state.
    fn fold(
        &mut self,
        idx: usize,
        seg: &MultiParquetSource,
        relabel: Option<&Arc<dyn ColumnRelabel>>,
        memo: &mut IdentityMemo,
    ) -> Result<(), Box<dyn Error>> {
        check_histogram_configs(idx, seg)?;
        let mut unresolved = false;
        // Each counter and gauge column's identities, registered once per
        // open.
        for (kind, identity, columns) in [
            (
                Kind::Counter,
                &mut self.counter_identity,
                seg.counter_columns(),
            ),
            (Kind::Gauge, &mut self.gauge_identity, seg.gauge_columns()),
        ] {
            for (name, labels) in columns {
                let sets = memo.presents(relabel, &name, &labels, &mut unresolved);
                if memo.register(kind, &name, &labels) {
                    identity.extend(sets.of(&labels).iter().map(|l| (name.clone(), l.clone())));
                }
            }
        }
        // Where each counter series' samples are, by segment: the
        // column, located once here so a stream can read it back
        // without a schema scan. A relabelled column feeds every series
        // it can present as.
        for col in seg.counter_column_refs() {
            let positions = memo.positions(
                relabel,
                &self.counter_identity,
                &col.name,
                &col.labels,
                &mut unresolved,
            );
            let table = self.counter_columns.entry(col.name.clone()).or_default();
            // The column's own labels, interned: one copy per distinct
            // set rather than one per segment it appears in.
            let column_labels = match table.labels_index.get(&col.labels) {
                Some(i) => *i,
                None => {
                    table.labels.push(col.labels.clone());
                    table
                        .labels_index
                        .insert(col.labels.clone(), table.labels.len() - 1);
                    table.labels.len() - 1
                }
            } as u32;
            for pos in positions.iter().flatten() {
                if table.by_series.len() <= *pos {
                    table.by_series.resize_with(pos + 1, Vec::new);
                }
                table.by_series[*pos].push(Location {
                    segment: idx as u32,
                    column_labels,
                    position: col.position,
                });
            }
        }
        // Histograms are tracked per segment (their runs), so their sets
        // are listed every time.
        let histogram_columns: Vec<(String, Labels)> = seg
            .histogram_columns()
            .into_iter()
            .flat_map(|(name, labels)| {
                let sets = memo.presents(relabel, &name, &labels, &mut unresolved);
                sets.of(&labels)
                    .iter()
                    .map(|l| (name.clone(), l.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        self.histogram_runs.observe(idx, seg.histogram_configs());
        self.histogram_runs.observe_labels(idx, &histogram_columns);
        self.histogram_identity.extend(histogram_columns);
        for (metric, cols) in seg.column_map() {
            // Mirror the run tagging `histogram_stream` and
            // `histogram_labels` apply. `QueryEngine::columns` requires
            // every filter key to be PRESENT on the label set, so without
            // `__run__` here a run-qualified selector resolves to no
            // columns at all — and the only production consumer
            // (rezolus' `RezReader`) routes every query through
            // `columns()` first, so it would reject
            // `histogram_mean(latency{__run__="1"})` outright and leave
            // the conflict policy's documented escape hatch unreachable.
            //
            // Tagged for every histogram the run index knows, not just the
            // drifted ones, so `__run__="0"` also addresses a metric that
            // never drifted — matching `histogram_stream`, which accepts
            // run 0 in the single-run case. These labels are routing keys,
            // never user-visible series labels (that is `histogram_labels`,
            // which still only tags an actual conflict).
            let run = self.histogram_runs.segment_run(&metric, idx);
            let entry = self.column_map.entry(metric.clone()).or_default();
            for (labels, col) in cols {
                let sets = memo.presents(relabel, &metric, &labels, &mut unresolved);
                // Without a run tag a column's entries are the same in
                // every segment, and the first one stays.
                if run.is_none() && !memo.register(Kind::ColumnMap, &metric, &labels) {
                    continue;
                }
                for set in sets.of(&labels) {
                    let mut set = set.clone();
                    if let Some(run) = run {
                        set.inner.insert("__run__".to_string(), run.to_string());
                    }
                    entry.entry(set).or_insert_with(|| col.clone());
                }
            }
        }
        // Last segment wins on collision.
        self.file_metadata.extend(seg.file_metadata());
        self.catalog.push(SegmentCatalog {
            span: seg.time_range(),
            interval: seg.interval(),
            present: true,
        });
        self.opened += 1;
        self.unresolved |= unresolved;
        Ok(())
    }
}

/// What open keeps about one segment once its footer has been read and
/// dropped: enough to decide whether a query touches it without fetching it.
#[derive(Clone, Copy, Debug)]
struct SegmentCatalog {
    /// Row-time span from the footer's statistics; `None` when the segment
    /// has no timestamp statistics, which makes it a candidate for every
    /// query.
    span: Option<(u64, u64)>,
    /// Declared sampling interval, seconds.
    interval: f64,
    /// False for a segment the store no longer had at open. Never fetched.
    present: bool,
}

impl SegmentCatalog {
    const GONE: Self = Self {
        span: None,
        interval: f64::MAX,
        present: false,
    };

    /// Whether a query over `[start_ns, end_ns]` can touch this segment.
    fn touches(&self, start_ns: u64, end_ns: u64) -> bool {
        self.present
            && match self.span {
                Some((lo, hi)) => lo <= end_ns && hi >= start_ns,
                None => true,
            }
    }
}

/// Opened segments, most recently used last, bounded by what they hold.
///
/// A segment is fetched from the store and its footer parsed when a query
/// first touches it; the next query over the same range finds it here. Each
/// entry is charged [`MultiParquetSource::resident_estimate`] — its bytes plus
/// what its parsed footer and column descriptors take, which on a wide
/// table is more than the bytes — against the budget the pool already asks
/// the operator for.
struct SegmentCache {
    entries: LruCache<usize, (Arc<MultiParquetSource>, usize)>,
    bytes: usize,
    max_bytes: usize,
}

impl SegmentCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            entries: LruCache::unbounded(),
            bytes: 0,
            max_bytes,
        }
    }

    fn get(&mut self, idx: usize) -> Option<Arc<MultiParquetSource>> {
        self.entries.get(&idx).map(|(seg, _)| Arc::clone(seg))
    }

    /// Insert, then evict least recently used entries until within budget —
    /// never the one just inserted, so a segment larger than the whole
    /// budget still opens.
    fn insert(&mut self, idx: usize, seg: Arc<MultiParquetSource>, size: usize) {
        if let Some((_, old)) = self.entries.push(idx, (seg, size)) {
            self.bytes = self.bytes.saturating_sub(old.1);
        }
        self.bytes += size;
        while self.bytes > self.max_bytes && self.entries.len() > 1 {
            if let Some((_, (_, freed))) = self.entries.pop_lru() {
                self.bytes = self.bytes.saturating_sub(freed);
            }
        }
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
/// [`MultiParquetSource::histogram_config_variants`]) — no row-group decode.
fn check_histogram_configs(idx: usize, segment: &MultiParquetSource) -> Result<(), Box<dyn Error>> {
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
    Ok(())
}

/// The hasher for maps keyed by a series' labels, which the reader builds at
/// open and probes per column: foldhash rather than SipHash, being faster on
/// these keys and still seeded per process.
type LabelsHash = foldhash::fast::RandomState;

/// `(name, labels) -> position` for one metric kind (counter, gauge, or raw
/// per-column histogram identity), built ONCE at open from footer-only
/// column lists (see [`MultiParquetSource::counter_columns`] and its gauge/
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
/// It is also what answers `*_names` and `*_labels` once the footers are
/// gone: the union of every segment's identity, which is what those used
/// to compute from the footers per call.
///
/// Position order is "first appearance across segments, in segment order" —
/// the same ordering contract splicing has always promised
/// (`two_label_sets_splice_independently_across_three_segments` is the
/// regression test). Built from RAW schema order
/// (`counter_columns`/`gauge_columns`/`histogram_columns`), not the sorted
/// order `counter_labels`/etc. return for display.
#[derive(Clone, Default)]
struct SeriesIdentity {
    /// name -> distinct label sets, index == position.
    order: HashMap<String, Vec<Labels>>,
    /// name -> (labels -> position), mirrors `order`.
    pos: HashMap<String, HashMap<Labels, usize, LabelsHash>>,
}

impl SeriesIdentity {
    /// Fold one segment's columns in, in its schema order.
    fn extend(&mut self, columns: impl IntoIterator<Item = (String, Labels)>) {
        for (name, labels) in columns {
            let list = self.order.entry(name.clone()).or_default();
            let p = self.pos.entry(name).or_default();
            if !p.contains_key(&labels) {
                p.insert(labels.clone(), list.len());
                list.push(labels);
            }
        }
    }

    /// Ordered distinct label sets for `name` (empty if `name` is unknown).
    fn order(&self, name: &str) -> &[Labels] {
        self.order.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// O(1) position of `labels` within `name`'s series, if known.
    fn position(&self, name: &str, labels: &Labels) -> Option<usize> {
        self.pos.get(name)?.get(labels).copied()
    }

    /// Every name, sorted.
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.order.keys().cloned().collect();
        names.sort();
        names
    }

    /// The label sets of `name` as the listing surface reports them:
    /// sorted, and deduplicated by construction.
    fn labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        let mut sets: Vec<BTreeMap<String, String>> =
            self.order(name).iter().map(|l| l.inner.clone()).collect();
        sets.sort();
        sets
    }
}

/// Per-histogram-metric run assignment, built once at open from field
/// metadata only (via [`MultiParquetSource::histogram_configs`], itself
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
#[derive(Clone, Default)]
struct HistogramRunIndex {
    /// name -> distinct configs, in first-appearance order (index == run).
    runs: HashMap<String, Vec<(u8, u8)>>,
    /// name -> (segment index -> run index).
    segment_run: HashMap<String, HashMap<usize, usize>>,
    /// name -> the label sets seen under each run, for the listing surface
    /// once the footers are gone.
    run_labels: HashMap<String, Vec<RunLabels>>,
}

/// One label set of a histogram, and the run it was seen under.
type RunLabels = (usize, BTreeMap<String, String>);

impl HistogramRunIndex {
    /// Fold one segment's histogram configs in.
    fn observe(&mut self, idx: usize, configs: BTreeMap<String, (u8, u8)>) {
        for (name, config) in configs {
            let list = self.runs.entry(name.clone()).or_default();
            let run = list.iter().position(|c| *c == config).unwrap_or_else(|| {
                list.push(config);
                list.len() - 1
            });
            self.segment_run.entry(name).or_default().insert(idx, run);
        }
    }

    /// Record the label sets a segment carries for its histograms, under the
    /// run that segment belongs to. Called after [`observe`](Self::observe)
    /// for the same segment.
    fn observe_labels(&mut self, idx: usize, columns: &[(String, Labels)]) {
        for (name, labels) in columns {
            let Some(run) = self.segment_run(name, idx) else {
                continue;
            };
            let list = self.run_labels.entry(name.clone()).or_default();
            if !list.iter().any(|(r, l)| *r == run && *l == labels.inner) {
                list.push((run, labels.inner.clone()));
            }
        }
    }

    /// Split, warn, never coerce: log the conflict ONCE per open (not once
    /// per query) for every name that resolved to more than one run, naming
    /// the metric and each run's config so an operator can tell a genuine
    /// agent restart from a misconfigured recorder.
    fn report(&self) {
        for (name, configs) in &self.runs {
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
pub struct SegmentedSource {
    /// Where segment bytes come from when a query needs them.
    store: Arc<dyn SegmentStore>,
    /// Segments opened by queries, bounded by bytes.
    cache: Mutex<SegmentCache>,
    /// Decode cache every opened segment is wired to.
    pool: Arc<BufferPool>,
    /// Time-varying identity for the columns, if the archive has one.
    relabel: Option<Arc<dyn ColumnRelabel>>,
    /// What open built from every segment.
    state: Arc<OpenState>,
    /// What open built from the leading keyed segments, for a reader
    /// reopened over the same table.
    sealed: Option<Sealed>,
}

/// The open state after a table's leading keyed segments.
#[derive(Clone)]
struct Sealed {
    /// The keys of those segments, in store order.
    keys: Vec<u64>,
    /// Whether the state was built with a relabel.
    relabeled: bool,
    state: Arc<OpenState>,
}

/// name -> where each series' samples are.
type CounterColumns = HashMap<String, ColumnTable>;

/// For one metric: the distinct column label sets, and per series position
/// the columns that feed it, in segment order.
#[derive(Clone, Default)]
struct ColumnTable {
    labels: Vec<Labels>,
    labels_index: HashMap<Labels, usize, LabelsHash>,
    by_series: Vec<Vec<Location>>,
}

/// One column of one segment. Sixteen bytes, because a wide table has
/// hundreds of thousands of these and they live for the reader.
#[derive(Clone, Copy, Debug)]
struct Location {
    segment: u32,
    /// Index into [`ColumnTable::labels`].
    column_labels: u32,
    position: crate::ColumnPosition,
}

/// What one reader of a table hands the next: see
/// `metriken_query::SegmentedParquetReader::handover`.
#[derive(Clone)]
pub struct Handover {
    sealed: Sealed,
    /// The pool `segments` were opened on.
    pool: Arc<BufferPool>,
    /// Opened keyed segments, `(index, segment, size)`, least recently used
    /// first.
    segments: Vec<(usize, Arc<MultiParquetSource>, usize)>,
}

impl std::fmt::Debug for Handover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handover")
            .field("keyed_segments", &self.sealed.keys.len())
            .field("open_segments", &self.segments.len())
            .finish()
    }
}

impl Handover {
    /// This handover without the opened segments: the state alone, for a
    /// handover that is kept while no reader of the table is open.
    pub fn without_segments(&self) -> Handover {
        Handover {
            sealed: self.sealed.clone(),
            pool: Arc::clone(&self.pool),
            segments: Vec::new(),
        }
    }

    /// The state after the handing reader's keyed segments, and how many
    /// there are, when they are the first of `keys` and were read with a
    /// relabel exactly when `relabeled` says so. A reader with a relabel
    /// and one without never share state.
    fn sealed_prefix(&self, keys: &[u64], relabeled: bool) -> Option<(Arc<OpenState>, usize)> {
        let sealed = &self.sealed;
        let n = sealed.keys.len();
        (sealed.relabeled == relabeled && keys.len() >= n && keys[..n] == sealed.keys[..])
            .then(|| (Arc::clone(&sealed.state), n))
    }
}

impl SegmentedSource {
    pub fn open(
        store: Arc<dyn SegmentStore>,
        pool: Arc<BufferPool>,
        relabel: Option<Arc<dyn ColumnRelabel>>,
        previous: Option<&Handover>,
        keep: bool,
    ) -> Result<Arc<Self>, Box<dyn Error>> {
        if store.is_empty() {
            return Err("SegmentedParquetReader requires at least one segment".into());
        }

        // The leading segments the store can name, whose bytes do not
        // change while their keys stay the same. The state after them is
        // what a later reopen can reuse.
        let keys: Vec<u64> = (0..store.len()).map_while(|i| store.key(i)).collect();
        let relabeled = relabel.is_some();
        // Only a reader opened with `open_after` saves state or starts from it.
        let reusable = keep && relabel.as_ref().is_none_or(|r| r.identities_are_fixed());
        let reused = previous
            .filter(|_| reusable)
            .and_then(|p| p.sealed_prefix(&keys, relabeled));

        let (state, sealed) = match &reused {
            // Every segment is one the previous reader read: its state is
            // this reader's, unchanged.
            Some((prev, len)) if *len == store.len() => (Arc::clone(prev), Some(Arc::clone(prev))),
            _ => {
                let (mut state, start) = match &reused {
                    Some((prev, len)) => ((**prev).clone(), *len),
                    None => (OpenState::default(), 0),
                };
                // Taken at the boundary between the keyed segments and the
                // rest; when there is no rest, the full state serves as both.
                let mut sealed = match &reused {
                    Some((prev, len)) if *len == keys.len() => Some(Arc::clone(prev)),
                    _ => None,
                };
                let mut memo = IdentityMemo::default();
                for idx in start..store.len() {
                    if reusable && idx == keys.len() && !keys.is_empty() && sealed.is_none() {
                        sealed = Some(Arc::new(state.clone()));
                    }
                    let Some(bytes) = store.bytes(idx).map_err(|e| e.to_string())? else {
                        // Gone between the store's construction and now. The
                        // catalog keeps its place so indices stay positions.
                        state.catalog.push(SegmentCatalog::GONE);
                        continue;
                    };
                    // Footer only: nothing is decoded into the pool here, so
                    // the segment needs no content-derived id.
                    let seg = MultiParquetSource::open_bytes_with_pool(bytes, Arc::clone(&pool))?;
                    state.fold(idx, &seg, relabel.as_ref(), &mut memo)?;
                }
                if state.opened == 0 {
                    return Err(
                        "every segment of the table was gone by the time it was opened".into(),
                    );
                }
                state.histogram_runs.report();
                let state = Arc::new(state);
                if !keys.is_empty() && keys.len() == store.len() {
                    sealed = Some(Arc::clone(&state));
                }
                (state, sealed)
            }
        };
        // A column naming an occupant the relabel did not describe can
        // present differently to a later reader, so nothing is saved.
        let sealed = sealed.filter(|s| reusable && !s.unresolved);

        // The previous reader's opened segments for the shared ones, so a
        // query after a reopen does not fetch and parse them again. Only on
        // the same pool, whose budget they are decoded into.
        let mut cache = SegmentCache::new(pool.max_bytes());
        if let (Some(prev), Some((_, len))) = (previous, &reused) {
            if Arc::ptr_eq(&prev.pool, &pool) {
                for (idx, seg, size) in &prev.segments {
                    if idx < len {
                        cache.insert(*idx, Arc::clone(seg), *size);
                    }
                }
            }
        }

        let source = Arc::new(SegmentedSource {
            store,
            cache: Mutex::new(cache),
            pool,
            relabel,
            state,
            sealed: sealed.map(|state| Sealed {
                keys,
                relabeled,
                state,
            }),
        });
        Ok(source)
    }

    pub fn handover(&self) -> Option<Handover> {
        let sealed = self.sealed.clone()?;
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        // Least recently used first, so a cache filled in this order keeps it.
        let segments = cache
            .entries
            .iter()
            .rev()
            .filter(|(idx, _)| **idx < sealed.keys.len())
            .map(|(idx, (seg, size))| (*idx, Arc::clone(seg), *size))
            .collect();
        Some(Handover {
            sealed,
            pool: Arc::clone(&self.pool),
            segments,
        })
    }

    /// Each segment's catalog span (first and last timestamp), in store
    /// order; `None` for a segment that is gone or has no timestamps.
    pub fn segment_spans(&self) -> Vec<Option<(u64, u64)>> {
        self.state.catalog.iter().map(|c| c.span).collect()
    }

    /// Number of segments backing this source, gone ones included.
    pub fn segment_count(&self) -> usize {
        self.state.catalog.len()
    }

    /// How many segments the cache currently holds open, and their bytes.
    pub fn cached_segments(&self) -> (usize, usize) {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        (cache.entries.len(), cache.bytes)
    }

    /// See [`DataSource::counter_scan`]. `None` when the relabel's
    /// identities are not fixed, when a series has two locations in one
    /// segment, or when nothing matches; the dispatcher then uses the
    /// per-series path.
    fn scan_counters(
        &self,
        name: &str,
        filter: &Labels,
        start: u64,
        end: u64,
    ) -> Option<crate::scan::CounterScan<'_>> {
        use crate::scan::{CounterScan, ScanSeries};
        if !self
            .relabel
            .as_deref()
            .is_none_or(|r| r.identities_are_fixed())
        {
            return None;
        }
        let order = self.state.counter_identity.order(name);
        let table = self.state.counter_columns.get(name)?;

        // The series read, and per segment which column (and occupant, in a
        // long column) each is.
        let mut series: Vec<ScanSeries> = Vec::new();
        let mut plans: BTreeMap<u32, HashMap<u32, ColPlan>> = BTreeMap::new();
        for (pos, l) in order.iter().enumerate() {
            if !l.matches(filter) {
                continue;
            }
            let Some(locations) = table.by_series.get(pos) else {
                continue;
            };
            let s = series.len();
            let windowed = locations
                .first()
                .is_some_and(|l| l.position.begin_col.is_some() && l.position.width_col.is_some());
            let mut segments = std::collections::HashSet::new();
            for l in locations {
                if !self.state.catalog[l.segment as usize].touches(start, end) {
                    continue;
                }
                if !segments.insert(l.segment) {
                    return None;
                }
                let plan = plans
                    .entry(l.segment)
                    .or_default()
                    .entry(l.position.col_idx)
                    .or_insert_with(|| ColPlan {
                        begin: l.position.begin_col,
                        width: l.position.width_col,
                        wide: None,
                        long: HashMap::new(),
                    });
                let duplicate = match l.position.occupant {
                    Some(o) => plan.long.insert(o, s).is_some(),
                    None => plan.wide.replace(s).is_some(),
                };
                if duplicate {
                    return None;
                }
            }
            series.push(ScanSeries {
                labels: l.clone(),
                windowed,
            });
        }
        if series.is_empty() {
            return None;
        }
        let segments: Vec<u32> = plans.keys().copied().collect();
        // The earliest time a sample can have from each segment on; `None`
        // when one of them has no span.
        let mut rest_from: Vec<Option<u64>> = vec![Some(u64::MAX); segments.len() + 1];
        for (i, seg) in segments.iter().enumerate().rev() {
            let span = self.state.catalog[*seg as usize].span.map(|s| s.0);
            rest_from[i] = rest_from[i + 1].zip(span).map(|(a, b)| a.min(b));
        }
        Some(CounterScan::new(
            series,
            Box::new(SegmentedScan {
                source: self,
                plans,
                segments,
                rest_from,
                next: 0,
                start,
                end,
            }),
        ))
    }

    /// The filter a segment is asked with: the query's own, unless a
    /// relabelling has keys the columns do not carry — see
    /// [`ColumnRelabel::segment_filter`].
    fn segment_filter(&self, name: &str, filter: &Labels) -> Labels {
        match &self.relabel {
            Some(r) => r.segment_filter(name, filter),
            None => filter.clone(),
        }
    }

    /// Segment `idx`, opened footer-only — from the cache, or fetched from
    /// the store and cached. `None` when the store no longer has it.
    fn segment(&self, idx: usize) -> Result<Option<Arc<MultiParquetSource>>, Box<dyn Error>> {
        if let Some(seg) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(idx)
        {
            return Ok(Some(seg));
        }
        // Fetched and parsed outside the lock: two queries racing on one
        // segment open it twice, which costs a parse and nothing else.
        let Some(bytes) = self.store.bytes(idx).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let seg = Arc::new(MultiParquetSource::open_content_keyed(
            bytes,
            Arc::clone(&self.pool),
        )?);
        let size = seg.resident_estimate();
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(idx, Arc::clone(&seg), size);
        Ok(Some(seg))
    }

    /// The segments a query over `[start_ns, end_ns]` touches, opened as
    /// the caller reaches them, in order.
    ///
    /// An iterator and not a `Vec` on purpose: a collected list would hold
    /// every touched segment open at once for the whole query, and the cache
    /// bound would then bound nothing — a query over a wide table's full
    /// range would have the whole table resident again, which is what this
    /// store exists to avoid. Opened one at a time, a segment lives for its
    /// own decode and then only as long as the cache keeps it.
    ///
    /// A segment the store has lost is skipped; a read failure is logged and
    /// the segment skipped, since a `DataSource` method has no error to
    /// return and a partial answer is what the caller can use.
    fn touched(
        &self,
        start_ns: u64,
        end_ns: u64,
    ) -> impl Iterator<Item = (usize, Arc<MultiParquetSource>)> + '_ {
        self.state
            .catalog
            .iter()
            .enumerate()
            .filter(move |(_, entry)| entry.touches(start_ns, end_ns))
            .filter_map(move |(idx, _)| match self.segment(idx) {
                Ok(Some(seg)) => Some((idx, seg)),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(segment = idx, "fetching a segment: {e}");
                    None
                }
            })
    }
}

/// Cut one column's samples from one segment into the runs a relabelling
/// says they present as, keeping only the runs the query's filter accepts.
/// Without a relabelling the series is one run, its own labels, already
/// filtered by the segment.
fn relabel_counter(
    relabel: Option<&dyn ColumnRelabel>,
    name: &str,
    filter: &Labels,
    c: Counter,
) -> Vec<Counter> {
    let Some(r) = relabel else {
        return vec![c];
    };
    let Some(runs) = r.split(name, &c.labels, &c.timestamps) else {
        return if c.labels.matches(filter) {
            vec![c]
        } else {
            Vec::new()
        };
    };
    runs.into_iter()
        .filter(|(labels, _)| labels.matches(filter))
        .map(|(labels, range)| Counter {
            labels,
            timestamps: c.timestamps[range.clone()].to_vec(),
            values: c.values[range.clone()].to_vec(),
            windows: c.windows.as_ref().map(|w| w[range].to_vec()),
        })
        .collect()
}

/// Gauge twin of [`relabel_counter`].
fn relabel_gauge(
    relabel: Option<&dyn ColumnRelabel>,
    name: &str,
    filter: &Labels,
    g: Gauge,
) -> Vec<Gauge> {
    let Some(r) = relabel else {
        return vec![g];
    };
    let Some(runs) = r.split(name, &g.labels, &g.timestamps) else {
        return if g.labels.matches(filter) {
            vec![g]
        } else {
            Vec::new()
        };
    };
    runs.into_iter()
        .filter(|(labels, _)| labels.matches(filter))
        .map(|(labels, range)| Gauge {
            labels,
            timestamps: g.timestamps[range.clone()].to_vec(),
            values: g.values[range.clone()].to_vec(),
            windows: g.windows.as_ref().map(|w| w[range].to_vec()),
        })
        .collect()
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
/// `__run__` is therefore a RESERVED label name — an internal label under
/// the `__` rule (`labels::is_internal_label`), like `__name__` — a real user
/// label with that name (e.g. from `record --label __run__=x`) would
/// otherwise be silently overwritten here, which is exactly the
/// silent-coercion class this feature exists to prevent.
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

/// Relabel one segment's histogram stream row by row: each row's series
/// becomes whatever its column presents as at the row's timestamp, and rows
/// the query's filter does not accept are dropped. Without a relabelling
/// the stream passes through.
///
/// Row by row because a histogram stream is one; the runs `split` gives a
/// counter have no equivalent here. An implementation answering `at` for
/// ascending timestamps of one column can keep its place.
fn relabel_histogram_stream(
    relabel: Option<Arc<dyn ColumnRelabel>>,
    name: &str,
    filter: &Labels,
    stream: HistogramStream,
) -> HistogramStream {
    let Some(relabel) = relabel else {
        return stream;
    };
    let name = name.to_string();
    let filter = filter.clone();
    let base = stream.meta.series.clone();
    // The relabelled series list grows as rows arrive; the meta must be
    // complete before the rows are consumed, so it is built from every
    // identity each column can present as, in column order, and rows map
    // into it.
    let mut series: Vec<Labels> = Vec::new();
    let mut position: HashMap<Labels, usize, LabelsHash> = HashMap::default();
    for labels in &base {
        let sets = relabel
            .identities(&name, labels)
            .unwrap_or_else(|| vec![labels.clone()]);
        for l in sets {
            if !position.contains_key(&l) {
                position.insert(l.clone(), series.len());
                series.push(l);
            }
        }
    }
    let rows = stream.rows.filter_map(move |mut row| {
        let column = &base[row.series_idx];
        let labels = relabel
            .at(&name, column, row.timestamp)
            .unwrap_or_else(|| column.clone());
        if !labels.matches(&filter) {
            return None;
        }
        row.series_idx = *position.get(&labels)?;
        Some(row)
    });
    HistogramStream {
        meta: HistogramStreamMeta {
            config: stream.meta.config,
            series,
        },
        rows: Box::new(rows),
    }
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
    fn counter_scan(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<crate::scan::CounterScan<'_>> {
        self.scan_counters(name, filter, start_ns, end_ns)
    }

    fn counters(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Counters> {
        // Slots sized/positioned from the open-time identity index: O(1)
        // per incoming sample instead of the O(series) `Vec` scan splicing
        // used to do (see `SeriesIdentity`). A label-filtered query simply
        // leaves the non-matching slots `None`, dropped by `flatten()`
        // below — same outcome as before, just not by linear search.
        let order = self.state.counter_identity.order(name);
        if order.is_empty() {
            return None;
        }
        let mut slots: Vec<Option<Counter>> = (0..order.len()).map(|_| None).collect();
        let seg_filter = self.segment_filter(name, filter);
        for (_, seg) in self.touched(start_ns, end_ns) {
            let Some(chunk) = seg.counters(name, &seg_filter, start_ns, end_ns) else {
                continue;
            };
            for c in chunk
                .series
                .into_iter()
                .flat_map(|c| relabel_counter(self.relabel.as_deref(), name, filter, c))
            {
                match self.state.counter_identity.position(name, &c.labels) {
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

    /// One stream per series the filter accepts, each reading its own column
    /// out of each touched segment as it is pulled. What `counters` holds
    /// all at once, this holds one segment's worth of one series at a time;
    /// an aggregate pulling every series in lockstep keeps the segment they
    /// are all reading in the cache and the decoded columns in the pool.
    fn counter_streams<'s>(
        &'s self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<crate::CounterStream<'s>>> {
        let order = self.state.counter_identity.order(name);
        if order.is_empty() {
            return None;
        }
        let table = self.state.counter_columns.get(name)?;
        let name: Arc<str> = Arc::from(name);
        // Few series: a long segment may decode only each one's pages. Many:
        // each segment decodes a row group once for all of them.
        let selective = order.iter().filter(|l| l.matches(filter)).count()
            <= crate::parquet::PRUNE_MAX_OCCUPANTS;
        let mut out = Vec::new();
        for (pos, labels) in order.iter().enumerate() {
            if !labels.matches(filter) {
                continue;
            }
            let Some(locations) = table.by_series.get(pos) else {
                continue;
            };
            let windowed = locations
                .first()
                .is_some_and(|l| l.position.begin_col.is_some() && l.position.width_col.is_some());
            let labels = labels.clone();
            let series_labels = labels.clone();
            let name = Arc::clone(&name);
            let samples = locations
                .iter()
                .filter(move |l| self.state.catalog[l.segment as usize].touches(start_ns, end_ns))
                .filter_map(move |l| {
                    let idx = l.segment as usize;
                    let seg = match self.segment(idx) {
                        Ok(Some(seg)) => seg,
                        Ok(None) => return None,
                        Err(e) => {
                            tracing::warn!(segment = idx, "fetching a segment: {e}");
                            return None;
                        }
                    };
                    let chunk = seg.counter_column(&l.position, start_ns, end_ns, selective)?;
                    // A read of one occupant of a long column holds only that
                    // occupant's samples, which present as this series' labels
                    // throughout when the relabel's identities are fixed, so
                    // relabelling it would only rebuild them.
                    if l.position.occupant.is_some()
                        && self
                            .relabel
                            .as_deref()
                            .is_none_or(|r| r.identities_are_fixed())
                    {
                        return Some(vec![chunk.labeled(series_labels.clone())]);
                    }
                    let chunk = chunk.labeled(table.labels[l.column_labels as usize].clone());
                    // A relabelled column carries every occupant's samples;
                    // this stream is one occupant's.
                    let pieces =
                        relabel_counter(self.relabel.as_deref(), &name, &Labels::default(), chunk);
                    let mine: Vec<Counter> = pieces
                        .into_iter()
                        .filter(|c| c.labels == series_labels)
                        .collect();
                    Some(mine)
                })
                .flatten()
                .flat_map(|c| crate::CounterStream::from(c).samples);
            out.push(crate::CounterStream {
                labels,
                windowed,
                samples: Box::new(samples),
            });
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        let order = self.state.gauge_identity.order(name);
        if order.is_empty() {
            return None;
        }
        let mut slots: Vec<Option<Gauge>> = (0..order.len()).map(|_| None).collect();
        let seg_filter = self.segment_filter(name, filter);
        for (_, seg) in self.touched(start_ns, end_ns) {
            let Some(chunk) = seg.gauges(name, &seg_filter, start_ns, end_ns) else {
                continue;
            };
            for g in chunk
                .series
                .into_iter()
                .flat_map(|g| relabel_gauge(self.relabel.as_deref(), name, filter, g))
            {
                match self.state.gauge_identity.position(name, &g.labels) {
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
        if self.state.histogram_runs.run_count(name) <= 1 {
            // No conflict, so the segments' own labels carry no `__run__` and
            // `Labels::matches` would fail closed on one. A dashboard can
            // legitimately pin `__run__="0"` to keep one query working across
            // an A/B pair where only the other side drifted, and run 0 IS this
            // single run — strip it. Any other run genuinely has no data here.
            let mut effective = filter.clone();
            match effective.inner.remove("__run__") {
                None => {}
                Some(v) if v == "0" => {}
                Some(_) => return None,
            }
            let seg_filter = self.segment_filter(name, &effective);
            let streams: Vec<HistogramStream> = self
                .touched(start_ns, end_ns)
                .filter_map(|(_, seg)| seg.histogram_stream(name, &seg_filter, start_ns, end_ns))
                .map(|s| relabel_histogram_stream(self.relabel.clone(), name, &effective, s))
                .collect();
            return splice_histogram_streams(name, &self.state.histogram_identity, streams);
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

        let seg_filter = self.segment_filter(name, &inner_filter);
        let streams: Vec<HistogramStream> = self
            .touched(start_ns, end_ns)
            .filter(|(idx, _)| self.state.histogram_runs.segment_run(name, *idx) == Some(want_run))
            .filter_map(|(_, seg)| seg.histogram_stream(name, &seg_filter, start_ns, end_ns))
            .map(|s| relabel_histogram_stream(self.relabel.clone(), name, &inner_filter, s))
            .collect();

        let spliced = splice_histogram_streams(name, &self.state.histogram_identity, streams)?;
        Some(relabel_with_run(spliced, want_run))
    }

    fn interval(&self) -> f64 {
        self.state
            .catalog
            .iter()
            .filter(|c| c.present)
            .map(|c| c.interval)
            .fold(f64::MAX, f64::min)
    }

    fn time_range(&self) -> Option<(u64, u64)> {
        self.state
            .catalog
            .iter()
            .filter_map(|c| c.span)
            .fold(None, |acc, (lo, hi)| match acc {
                None => Some((lo, hi)),
                Some((alo, ahi)) => Some((alo.min(lo), ahi.max(hi))),
            })
    }

    fn counter_names(&self) -> Vec<String> {
        self.state.counter_identity.names()
    }

    fn gauge_names(&self) -> Vec<String> {
        self.state.gauge_identity.names()
    }

    fn histogram_names(&self) -> Vec<String> {
        self.state.histogram_identity.names()
    }

    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.state.counter_identity.labels(name)
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.state.gauge_identity.labels(name)
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        if self.state.histogram_runs.run_count(name) <= 1 {
            return self.state.histogram_identity.labels(name);
        }
        // Conflict: surface each run's label sets tagged with `__run__` so
        // the split is addressable (`histogram_mean(latency{__run__="1"})`).
        // `__run__` is a RESERVED label name (see `relabel_with_run`) — a
        // real user label with this name would otherwise be silently
        // overwritten below.
        let mut sets: Vec<BTreeMap<String, String>> = Vec::new();
        for (run, labels) in self
            .state
            .histogram_runs
            .run_labels
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let mut labels = labels.clone();
            debug_assert!(
                !labels.contains_key("__run__"),
                "'__run__' is a reserved label name; a real label with this name would be \
                 silently overwritten by the run-disambiguation policy"
            );
            labels.insert("__run__".to_string(), run.to_string());
            sets.push(labels);
        }
        sets.sort();
        sets.dedup();
        sets
    }

    fn file_metadata(&self) -> HashMap<String, String> {
        self.state.file_metadata.clone()
    }

    fn metadata_get(&self, key: &str) -> Option<String> {
        self.state.file_metadata.get(key).cloned()
    }

    fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
        self.state.column_map.clone()
    }

    fn sample_timestamps(&self) -> Vec<u64> {
        // Per-sample timestamps, concatenated in segment order — same splice
        // contract as the query path, no sort/dedup. Touches every segment.
        let mut out = Vec::new();
        for (_, seg) in self.touched(0, u64::MAX) {
            out.extend(seg.sample_timestamps());
        }
        out
    }
}

/// Per segment, how a read column's rows map to series.
struct ColPlan {
    begin: Option<u32>,
    width: Option<u32>,
    wide: Option<usize>,
    long: HashMap<u64, usize>,
}

/// A segmented source's counter scan: the planned segments, read a chunk at
/// a time, one thread per segment where threads exist.
struct SegmentedScan<'a> {
    source: &'a SegmentedSource,
    plans: BTreeMap<u32, HashMap<u32, ColPlan>>,
    segments: Vec<u32>,
    /// `rest_from[i]`: the earliest catalog start of `segments[i..]`;
    /// `None` when one of them has no span.
    rest_from: Vec<Option<u64>>,
    next: usize,
    start: u64,
    end: u64,
}

impl SegmentedScan<'_> {
    /// Segment `seg`'s planned columns. `Ok(None)` for a segment the store
    /// no longer has, as the per-series path skips it.
    fn read(&self, seg: u32) -> Result<Option<crate::scan::ScanSegment>, crate::scan::ScanError> {
        use crate::scan::{RowSeries, ScanColumn, ScanError, ScanSegment};
        let reader = match self.source.segment(seg as usize) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(None),
            Err(e) => {
                tracing::warn!(segment = seg, "fetching a segment: {e}");
                return Err(ScanError);
            }
        };
        let plan = &self.plans[&seg];
        let mut read_cols: Vec<u32> = Vec::new();
        let mut cols: Vec<usize> = Vec::new();
        for (col, p) in plan {
            read_cols.push(*col);
            cols.push(*col as usize);
            cols.extend(p.begin.map(|c| c as usize));
            cols.extend(p.width.map(|c| c as usize));
        }
        let Some(columns) = reader.batch_columns(&cols, self.start, self.end) else {
            return Err(ScanError);
        };
        let series = columns
            .batches
            .iter()
            .map(|batch| {
                let occupant = columns.occupant.and_then(|c| columns.u64s(batch, c));
                read_cols
                    .iter()
                    .map(|col| {
                        let p = &plan[col];
                        match (p.wide, occupant) {
                            (Some(s), _) if p.long.is_empty() => RowSeries::One(s as u32),
                            (_, Some(occ)) => RowSeries::PerRow(
                                (0..batch.num_rows())
                                    .map(|r| {
                                        (!arrow::array::Array::is_null(occ, r))
                                            .then(|| p.long.get(&occ.value(r)))
                                            .flatten()
                                            .map_or(u32::MAX, |s| *s as u32)
                                    })
                                    .collect(),
                            ),
                            _ => RowSeries::PerRow(Vec::new()),
                        }
                    })
                    .collect()
            })
            .collect();
        let cols = read_cols
            .iter()
            .map(|col| {
                let p = &plan[col];
                ScanColumn {
                    values: *col as usize,
                    begin: p.begin.map(|c| c as usize),
                    width: p.width.map(|c| c as usize),
                }
            })
            .collect();
        Ok(Some(ScanSegment {
            columns,
            cols,
            series,
        }))
    }
}

impl crate::scan::ChunkReader for SegmentedScan<'_> {
    fn next_chunk(
        &mut self,
        n: usize,
    ) -> Result<Option<crate::scan::ScanChunk>, crate::scan::ScanError> {
        if self.next >= self.segments.len() {
            return Ok(None);
        }
        let to = (self.next + n.max(1)).min(self.segments.len());
        let chunk = &self.segments[self.next..to];
        let this = &*self;
        let segments = read_all(chunk, &|seg| this.read(seg))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        self.next = to;
        let rest_start = if to == self.segments.len() {
            None
        } else {
            self.rest_from[to]
        };
        Ok(Some(crate::scan::ScanChunk {
            segments,
            rest_start,
        }))
    }
}

/// Read `segments`, in parallel where threads exist, in their order. A
/// panic in a read is raised again here.
fn read_all<T, F>(segments: &[u32], read: &F) -> Vec<T>
where
    T: Send,
    F: Fn(u32) -> T + Sync,
{
    #[cfg(target_arch = "wasm32")]
    {
        segments.iter().map(|s| read(*s)).collect()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if segments.len() < 2 {
            return segments.iter().map(|s| read(*s)).collect();
        }
        std::thread::scope(|scope| {
            let handles: Vec<_> = segments
                .iter()
                .map(|s| scope.spawn(move || read(*s)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
                .collect()
        })
    }
}

#[cfg(test)]
mod internal_tests {
    use std::sync::Arc;

    use arrow::array::{ArrayRef, UInt64Array};

    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;

    use super::*;
    use crate::parquet::MultiParquetSource;

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
    fn histogram_run_index_flags_cross_segment_config_drift() {
        // Direct unit test of the split-detection step `histogram_power_drift_splits_series`
        // exercises end-to-end: two segments with different powers for the
        // same name must resolve to two runs, each segment assigned to its
        // own run in first-appearance order. This is also where
        // `HistogramRunIndex::report` logs the "splitting into __run__
        // series" warning (see its doc comment) — once per open, not once
        // per query.
        let n2 = ::histogram::Config::new(2, 8).unwrap().total_buckets();
        let n3 = ::histogram::Config::new(3, 8).unwrap().total_buckets();
        let seg_a = MultiParquetSource::open_bytes_with_pool(
            segment_histogram("latency", 2, 8, &[(1_000_000_000, vec![0u64; n2])]),
            BufferPool::new(64 * 1024 * 1024),
        )
        .unwrap();
        let seg_b = MultiParquetSource::open_bytes_with_pool(
            segment_histogram("latency", 3, 8, &[(2_000_000_000, vec![0u64; n3])]),
            BufferPool::new(64 * 1024 * 1024),
        )
        .unwrap();

        let mut index = HistogramRunIndex::default();
        for (idx, seg) in [seg_a, seg_b].iter().enumerate() {
            index.observe(idx, seg.histogram_configs());
        }
        index.report();
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
}
