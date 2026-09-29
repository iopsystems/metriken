//! Families: one metric whose members are created and dropped at runtime.
//!
//! A service with a counter per tenant, per route or per connection wants a
//! member when the tenant appears and none once it leaves. A family is one
//! registered metric that owns its members: `member(labels)` takes a slot
//! and returns a handle, updates go straight to the handle's atomic, and
//! dropping the handle frees the slot.
//!
//! A family answers as a group ([`Value::CounterGroup`],
//! [`Value::GaugeGroup`]), so anything that reads groups reads families: a
//! member is a slot, and its labels are the slot's metadata. Each member's
//! labels carry a [`UID_LABEL`](crate::group::UID_LABEL) minted when it is
//! created, so a slot freed and taken again holds a new occupant, which an
//! archive stores as a new series.
//!
//! Why a family keeps its own table rather than registering a metric per
//! member: measured in metriken's
//! `docs/journal/2026-09-29-members-that-come-and-go.md`, a registry entry per
//! member costs about 650 B, and a snapshot holds the registry's global guard
//! for its whole walk (178 ms at 1M members), which a member created
//! mid-snapshot waits behind. A family is one registry entry. Reading it
//! copies each member's label reference and value under the family's lock and
//! releases it before any consumer code runs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use metriken_core::{CounterGroupMetric, GaugeGroupMetric, Metric, Value};

use crate::group::UID_LABEL;

/// A member's labels, shared between the family's table and its readers.
type Labels = Arc<HashMap<String, String>>;

struct Slot<C> {
    labels: Labels,
    cell: Arc<C>,
}

struct Table<C> {
    slots: Vec<Option<Slot<C>>>,
    free: Vec<usize>,
    live: usize,
    /// Bumped on every member created or dropped: what a reader caching a
    /// schema checks.
    version: u64,
}

impl<C> Table<C> {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            live: 0,
            version: 0,
        }
    }

    fn insert(&mut self, labels: Labels, cell: Arc<C>) -> usize {
        let slot = Slot { labels, cell };
        let idx = match self.free.pop() {
            Some(idx) => {
                self.slots[idx] = Some(slot);
                idx
            }
            None => {
                self.slots.push(Some(slot));
                self.slots.len() - 1
            }
        };
        self.live += 1;
        self.version += 1;
        idx
    }

    fn remove(&mut self, idx: usize) {
        if self.slots[idx].take().is_some() {
            self.free.push(idx);
            self.live -= 1;
            self.version += 1;
        }
    }
}

/// The table behind a family, with the reads a group consumer makes.
struct Members<C> {
    table: RwLock<Table<C>>,
}

impl<C> Members<C> {
    const fn new() -> Self {
        Self {
            table: RwLock::new(Table::new()),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Table<C>> {
        self.table.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Table<C>> {
        self.table.write().unwrap_or_else(|e| e.into_inner())
    }

    fn create(
        &self,
        labels: impl IntoIterator<Item = (String, String)>,
        cell: C,
    ) -> (usize, Arc<C>) {
        let mut labels: HashMap<String, String> = labels.into_iter().collect();
        labels.insert(UID_LABEL.to_string(), crate::group::identity::next_uid());
        let cell = Arc::new(cell);
        let idx = self.write().insert(Arc::new(labels), Arc::clone(&cell));
        (idx, cell)
    }

    fn entries(&self) -> usize {
        self.read().slots.len()
    }

    fn len(&self) -> usize {
        self.read().live
    }

    fn version(&self) -> u64 {
        self.read().version
    }

    fn cell(&self, idx: usize) -> Option<Arc<C>> {
        self.read()
            .slots
            .get(idx)?
            .as_ref()
            .map(|s| Arc::clone(&s.cell))
    }

    fn labels(&self, idx: usize) -> Option<Labels> {
        self.read()
            .slots
            .get(idx)?
            .as_ref()
            .map(|s| Arc::clone(&s.labels))
    }

    /// Every live member's slot and label reference, copied under the lock so
    /// a consumer's callback runs without it.
    fn all_labels(&self) -> Vec<(usize, Labels)> {
        self.read()
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|s| (i, Arc::clone(&s.labels))))
            .collect()
    }

    fn load<V>(&self, empty: V, get: impl Fn(&C) -> V) -> Vec<V>
    where
        V: Copy,
    {
        self.read()
            .slots
            .iter()
            .map(|s| s.as_ref().map_or(empty, |s| get(&s.cell)))
            .collect()
    }
}

macro_rules! family_reads {
    () => {
        /// How many members exist.
        pub fn len(&self) -> usize {
            self.members.len()
        }

        /// Whether the family has no members.
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }
    };
}

macro_rules! group_metadata_reads {
    () => {
        fn entries(&self) -> usize {
            self.members.entries()
        }

        fn load_metadata(&self, idx: usize) -> Option<HashMap<String, String>> {
            self.members.labels(idx).map(|l| (*l).clone())
        }

        fn metadata_snapshot(&self) -> Vec<(usize, HashMap<String, String>)> {
            self.members
                .all_labels()
                .into_iter()
                .map(|(i, l)| (i, (*l).clone()))
                .collect()
        }

        fn metadata_version(&self) -> u64 {
            self.members.version()
        }

        fn with_metadata(&self, idx: usize, f: &mut dyn FnMut(Option<&HashMap<String, String>>)) {
            let labels = self.members.labels(idx);
            f(labels.as_deref());
        }

        fn for_each_metadata(&self, f: &mut dyn FnMut(usize, &HashMap<String, String>)) {
            for (i, labels) in self.members.all_labels() {
                f(i, &labels);
            }
        }
    };
}

/// A family of counters: one metric, a counter per member.
///
/// ```
/// use metriken::{metric, CounterFamily};
///
/// #[metric(name = "requests")]
/// static REQUESTS: CounterFamily = CounterFamily::new();
///
/// let acme = REQUESTS.member([("tenant", "acme")]);
/// acme.increment();
/// assert_eq!(acme.value(), 1);
/// drop(acme); // the member is gone from the family
/// assert!(REQUESTS.is_empty());
/// ```
pub struct CounterFamily {
    members: Members<AtomicU64>,
}

impl CounterFamily {
    pub const fn new() -> Self {
        Self {
            members: Members::new(),
        }
    }

    /// A new member with these labels. It starts at zero and leaves the
    /// family when the returned handle is dropped.
    pub fn member<K, V>(&self, labels: impl IntoIterator<Item = (K, V)>) -> CounterMember<'_>
    where
        K: Into<String>,
        V: Into<String>,
    {
        let (idx, cell) = self.members.create(
            labels.into_iter().map(|(k, v)| (k.into(), v.into())),
            AtomicU64::new(0),
        );
        CounterMember {
            family: self,
            idx,
            cell,
        }
    }

    family_reads!();
}

impl Default for CounterFamily {
    fn default() -> Self {
        Self::new()
    }
}

/// One member of a [`CounterFamily`]. Dropping it removes the member.
pub struct CounterMember<'a> {
    family: &'a CounterFamily,
    idx: usize,
    cell: Arc<AtomicU64>,
}

impl CounterMember<'_> {
    pub fn increment(&self) -> u64 {
        self.add(1)
    }

    /// Add `value`, returning the previous value.
    pub fn add(&self, value: u64) -> u64 {
        self.cell.fetch_add(value, Ordering::Relaxed)
    }

    pub fn value(&self) -> u64 {
        self.cell.load(Ordering::Relaxed)
    }

    /// The member's slot in the family, as a group reader sees it.
    pub fn slot(&self) -> usize {
        self.idx
    }
}

impl Drop for CounterMember<'_> {
    fn drop(&mut self) {
        self.family.members.write().remove(self.idx);
    }
}

impl CounterGroupMetric for CounterFamily {
    group_metadata_reads!();

    fn counter_value(&self, idx: usize) -> Option<u64> {
        self.members.cell(idx).map(|c| c.load(Ordering::Relaxed))
    }

    fn load_counters(&self) -> Option<Vec<u64>> {
        Some(self.members.load(u64::MAX, |c| c.load(Ordering::Relaxed)))
    }
}

impl Metric for CounterFamily {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::CounterGroup(self))
    }
}

/// A family of gauges: one metric, a gauge per member.
pub struct GaugeFamily {
    members: Members<AtomicI64>,
}

impl GaugeFamily {
    pub const fn new() -> Self {
        Self {
            members: Members::new(),
        }
    }

    /// A new member with these labels. It starts at zero and leaves the
    /// family when the returned handle is dropped.
    pub fn member<K, V>(&self, labels: impl IntoIterator<Item = (K, V)>) -> GaugeMember<'_>
    where
        K: Into<String>,
        V: Into<String>,
    {
        let (idx, cell) = self.members.create(
            labels.into_iter().map(|(k, v)| (k.into(), v.into())),
            AtomicI64::new(0),
        );
        GaugeMember {
            family: self,
            idx,
            cell,
        }
    }

    family_reads!();
}

impl Default for GaugeFamily {
    fn default() -> Self {
        Self::new()
    }
}

/// One member of a [`GaugeFamily`]. Dropping it removes the member.
pub struct GaugeMember<'a> {
    family: &'a GaugeFamily,
    idx: usize,
    cell: Arc<AtomicI64>,
}

impl GaugeMember<'_> {
    pub fn set(&self, value: i64) -> i64 {
        self.cell.swap(value, Ordering::Relaxed)
    }

    pub fn add(&self, value: i64) -> i64 {
        self.cell.fetch_add(value, Ordering::Relaxed)
    }

    pub fn value(&self) -> i64 {
        self.cell.load(Ordering::Relaxed)
    }

    /// The member's slot in the family, as a group reader sees it.
    pub fn slot(&self) -> usize {
        self.idx
    }
}

impl Drop for GaugeMember<'_> {
    fn drop(&mut self) {
        self.family.members.write().remove(self.idx);
    }
}

impl GaugeGroupMetric for GaugeFamily {
    group_metadata_reads!();

    fn gauge_value(&self, idx: usize) -> Option<i64> {
        self.members.cell(idx).map(|c| c.load(Ordering::Relaxed))
    }

    fn load_gauges(&self) -> Option<Vec<i64>> {
        Some(self.members.load(i64::MIN, |c| c.load(Ordering::Relaxed)))
    }
}

impl Metric for GaugeFamily {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn value(&self) -> Option<Value<'_>> {
        Some(Value::GaugeGroup(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A member is a slot with its labels and a uid; dropping it frees the
    /// slot, and the next member there is a new occupant.
    #[test]
    fn members_come_and_go() {
        static F: CounterFamily = CounterFamily::new();
        let a = F.member([("tenant", "a")]);
        let b = F.member([("tenant", "b")]);
        a.add(5);
        b.increment();
        assert_eq!(F.len(), 2);
        assert_eq!(F.counter_value(a.slot()), Some(5));
        let la = F.load_metadata(a.slot()).unwrap();
        assert_eq!(la["tenant"], "a");
        let uid_a = la[UID_LABEL].clone();
        assert_eq!(uid_a.len(), 16);

        let slot = a.slot();
        let version = F.metadata_version();
        drop(a);
        assert_ne!(
            F.metadata_version(),
            version,
            "a drop is a membership change"
        );
        assert_eq!(F.counter_value(slot), None);
        assert_eq!(
            F.load_counters().unwrap()[slot],
            u64::MAX,
            "an empty slot reads as the sentinel"
        );

        let again = F.member([("tenant", "a")]);
        assert_eq!(again.slot(), slot, "the freed slot is taken again");
        assert_ne!(
            F.load_metadata(slot).unwrap()[UID_LABEL],
            uid_a,
            "by a new occupant"
        );
        assert_eq!(again.value(), 0);
    }

    /// A consumer reading every member's labels sees each live member once.
    #[test]
    fn for_each_metadata_sees_every_live_member() {
        static F: GaugeFamily = GaugeFamily::new();
        let members: Vec<_> = (0..100).map(|i| F.member([("n", i.to_string())])).collect();
        members[7].set(-3);
        let (even, odd): (Vec<_>, Vec<_>) = members.into_iter().partition(|m| m.slot() % 2 == 0);
        drop(even);
        let mut seen = Vec::new();
        F.for_each_metadata(&mut |i, l| seen.push((i, l["n"].clone())));
        seen.sort();
        assert_eq!(seen.len(), 50);
        assert!(seen.iter().all(|(i, l)| i % 2 == 1 && *l == i.to_string()));
        assert_eq!(F.gauge_value(7), Some(-3));
        assert_eq!(F.gauge_value(8), None);
        drop(odd);
        assert!(F.is_empty());
    }

    /// A member created while another thread reads the family is not held
    /// behind that read for longer than copying the labels.
    #[test]
    fn members_are_created_while_the_family_is_read() {
        static F: CounterFamily = CounterFamily::new();
        let base: Vec<_> = (0..10_000)
            .map(|i| F.member([("n", i.to_string())]))
            .collect();
        std::thread::scope(|s| {
            s.spawn(|| {
                for _ in 0..50 {
                    let mut n = 0;
                    F.for_each_metadata(&mut |_, _| n += 1);
                    assert!(n >= 10_000);
                }
            });
            for i in 0..1000 {
                let m = F.member([("extra", i.to_string())]);
                m.increment();
            }
        });
        assert_eq!(F.len(), base.len());
    }
}
