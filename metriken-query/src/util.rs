//! Small helpers shared by more than one [`crate::DataSource`] composition
//! (currently [`crate::segmented`] and [`crate::union`]).

/// Sorted, deduplicated union of metric names across several sources'
/// already-computed name lists. Used by both a splicing composition
/// (`SegmentedSource`, several segments of ONE table) and a dispatching one
/// (`UnionSource`, several tables of disjoint identity) — the set-union
/// itself is identical either way, only what the caller does with the
/// index differs.
pub(crate) fn union_names<I: IntoIterator<Item = Vec<String>>>(lists: I) -> Vec<String> {
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for list in lists {
        names.extend(list);
    }
    names.into_iter().collect()
}
