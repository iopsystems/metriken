//! Process groups: which sources of an archive recorded one producer
//! process, so that a query can read that process once.
//!
//! The rules are the data model and P6 of
//! `docs/journal/2026-10-19-recording-identity-and-views.md`:
//!
//! 1. Sources sharing a uuid are copies of one source and are taken as one:
//!    a [`UuidGroup`].
//! 2. Bridges are set aside. A bridge is a uuid group that overlaps two other
//!    uuid groups which do not overlap each other, and whose span covers the
//!    gap between them (a continuous recording beside one recording per
//!    day). Two uuid groups that share a producer epoch overlap when they
//!    overlap within it (rule 3); other pairs overlap when their spans
//!    intersect, from the first row of any member to the last. When any
//!    source carries `arm`, an armed uuid group bridges groups of its own
//!    arm only, and an unarmed one (shared context) bridges armed groups,
//!    so it joins no two of them that do not overlap each other (owner
//!    decision on Q39); a uuid group carrying two arms is never a bridge.
//! 3. The other uuid groups join one [`ProcessGroup`] when they share a
//!    producer epoch and the intersection of their spans in that epoch holds
//!    at least one row of each (R3); the relation is closed transitively.
//!    Groups are formed regardless of the producer's kind, and the key is
//!    the epoch alone (Q9's first candidate, until a producer identity
//!    exists): two in-process sources of one process, which share its
//!    epoch, join.
//!
//! What happens to a bridge afterwards (slicing it at the clusters the
//! presented recordings form, and joining its pieces) depends on
//! presentation, so it is left to the caller: [`ProcessGroups::bridges`]
//! lists them with the pairs they bridge.
//!
//! [`read_groups`] turns the result into what a query reads as one
//! recording: each process group, or each part of one whose sources carry
//! different values of a label. `ArchiveReader::series_labels` labels a read
//! group's series with the group's labels, so the sources of one group are
//! not told apart by `__recording__`; metriken-query evaluates each source
//! of a group alone and combines the results per series (S9).
//!
//! [`merge_metadata`] is a read group's metadata, from its sources'. A tool
//! that shows one recording per read group opens them with
//! `ArchiveReader::open_read_groups` (or `flatten_read_groups` over readers
//! it already has), whose readers carry it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The label a comparison's arms are named by. When any source carries it,
/// bridges are found within each arm.
pub const ARM_LABEL: &str = "arm";

/// The label naming the machine a source was recorded on.
pub const HOST_LABEL: &str = "host";

/// One producer epoch's rows within one source: from its first row to its
/// last, in nanoseconds, both inclusive, as far as the source's metadata and
/// span say.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EpochSpan {
    /// The epoch id (`producer_epoch`).
    pub epoch: String,
    pub first_ns: u64,
    pub last_ns: u64,
}

/// What the grouping knows about one source.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SourceInfo {
    /// The source's labels, as the catalog holds them.
    pub labels: BTreeMap<String, String>,
    /// The source's position among its archive's catalog sources, in id
    /// order.
    pub catalog_index: usize,
    /// The source's uuid, kept by every dendro copy; `None` where the
    /// container has none (a `.rez`, a source built in memory).
    pub uuid: Option<String>,
    /// The first and last row of any of the source's tables, in
    /// nanoseconds; `None` for a source with no rows.
    pub span_ns: Option<(u64, u64)>,
    /// The producer epochs the source holds rows of, oldest first, each
    /// clipped to `span_ns`. Empty for a source with no epoch.
    pub epochs: Vec<EpochSpan>,
}

impl SourceInfo {
    /// A source's facts, its epochs read from `metadata` as
    /// [`epoch_spans`] reads them and clipped to `span_ns`.
    pub fn new(
        labels: BTreeMap<String, String>,
        catalog_index: usize,
        uuid: Option<String>,
        span_ns: Option<(u64, u64)>,
        metadata: &BTreeMap<String, String>,
    ) -> Self {
        let epochs = epoch_spans(metadata, span_ns);
        Self {
            labels,
            catalog_index,
            uuid,
            span_ns,
            epochs,
        }
    }
}

/// The metadata key holding the current producer epoch (dendro's
/// `keys::PRODUCER_EPOCH`).
pub const PRODUCER_EPOCH: &str = "producer_epoch";

/// The metadata key holding every epoch a source observed (dendro's
/// `keys::PRODUCER_EPOCHS`): a JSON array of `{"epoch", "from_ts"}`.
pub const PRODUCER_EPOCHS: &str = "producer_epochs";

/// The epochs a source's rows belong to, clipped to `span`.
///
/// The epochs are the recorder layer's entries that carry an `epoch`
/// ([`layers`](crate::layers)), in fold order. A source without a recorder
/// layer, recorded before the layers, or whose recorder layer does not parse
/// (logged), has them in `producer_epochs` and `producer_epoch`. Each epoch
/// runs from its `from_ts` to the next epoch's, exclusive, and the last to
/// the source's last row. Epochs are taken in `from_ts` order rather than
/// list order: restart stamps come from clocks that need not agree, and an
/// epoch stamped before the one listed ahead of it would otherwise cover
/// nothing; epochs with one `from_ts` keep their order. The first epoch also
/// covers any rows before its `from_ts`, since it is the epoch the source
/// opened with. Without a `producer_epochs` list that parses (logged),
/// `producer_epoch` covers the whole span. An epoch with no rows in the span
/// is left out.
pub fn epoch_spans(
    metadata: &BTreeMap<String, String>,
    span: Option<(u64, u64)>,
) -> Vec<EpochSpan> {
    let Some((first, last)) = span else {
        return Vec::new();
    };
    if let Some(listed) = recorder_epochs(metadata) {
        return spans_from(listed, first, last);
    }
    let listed = metadata.get(PRODUCER_EPOCHS).and_then(|s| {
        let parsed = listed_epochs(s);
        if parsed.is_none() {
            tracing::warn!(
                "{PRODUCER_EPOCHS} does not parse ({s:?}); taking {PRODUCER_EPOCH} as the \
                 source's only epoch"
            );
        }
        parsed
    });
    let Some(listed) = listed.filter(|l| !l.is_empty()) else {
        return match metadata.get(PRODUCER_EPOCH) {
            Some(epoch) if !epoch.is_empty() => vec![EpochSpan {
                epoch: epoch.clone(),
                first_ns: first,
                last_ns: last,
            }],
            _ => Vec::new(),
        };
    };
    spans_from(listed, first, last)
}

/// The recorder layer's epoch entries as `(epoch, from_ts)` pairs, an entry
/// with no `from_ts` (written at open) taking 0, so it covers from the first
/// row; `None` for a source without a recorder layer, or one whose recorder
/// layer does not parse (logged), which then reads the flat keys. A damaged
/// user or producer layer does not affect it.
fn recorder_epochs(metadata: &BTreeMap<String, String>) -> Option<Vec<(String, u64)>> {
    if !metadata.contains_key(crate::layers::RECORDER_KEY) {
        return None;
    }
    let recorder = match crate::layers::read_recorder(metadata) {
        Ok(recorder) => recorder?,
        Err(e) => {
            tracing::warn!("{e}; reading the flat {PRODUCER_EPOCHS} instead");
            return None;
        }
    };
    // In fold order, an absent `from_ts` first, so `spans_from`'s stable
    // sort on the stamp keeps it ahead of a `Some(0)`.
    Some(
        recorder
            .ordered(crate::layers::Layer::Recorder)
            .into_iter()
            .filter_map(|e| Some((e.epoch.clone()?, e.from_ts.unwrap_or(0))))
            .collect(),
    )
}

/// Spans for `listed` `(epoch, from_ts)` pairs within `first..=last`.
fn spans_from(mut listed: Vec<(String, u64)>, first: u64, last: u64) -> Vec<EpochSpan> {
    // Stable, so entries with one stamp keep their listed order.
    listed.sort_by_key(|(_, from)| *from);
    let mut out = Vec::new();
    for (i, (epoch, from)) in listed.iter().enumerate() {
        let begin = if i == 0 { first } else { (*from).max(first) };
        let end = match listed.get(i + 1) {
            Some((_, next)) => next.saturating_sub(1).min(last),
            None => last,
        };
        if begin <= end {
            out.push(EpochSpan {
                epoch: epoch.clone(),
                first_ns: begin,
                last_ns: end,
            });
        }
    }
    out
}

/// The `(epoch, from_ts)` pairs of a `producer_epochs` value; `None` when it
/// does not parse. Other fields of an entry (`version`) are ignored.
pub(crate) fn listed_epochs(s: &str) -> Option<Vec<(String, u64)>> {
    #[derive(serde::Deserialize)]
    struct Entry {
        epoch: String,
        from_ts: u64,
    }
    let entries: Vec<Entry> = serde_json::from_str(s).ok()?;
    Some(entries.into_iter().map(|e| (e.epoch, e.from_ts)).collect())
}

/// Sources sharing a uuid (copies of one source), or one source without a
/// uuid or whose uuid no other source carries.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct UuidGroup {
    pub uuid: Option<String>,
    /// Positions in the source list, ascending.
    pub sources: Vec<usize>,
    /// From the first row of any member to the last.
    pub span_ns: Option<(u64, u64)>,
}

/// Two uuid groups joined because they share a producer epoch and overlap
/// within it by a row of each.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EpochJoin {
    pub epoch: String,
    /// Indices into [`ProcessGroups::uuid_groups`], ascending.
    pub uuid_groups: [usize; 2],
    /// One source of each uuid group whose rows overlap.
    pub sources: [usize; 2],
    /// The intersection of those two sources' spans in the epoch.
    pub overlap_ns: (u64, u64),
}

/// The sources of one producer process.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProcessGroup {
    /// Indices into [`ProcessGroups::uuid_groups`], ascending.
    pub uuid_groups: Vec<usize>,
    /// Positions in the source list, ascending.
    pub sources: Vec<usize>,
    /// The joins that connected its uuid groups, one fewer than there are
    /// uuid groups; other pairs of them may overlap as well.
    pub joins: Vec<EpochJoin>,
}

/// A uuid group set aside from the union because it spans the gap between
/// two others.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Bridge {
    /// Index into [`ProcessGroups::uuid_groups`].
    pub uuid_group: usize,
    /// One pair of uuid groups it bridges: each overlaps the bridge and not
    /// the other. Where their spans are disjoint, the earliest-ending and the
    /// latest-starting group the bridge overlaps and can bridge (by its arm).
    pub between: [usize; 2],
}

/// Something a consumer should tell the user.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Notice {
    /// Copies of one source (one uuid) whose `host` labels differ.
    HostsDiffer {
        uuid_group: usize,
        hosts: Vec<String>,
    },
    /// Two uuid groups share a producer epoch and their spans in it
    /// intersect, but the intersection does not hold a row of each, so they
    /// were not joined. An aggregate over both may count one process twice.
    EpochWithoutSharedRow {
        epoch: String,
        uuid_groups: [usize; 2],
        intersection_ns: (u64, u64),
    },
    /// Two uuid groups share a producer epoch, and whether their spans in it
    /// hold a row of each could not be read (a read failed, or the rows
    /// moved since the reader opened). They were not joined.
    RowsUnread {
        epoch: String,
        uuid_groups: [usize; 2],
    },
}

/// An archive's sources grouped by producer process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProcessGroups {
    /// Every uuid group, in order of their first source.
    pub uuid_groups: Vec<UuidGroup>,
    /// The process groups, in order of their first source. Bridges are in
    /// none.
    pub groups: Vec<ProcessGroup>,
    /// The bridges set aside, in order of their uuid group.
    pub bridges: Vec<Bridge>,
    pub notices: Vec<Notice>,
}

impl ProcessGroups {
    /// The process group holding source `position`; `None` for a bridge's
    /// source, or a position out of range.
    pub fn group_of(&self, position: usize) -> Option<usize> {
        self.groups
            .iter()
            .position(|g| g.sources.binary_search(&position).is_ok())
    }

    /// The bridge holding source `position`, as an index into
    /// [`bridges`](Self::bridges).
    pub fn bridge_of(&self, position: usize) -> Option<usize> {
        self.bridges.iter().position(|b| {
            self.uuid_groups[b.uuid_group]
                .sources
                .binary_search(&position)
                .is_ok()
        })
    }
}

/// The root of `i` in a union-find forest, halving the path.
fn root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// An epoch (by number) and an intersection in it, or `None`.
type NearMiss = Option<(usize, (u64, u64))>;

/// What is known about two uuid groups whose spans intersect.
enum Relation {
    /// They share no epoch, so they overlap by span.
    Plain,
    /// They share an epoch, and a source of each has a row inside the
    /// intersection of their spans in it (R3).
    Join(EpochJoin),
    /// They share an epoch and no such row was found; the intersection is
    /// the first one checked, `None` when their spans in the shared epochs
    /// do not intersect at all.
    Apart(NearMiss),
    /// They share an epoch and a row check could not tell.
    Unknown(usize),
}

/// A uuid group's arm, from the sources of it that carry one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm<'a> {
    None,
    One(&'a str),
    /// Two or more values; the label rule splits such a group later.
    Mixed,
}

fn intersect(a: (u64, u64), b: (u64, u64)) -> Option<(u64, u64)> {
    let lo = a.0.max(b.0);
    let hi = a.1.min(b.1);
    (lo <= hi).then_some((lo, hi))
}

/// The earliest-ending and the latest-starting uuid group seen, with their
/// end and start.
#[derive(Clone, Copy, Default)]
struct Extremes {
    earliest_end: Option<(u64, usize)>,
    latest_start: Option<(u64, usize)>,
}

impl Extremes {
    fn add(&mut self, group: usize, hull: (u64, u64)) {
        if self.earliest_end.is_none_or(|(end, _)| hull.1 < end) {
            self.earliest_end = Some((hull.1, group));
        }
        if self.latest_start.is_none_or(|(start, _)| hull.0 > start) {
            self.latest_start = Some((hull.0, group));
        }
    }

    /// Two groups whose spans do not intersect, if any were seen.
    fn disjoint(&self) -> Option<[usize; 2]> {
        match (self.earliest_end, self.latest_start) {
            (Some((end, x)), Some((start, y))) if end < start => Some([x, y]),
            _ => None,
        }
    }
}

/// Every pair of uuid groups whose spans intersect, `(i, j)` with `i < j`,
/// in one sweep over the spans in start order.
fn intersecting_pairs(hulls: &[Option<(u64, u64)>], mut visit: impl FnMut(usize, usize)) {
    let mut order: Vec<usize> = (0..hulls.len()).filter(|&i| hulls[i].is_some()).collect();
    order.sort_by_key(|&i| hulls[i].map(|h| h.0));
    let mut active: Vec<usize> = Vec::new();
    for g in order {
        let (start, _) = hulls[g].expect("filtered");
        active.retain(|&a| hulls[a].is_some_and(|h| h.1 >= start));
        for &a in &active {
            visit(a.min(g), a.max(g));
        }
        active.push(g);
    }
}

/// Group `sources` by producer process. `has_row(position, lo, hi)` answers
/// whether source `position` has a row with a timestamp in `[lo, hi]`:
/// `Some(true)` or `Some(false)`, or `None` when it could not tell (a read
/// failed, or the rows moved since the reader opened). A pair whose rows
/// could not be told is left unjoined, and reported as
/// [`Notice::RowsUnread`] unless either uuid group is a bridge or both end
/// in one process group.
///
/// Time is linear in the number of pairs of uuid groups whose spans
/// intersect, plus up to (uuid groups × pairs of one epoch without a shared
/// row) for the bridge search those pairs need; memory is linear in the
/// uuid groups plus those pairs, which are kept for the notices.
pub fn form(
    sources: &[SourceInfo],
    has_row: &mut dyn FnMut(usize, u64, u64) -> Option<bool>,
) -> ProcessGroups {
    // 1. Uuid groups, in order of their first source.
    let mut uuid_groups: Vec<UuidGroup> = Vec::new();
    let mut by_uuid: HashMap<&str, usize> = HashMap::new();
    for (position, source) in sources.iter().enumerate() {
        let known = source.uuid.as_deref().and_then(|u| by_uuid.get(u).copied());
        match known {
            Some(i) => uuid_groups[i].sources.push(position),
            None => {
                if let Some(uuid) = source.uuid.as_deref() {
                    by_uuid.insert(uuid, uuid_groups.len());
                }
                uuid_groups.push(UuidGroup {
                    uuid: source.uuid.clone(),
                    sources: vec![position],
                    span_ns: None,
                });
            }
        }
    }
    for g in &mut uuid_groups {
        g.span_ns = g
            .sources
            .iter()
            .filter_map(|&s| sources[s].span_ns)
            .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)));
    }
    let n = uuid_groups.len();
    let hulls: Vec<Option<(u64, u64)>> = uuid_groups.iter().map(|g| g.span_ns).collect();

    let mut notices = Vec::new();
    for (i, g) in uuid_groups.iter().enumerate() {
        let hosts: BTreeSet<&String> = g
            .sources
            .iter()
            .filter_map(|&s| sources[s].labels.get(HOST_LABEL))
            .collect();
        if hosts.len() > 1 {
            notices.push(Notice::HostsDiffer {
                uuid_group: i,
                hosts: hosts.into_iter().cloned().collect(),
            });
        }
    }

    // Epochs by number, and each uuid group's, sorted.
    let mut epoch_ids: HashMap<&str, usize> = HashMap::new();
    let mut epoch_names: Vec<&str> = Vec::new();
    let mut source_epochs: Vec<Vec<(usize, u64, u64)>> = Vec::with_capacity(sources.len());
    for s in sources {
        let mut epochs = Vec::with_capacity(s.epochs.len());
        for e in &s.epochs {
            let id = *epoch_ids.entry(e.epoch.as_str()).or_insert_with(|| {
                epoch_names.push(e.epoch.as_str());
                epoch_names.len() - 1
            });
            epochs.push((id, e.first_ns, e.last_ns));
        }
        source_epochs.push(epochs);
    }
    let group_epochs: Vec<Vec<usize>> = uuid_groups
        .iter()
        .map(|g| {
            let mut e: Vec<usize> = g
                .sources
                .iter()
                .flat_map(|&s| source_epochs[s].iter().map(|e| e.0))
                .collect();
            e.sort_unstable();
            e.dedup();
            e
        })
        .collect();
    let share_epoch = |i: usize, j: usize| {
        let (a, b) = (&group_epochs[i], &group_epochs[j]);
        let (mut x, mut y) = (0, 0);
        while x < a.len() && y < b.len() {
            match a[x].cmp(&b[y]) {
                std::cmp::Ordering::Less => x += 1,
                std::cmp::Ordering::Greater => y += 1,
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    };
    // R3, source pair by source pair, for two groups that share an epoch.
    let mut relate = |i: usize, j: usize| -> Relation {
        if !share_epoch(i, j) {
            return Relation::Plain;
        }
        let mut near_miss = None;
        let mut unknown = None;
        for &a in &uuid_groups[i].sources {
            for &b in &uuid_groups[j].sources {
                for &(ea, a0, a1) in &source_epochs[a] {
                    for &(_, b0, b1) in source_epochs[b].iter().filter(|e| e.0 == ea) {
                        let Some(both) = intersect((a0, a1), (b0, b1)) else {
                            continue;
                        };
                        // A row of each, as a three-valued AND: one source
                        // known to have no row decides it whatever the other.
                        let row = match has_row(a, both.0, both.1) {
                            Some(false) => Some(false),
                            ra => match (ra, has_row(b, both.0, both.1)) {
                                (_, Some(false)) => Some(false),
                                (Some(true), Some(true)) => Some(true),
                                _ => None,
                            },
                        };
                        match row {
                            Some(true) => {
                                return Relation::Join(EpochJoin {
                                    epoch: epoch_names[ea].to_string(),
                                    uuid_groups: [i, j],
                                    sources: [a, b],
                                    overlap_ns: both,
                                })
                            }
                            Some(false) => {
                                near_miss.get_or_insert((ea, both));
                            }
                            None => {
                                unknown.get_or_insert(ea);
                            }
                        }
                    }
                }
            }
        }
        match unknown {
            Some(e) => Relation::Unknown(e),
            None => Relation::Apart(near_miss),
        }
    };

    // 2. Arms, from the sources that carry one.
    let armed = sources.iter().any(|s| s.labels.contains_key(ARM_LABEL));
    let arm_of: Vec<Arm> = uuid_groups
        .iter()
        .map(|g| {
            let arms: BTreeSet<&str> = g
                .sources
                .iter()
                .filter_map(|&s| sources[s].labels.get(ARM_LABEL).map(String::as_str))
                .collect();
            let mut arms = arms.into_iter();
            match (arms.next(), arms.next()) {
                (None, _) => Arm::None,
                (Some(arm), None) => Arm::One(arm),
                (Some(_), Some(_)) => Arm::Mixed,
            }
        })
        .collect();
    // Whether `b` can bridge `x`: within an arm; an unarmed group (shared
    // context) between armed ones (owner decision on Q39); a mixed group
    // never.
    let bridges_over = |b: usize, x: usize| match (armed, arm_of[b]) {
        (false, _) => true,
        (true, Arm::One(arm)) => arm_of[x] == Arm::One(arm),
        (true, Arm::None) => arm_of[x] != Arm::None,
        (true, Arm::Mixed) => false,
    };

    // 3. One sweep over the intersecting pairs: for each group, the
    // earliest-ending and latest-starting group it overlaps and could
    // bridge; and the pairs of one epoch that do not overlap.
    // The joins that connect the join graph's components, bridges
    // included, kept so the union below reads no row twice for them.
    let mut extremes = vec![Extremes::default(); n];
    let mut apart: Vec<(usize, usize, NearMiss)> = Vec::new();
    let mut unread: Vec<(usize, usize, usize)> = Vec::new();
    let mut forest: HashMap<(usize, usize), EpochJoin> = HashMap::new();
    let mut everyone: Vec<usize> = (0..n).collect();
    intersecting_pairs(&hulls, |i, j| {
        let relation = relate(i, j);
        if let Relation::Plain | Relation::Join(_) = relation {
            for (b, x) in [(i, j), (j, i)] {
                if bridges_over(b, x) {
                    extremes[b].add(x, hulls[x].expect("intersects"));
                }
            }
        }
        match relation {
            Relation::Plain => {}
            Relation::Join(join) => {
                let (a, b) = (root(&mut everyone, i), root(&mut everyone, j));
                if a != b {
                    everyone[a.max(b)] = a.min(b);
                    forest.insert((i, j), join);
                }
            }
            Relation::Apart(miss) => apart.push((i, j, miss)),
            Relation::Unknown(e) => unread.push((i, j, e)),
        }
    });

    // 4. Bridges: a group overlapping two that do not overlap each other.
    // Covering the gap between them follows: its span runs from its first
    // row to its last and holds a point of each. Two groups whose spans do
    // not intersect never overlap; when all a group's overlapping groups
    // intersect pairwise (they then share a point), only a pair of one epoch
    // without a shared row can be the pair, and such a pair is taken as not
    // overlapping even though their spans intersect (this module's reading
    // of Q39's open case). A pair whose rows could not be read is taken as
    // overlapping, so it makes no bridge.
    let mut bridges = Vec::new();
    for b in 0..n {
        if armed && arm_of[b] == Arm::Mixed {
            continue;
        }
        let pair = extremes[b].disjoint().or_else(|| {
            let hb = hulls[b]?;
            apart.iter().find_map(|&(x, y, _)| {
                let usable = |x: usize| {
                    x != b
                        && bridges_over(b, x)
                        && hulls[x].and_then(|h| intersect(h, hb)).is_some()
                };
                if !usable(x) || !usable(y) {
                    return None;
                }
                let near = |r: Relation| matches!(r, Relation::Plain | Relation::Join(_));
                (near(relate(b, x)) && near(relate(b, y))).then_some([x, y])
            })
        });
        if let Some(between) = pair {
            bridges.push(Bridge {
                uuid_group: b,
                between,
            });
        }
    }
    let mut is_bridge = vec![false; n];
    for b in &bridges {
        is_bridge[b.uuid_group] = true;
    }

    // 5. Process groups: the other uuid groups, joined by a shared epoch.
    // A second sweep; a pair already connected, or known not to join, is
    // not checked again.
    let mut parent: Vec<usize> = (0..n).collect();
    let settled: std::collections::HashSet<(usize, usize)> = apart
        .iter()
        .map(|&(i, j, _)| (i, j))
        .chain(unread.iter().map(|&(i, j, _)| (i, j)))
        .collect();
    let mut joins: Vec<EpochJoin> = Vec::new();
    intersecting_pairs(&hulls, |i, j| {
        if is_bridge[i] || is_bridge[j] || settled.contains(&(i, j)) || !share_epoch(i, j) {
            return;
        }
        let (a, b) = (root(&mut parent, i), root(&mut parent, j));
        if a == b {
            return;
        }
        // A pair checked again can answer differently (a live archive
        // sealed or evicted rows between the sweeps); what it answers now is
        // kept, so a pair left apart is still reported.
        let join = match forest.remove(&(i, j)) {
            Some(join) => Some(join),
            None => match relate(i, j) {
                Relation::Join(join) => Some(join),
                Relation::Plain => None,
                Relation::Apart(miss) => {
                    apart.push((i, j, miss));
                    None
                }
                Relation::Unknown(e) => {
                    unread.push((i, j, e));
                    None
                }
            },
        };
        if let Some(join) = join {
            parent[a.max(b)] = a.min(b);
            joins.push(join);
        }
    });
    let mut groups: Vec<ProcessGroup> = Vec::new();
    let mut group_of_root: HashMap<usize, usize> = HashMap::new();
    for i in (0..n).filter(|&i| !is_bridge[i]) {
        let r = root(&mut parent, i);
        let g = *group_of_root.entry(r).or_insert_with(|| {
            groups.push(ProcessGroup {
                uuid_groups: Vec::new(),
                sources: Vec::new(),
                joins: Vec::new(),
            });
            groups.len() - 1
        });
        groups[g].uuid_groups.push(i);
        groups[g].sources.extend(&uuid_groups[i].sources);
    }
    for join in joins {
        let r = root(&mut parent, join.uuid_groups[0]);
        groups[group_of_root[&r]].joins.push(join);
    }
    for g in &mut groups {
        g.sources.sort_unstable();
    }
    // Uuid groups are numbered by first source, so groups built in that
    // order are already in order of their first source.

    // 6. Pairs of one epoch left apart.
    for (i, j, miss) in apart {
        let Some((epoch, intersection)) = miss else {
            continue;
        };
        if is_bridge[i] || is_bridge[j] || root(&mut parent, i) == root(&mut parent, j) {
            continue;
        }
        notices.push(Notice::EpochWithoutSharedRow {
            epoch: epoch_names[epoch].to_string(),
            uuid_groups: [i, j],
            intersection_ns: intersection,
        });
    }
    for (i, j, epoch) in unread {
        if is_bridge[i] || is_bridge[j] || root(&mut parent, i) == root(&mut parent, j) {
            continue;
        }
        notices.push(Notice::RowsUnread {
            epoch: epoch_names[epoch].to_string(),
            uuid_groups: [i, j],
        });
    }

    ProcessGroups {
        uuid_groups,
        groups,
        bridges,
        notices,
    }
}

/// The sources a query reads as one recording: a process group, a bridge's
/// uuid group, or the part of one whose sources agree on every label (S1, S2
/// and the data model's "Labels of a process group").
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadGroup {
    /// Positions in the source list, ascending.
    pub sources: Vec<usize>,
    /// The labels its series carry: each key some of its sources carry, with
    /// the value they carry.
    pub labels: BTreeMap<String, String>,
}

/// The read groups of `sources`, grouped as `groups` says, in order of their
/// first source.
///
/// Each process group, and each bridge's uuid group (until a bridge's pieces
/// join their clusters, which is the caller's), is one read group, labelled
/// with every key some of its sources carry. Where two of its sources carry
/// one key with different values, the group is split by the values of those
/// keys: one recorder run with `--label arm=a` and one with `arm=b` of one
/// agent are two read groups. A source that lacks such a key takes the value
/// its uuid group's sources carry, when they carry one (an annotated or
/// filtered copy keeps its original's), and is otherwise a part of its own
/// for that key. A source in no group (a position `groups` does not name) is
/// a read group of one.
pub fn read_groups(sources: &[SourceInfo], groups: &ProcessGroups) -> Vec<ReadGroup> {
    let mut units: Vec<Vec<usize>> = groups.groups.iter().map(|g| g.sources.clone()).collect();
    units.extend(
        groups
            .bridges
            .iter()
            .map(|b| groups.uuid_groups[b.uuid_group].sources.clone()),
    );
    let mut placed = vec![false; sources.len()];
    for unit in &units {
        for &s in unit {
            if let Some(p) = placed.get_mut(s) {
                *p = true;
            }
        }
    }
    units.extend((0..sources.len()).filter(|&s| !placed[s]).map(|s| vec![s]));

    let mut out: Vec<ReadGroup> = Vec::new();
    for unit in units {
        // The keys whose values differ among the unit's sources.
        let mut values: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for &s in &unit {
            for (k, v) in &sources[s].labels {
                values.entry(k).or_default().insert(v);
            }
        }
        let split: Vec<&str> = values
            .iter()
            .filter(|(_, v)| v.len() > 1)
            .map(|(k, _)| *k)
            .collect();
        // Each source's values of those keys, a missing one filled from its
        // uuid group when that carries exactly one value.
        let fill = |s: usize, key: &str| -> Option<String> {
            if let Some(v) = sources[s].labels.get(key) {
                return Some(v.clone());
            }
            let uuid = sources[s].uuid.as_deref()?;
            let carried: BTreeSet<&String> = unit
                .iter()
                .filter(|&&o| sources[o].uuid.as_deref() == Some(uuid))
                .filter_map(|&o| sources[o].labels.get(key))
                .collect();
            match carried.len() {
                1 => carried.into_iter().next().cloned(),
                _ => None,
            }
        };
        let mut parts: Vec<(Vec<Option<String>>, Vec<usize>)> = Vec::new();
        for &s in &unit {
            let key: Vec<Option<String>> = split.iter().map(|k| fill(s, k)).collect();
            match parts.iter_mut().find(|(k, _)| *k == key) {
                Some((_, members)) => members.push(s),
                None => parts.push((key, vec![s])),
            }
        }
        for (key, members) in parts {
            let mut labels: BTreeMap<String, String> = BTreeMap::new();
            for &s in &members {
                for (k, v) in &sources[s].labels {
                    labels.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
            for (k, v) in split.iter().zip(key) {
                match v {
                    Some(v) => labels.insert(k.to_string(), v),
                    None => labels.remove(*k),
                };
            }
            out.push(ReadGroup {
                sources: members,
                labels,
            });
        }
    }
    out.sort_by_key(|g| g.sources[0]);
    out
}

/// The metadata key a source's timeline events are stored under: a JSON
/// object whose `events` member is an array of events, each an object that
/// may carry a string `id` and an integer `timestamp` (nanoseconds).
pub const EVENTS: &str = "events";

/// The metadata of a read group, from its sources' metadata in archive
/// order: the metadata a group reader from
/// `ArchiveReader::flatten_read_groups` carries.
///
/// - [`EVENTS`] is the union of the sources' events. Events are records a
///   source gains one at a time (an annotation of a copy, a check of the
///   original), so the group shows each once: an event with a non-empty
///   string `id` is kept the first time that id is seen, and one without is
///   kept unless an identical event is already kept. The union is sorted by
///   `timestamp` (0 when absent), then by `id` (empty when absent), and
///   stored as `{"events": [...]}`, so members of a payload other than
///   `events` are not kept. A payload that is not a JSON object is skipped.
///   When only one source's payload is an object, the group's value is that
///   payload unchanged; when none is, the value is the first source's that
///   holds the key, as for any other key. A `check` event re-annotated in
///   one source alone shows the first source's window.
/// - Every other key is taken from the first source that holds it. These
///   are whole values with no merge rule (KPIs, `systeminfo`,
///   `descriptions`, `version`), and the sources of one group are copies of
///   one recording or recordings of one process, so they agree on a key
///   both hold unless one was edited alone. A key only one source holds is
///   the group's in whichever order the sources were combined. Removing a
///   key from one source therefore leaves it on the group while another
///   source holds it; an edit of the group is written to each of its
///   sources.
pub fn merge_metadata(sources: &[&BTreeMap<String, String>]) -> BTreeMap<String, String> {
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for source in sources {
        for (key, value) in source.iter() {
            if !merged.contains_key(key) {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    // Each source's events payload that is an object, in order.
    let mut payloads: Vec<(&String, Vec<serde_json::Value>)> = Vec::new();
    for source in sources {
        let Some(raw) = source.get(EVENTS) else {
            continue;
        };
        let Ok(serde_json::Value::Object(mut payload)) = serde_json::from_str(raw) else {
            continue;
        };
        let list = match payload.remove(EVENTS) {
            Some(serde_json::Value::Array(list)) => list,
            _ => Vec::new(),
        };
        payloads.push((raw, list));
    }
    match payloads.len() {
        0 => {}
        1 => {
            merged.insert(EVENTS.to_string(), payloads[0].0.clone());
        }
        _ => {
            let mut events: Vec<serde_json::Value> = Vec::new();
            let mut ids: BTreeSet<String> = BTreeSet::new();
            for (_, list) in payloads {
                for event in list {
                    let fresh = match event.get("id").and_then(|v| v.as_str()) {
                        Some(id) if !id.is_empty() => ids.insert(id.to_string()),
                        _ => !events.contains(&event),
                    };
                    if fresh {
                        events.push(event);
                    }
                }
            }
            // Stable, so events of one timestamp and id keep their order.
            events.sort_by(|a, b| {
                let key = |e: &serde_json::Value| {
                    (
                        e.get("timestamp").and_then(|t| t.as_u64()).unwrap_or(0),
                        e.get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    )
                };
                key(a).cmp(&key(b))
            });
            merged.insert(
                EVENTS.to_string(),
                serde_json::json!({ EVENTS: events }).to_string(),
            );
        }
    }
    merged
}

#[cfg(test)]
mod equiv_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// The recorder layer's epochs are read in fold order: an epoch with no
    /// `from_ts`, appended after one at 0, folds before it, so the epoch at 0
    /// covers every row.
    #[test]
    fn recorder_epochs_are_read_in_fold_order() {
        use crate::layers::{append, Entry, Layer, LegacyEvidence};
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        let evidence = LegacyEvidence::default();
        let mut at_zero = Entry::new("a", "recorder");
        at_zero.epoch = Some("e2".to_string());
        at_zero.from_ts = Some(0);
        let mut at_open = Entry::new("b", "recorder");
        at_open.epoch = Some("e1".to_string());
        for e in [at_zero, at_open] {
            append(
                &mut labels,
                &mut metadata,
                None,
                &evidence,
                Layer::Recorder,
                e,
            )
            .unwrap();
        }
        // Rows from 5: in fold order `e1` covers rows before `e2`'s 0, none.
        let spans = epoch_spans(&metadata, Some((5, 10)));
        let epochs: Vec<&str> = spans.iter().map(|s| s.epoch.as_str()).collect();
        assert_eq!(epochs, vec!["e2"]);
    }

    /// The recorder layer's epoch entries give the spans, and the flat keys
    /// beside them are not read.
    #[test]
    fn epoch_spans_read_the_recorder_layer() {
        let mut labels = BTreeMap::new();
        let mut metadata: BTreeMap<String, String> = [
            (PRODUCER_EPOCH.to_string(), "flat".to_string()),
            (
                PRODUCER_EPOCHS.to_string(),
                r#"[{"epoch":"flat","from_ts":0}]"#.to_string(),
            ),
        ]
        .into();
        // Written with the layers, so the flat list is not derived into them.
        metadata.insert(crate::layers::RECORDER_KEY.to_string(), "{}".to_string());
        for (id, epoch, from_ts) in [("a", "e1", None), ("b", "e2", Some(5 * M))] {
            let mut entry = crate::layers::Entry::new(id, "recorder");
            entry.epoch = Some(epoch.to_string());
            entry.from_ts = from_ts;
            crate::layers::append(
                &mut labels,
                &mut metadata,
                None,
                &crate::layers::LegacyEvidence::default(),
                crate::layers::Layer::Recorder,
                entry,
            )
            .unwrap();
        }
        let spans = epoch_spans(&metadata, Some((M, 9 * M)));
        assert_eq!(
            spans
                .iter()
                .map(|s| (s.epoch.as_str(), s.first_ns, s.last_ns))
                .collect::<Vec<_>>(),
            vec![("e1", M, 5 * M - 1), ("e2", 5 * M, 9 * M)]
        );

        // A recorder layer with no epoch entry: no spans, not the flat ones.
        let bare: BTreeMap<String, String> = [
            (PRODUCER_EPOCH.to_string(), "flat".to_string()),
            (crate::layers::RECORDER_KEY.to_string(), "{}".to_string()),
        ]
        .into();
        assert!(epoch_spans(&bare, Some((M, 9 * M))).is_empty());
    }

    /// A damaged recorder layer is read past: the flat keys give the epochs.
    #[test]
    fn a_damaged_recorder_layer_falls_back_to_the_flat_keys() {
        let metadata: BTreeMap<String, String> = [
            (PRODUCER_EPOCH.to_string(), "flat".to_string()),
            (
                crate::layers::RECORDER_KEY.to_string(),
                "garbage".to_string(),
            ),
        ]
        .into();
        let spans = epoch_spans(&metadata, Some((M, 2 * M)));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].epoch, "flat");
    }

    /// A damaged user layer does not hide the recorder layer's epochs.
    #[test]
    fn a_damaged_user_layer_does_not_hide_the_epochs() {
        let mut entry = crate::layers::Entry::new("a", "recorder");
        entry.epoch = Some("e1".to_string());
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        crate::layers::append(
            &mut labels,
            &mut metadata,
            None,
            &crate::layers::LegacyEvidence::default(),
            crate::layers::Layer::Recorder,
            entry,
        )
        .unwrap();
        metadata.insert(crate::layers::USER_KEY.to_string(), "garbage".to_string());
        let spans = epoch_spans(&metadata, Some((M, 2 * M)));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].epoch, "e1");
    }

    const M: u64 = 60_000_000_000;

    /// A source with labels, a uuid, rows every minute over `[from, to]`
    /// minutes, and one epoch (or none).
    fn source(
        labels: &[(&str, &str)],
        uuid: Option<&str>,
        span: Option<(u64, u64)>,
        epoch: Option<&str>,
    ) -> SourceInfo {
        let mut metadata = BTreeMap::new();
        if let Some(e) = epoch {
            metadata.insert(PRODUCER_EPOCH.to_string(), e.to_string());
        }
        SourceInfo::new(
            labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            0,
            uuid.map(str::to_string),
            span.map(|(a, b)| (a * M, b * M)),
            &metadata,
        )
    }

    /// Every source has a row at every whole minute of its span.
    fn minutely(sources: &[SourceInfo]) -> impl FnMut(usize, u64, u64) -> Option<bool> + '_ {
        |s, lo, hi| {
            let Some((a, b)) = sources[s].span_ns else {
                return Some(false);
            };
            let first = lo.max(a).div_ceil(M) * M;
            Some(first <= hi.min(b))
        }
    }

    #[test]
    fn copies_whose_hosts_differ_get_a_notice() {
        let s = vec![
            source(&[("host", "a")], Some("u"), Some((0, 10)), None),
            source(&[("host", "b")], Some("u"), Some((0, 10)), None),
            source(&[], Some("u"), Some((0, 10)), None),
        ];
        let g = form(&s, &mut minutely(&s));
        assert_eq!(g.groups.len(), 1);
        assert_eq!(
            g.notices,
            vec![Notice::HostsDiffer {
                uuid_group: 0,
                hosts: vec!["a".into(), "b".into()]
            }]
        );
    }

    #[test]
    fn a_copy_without_rows_still_joins_its_uuid_group() {
        let s = vec![
            source(&[], Some("u"), Some((0, 10)), Some("e")),
            source(&[], Some("u"), None, Some("e")),
        ];
        let g = form(&s, &mut minutely(&s));
        assert_eq!(g.groups.len(), 1);
        assert_eq!(g.groups[0].sources, vec![0, 1]);
        assert!(g.groups[0].joins.is_empty());
    }

    #[test]
    fn producer_epochs_split_a_source_at_each_from_ts() {
        let metadata: BTreeMap<String, String> = [
            (
                PRODUCER_EPOCHS.to_string(),
                format!(
                    r#"[{{"epoch":"e1","from_ts":{}}},{{"epoch":"e2","from_ts":{},"version":"x"}}]"#,
                    10 * M,
                    30 * M
                ),
            ),
            (PRODUCER_EPOCH.to_string(), "e2".to_string()),
        ]
        .into();
        // The first entry runs from the source's first row; e2 to its last.
        let spans = epoch_spans(&metadata, Some((5 * M, 40 * M)));
        assert_eq!(
            spans,
            vec![
                EpochSpan {
                    epoch: "e1".into(),
                    first_ns: 5 * M,
                    last_ns: 30 * M - 1
                },
                EpochSpan {
                    epoch: "e2".into(),
                    first_ns: 30 * M,
                    last_ns: 40 * M
                },
            ]
        );
        // A copy that starts after the restart holds e2 only.
        let spans = epoch_spans(&metadata, Some((35 * M, 40 * M)));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].epoch, "e2");
        // A list that does not parse falls back to the current epoch.
        let broken: BTreeMap<String, String> = [
            (PRODUCER_EPOCHS.to_string(), "[{".to_string()),
            (PRODUCER_EPOCH.to_string(), "e2".to_string()),
        ]
        .into();
        let spans = epoch_spans(&broken, Some((0, M)));
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].first_ns, spans[0].last_ns), (0, M));
    }

    #[test]
    fn producer_epochs_as_the_recorder_writes_them_parse() {
        // metriken-recorder's `restart::entry`, through serde_json.
        let s = r#"[{"epoch":"0b6c","from_ts":1700000000000000000,"version":"5.28.0"},{"epoch":"9f\"2","from_ts":1700000060000000000}]"#;
        assert_eq!(
            listed_epochs(s),
            Some(vec![
                ("0b6c".to_string(), 1_700_000_000_000_000_000),
                ("9f\"2".to_string(), 1_700_000_060_000_000_000),
            ])
        );
        assert_eq!(listed_epochs("[]"), Some(vec![]));
        assert_eq!(listed_epochs(r#"[{"epoch":"a"}]"#), None);
        assert_eq!(listed_epochs(r#"[{"epoch":{"x":1},"from_ts":1}]"#), None);
    }

    /// Q39's chain, epochless: the middle source is a bridge, by span.
    #[test]
    fn the_middle_of_a_chain_is_a_bridge() {
        let s = vec![
            source(&[], None, Some((9 * 60, 11 * 60)), None),
            source(&[], None, Some((10 * 60, 12 * 60)), None),
            source(&[], None, Some((11 * 60 + 30, 13 * 60)), None),
        ];
        let g = form(&s, &mut minutely(&s));
        assert_eq!(
            g.bridges,
            vec![Bridge {
                uuid_group: 1,
                between: [0, 2]
            }]
        );
        assert_eq!(g.groups.len(), 2);
        assert_eq!(g.group_of(1), None);
        assert_eq!(g.bridge_of(1), Some(0));
    }

    /// Two sources of one epoch whose spans intersect without a row of each
    /// do not overlap, so a third overlapping both is a bridge between them
    /// even with no gap to cover (the reading of Q39's "the cut when the two
    /// groups' spans overlap without an R3 overlap" this module takes).
    #[test]
    fn a_source_overlapping_two_that_share_no_row_is_a_bridge() {
        let s = vec![
            source(&[], None, Some((0, 10)), Some("e")),
            source(&[], None, Some((5, 20)), Some("e")),
            source(&[], None, Some((0, 30)), Some("e")),
        ];
        // Sources 0 and 1 intersect over minutes 5..10 and have no row of
        // each there; 2 shares rows with both.
        let mut rows = |s: usize, lo: u64, hi: u64| Some(s == 2 || !(lo >= 5 * M && hi <= 10 * M));
        let g = form(&s, &mut rows);
        assert_eq!(
            g.bridges,
            vec![Bridge {
                uuid_group: 2,
                between: [0, 1]
            }]
        );
        assert_eq!(g.groups.len(), 2);
        assert_eq!(
            g.notices,
            vec![Notice::EpochWithoutSharedRow {
                epoch: "e".into(),
                uuid_groups: [0, 1],
                intersection_ns: (5 * M, 10 * M),
            }]
        );
    }

    /// In an armed archive an unarmed source overlapping two armed sources
    /// that do not overlap each other is a bridge between them (owner
    /// decision on Q39), so it joins neither, and the pair's notice stands.
    #[test]
    fn shared_context_does_not_join_two_armed_sources() {
        let s = vec![
            source(&[("arm", "a")], None, Some((0, 10)), Some("e")),
            source(&[("arm", "a")], None, Some((5, 20)), Some("e")),
            source(&[], None, Some((0, 30)), Some("e")),
        ];
        let mut rows = |s: usize, lo: u64, hi: u64| Some(s == 2 || !(lo >= 5 * M && hi <= 10 * M));
        let g = form(&s, &mut rows);
        assert_eq!(g.bridges.len(), 1);
        assert_eq!(g.bridges[0].uuid_group, 2);
        assert_eq!(g.groups.len(), 2);
        assert_eq!(g.notices.len(), 1, "{:?}", g.notices);
    }

    /// Two armed sources that share no row, joined through a source of
    /// another arm (which cannot bridge them): one process group, no notice.
    #[test]
    fn a_near_miss_joined_through_another_arm_is_not_reported() {
        let s = vec![
            source(&[("arm", "a")], None, Some((0, 10)), Some("e")),
            source(&[("arm", "a")], None, Some((5, 20)), Some("e")),
            source(&[("arm", "b")], None, Some((0, 30)), Some("e")),
        ];
        let mut rows = |s: usize, lo: u64, hi: u64| Some(s == 2 || !(lo >= 5 * M && hi <= 10 * M));
        let g = form(&s, &mut rows);
        assert!(g.bridges.is_empty());
        assert_eq!(g.groups.len(), 1);
        assert!(g.notices.is_empty(), "{:?}", g.notices);
    }

    /// The reviewer's case: a continuous `arm=a` recording and a filtered
    /// copy of it without `arm` (one uuid), beside two `arm=a` attempts of
    /// one epoch. The uuid group carries `a`, so it is a bridge and the
    /// attempts stay apart.
    #[test]
    fn a_copy_without_arm_keeps_its_group_a_bridge() {
        let s = vec![
            source(
                &[("arm", "a")],
                Some("c"),
                Some((9 * 60, 18 * 60)),
                Some("e"),
            ),
            source(&[], Some("c"), Some((9 * 60, 18 * 60)), Some("e")),
            source(
                &[("arm", "a")],
                Some("x"),
                Some((9 * 60, 10 * 60)),
                Some("e"),
            ),
            source(
                &[("arm", "a")],
                Some("y"),
                Some((17 * 60, 18 * 60)),
                Some("e"),
            ),
        ];
        let g = form(&s, &mut minutely(&s));
        assert_eq!(g.bridges.len(), 1);
        assert_eq!(g.uuid_groups[g.bridges[0].uuid_group].sources, vec![0, 1]);
        assert_eq!(
            g.groups
                .iter()
                .map(|g| g.sources.clone())
                .collect::<Vec<_>>(),
            vec![vec![2], vec![3]]
        );
    }

    /// Restart clocks need not agree: an entry listed after another but
    /// stamped before it keeps its rows.
    #[test]
    fn out_of_order_producer_epochs_keep_every_epoch() {
        let metadata: BTreeMap<String, String> = [(
            PRODUCER_EPOCHS.to_string(),
            format!(
                r#"[{{"epoch":"e1","from_ts":{}}},{{"epoch":"e3","from_ts":{}}},{{"epoch":"e2","from_ts":{}}}]"#,
                0,
                30 * M,
                20 * M
            ),
        )]
        .into();
        let spans = epoch_spans(&metadata, Some((0, 40 * M)));
        let got: Vec<(&str, u64, u64)> = spans
            .iter()
            .map(|e| (e.epoch.as_str(), e.first_ns, e.last_ns))
            .collect();
        assert_eq!(
            got,
            vec![
                ("e1", 0, 20 * M - 1),
                ("e2", 20 * M, 30 * M - 1),
                ("e3", 30 * M, 40 * M)
            ]
        );
    }

    /// A group that shares an epoch with X but no row of it does not overlap
    /// X, so it is no bridge between X and a Y it does overlap.
    #[test]
    fn a_source_sharing_no_row_with_one_of_two_is_not_a_bridge() {
        let s = vec![
            source(&[], None, Some((0, 30)), Some("e")),
            source(&[], None, Some((5, 10)), Some("e")),
            source(&[], None, Some((20, 40)), None),
        ];
        // 0 and 1 share epoch e with no row of 1 inside their intersection.
        let mut rows = |s: usize, _: u64, _: u64| Some(s != 1);
        let g = form(&s, &mut rows);
        assert!(g.bridges.is_empty(), "{:?}", g.bridges);
        assert_eq!(g.groups.len(), 3);
    }

    /// The copy-without-arm case with attempts of another arm: the group
    /// carries `a`, and a source is never a bridge across arms, so the
    /// attempts join through it.
    #[test]
    fn a_copy_without_arm_does_not_bridge_another_arm() {
        let s = vec![
            source(
                &[("arm", "a")],
                Some("c"),
                Some((9 * 60, 18 * 60)),
                Some("e"),
            ),
            source(&[], Some("c"), Some((9 * 60, 18 * 60)), Some("e")),
            source(
                &[("arm", "b")],
                Some("x"),
                Some((9 * 60, 10 * 60)),
                Some("e"),
            ),
            source(
                &[("arm", "b")],
                Some("y"),
                Some((17 * 60, 18 * 60)),
                Some("e"),
            ),
        ];
        let g = form(&s, &mut minutely(&s));
        assert!(g.bridges.is_empty(), "{:?}", g.bridges);
        assert_eq!(
            g.groups
                .iter()
                .map(|g| g.sources.clone())
                .collect::<Vec<_>>(),
            vec![vec![0, 1, 2, 3]]
        );
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn read(s: &[SourceInfo]) -> Vec<(Vec<usize>, BTreeMap<String, String>)> {
        let g = form(s, &mut minutely(s));
        read_groups(s, &g)
            .into_iter()
            .map(|r| (r.sources, r.labels))
            .collect()
    }

    /// A key some sources of a group carry takes the carried value; a group
    /// whose sources agree on every key is one read group.
    #[test]
    fn a_carried_label_fills_a_gap() {
        let s = vec![
            source(
                &[("host", "a"), ("arm", "x")],
                Some("u1"),
                Some((0, 10)),
                Some("e"),
            ),
            source(&[("host", "a")], Some("u2"), Some((5, 20)), Some("e")),
        ];
        assert_eq!(
            read(&s),
            vec![(vec![0, 1], labels(&[("arm", "x"), ("host", "a")]))]
        );
    }

    /// Two values of one key split the group by value; a copy lacking the
    /// key takes its uuid group's value and goes with it.
    #[test]
    fn two_values_of_a_label_split_the_group() {
        let s = vec![
            source(
                &[("host", "a"), ("arm", "x")],
                Some("u1"),
                Some((0, 10)),
                Some("e"),
            ),
            source(
                &[("host", "a"), ("arm", "y")],
                Some("u2"),
                Some((5, 20)),
                Some("e"),
            ),
            // A filtered copy of u1 without `arm`.
            source(&[("host", "a")], Some("u1"), Some((0, 10)), Some("e")),
            // A third uuid without `arm`: a part of its own.
            source(&[("host", "a")], Some("u3"), Some((8, 12)), Some("e")),
        ];
        assert_eq!(
            read(&s),
            vec![
                (vec![0, 2], labels(&[("arm", "x"), ("host", "a")])),
                (vec![1], labels(&[("arm", "y"), ("host", "a")])),
                (vec![3], labels(&[("host", "a")])),
            ]
        );
    }

    /// Sources in no group of several, and bridges, are read groups of their
    /// own sources, with their own labels.
    #[test]
    fn singles_and_bridges_are_their_own_read_groups() {
        let s = vec![
            source(
                &[("host", "a")],
                Some("c"),
                Some((9 * 60, 18 * 60)),
                Some("e"),
            ),
            source(
                &[("host", "a")],
                Some("x"),
                Some((9 * 60, 10 * 60)),
                Some("e"),
            ),
            source(
                &[("host", "a")],
                Some("y"),
                Some((17 * 60, 18 * 60)),
                Some("e"),
            ),
            source(&[("host", "b")], Some("z"), Some((0, 5)), Some("f")),
        ];
        let g = form(&s, &mut minutely(&s));
        assert_eq!(g.bridges.len(), 1);
        let r: Vec<Vec<usize>> = read_groups(&s, &g).into_iter().map(|r| r.sources).collect();
        assert_eq!(r, vec![vec![0], vec![1], vec![2], vec![3]]);
    }

    /// A key both sources hold is the first's, in either order; a key one
    /// holds is the group's in either order.
    #[test]
    fn each_key_is_the_first_source_holding_it() {
        let a = labels(&[("k", "first")]);
        let b = labels(&[("k", "second"), ("only", "x")]);
        assert_eq!(
            merge_metadata(&[&a, &b]),
            labels(&[("k", "first"), ("only", "x")])
        );
        assert_eq!(
            merge_metadata(&[&b, &a]),
            labels(&[("k", "second"), ("only", "x")])
        );
        assert_eq!(merge_metadata(&[&a]), a);
        assert!(merge_metadata(&[]).is_empty());
    }

    fn events(list: serde_json::Value) -> BTreeMap<String, String> {
        let payload = serde_json::json!({ EVENTS: list }).to_string();
        labels(&[(EVENTS, payload.as_str())])
    }

    fn merged_events(sources: &[&BTreeMap<String, String>]) -> serde_json::Value {
        let merged = merge_metadata(sources);
        let payload: serde_json::Value = serde_json::from_str(&merged[EVENTS]).unwrap();
        payload[EVENTS].clone()
    }

    /// The events of a group are every source's, each id once (the first
    /// source's copy), an event without an id once per distinct event, in
    /// timestamp order and then id order.
    #[test]
    fn events_are_unioned_by_id_and_sorted() {
        use serde_json::json;
        let a = events(json!([
            {"id": "e2", "timestamp": 20, "description": "a's"},
            {"timestamp": 5, "description": "anonymous"},
            {"id": "", "timestamp": 1, "description": "empty id"},
        ]));
        let b = events(json!([
            {"id": "e2", "timestamp": 20, "description": "b's"},
            {"id": "e1", "timestamp": 10},
            {"timestamp": 5, "description": "anonymous"},
            {"id": "", "timestamp": 1, "description": "empty id"},
            {"id": "z", "timestamp": 10},
            {"id": "c", "timestamp": 10},
            {"description": "no timestamp"},
        ]));
        let want = json!([
            {"description": "no timestamp"},
            {"id": "", "timestamp": 1, "description": "empty id"},
            {"timestamp": 5, "description": "anonymous"},
            {"id": "c", "timestamp": 10},
            {"id": "e1", "timestamp": 10},
            {"id": "z", "timestamp": 10},
            {"id": "e2", "timestamp": 20, "description": "a's"},
        ]);
        assert_eq!(merged_events(&[&a, &b]), want);
        // The other order keeps b's copy of e2, and sorts the same.
        let mut want = want;
        want[6]["description"] = json!("b's");
        assert_eq!(merged_events(&[&b, &a]), want);
    }

    /// One source's payload is the group's as it is stored, members other
    /// than `events` and the events' order included.
    #[test]
    fn one_payload_is_kept_as_written() {
        let raw =
            r#"{"events": [{"id": "b", "timestamp": 2}, {"id": "a", "timestamp": 1}], "v": 1}"#;
        let a = labels(&[("k", "v")]);
        let b = labels(&[(EVENTS, raw)]);
        assert_eq!(merge_metadata(&[&a, &b])[EVENTS], raw);
    }

    /// A payload that is not a JSON object is skipped: with one other
    /// payload, that payload is the group's; with none, the first source's
    /// value is, as for any key.
    #[test]
    fn a_payload_that_does_not_parse_is_skipped() {
        let bad = labels(&[(EVENTS, "not json")]);
        let list = labels(&[(EVENTS, "[]")]);
        // Spaced as no serializer writes it, so a payload that was merged
        // rather than kept shows.
        let good = labels(&[(EVENTS, r#"{ "events": [ {"id": "a", "timestamp": 1} ] }"#)]);
        assert_eq!(merge_metadata(&[&bad, &good])[EVENTS], good[EVENTS]);
        assert_eq!(merge_metadata(&[&list, &good])[EVENTS], good[EVENTS]);
        assert_eq!(merge_metadata(&[&bad, &list])[EVENTS], "not json");
        // Two good payloads and a bad one between them still union.
        let other = events(serde_json::json!([{"id": "b", "timestamp": 0}]));
        assert_eq!(
            merged_events(&[&good, &bad, &other]),
            serde_json::json!([{"id": "b", "timestamp": 0}, {"id": "a", "timestamp": 1}])
        );
    }
}
