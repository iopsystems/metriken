//! [`UnionSource`]: several sources with disjoint metric-name sets as one,
//! such as one sampler's acquisition-group tables, each holding a different
//! subset of the sampler's metrics. A per-name read goes to the one child
//! that owns the name. `metriken_query::UnionMetricsSource` queries one.
//!
//! This is the opposite of [`MultiParquetSource`](crate::parquet::MultiParquetSource),
//! which composes the same identities from several files and keeps each
//! file's series apart.
//!
//! There is no timestamp splicing or join. Each child keeps its own
//! samples, windows and cadence; the engine aligns series from different
//! children on its evaluation grid, and each metric's acquisition window
//! comes from its own child.
//!
//! Identity must be disjoint across children. The composition set can be
//! derived from untrusted archive bytes, so the two constructors differ:
//! [`UnionSource::new`] keeps the first child's series for a name present
//! in more than one; [`UnionSource::try_new`] returns
//! [`UnionError::NonDisjoint`] instead.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;

use crate::types::{Counters, Gauges};
use crate::util::union_names;
use crate::DataSource;

/// One child of a [`UnionSource`] (and of `metriken_query::UnionMetricsSource`).
///
/// `metriken-query` builds one from its readers (`From<&ParquetReader>`,
/// `From<&SegmentedParquetReader>`, `From<&MemoryStore>`), which borrow an
/// `Arc` rather than consume the reader; [`from_source`](Self::from_source)
/// takes any source.
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

/// Several sources with disjoint metric names as one: each per-name read goes
/// to the child that owns the name. See the module docs.
pub struct UnionSource {
    children: Vec<Arc<dyn DataSource>>,
    counter_index: Index,
    gauge_index: Index,
    histogram_index: Index,
    gauge_histogram_index: Index,
}

impl UnionSource {
    /// A union over `children`, trusting the caller's partitioning: a name
    /// present in more than one child keeps its first child's series.
    pub fn new(children: Vec<UnionChild>) -> Self {
        Self::from_sources(children.into_iter().map(|c| c.0).collect())
    }

    /// A union over `children`, rejecting empty input or a metric name
    /// (within one kind) present in more than one child.
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
        let gauge_histogram_index = build_index(&children, |c| c.gauge_histogram_names());
        Self {
            children,
            counter_index,
            gauge_index,
            histogram_index,
            gauge_histogram_index,
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

    /// The parts of the table holding `name`, at that table's interval.
    fn gauge_parts(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
        step_ns: u64,
    ) -> Option<Vec<(Gauges, f64)>> {
        let i = *self.gauge_index.get(name)?;
        self.children[i].gauge_parts(name, filter, start_ns, end_ns, step_ns)
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

    fn gauge_histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let i = *self.gauge_histogram_index.get(name)?;
        self.children[i].gauge_histogram_stream(name, filter, start_ns, end_ns)
    }

    fn gauge_histogram_parts(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<(HistogramStream, f64)>> {
        let i = *self.gauge_histogram_index.get(name)?;
        self.children[i].gauge_histogram_parts(name, filter, start_ns, end_ns)
    }

    /// The holding table's interval.
    fn gauge_histogram_interval(&self, name: &str) -> f64 {
        match self.gauge_histogram_index.get(name) {
            Some(i) => self.children[*i].gauge_histogram_interval(name),
            None => self.interval(),
        }
    }

    /// The holding table's interval.
    fn histogram_interval(&self, name: &str) -> f64 {
        match self.histogram_index.get(name) {
            Some(i) => self.children[*i].histogram_interval(name),
            None => self.interval(),
        }
    }

    /// The holding table's interval.
    fn counter_interval(&self, name: &str) -> f64 {
        match self.counter_index.get(name) {
            Some(i) => self.children[*i].counter_interval(name),
            None => self.interval(),
        }
    }

    /// The holding table's interval.
    fn gauge_interval(&self, name: &str) -> f64 {
        match self.gauge_index.get(name) {
            Some(i) => self.children[*i].gauge_interval(name),
            None => self.interval(),
        }
    }

    /// The finest (minimum) interval across children: the default step
    /// tracks the fastest-ticking child.
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

    fn gauge_histogram_names(&self) -> Vec<String> {
        union_names(self.children.iter().map(|c| c.gauge_histogram_names()))
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

    fn gauge_histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        match self.gauge_histogram_index.get(name) {
            Some(&i) => self.children[i].gauge_histogram_labels(name),
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

/// Error from [`UnionSource::try_new`] and `metriken_query::UnionMetricsSource::try_new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnionError {
    /// `children` was empty — there is nothing to dispatch to.
    Empty,
    /// The same metric name (within one kind — counter, gauge, or
    /// histogram) is present in more than one child. Disjoint identity
    /// across children is a construction contract (see the module docs):
    /// [`UnionSource::new`] keeps the first child's series, and
    /// [`UnionSource::try_new`] reports the names instead.
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
        DataSource::gauge_histogram_names,
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
