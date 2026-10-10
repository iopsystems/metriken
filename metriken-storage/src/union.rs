//! `metriken_query::UnionMetricsSource`: presents several readers with DISJOINT
//! metric-name sets as one logical `metriken_query::MetricsSource` — e.g. one sampler's
//! several acquisition-group tables, each holding a different subset of
//! that sampler's metrics (rezolus's `.rez` V3 container tables
//! `<sampler>/<group>`, not `<sampler>`, once a sampler is split).
//!
//! This is the opposite problem from `metriken_query::ParquetBuilder`'s multi-file
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
//! **Identity must be disjoint across children.** Which readers to compose
//! is a decision the CALLER makes (e.g. rezolus grouping a sampler's group
//! tables) — but which children end up in that decision can still trace
//! back to untrusted wire bytes (rezolus derives its composition set from
//! archive table schemas), so a name present in more than one child can be
//! a real producer/archive bug reaching this type, not merely a
//! caller-code bug. Two constructors, two policies:
//! `metriken_query::UnionMetricsSource::new` trusts the caller and silently keeps the
//! first child's series on a collision (see `build_index`; deterministic,
//! never a panic); `metriken_query::UnionMetricsSource::try_new` checks first and returns
//! [`UnionError::NonDisjoint`] instead of building anything. Prefer
//! `try_new` whenever the composition set isn't hand-picked by code that
//! can vouch for disjointness itself.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;

use crate::types::{Counters, Gauges};
use crate::util::union_names;
use crate::DataSource;

/// One child of a `metriken_query::UnionMetricsSource`.
///
/// Only the two concrete reader types this crate provides are accepted —
/// `MetricsSource` alone isn't enough, since composing at the union layer
/// needs each child's raw [`DataSource`] handle, which is deliberately not
/// part of the public `MetricsSource` surface (see
/// `metriken_query::ParquetReader::data_source`/`metriken_query::SegmentedParquetReader::data_source`).
/// Built via `From<&ParquetReader>`/`From<&SegmentedParquetReader>`, which
/// borrow — an `Arc` clone under the hood — rather than consume, so the
/// original reader stays usable on its own after contributing to a union.
pub struct UnionChild(Arc<dyn DataSource>);

impl UnionChild {
    /// A child over `source`.
    pub fn from_source(source: Arc<dyn DataSource>) -> Self {
        UnionChild(source)
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

/// The dispatching [`DataSource`] the PromQL engine evaluates over: a
/// per-metric-name lookup into whichever child owns it. See the module docs
/// for the full contract.
pub struct UnionSource {
    children: Vec<Arc<dyn DataSource>>,
    counter_index: Index,
    gauge_index: Index,
    histogram_index: Index,
}

impl UnionSource {
    /// A union over `children`, trusting the caller's partitioning; see
    /// `metriken_query::UnionMetricsSource::new`.
    pub fn new(children: Vec<UnionChild>) -> Self {
        Self::from_sources(children.into_iter().map(|c| c.0).collect())
    }

    /// A union over `children`, rejecting empty input or a metric name
    /// (within one kind) present in more than one child; see
    /// `metriken_query::UnionMetricsSource::try_new`.
    pub fn try_new(children: Vec<UnionChild>) -> Result<Self, UnionError> {
        if children.is_empty() {
            return Err(UnionError::Empty);
        }
        let raw: Vec<Arc<dyn DataSource>> = children.into_iter().map(|c| c.0).collect();
        let duplicates = duplicate_names(&raw);
        if !duplicates.is_empty() {
            return Err(UnionError::NonDisjoint {
                duplicates: duplicates.into_iter().collect(),
            });
        }
        Ok(Self::from_sources(raw))
    }

    fn from_sources(children: Vec<Arc<dyn DataSource>>) -> Self {
        debug_assert!(
            !children.is_empty(),
            "UnionSource requires at least one child — an empty union has no \
             sensible interval() (see interval()'s empty-input guard) and \
             nothing to dispatch to"
        );
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
    ) -> Option<Counters> {
        let i = *self.counter_index.get(name)?;
        self.children[i].counters(name, filter, start_ns, end_ns)
    }

    fn counter_streams<'s>(
        &'s self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<crate::CounterStream<'s>>> {
        let i = *self.counter_index.get(name)?;
        self.children[i].counter_streams(name, filter, start_ns, end_ns)
    }

    fn counter_scan(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<crate::scan::CounterScan<'_>> {
        let i = *self.counter_index.get(name)?;
        self.children[i].counter_scan(name, filter, start_ns, end_ns)
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        let i = *self.gauge_index.get(name)?;
        self.children[i].gauges(name, filter, start_ns, end_ns)
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
    /// [`crate::segmented`]'s `SegmentedSource::interval()` uses: `rate()`'s
    /// default grid step should track the fastest-ticking child, not be
    /// dragged wide by a lagging one.
    ///
    /// What this does NOT do today: `interval()` never measures actual row
    /// spacing. Each child's `interval()` is `MultiParquetSource::interval`
    /// reading the `sampling_interval_ms` FOOTER key-value (defaulting to
    /// 1000ms when absent) — and a `.rez` table's parquet carries no
    /// file-level KV metadata at all (recording metadata lives in the
    /// manifest JSON), so every child of a rezolus union reports the same
    /// 1.0s default and this `min` is a no-op in practice. The policy is
    /// still the right one to keep — a child that skipped ticks (a group's
    /// window-advance dedup) genuinely does tick slower than a sibling that
    /// didn't, and `min` is the correct response once intervals are real —
    /// but that genuinely-different-cadence case can't be exercised by a
    /// test until `.rez` stamps a per-table interval instead of relying on
    /// the shared default.
    fn interval(&self) -> f64 {
        // `f64::INFINITY` (not `f64::MAX`) as the fold seed: an empty
        // `children` must not silently report `f64::MAX` as "the interval",
        // and `is_finite()` below only rejects the seed value, not a real
        // one — `new()` already debug_asserts against empty, this is the
        // release-mode fallback for the same case.
        let finest = self
            .children
            .iter()
            .map(|c| c.interval())
            .fold(f64::INFINITY, f64::min);
        if finest.is_finite() {
            finest
        } else {
            1.0
        }
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

/// Error from `metriken_query::UnionMetricsSource::try_new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnionError {
    /// `children` was empty — there is nothing to dispatch to.
    Empty,
    /// The same metric name (within one kind — counter, gauge, or
    /// histogram) is present in more than one child. Disjoint identity
    /// across children is a construction contract (see the module docs);
    /// `metriken_query::UnionMetricsSource::new` tolerates a violation by silently
    /// keeping the first child's series (see `build_index`),
    /// `metriken_query::UnionMetricsSource::try_new` instead reports it so a
    /// producer/archive bug that broke the caller's partitioning becomes a
    /// loud error rather than a silently wrong answer.
    NonDisjoint {
        /// Every duplicated name, sorted and deduplicated (a name counted
        /// once here even if it collides across more than two children, or
        /// under more than one metric kind).
        duplicates: Vec<String>,
    },
}

impl std::fmt::Display for UnionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnionError::Empty => {
                write!(f, "UnionMetricsSource::try_new requires at least one child")
            }
            UnionError::NonDisjoint { duplicates } => write!(
                f,
                "union children are not disjoint: {} metric name(s) present in more than \
                 one child: {}",
                duplicates.len(),
                duplicates.join(", ")
            ),
        }
    }
}

impl std::error::Error for UnionError {}

/// Every name present in more than one child, across all three metric
/// kinds — the check `metriken_query::UnionMetricsSource::try_new` runs before trusting
/// `build_index`'s silent first-wins tie-break. Footer-only (each kind's
/// `*_names()` is already-open-time metadata), so this costs nothing more
/// than building the index itself would.
fn duplicate_names(children: &[Arc<dyn DataSource>]) -> BTreeSet<String> {
    let mut duplicates = BTreeSet::new();
    for names in [
        DataSource::counter_names,
        DataSource::gauge_names,
        DataSource::histogram_names,
    ] {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for child in children {
            for name in names(child.as_ref()) {
                if !seen.insert(name.clone()) {
                    duplicates.insert(name);
                }
            }
        }
    }
    duplicates
}
