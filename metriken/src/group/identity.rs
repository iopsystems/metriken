//! Which occupant holds a slot of a fixed-capacity group.
//!
//! A group's values are positional, and what slot 3 *is* (which task, which
//! cgroup, which tenant) changes while the process runs. [`SlotIdentity`]
//! is the one place that says so: it owns a slot space shared by one or more
//! groups, writes each occupant's labels to every group's slot metadata, and
//! gives each occupant a [`UID_LABEL`] that tells it apart from any earlier
//! occupant of the same slot with the same labels.
//!
//! A consumer reads the labels, uid included, from the group's slot metadata
//! when it snapshots the group. An archive keys the occupant by its uid, so a
//! PID that wraps or a cgroup recreated at the same path becomes a new series
//! rather than a continuation of the old one.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::{CounterGroup, GaugeGroup, HistogramGroup, ShardedCounterGroup};
use crate::{WindowedCounterGroup, WindowedGaugeGroup};

/// The label that names one occupant of a slot.
///
/// A slot's labels say what it means (`comm=redis pid=4112`), and two
/// different occupants can share them: a PID wraps, a cgroup is deleted and
/// recreated at the same path, a task restarts under the same name. The uid
/// is minted once per assignment and travels with the labels, so two
/// consumers of one process see the same uid for the same occupant without
/// coordinating.
///
/// Internal under the `__` rule: part of a series' identity and matchable,
/// hidden from listings and legends.
pub const UID_LABEL: &str = "__uid__";

/// A group whose slots carry metadata: what [`SlotIdentity`] writes to.
///
/// Implemented for metriken's group types. A producer with its own group type
/// implements it to put that type under a [`SlotIdentity`].
pub trait SlotMetadata: Sync {
    /// Replace the metadata of slot `idx`.
    fn set_metadata(&self, idx: usize, metadata: HashMap<String, String>);
    /// Remove the metadata of slot `idx`.
    fn clear_metadata(&self, idx: usize);
    /// Every slot's metadata.
    fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)>;
}

macro_rules! slot_metadata {
    ($($t:ty),*) => {$(
        impl SlotMetadata for $t {
            fn set_metadata(&self, idx: usize, metadata: HashMap<String, String>) {
                <$t>::set_metadata(self, idx, metadata)
            }
            fn clear_metadata(&self, idx: usize) {
                <$t>::clear_metadata(self, idx)
            }
            fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
                <$t>::metadata_snapshot(self)
            }
        }
    )*};
}

slot_metadata!(
    CounterGroup,
    GaugeGroup,
    HistogramGroup,
    ShardedCounterGroup,
    WindowedCounterGroup,
    WindowedGaugeGroup
);

/// Assignments taken by this process, each numbered once. The uid is minted
/// from the number, so it is unique within the process; the producer epoch
/// folded into it makes it unique across processes.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// A uid for the assignment numbered `generation`: FNV-1a-64 over the
/// producer epoch and the number, as sixteen hex digits.
fn mint_uid(generation: u64) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in crate::epoch::producer_epoch()
        .as_bytes()
        .iter()
        .chain(generation.to_le_bytes().iter())
    {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// A uid for a new occupant: the next assignment's number, minted. Shared by
/// [`SlotIdentity`] and the families (`crate::family`), so a uid is unique
/// across both.
pub(crate) fn next_uid() -> String {
    mint_uid(GENERATION.fetch_add(1, Ordering::AcqRel) + 1)
}

/// Per live slot: the labels it was assigned (without the uid) and its uid.
type Occupants = BTreeMap<usize, (BTreeMap<String, String>, String)>;

/// The occupants of a slot space shared by one or more groups.
///
/// ```
/// use std::collections::BTreeMap;
/// use metriken::group::{SlotIdentity, SlotMetadata, UID_LABEL};
/// use metriken::CounterGroup;
///
/// static TASK_CPU: CounterGroup = CounterGroup::new(1024);
/// static TASKS: SlotIdentity = SlotIdentity::new(&[&TASK_CPU]);
///
/// let labels = BTreeMap::from([("comm".to_string(), "redis".to_string())]);
/// let uid = TASKS.assign(7, labels);
/// assert_eq!(TASK_CPU.load_metadata(7).unwrap()[UID_LABEL], uid);
/// TASKS.release(7);
/// assert!(TASK_CPU.load_metadata(7).is_none());
/// ```
///
/// Several groups, because one slot id routinely spans them: a cgroup's id
/// indexes its CPU time, its run-queue wait and its context switches, and
/// every one of them must agree on which occupant the slot holds.
pub struct SlotIdentity {
    groups: Groups,
    live: Mutex<Occupants>,
}

/// The groups a [`SlotIdentity`] writes to, as the producer declared them.
enum Groups {
    Flat(&'static [&'static dyn SlotMetadata]),
    Grouped(&'static [&'static [&'static dyn SlotMetadata]]),
}

impl Groups {
    fn each(&self, mut f: impl FnMut(&dyn SlotMetadata)) {
        match self {
            Groups::Flat(groups) => groups.iter().for_each(|g| f(*g)),
            Groups::Grouped(lists) => lists.iter().flat_map(|l| l.iter()).for_each(|g| f(*g)),
        }
    }
}

impl SlotIdentity {
    /// A slot space over these groups.
    pub const fn new(groups: &'static [&'static dyn SlotMetadata]) -> Self {
        Self {
            groups: Groups::Flat(groups),
            live: Mutex::new(BTreeMap::new()),
        }
    }

    /// A slot space over several lists of groups, for a producer that
    /// declares its metrics per acquisition group and shares those lists
    /// between statics: `&[TASK_METRICS, &[&CGROUP_CPU]]`. A `const` context
    /// cannot concatenate the lists, so they are kept as given.
    pub const fn grouped(lists: &'static [&'static [&'static dyn SlotMetadata]]) -> Self {
        Self {
            groups: Groups::Grouped(lists),
            live: Mutex::new(BTreeMap::new()),
        }
    }

    /// Say that `slot` holds the occupant described by `labels`, in every
    /// group, and return its uid.
    ///
    /// A live slot assigned the labels it already has is the same occupant
    /// announcing itself again (a producer that re-reads a drive's or an
    /// interface's labels every refresh): the uid is kept and nothing is
    /// written. Otherwise this is a new occupant, with a new uid, even when
    /// its labels equal a previous occupant's.
    ///
    /// The labels, uid included, are written with one call per group, so a
    /// reader never sees a slot part-way through changing hands.
    pub fn assign(&self, slot: usize, labels: BTreeMap<String, String>) -> String {
        let uid = {
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((current, uid)) = live.get(&slot) {
                if *current == labels {
                    return uid.clone();
                }
            }
            let uid = next_uid();
            live.insert(slot, (labels.clone(), uid.clone()));
            uid
        };
        let mut metadata: HashMap<String, String> = labels.into_iter().collect();
        metadata.insert(UID_LABEL.to_string(), uid.clone());
        self.groups
            .each(|group| group.set_metadata(slot, metadata.clone()));
        uid
    }

    /// Say that `slot` holds nothing. The next assignment to it is a new
    /// occupant whatever its labels, which is what makes a reused PID a new
    /// series.
    pub fn release(&self, slot: usize) {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&slot);
        self.groups.each(|group| group.clear_metadata(slot));
    }

    /// Release every live slot `keep` returns false for, and return how many
    /// were released.
    ///
    /// For the occupant whose departure was never reported: an exit event
    /// dropped under load, a cgroup removed with no removal event. The
    /// producer runs this at its own cadence with a check of its own (a
    /// task's start time, a cgroup's serial number), and a slot it finds gone
    /// is released as though its departure had been seen.
    pub fn retain(&self, mut keep: impl FnMut(usize, &BTreeMap<String, String>) -> bool) -> usize {
        let gone: Vec<usize> = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(slot, (labels, _))| !keep(**slot, labels))
            .map(|(slot, _)| *slot)
            .collect();
        for slot in &gone {
            self.release(*slot);
        }
        gone.len()
    }

    /// The uid of the occupant `slot` holds, if any.
    pub fn uid(&self, slot: usize) -> Option<String> {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&slot)
            .map(|(_, uid)| uid.clone())
    }

    /// How many slots hold an occupant.
    pub fn len(&self) -> usize {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether no slot holds an occupant.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(comm: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("comm".to_string(), comm.to_string())])
    }

    fn visible(m: HashMap<String, String>) -> BTreeMap<String, String> {
        m.into_iter().filter(|(k, _)| k != UID_LABEL).collect()
    }

    static A: CounterGroup = CounterGroup::new(64);
    static B: GaugeGroup = GaugeGroup::new(64);
    static BOTH: SlotIdentity = SlotIdentity::new(&[&A, &B]);

    /// An assignment writes the labels and one uid to every group the slot
    /// spans.
    #[test]
    fn an_assignment_writes_every_group_with_one_uid() {
        let uid = BOTH.assign(1, labels("redis"));
        assert_eq!(uid.len(), 16, "{uid}");
        for m in [A.load_metadata(1).unwrap(), B.load_metadata(1).unwrap()] {
            assert_eq!(m[UID_LABEL], uid);
            assert_eq!(visible(m), labels("redis"));
        }
        assert_eq!(BOTH.uid(1).as_deref(), Some(uid.as_str()));
    }

    /// The same labels on a live slot are the same occupant: same uid, and
    /// the groups are not rewritten.
    #[test]
    fn a_re_announcement_keeps_its_uid() {
        // Its own group: the version counts every write to a group, and the
        // other tests write to theirs concurrently.
        static G: CounterGroup = CounterGroup::new(8);
        static ONE: SlotIdentity = SlotIdentity::new(&[&G]);
        let uid = ONE.assign(2, labels("sshd"));
        let version = G.metadata_version();
        assert_eq!(ONE.assign(2, labels("sshd")), uid);
        assert_eq!(G.metadata_version(), version, "nothing was written");
        assert_ne!(
            ONE.assign(2, labels("cron")),
            uid,
            "a relabel is a new occupant"
        );
    }

    /// The case the uid exists for: the same labels after a release are a
    /// different occupant.
    #[test]
    fn a_reassignment_with_identical_labels_is_a_different_occupant() {
        let first = BOTH.assign(3, labels("nginx"));
        BOTH.release(3);
        assert!(A.load_metadata(3).is_none() && B.load_metadata(3).is_none());
        assert_eq!(BOTH.uid(3), None);
        let second = BOTH.assign(3, labels("nginx"));
        assert_ne!(first, second);
    }

    /// A grouped identity writes every metric of every list.
    #[test]
    fn a_grouped_identity_writes_every_list() {
        static C: CounterGroup = CounterGroup::new(4);
        static D: GaugeGroup = GaugeGroup::new(4);
        static E: CounterGroup = CounterGroup::new(4);
        static FIRST: &[&dyn SlotMetadata] = &[&C, &D];
        static LISTS: SlotIdentity = SlotIdentity::grouped(&[FIRST, &[&E]]);
        let uid = LISTS.assign(1, labels("x"));
        for m in [C.load_metadata(1), D.load_metadata(1), E.load_metadata(1)] {
            assert_eq!(m.unwrap()[UID_LABEL], uid);
        }
        LISTS.release(1);
        assert!(
            C.load_metadata(1).is_none()
                && D.load_metadata(1).is_none()
                && E.load_metadata(1).is_none()
        );
    }

    /// `retain` releases the slots its check reports gone, in every group.
    #[test]
    fn retain_releases_what_the_check_reports_gone() {
        static C: CounterGroup = CounterGroup::new(8);
        static ONE: SlotIdentity = SlotIdentity::new(&[&C]);
        ONE.assign(1, labels("alive"));
        ONE.assign(2, labels("phantom"));
        ONE.assign(3, labels("alive"));
        let released = ONE.retain(|_, l| l["comm"] == "alive");
        assert_eq!(released, 1);
        assert_eq!(ONE.len(), 2);
        assert!(C.load_metadata(2).is_none());
        assert!(C.load_metadata(1).is_some() && C.load_metadata(3).is_some());
    }

    /// Uids are distinct across many assignments of one slot.
    #[test]
    fn uids_do_not_repeat() {
        static D: CounterGroup = CounterGroup::new(1);
        static ONE: SlotIdentity = SlotIdentity::new(&[&D]);
        let mut seen = std::collections::HashSet::new();
        for i in 0..10_000 {
            ONE.release(0);
            assert!(seen.insert(ONE.assign(0, labels(&format!("t{}", i % 3)))));
        }
    }
}
