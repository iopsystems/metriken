//! [`UnionSource`]: several sources with disjoint series as one, such as one
//! sampler's acquisition-group tables, each holding a different subset of
//! the sampler's metrics. A per-name read goes to the child that owns the
//! name, or to every child holding series of it (see below).
//! `metriken_query::UnionMetricsSource` queries one.
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
//! in more than one. [`UnionSource::try_new`] accepts a name present in
//! several children whose series of it carry different label sets (one
//! metric family read in two acquisition groups, each over its own
//! population), and a read of that name returns every such child's series,
//! as [`RecordingsSource`] does; a
//! histogram held by several children is pooled on the row times of the one
//! with the longest interval, as there. It returns
//! [`UnionError::NonDisjoint`] when one label set of a name is in more than
//! one child, or a child holding a name lists no label sets for it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::recordings::RecordingsSource;

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

/// `name -> index into owners that answers it`.
type Index = HashMap<String, usize>;

/// One metric kind's names, each with the children holding it in
/// `children` order, from each child's own name list
/// (`counter_names()`/`gauge_names()`/`histogram_names()` — footer-only,
/// already-open-time metadata, so this is cheap: no row-group decode).
fn build_index(
    children: &[Arc<dyn DataSource>],
    names: &dyn Fn(&Arc<dyn DataSource>) -> Vec<String>,
) -> HashMap<String, Vec<usize>> {
    let mut index: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, child) in children.iter().enumerate() {
        for name in names(child) {
            let holders = index.entry(name).or_default();
            if holders.last() != Some(&i) {
                holders.push(i);
            }
        }
    }
    index
}

/// Several sources with disjoint series as one: a per-name read goes to the
/// child holding the name, or to every child holding series of it. See the
/// module docs.
pub struct UnionSource {
    children: Vec<Arc<dyn DataSource>>,
    /// What each index entry points into: `children`, then one
    /// [`RecordingsSource`] per set of children that share a name.
    owners: Vec<Arc<dyn DataSource>>,
    counter_index: Index,
    gauge_index: Index,
    histogram_index: Index,
    gauge_histogram_index: Index,
}

impl UnionSource {
    /// A union over `children`, trusting the caller's partitioning: a name
    /// present in more than one child keeps its first child's series.
    pub fn new(children: Vec<UnionChild>) -> Self {
        Self::from_sources(children.into_iter().map(|c| c.0).collect(), false)
    }

    /// A union over `children`, rejecting empty input, a series (a metric
    /// name, within one kind, with one label set) present in more than one
    /// child, or a name held by several children one of which lists no
    /// label sets for it. A name whose children hold different series of it
    /// reads every one of them.
    pub fn try_new(children: Vec<UnionChild>) -> Result<Self, UnionError> {
        if children.is_empty() {
            return Err(UnionError::Empty);
        }
        let raw: Vec<Arc<dyn DataSource>> = children.into_iter().map(|c| c.0).collect();
        let duplicates = duplicate_series(&raw);
        if !duplicates.is_empty() {
            return Err(UnionError::NonDisjoint {
                duplicates: duplicates.into_iter().collect(),
            });
        }
        Ok(Self::from_sources(raw, true))
    }

    /// `join`: a name in several children is read from all of them (for
    /// `try_new`, which checked their series are disjoint); otherwise from
    /// the first.
    fn from_sources(children: Vec<Arc<dyn DataSource>>, join: bool) -> Self {
        debug_assert!(
            !children.is_empty(),
            "UnionSource requires at least one child — an empty union has no \
             sensible interval() (see interval()'s empty-input guard) and \
             nothing to dispatch to"
        );
        let mut owners = children.clone();
        let mut joined: HashMap<Vec<usize>, usize> = HashMap::new();
        let mut index = |names: &dyn Fn(&Arc<dyn DataSource>) -> Vec<String>| {
            build_index(&children, names)
                .into_iter()
                .map(|(name, holders)| {
                    let i = match holders.as_slice() {
                        [only] => *only,
                        [first, ..] if !join => *first,
                        _ => *joined.entry(holders.clone()).or_insert_with(|| {
                            let set = holders.iter().map(|&h| Arc::clone(&children[h])).collect();
                            owners.push(Arc::new(RecordingsSource::new(set)));
                            owners.len() - 1
                        }),
                    };
                    (name, i)
                })
                .collect::<Index>()
        };
        let counter_index = index(&|c| c.counter_names());
        let gauge_index = index(&|c| c.gauge_names());
        let histogram_index = index(&|c| c.histogram_names());
        let gauge_histogram_index = index(&|c| c.gauge_histogram_names());
        Self {
            children,
            owners,
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
        self.owners[i].counters(name, filter, start_ns, end_ns)
    }

    fn counter_streams<'s>(
        &'s self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<crate::CounterStream<'s>>> {
        let i = *self.counter_index.get(name)?;
        self.owners[i].counter_streams(name, filter, start_ns, end_ns)
    }

    fn counter_scan(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<crate::scan::CounterScan<'_>> {
        let i = *self.counter_index.get(name)?;
        self.owners[i].counter_scan(name, filter, start_ns, end_ns)
    }

    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges> {
        let i = *self.gauge_index.get(name)?;
        self.owners[i].gauges(name, filter, start_ns, end_ns)
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
        self.owners[i].gauge_parts(name, filter, start_ns, end_ns, step_ns)
    }

    fn histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let i = *self.histogram_index.get(name)?;
        self.owners[i].histogram_stream(name, filter, start_ns, end_ns)
    }

    fn gauge_histogram_stream(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<HistogramStream> {
        let i = *self.gauge_histogram_index.get(name)?;
        self.owners[i].gauge_histogram_stream(name, filter, start_ns, end_ns)
    }

    fn gauge_histogram_parts(
        &self,
        name: &str,
        filter: &Labels,
        start_ns: u64,
        end_ns: u64,
    ) -> Option<Vec<(HistogramStream, f64)>> {
        let i = *self.gauge_histogram_index.get(name)?;
        self.owners[i].gauge_histogram_parts(name, filter, start_ns, end_ns)
    }

    /// The holding table's interval.
    fn gauge_histogram_interval(&self, name: &str) -> f64 {
        match self.gauge_histogram_index.get(name) {
            Some(i) => self.owners[*i].gauge_histogram_interval(name),
            None => self.interval(),
        }
    }

    /// The holding table's interval.
    fn histogram_interval(&self, name: &str) -> f64 {
        match self.histogram_index.get(name) {
            Some(i) => self.owners[*i].histogram_interval(name),
            None => self.interval(),
        }
    }

    /// The holding table's interval.
    fn counter_interval(&self, name: &str) -> f64 {
        match self.counter_index.get(name) {
            Some(i) => self.owners[*i].counter_interval(name),
            None => self.interval(),
        }
    }

    /// The holding table's interval.
    fn gauge_interval(&self, name: &str) -> f64 {
        match self.gauge_index.get(name) {
            Some(i) => self.owners[*i].gauge_interval(name),
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

    /// The children's sample timestamps, ascending, each instant once.
    fn sample_timestamps(&self) -> Vec<u64> {
        let mut out: Vec<u64> = Vec::new();
        for c in &self.children {
            out.extend(c.sample_timestamps());
        }
        out.sort_unstable();
        out.dedup();
        out
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
            Some(&i) => self.owners[i].counter_labels(name),
            None => Vec::new(),
        }
    }

    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        match self.gauge_index.get(name) {
            Some(&i) => self.owners[i].gauge_labels(name),
            None => Vec::new(),
        }
    }

    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        match self.histogram_index.get(name) {
            Some(&i) => self.owners[i].histogram_labels(name),
            None => Vec::new(),
        }
    }

    fn gauge_histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        match self.gauge_histogram_index.get(name) {
            Some(&i) => self.owners[i].gauge_histogram_labels(name),
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
    /// One series (a metric name within one kind — counter, gauge, or
    /// histogram — with one label set) is present in more than one child,
    /// or a child holding a name that another child also holds lists no
    /// label sets for it.
    /// Disjoint identity across children is a construction contract (see
    /// the module docs): [`UnionSource::new`] keeps the first child's
    /// series of the name, and [`UnionSource::try_new`] reports the names
    /// instead.
    NonDisjoint {
        /// Every name with a series in more than one child, sorted and
        /// deduplicated (a name counted once here even if it collides across
        /// more than two children, or under more than one metric kind).
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
                "union children are not disjoint: {} metric name(s) with a series present \
                 in more than one child: {}",
                duplicates.len(),
                duplicates.join(", ")
            ),
        }
    }
}

impl std::error::Error for UnionError {}

/// Every name with one label set in more than one child, across every
/// metric kind — the check `metriken_query::UnionMetricsSource::try_new`
/// runs before trusting the index. A name in one child costs a footer-only
/// name listing; only a name in several children has its label sets
/// listed.
fn duplicate_series(children: &[Arc<dyn DataSource>]) -> BTreeSet<String> {
    type Names = fn(&dyn DataSource) -> Vec<String>;
    type Listing = fn(&dyn DataSource, &str) -> Vec<BTreeMap<String, String>>;
    let kinds: [(Names, Listing); 4] = [
        (|c| c.counter_names(), |c, n| c.counter_labels(n)),
        (|c| c.gauge_names(), |c, n| c.gauge_labels(n)),
        (|c| c.histogram_names(), |c, n| c.histogram_labels(n)),
        (
            |c| c.gauge_histogram_names(),
            |c, n| c.gauge_histogram_labels(n),
        ),
    ];
    let mut duplicates = BTreeSet::new();
    for (names, labels) in kinds {
        let mut holders: HashMap<String, Vec<&dyn DataSource>> = HashMap::new();
        for child in children {
            for name in names(child.as_ref()) {
                holders.entry(name).or_default().push(child.as_ref());
            }
        }
        for (name, held) in holders {
            if held.len() < 2 {
                continue;
            }
            let mut seen: BTreeSet<BTreeMap<String, String>> = BTreeSet::new();
            let clash = held.iter().any(|child| {
                let own: BTreeSet<_> = labels(*child, &name).into_iter().collect();
                // A child that lists no series of a name it holds says
                // nothing about which series they are, so it cannot be
                // shown disjoint.
                own.is_empty() || own.into_iter().any(|l| !seen.insert(l))
            });
            if clash {
                duplicates.insert(name);
            }
        }
    }
    duplicates
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source holding counter `c`, whose listing of `c`'s series is
    /// `series`. It answers no reads.
    struct Listed {
        series: Vec<BTreeMap<String, String>>,
    }

    impl DataSource for Listed {
        fn counters(&self, _: &str, _: &Labels, _: u64, _: u64) -> Option<Counters> {
            None
        }
        fn gauges(&self, _: &str, _: &Labels, _: u64, _: u64) -> Option<Gauges> {
            None
        }
        fn histogram_stream(&self, _: &str, _: &Labels, _: u64, _: u64) -> Option<HistogramStream> {
            None
        }
        fn interval(&self) -> f64 {
            1.0
        }
        fn time_range(&self) -> Option<(u64, u64)> {
            None
        }
        fn counter_names(&self) -> Vec<String> {
            vec!["c".into()]
        }
        fn gauge_names(&self) -> Vec<String> {
            vec![]
        }
        fn histogram_names(&self) -> Vec<String> {
            vec![]
        }
        fn counter_labels(&self, _: &str) -> Vec<BTreeMap<String, String>> {
            self.series.clone()
        }
        fn gauge_labels(&self, _: &str) -> Vec<BTreeMap<String, String>> {
            vec![]
        }
        fn histogram_labels(&self, _: &str) -> Vec<BTreeMap<String, String>> {
            vec![]
        }
        fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
            HashMap::new()
        }
    }

    fn child(series: &[&[(&str, &str)]]) -> UnionChild {
        let series = series
            .iter()
            .map(|l| {
                l.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            })
            .collect();
        UnionChild::from_source(Arc::new(Listed { series }))
    }

    #[test]
    fn different_series_of_one_name_are_disjoint() {
        let union = UnionSource::try_new(vec![child(&[&[("k", "a")]]), child(&[&[("k", "b")]])])
            .expect("different label sets of one name are disjoint");
        assert_eq!(union.counter_labels("c").len(), 2);
    }

    #[test]
    fn a_child_that_lists_no_series_of_a_name_it_holds_is_not_disjoint() {
        let Err(err) = UnionSource::try_new(vec![child(&[&[("k", "a")]]), child(&[])]) else {
            panic!("a name with no listed series cannot be shown disjoint");
        };
        assert_eq!(
            err,
            UnionError::NonDisjoint {
                duplicates: vec!["c".into()]
            }
        );
    }

    #[test]
    fn one_series_in_two_children_is_not_disjoint() {
        let Err(err) = UnionSource::try_new(vec![
            child(&[&[("k", "a")]]),
            child(&[&[("k", "b")], &[("k", "a")]]),
        ]) else {
            panic!("one label set in two children is a duplicate series");
        };
        assert_eq!(
            err,
            UnionError::NonDisjoint {
                duplicates: vec!["c".into()]
            }
        );
    }
}
