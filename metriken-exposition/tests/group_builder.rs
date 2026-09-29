//! The group builder against a router shaped like rezolus's: groups declared
//! in a registry keyed by `(namespace, name)`, a metric naming its group in
//! `acq_group` metadata, undeclared metrics in `{namespace}/main`, and three
//! window sources (none, stamped by a writer, the builder's own read).
//!
//! Ported from rezolus's `create_v3` tests; only the fixture (this file's
//! `TestGroup` in place of rezolus's `AcquisitionGroup`) is new. Every test
//! uses groups of its own, since tests share one registry.

#![cfg(feature = "msgpack")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use metriken::{metric, MetricEntry, Window};
use metriken_exposition::group_builder::{
    is_family, Acquisition, ByName, DefaultRouter, ExtraGroup, GroupBuilder, GroupId, Membership,
    ReadGuard, Route, Router, Stamp, GROUP_METADATA_KEY,
};
use metriken_exposition::{GroupSchema, GroupSnapshot, MetricDesc};

// --- allocation counting ----------------------------------------------------
//
// Counts allocations made on the current thread while enabled. Thread-local,
// so other tests running in parallel in this binary are not counted.

struct CountingAllocator;

thread_local! {
    static ALLOC_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ALLOC_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if ALLOC_ENABLED.with(|e| e.get()) {
            ALLOC_COUNT.with(|c| c.set(c.get() + 1));
        }
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        if ALLOC_ENABLED.with(|e| e.get()) {
            ALLOC_COUNT.with(|c| c.set(c.get() + 1));
        }
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING_ALLOCATOR: CountingAllocator = CountingAllocator;

fn count_allocations<T>(f: impl FnOnce() -> T) -> (T, usize) {
    ALLOC_COUNT.with(|c| c.set(0));
    ALLOC_ENABLED.with(|e| e.set(true));
    let result = f();
    ALLOC_ENABLED.with(|e| e.set(false));
    (result, ALLOC_COUNT.with(|c| c.get()))
}

// --- the fixture: a declared group ----------------------------------------------

const NS: &str = "unattributed";

/// Stands in for rezolus's `AcquisitionGroup`: a name, a window slot, a
/// member bound or set, and whether the builder's read is the acquisition.
struct TestGroup {
    name: &'static str,
    window: Mutex<Option<Window>>,
    bound: AtomicUsize,
    set: OnceLock<Vec<usize>>,
    reader_stamped: AtomicBool,
}

impl TestGroup {
    const fn new(name: &'static str) -> Self {
        Self {
            name,
            window: Mutex::new(None),
            bound: AtomicUsize::new(usize::MAX),
            set: OnceLock::new(),
            reader_stamped: AtomicBool::new(false),
        }
    }

    const fn new_reader_stamped(name: &'static str) -> Self {
        Self {
            name,
            window: Mutex::new(None),
            bound: AtomicUsize::new(usize::MAX),
            set: OnceLock::new(),
            reader_stamped: AtomicBool::new(true),
        }
    }

    fn set_member_bound(&self, n: usize) {
        self.bound.store(n, Ordering::Relaxed);
    }

    fn set_reader_stamped(&self) {
        self.reader_stamped.store(true, Ordering::Relaxed);
    }

    fn acquire(&'static self) -> TestGuard {
        let begin = Instant::now();
        TestGuard {
            group: self,
            begin_ns: metriken::epoch::anchored_ts(begin).max(0) as u64,
            begin,
            marked_end_ns: None,
        }
    }

    fn window(&self) -> Option<Window> {
        *self.window.lock().unwrap()
    }

    fn membership(&'static self) -> Membership<'static> {
        if self.reader_stamped.load(Ordering::Relaxed) {
            Membership::Slots
        } else if let Some(set) = self.set.get() {
            Membership::Set(set)
        } else {
            match self.bound.load(Ordering::Relaxed) {
                usize::MAX => Membership::All,
                n => Membership::Prefix(n),
            }
        }
    }
}

/// Stands in for rezolus's `AcquisitionGuard`.
struct TestGuard {
    group: &'static TestGroup,
    begin_ns: u64,
    begin: Instant,
    marked_end_ns: Option<u64>,
}

impl TestGuard {
    fn finish(self) -> Option<Window> {
        let end_ns = self
            .marked_end_ns
            .unwrap_or_else(|| self.begin_ns + self.begin.elapsed().as_nanos() as u64);
        let w = Window::new(self.begin_ns, end_ns);
        *self.group.window.lock().unwrap() = Some(w);
        Some(w)
    }

    fn discard(self) {}
}

impl ReadGuard for TestGuard {
    fn mark_end(&mut self) {
        self.marked_end_ns = Some(self.begin_ns + self.begin.elapsed().as_nanos() as u64);
    }

    fn finish(self) -> Option<Window> {
        TestGuard::finish(self)
    }
}

/// Every declared group in this binary.
static GROUPS: &[&TestGroup] = &[
    &PROBE_GROUP,
    &STABILITY_GROUP,
    &SLOT_ORDER_GROUP,
    &RECYCLE_GROUP,
    &ALLOC_GROUP,
    &SPARSE_CHURN_GROUP,
    &UNUSED_GROUP,
    &UNHANDLED_GROUP,
    &DECLARED_COUNTER_GROUP,
    &BOUNDED_GROUP,
    &OVERBOUND_GROUP,
    &READER_STAMPED_GROUP,
    &MAX_PID_SCALE_GROUP,
    &SPARSE_GROUP,
    &SWEEP_GROUP,
    &SWEEP_DISCARD_GROUP,
];

fn group(name: &str) -> Option<&'static TestGroup> {
    GROUPS.iter().copied().find(|g| g.name == name)
}

/// rezolus's routing: a declared `acq_group` wins if it is registered,
/// otherwise the namespace's `main` group with value-derived membership.
/// `log_` metrics are not exposed.
struct TestRouter;

impl Router for TestRouter {
    type Guard = TestGuard;

    fn route<'a>(&'a self, metric: &'a MetricEntry) -> Option<Route<'a>> {
        if metric.name().starts_with("log_") {
            return None;
        }
        match metric.metadata().get(GROUP_METADATA_KEY).and_then(group) {
            Some(g) => Some(Route {
                group: GroupId::new(NS, g.name),
                membership: g.membership(),
            }),
            None => Some(Route {
                group: GroupId::new(NS, "main"),
                membership: Membership::Present,
            }),
        }
    }

    fn acquire(&self, id: GroupId<'_>) -> Acquisition<TestGuard> {
        match group(id.name).filter(|_| id.namespace == NS) {
            Some(g) if g.reader_stamped.load(Ordering::Relaxed) => Acquisition::Reader(g.acquire()),
            Some(g) => Acquisition::Stamped(g.window()),
            None => Acquisition::Windowless,
        }
    }

    fn window(&self, id: GroupId<'_>) -> Option<Window> {
        group(id.name)
            .filter(|_| id.namespace == NS)
            .and_then(|g| g.window())
    }

    fn annotate(&self, _: &MetricEntry, metadata: &mut BTreeMap<String, String>) {
        metadata.insert("sampler".to_string(), NS.to_string());
    }
}

/// One build at a time, as in production, where one snapshot builder runs.
/// A reader-stamped group's window slot is written by every build, so two
/// overlapping builds could publish out of order.
static BUILD_LOCK: Mutex<()> = Mutex::new(());

fn builder() -> GroupBuilder<TestRouter> {
    GroupBuilder::new(TestRouter)
}

fn build(b: &mut GroupBuilder<TestRouter>) -> Vec<GroupSnapshot> {
    let _one = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    b.build_groups(Vec::new())
}

fn find<'s>(groups: &'s [GroupSnapshot], name: &str) -> &'s GroupSnapshot {
    groups
        .iter()
        .find(|g| g.name == name)
        .unwrap_or_else(|| panic!("group `{name}` present"))
}

fn metric_name(d: &MetricDesc) -> Option<&str> {
    d.metadata.get("metric").map(String::as_str)
}

// --- declared and default groups ------------------------------------------------

static PROBE_GROUP: TestGroup = TestGroup::new("probe");

#[metric(name = "snapshot_v3_probe_counter", metadata = { acq_group = "probe" })]
static PROBE_COUNTER: metriken::Counter = metriken::Counter::new();

#[metric(name = "snapshot_sampler_label_probe")]
static SAMPLER_LABEL_PROBE: metriken::Counter = metriken::Counter::new();

#[test]
fn a_declared_group_carries_its_window_and_a_valid_schema() {
    PROBE_COUNTER.increment();
    PROBE_GROUP.acquire().finish();

    let groups = build(&mut builder());
    for g in &groups {
        assert_eq!(
            g.validate(),
            Ok(()),
            "group `{}` failed to validate",
            g.name
        );
    }

    let group = find(&groups, "unattributed/probe");
    assert!(group.window.is_some(), "declared group carries a window");
    let schema = group.schema.as_ref().expect("schema present");
    let idx = schema
        .counters
        .iter()
        .position(|d| metric_name(d) == Some("snapshot_v3_probe_counter"))
        .expect("probe counter present in schema");
    assert!(group.counters[idx].is_some());
    assert!(
        !schema.counters[idx].metadata.contains_key("acq_group"),
        "acq_group repeats the group's own name and is stripped from each member"
    );
    assert_eq!(
        schema.counters[idx]
            .metadata
            .get("sampler")
            .map(String::as_str),
        Some(NS),
        "the router's annotation is on the member"
    );
}

#[test]
fn undeclared_metrics_land_in_a_windowless_default_group() {
    SAMPLER_LABEL_PROBE.increment();
    let groups = build(&mut builder());
    let group = find(&groups, "unattributed/main");
    assert_eq!(group.validate(), Ok(()));
    assert!(group.window.is_none(), "default group carries no window");
    assert!(group
        .schema
        .as_ref()
        .unwrap()
        .counters
        .iter()
        .any(|d| metric_name(d) == Some("snapshot_sampler_label_probe")));
}

#[metric(name = "log_snapshot_filtered_probe")]
static LOG_FILTERED_PROBE: metriken::Counter = metriken::Counter::new();

/// A metric the router declines is in no group.
#[test]
fn a_metric_the_router_declines_is_not_exposed() {
    LOG_FILTERED_PROBE.increment();
    let groups = build(&mut builder());
    for g in &groups {
        let schema = g.schema.as_ref().unwrap();
        assert!(!schema
            .counters
            .iter()
            .any(|d| metric_name(d) == Some("log_snapshot_filtered_probe")));
    }
}

static STABILITY_GROUP: TestGroup = TestGroup::new("stability_probe");

#[metric(name = "snapshot_v3_stability_probe", metadata = { acq_group = "stability_probe" })]
static STABILITY_PROBE: metriken::Counter = metriken::Counter::new();

#[test]
fn the_schema_hash_is_stable_across_builds() {
    STABILITY_PROBE.increment();
    STABILITY_GROUP.acquire().finish();

    let mut b = builder();
    let s1 = build(&mut b);
    assert!(b.rebuilds() > 0, "the first build builds every schema");
    let s2 = build(&mut b);

    // Scoped to this test's own group: `rebuilds()` counts every group, and
    // other tests change theirs concurrently.
    let hash1 = find(&s1, "unattributed/stability_probe").schema_hash;
    let group2 = find(&s2, "unattributed/stability_probe");
    assert_eq!(hash1, group2.schema_hash);
    assert_eq!(group2.validate(), Ok(()));
}

#[test]
fn member_names_are_unique_across_groups() {
    let groups = build(&mut builder());
    let mut seen = std::collections::HashSet::new();
    for g in &groups {
        let schema = g.schema.as_ref().unwrap();
        for d in schema
            .counters
            .iter()
            .chain(&schema.gauges)
            .chain(&schema.histograms)
        {
            assert!(
                seen.insert(d.name.clone()),
                "duplicate MetricDesc.name `{}` (group `{}`)",
                d.name,
                g.name
            );
        }
    }
}

// --- slots ------------------------------------------------------------------------

static SLOT_ORDER_GROUP: TestGroup = TestGroup::new_reader_stamped("slot_order_probe");

#[metric(name = "snapshot_v3_slot_order_a", metadata = { acq_group = "slot_order_probe" })]
static SLOT_ORDER_A: metriken::CounterGroup = metriken::CounterGroup::new(16);

#[metric(name = "snapshot_v3_slot_order_b", metadata = { acq_group = "slot_order_probe" })]
static SLOT_ORDER_B: metriken::CounterGroup = metriken::CounterGroup::new(16);

/// Within a metric, members come out in ascending slot order (a row's values
/// are positional); and every metric of a group agrees on what a slot means.
#[test]
fn a_groups_metrics_agree_on_slot_order_and_on_what_each_slot_means() {
    for (slot, comm) in [(9usize, "nine"), (2, "two"), (13, "thirteen")] {
        for metric in [&SLOT_ORDER_A, &SLOT_ORDER_B] {
            metric.set(slot, 1);
            metric.set_metadata(
                slot,
                [
                    ("comm".to_string(), comm.to_string()),
                    ("cgroup".to_string(), "/system.slice".to_string()),
                ]
                .into(),
            );
        }
    }

    let groups = build(&mut builder());
    let group = find(&groups, "unattributed/slot_order_probe");
    let schema = group.schema.as_ref().unwrap();

    let mut by_metric: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    type SlotClaim<'a> = (&'a str, BTreeMap<&'a str, &'a str>);
    let mut labels_by_slot: BTreeMap<u32, Vec<SlotClaim<'_>>> = BTreeMap::new();
    for desc in &schema.counters {
        let Some(metric) = metric_name(desc) else {
            continue;
        };
        let slot: u32 = desc
            .name
            .split_once('x')
            .expect("a group member's name is `{metric_id}x{slot}`")
            .1
            .parse()
            .unwrap();
        by_metric.entry(metric).or_default().push(slot);
        let identity: BTreeMap<&str, &str> = desc
            .metadata
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "metric" | "sampler" | "id"))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        labels_by_slot
            .entry(slot)
            .or_default()
            .push((metric, identity));
    }

    assert_eq!(by_metric.len(), 2, "both metrics emitted members");
    for (metric, slots) in &by_metric {
        assert_eq!(
            slots,
            &vec![2, 9, 13],
            "`{metric}`: the populated slots, ascending"
        );
    }
    for (slot, seen) in &labels_by_slot {
        let (first_metric, first) = &seen[0];
        let expected = match slot {
            2 => "two",
            9 => "nine",
            13 => "thirteen",
            other => panic!("unexpected slot {other}"),
        };
        assert_eq!(first.get("comm").copied(), Some(expected));
        for (metric, identity) in &seen[1..] {
            assert_eq!(
                identity, first,
                "slot {slot} means one thing to `{first_metric}` and another to `{metric}`"
            );
        }
    }
    assert_eq!(labels_by_slot.len(), 3);
}

static RECYCLE_GROUP: TestGroup = TestGroup::new("recycle_probe");

#[metric(name = "snapshot_v3_recycle_probe", metadata = { acq_group = "recycle_probe" })]
static RECYCLE_COUNTERS: metriken::CounterGroup = metriken::CounterGroup::new(1);

/// Metadata changed at a stable index (a slot's occupant replaced) is a
/// schema change: new labels and a new hash, although the member's name is
/// the same. Mutated through metriken directly, so the signal is the store's
/// version and not any particular writer.
#[test]
fn a_declared_group_schema_reflects_metadata_mutated_at_a_stable_index() {
    RECYCLE_COUNTERS.set(0, 1);
    RECYCLE_COUNTERS.set_metadata(0, [("comm".to_string(), "old_task".to_string())].into());
    RECYCLE_GROUP.acquire().finish();

    let mut b = builder();
    let s1 = build(&mut b);
    let group1 = find(&s1, "unattributed/recycle_probe");
    let desc1 = group1.schema.as_ref().unwrap().counters[0].clone();
    assert_eq!(
        desc1.metadata.get("comm").map(String::as_str),
        Some("old_task")
    );

    RECYCLE_COUNTERS.set(0, 1);
    RECYCLE_COUNTERS.set_metadata(0, [("comm".to_string(), "new_task".to_string())].into());
    RECYCLE_GROUP.acquire().finish();

    let s2 = build(&mut b);
    let group2 = find(&s2, "unattributed/recycle_probe");
    let desc2 = &group2.schema.as_ref().unwrap().counters[0];
    assert_eq!(desc2.name, desc1.name, "same slot, same member name");
    assert_eq!(
        desc2.metadata.get("comm").map(String::as_str),
        Some("new_task")
    );
    assert_ne!(group1.schema_hash, group2.schema_hash);
}

// --- allocation on a hit --------------------------------------------------------------

const ALLOC_TEST_SMALL_N: usize = 8;
const ALLOC_TEST_LARGE_N: usize = 512;

static ALLOC_GROUP: TestGroup = TestGroup::new("alloc_probe");

#[metric(name = "snapshot_v3_alloc_counters", metadata = { acq_group = "alloc_probe" })]
static ALLOC_COUNTERS: metriken::CounterGroup = metriken::CounterGroup::new(ALLOC_TEST_LARGE_N);

fn grow_alloc_probe(from: usize, to: usize) {
    for idx in from..to {
        ALLOC_COUNTERS.add(idx, idx as u64 + 1);
        ALLOC_COUNTERS.set_metadata(idx, [("cpu".to_string(), idx.to_string())].into());
    }
    ALLOC_GROUP.set_member_bound(to);
    ALLOC_GROUP.acquire().finish();
}

const HIT_SAMPLES: usize = 5;

/// Warm the cache, then measure a hit build `HIT_SAMPLES` times and keep the
/// smallest count. Other tests change their own groups between builds, which
/// makes this build pay a miss for a group it does not own; that can only
/// add allocations, so the minimum converges on the clean cost.
fn warm_then_measure_hit(b: &mut GroupBuilder<TestRouter>) -> (Vec<GroupSnapshot>, usize) {
    let _ = build(b);
    let mut best = usize::MAX;
    let mut snapshot = None;
    for _ in 0..HIT_SAMPLES {
        let (snap, allocs) = count_allocations(|| build(b));
        best = best.min(allocs);
        snapshot = Some(snap);
    }
    (snapshot.unwrap(), best)
}

/// A hit reuses the cached `Arc<GroupSchema>` and allocates nothing per
/// member: growing a group from 8 to 512 members does not move a hit
/// build's allocation count. The two builds walk the same registry, so
/// comparing them cancels the cost of every other group.
#[test]
fn hit_allocations_are_a_small_constant_not_o_n() {
    let mut b = builder();

    // Settle lazily initialized state before measuring, with a throwaway
    // builder so this one's first build is still a miss.
    {
        let mut settle = builder();
        for _ in 0..3 {
            let _ = build(&mut settle);
        }
    }

    grow_alloc_probe(0, ALLOC_TEST_SMALL_N);
    let (snap_small, small_allocs) = warm_then_measure_hit(&mut b);
    let schema_small = find(&snap_small, "unattributed/alloc_probe")
        .schema
        .clone()
        .unwrap();
    assert_eq!(schema_small.counters.len(), ALLOC_TEST_SMALL_N);

    grow_alloc_probe(ALLOC_TEST_SMALL_N, ALLOC_TEST_LARGE_N);
    let (snap_large, large_allocs) = warm_then_measure_hit(&mut b);
    let group_large = find(&snap_large, "unattributed/alloc_probe");
    let schema_large = group_large.schema.as_ref().unwrap();
    assert_eq!(schema_large.counters.len(), ALLOC_TEST_LARGE_N);
    assert!(!Arc::ptr_eq(&schema_small, schema_large));

    let again = build(&mut b);
    let group_again = find(&again, "unattributed/alloc_probe");
    assert!(
        Arc::ptr_eq(schema_large, group_again.schema.as_ref().unwrap()),
        "an unchanged 512-member group reuses the cached Arc<GroupSchema>"
    );
    assert_eq!(group_large.schema_hash, group_again.schema_hash);
    assert_eq!(group_large.counters, group_again.counters);

    let delta = large_allocs.abs_diff(small_allocs);
    assert!(
        delta <= 100,
        "growing this group from {ALLOC_TEST_SMALL_N} to {ALLOC_TEST_LARGE_N} members \
         changed hit allocations by {delta} ({small_allocs} -> {large_allocs}); a \
         per-member allocation would move this by over a thousand"
    );
}

// --- membership changes -----------------------------------------------------------------

static SPARSE_CHURN_GROUP: TestGroup = TestGroup::new_reader_stamped("sparse_churn_probe");

#[metric(name = "snapshot_v3_sparse_churn_counters", metadata = { acq_group = "sparse_churn_probe" })]
static SPARSE_CHURN_COUNTERS: metriken::CounterGroup = metriken::CounterGroup::new(64);

/// An unchanged slot population is a hit (the same allocation); a member
/// appearing or leaving is a miss (a new allocation and a new hash).
#[test]
fn slot_membership_unchanged_hits_changed_misses() {
    SPARSE_CHURN_COUNTERS.set_metadata(3, [("cgroup".to_string(), "/a".to_string())].into());
    SPARSE_CHURN_COUNTERS.add(3, 1);
    SPARSE_CHURN_COUNTERS.set_metadata(40, [("cgroup".to_string(), "/b".to_string())].into());
    SPARSE_CHURN_COUNTERS.add(40, 2);

    let name = "unattributed/sparse_churn_probe";
    let mut b = builder();
    let s1 = build(&mut b);
    let group1 = find(&s1, name);
    let schema1 = group1.schema.clone().unwrap();
    assert_eq!(schema1.counters.len(), 2);

    let s2 = build(&mut b);
    assert!(Arc::ptr_eq(
        &schema1,
        find(&s2, name).schema.as_ref().unwrap()
    ));

    SPARSE_CHURN_COUNTERS.set_metadata(50, [("cgroup".to_string(), "/c".to_string())].into());
    SPARSE_CHURN_COUNTERS.add(50, 3);
    let s3 = build(&mut b);
    let group3 = find(&s3, name);
    let schema3 = group3.schema.as_ref().unwrap();
    assert_eq!(schema3.counters.len(), 3);
    assert!(!Arc::ptr_eq(&schema1, schema3));
    assert_ne!(group1.schema_hash, group3.schema_hash);

    SPARSE_CHURN_COUNTERS.clear_metadata(50);
    let s4 = build(&mut b);
    let group4 = find(&s4, name);
    let schema4 = group4.schema.as_ref().unwrap();
    assert_eq!(schema4.counters.len(), 2);
    assert!(!Arc::ptr_eq(schema3, schema4));
    assert_ne!(group3.schema_hash, group4.schema_hash);
}

static UNUSED_GROUP: TestGroup = TestGroup::new("never_routed");

#[test]
fn a_group_nothing_routes_to_is_absent() {
    let _ = &UNUSED_GROUP;
    let groups = build(&mut builder());
    assert!(!groups.iter().any(|g| g.name == "unattributed/never_routed"));
}

static UNHANDLED_GROUP: TestGroup = TestGroup::new("unhandled_probe");

#[metric(name = "snapshot_v3_unhandled_probe", metadata = { acq_group = "unhandled_probe" })]
static UNHANDLED_HISTOGRAM_GROUP: metriken::HistogramGroup =
    metriken::HistogramGroup::new(2, 7, 32);

/// A group whose only routed metric is a value kind the builder does not
/// expose is absent, not an empty group hashed and sent every build.
#[test]
fn a_group_with_only_unhandled_value_metrics_is_not_emitted() {
    let _ = &UNHANDLED_HISTOGRAM_GROUP;
    let groups = build(&mut builder());
    assert!(!groups
        .iter()
        .any(|g| g.name == "unattributed/unhandled_probe"));
}

static DECLARED_COUNTER_GROUP: TestGroup = TestGroup::new("counter_group_probe");

#[metric(name = "snapshot_v3_declared_counter_group", metadata = { acq_group = "counter_group_probe" })]
static DECLARED_COUNTERS: metriken::CounterGroup = metriken::CounterGroup::new(2);

#[metric(name = "snapshot_v3_default_counter_group")]
static DEFAULT_COUNTERS: metriken::CounterGroup = metriken::CounterGroup::new(2);

/// Same shape, same writes (only index 0): a declared group emits index 1 as
/// `None` (registered, unwritten), a default group leaves it out.
#[test]
fn a_declared_group_includes_unwritten_entries_a_default_group_skips_them() {
    DECLARED_COUNTERS.increment(0);
    DEFAULT_COUNTERS.increment(0);
    DECLARED_COUNTER_GROUP.acquire().finish();

    let groups = build(&mut builder());

    let declared = find(&groups, "unattributed/counter_group_probe");
    let schema = declared.schema.as_ref().unwrap();
    let at = |id: &str| {
        schema.counters.iter().position(|d| {
            metric_name(d) == Some("snapshot_v3_declared_counter_group")
                && d.metadata.get("id").map(String::as_str) == Some(id)
        })
    };
    assert_eq!(declared.counters[at("0").unwrap()], Some(1));
    assert_eq!(
        declared.counters[at("1").expect("index 1 by registration")],
        None
    );

    let default = find(&groups, "unattributed/main");
    let schema = default.schema.as_ref().unwrap();
    let present = |id: &str| {
        schema.counters.iter().any(|d| {
            metric_name(d) == Some("snapshot_v3_default_counter_group")
                && d.metadata.get("id").map(String::as_str) == Some(id)
        })
    };
    assert!(present("0"));
    assert!(!present("1"), "the default group's sentinel skip drops it");
}

#[metric(name = "snapshot_v3_default_zero_cross_counter")]
static DEFAULT_ZERO_CROSS_COUNTERS: metriken::CounterGroup = metriken::CounterGroup::new(1);

/// A default group member crossing from zero (absent) to nonzero (present)
/// is a membership change: a miss, and both builds validate. This is the
/// case the walk's own stored identity exists for.
#[test]
fn a_default_group_member_crossing_zero_misses_and_validates() {
    DEFAULT_ZERO_CROSS_COUNTERS.set(0, 0);
    let at = |schema: &GroupSchema| {
        schema
            .counters
            .iter()
            .position(|d| metric_name(d) == Some("snapshot_v3_default_zero_cross_counter"))
    };

    let mut b = builder();
    let s1 = build(&mut b);
    let group1 = find(&s1, "unattributed/main");
    assert_eq!(group1.validate(), Ok(()));
    let schema1 = group1.schema.clone().unwrap();
    assert!(at(&schema1).is_none());

    DEFAULT_ZERO_CROSS_COUNTERS.set(0, 1);
    let s2 = build(&mut b);
    let group2 = find(&s2, "unattributed/main");
    assert_eq!(group2.validate(), Ok(()));
    let schema2 = group2.schema.as_ref().unwrap();
    assert_eq!(group2.counters[at(schema2).unwrap()], Some(1));
    assert!(!Arc::ptr_eq(&schema1, schema2));
}

static BOUNDED_GROUP: TestGroup = TestGroup::new("bounded_counter_probe");

#[metric(name = "snapshot_v3_bounded_counters", metadata = { acq_group = "bounded_counter_probe" })]
static BOUNDED_COUNTERS: metriken::CounterGroup = metriken::CounterGroup::new(8);

#[test]
fn a_member_bound_limits_membership_below_the_backing_array() {
    for idx in 0..8 {
        BOUNDED_COUNTERS.add(idx, 10 + idx as u64);
    }
    BOUNDED_GROUP.set_member_bound(3);
    BOUNDED_GROUP.acquire().finish();

    let groups = build(&mut builder());
    let group = find(&groups, "unattributed/bounded_counter_probe");
    assert_eq!(group.validate(), Ok(()));
    let schema = group.schema.as_ref().unwrap();
    assert_eq!(schema.counters.len(), 3);
    assert_eq!(group.counters.len(), 3);
    for (idx, desc) in schema.counters.iter().enumerate() {
        assert_eq!(desc.metadata.get("id"), Some(&idx.to_string()));
        assert!(!desc.metadata.contains_key("acq_group"));
    }
}

static OVERBOUND_GROUP: TestGroup = TestGroup::new("overbound_counter_probe");

#[metric(name = "snapshot_v3_overbound_counters", metadata = { acq_group = "overbound_counter_probe" })]
static OVERBOUND_COUNTERS: metriken::CounterGroup = metriken::CounterGroup::new(2);

#[test]
fn a_member_bound_larger_than_the_backing_array_is_clamped() {
    OVERBOUND_COUNTERS.add(0, 1);
    OVERBOUND_COUNTERS.add(1, 2);
    OVERBOUND_GROUP.set_member_bound(5);
    OVERBOUND_GROUP.acquire().finish();

    let groups = build(&mut builder());
    let group = find(&groups, "unattributed/overbound_counter_probe");
    assert_eq!(group.validate(), Ok(()));
    assert_eq!(group.schema.as_ref().unwrap().counters.len(), 2);
}

// --- windows --------------------------------------------------------------------------

static READER_STAMPED_GROUP: TestGroup = TestGroup::new("reader_stamped_probe");

#[metric(name = "snapshot_v3_reader_stamped_counters", metadata = { acq_group = "reader_stamped_probe" })]
static READER_STAMPED_COUNTERS: metriken::CounterGroup = metriken::CounterGroup::new(8);

/// A reader-stamped group's window is the builder's own read, bracketed
/// each build.
#[test]
fn a_reader_stamped_group_carries_a_window_spanning_the_read() {
    READER_STAMPED_GROUP.set_reader_stamped();
    READER_STAMPED_COUNTERS.set_metadata(1, [("cgroup".to_string(), "/a".to_string())].into());
    READER_STAMPED_COUNTERS.add(1, 5);

    let mut b = builder();
    let s1 = build(&mut b);
    let group1 = find(&s1, "unattributed/reader_stamped_probe");
    assert_eq!(group1.validate(), Ok(()));
    let window1 = group1.window.expect("stamped by the build itself");
    assert!(window1.begin_ns > 0);
    assert!(window1.end_ns >= window1.begin_ns);

    let s2 = build(&mut b);
    let window2 = find(&s2, "unattributed/reader_stamped_probe")
        .window
        .expect("stamped again");
    assert!(
        window2.begin_ns >= window1.begin_ns,
        "each build's bracket begins no earlier than the previous one's"
    );
}

/// Builders on several threads over a reader-stamped group complete.
#[test]
fn concurrent_builders_over_a_reader_stamped_group_do_not_panic() {
    READER_STAMPED_GROUP.set_reader_stamped();
    READER_STAMPED_COUNTERS.set_metadata(1, [("cgroup".to_string(), "/smoke".to_string())].into());
    READER_STAMPED_COUNTERS.add(1, 1);

    let threads: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                let mut b = builder();
                for _ in 0..25 {
                    let _ = build(&mut b);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("a concurrent builder panicked");
    }
}

/// rezolus's `MAX_PID`.
const MAX_PID: usize = 4_194_304;

static MAX_PID_SCALE_GROUP: TestGroup = TestGroup::new_reader_stamped("max_pid_scale_probe");

#[metric(name = "snapshot_v3_max_pid_scale_counters", metadata = { acq_group = "max_pid_scale_probe" })]
static MAX_PID_SCALE_COUNTERS: metriken::CounterGroup = metriken::CounterGroup::new(MAX_PID);

/// Slot membership walks the metadata store, never the backing array: 2
/// members out of 4.2M slots, quickly. A walk over the array would push
/// 4.2M descriptors.
#[test]
fn slot_membership_never_walks_the_backing_array() {
    MAX_PID_SCALE_COUNTERS.set_metadata(7, [("pid".to_string(), "7".to_string())].into());
    MAX_PID_SCALE_COUNTERS.add(7, 1);
    MAX_PID_SCALE_COUNTERS.set_metadata(
        4_000_000,
        [("pid".to_string(), "4000000".to_string())].into(),
    );
    MAX_PID_SCALE_COUNTERS.add(4_000_000, 1);

    let groups = build(&mut builder());
    let group = find(&groups, "unattributed/max_pid_scale_probe");
    assert_eq!(group.schema.as_ref().unwrap().counters.len(), 2);
}

static SPARSE_GROUP: TestGroup = TestGroup::new_reader_stamped("sparse_probe");

#[metric(name = "snapshot_v3_sparse_counters", metadata = { acq_group = "sparse_probe" })]
static SPARSE_COUNTERS: metriken::CounterGroup = metriken::CounterGroup::new(1000);

/// Exactly the slots with metadata, sorted, with an honest zero kept.
#[test]
fn slot_membership_emits_only_slots_with_metadata() {
    SPARSE_COUNTERS.set_metadata(5, [("cgroup".to_string(), "/a".to_string())].into());
    SPARSE_COUNTERS.add(5, 3);
    SPARSE_COUNTERS.set_metadata(500, [("cgroup".to_string(), "/b".to_string())].into());
    SPARSE_COUNTERS.add(500, 0);
    SPARSE_COUNTERS.set_metadata(999, [("cgroup".to_string(), "/c".to_string())].into());
    SPARSE_COUNTERS.add(999, 7);

    let name = "unattributed/sparse_probe";
    let mut b = builder();
    let s1 = build(&mut b);
    let group = find(&s1, name);
    assert_eq!(group.validate(), Ok(()));
    let schema = group.schema.as_ref().unwrap();
    let ids: Vec<&str> = schema
        .counters
        .iter()
        .map(|d| d.metadata["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["5", "500", "999"]);
    assert_eq!(group.counters[1], Some(0), "slot 500's zero is a reading");

    let s2 = build(&mut b);
    assert_eq!(group.schema_hash, find(&s2, name).schema_hash);

    SPARSE_COUNTERS.clear_metadata(500);
    let s3 = build(&mut b);
    let group3 = find(&s3, name);
    assert_eq!(group3.schema.as_ref().unwrap().counters.len(), 2);
    assert_ne!(group.schema_hash, group3.schema_hash);
}

static SWEEP_GROUP: TestGroup = TestGroup::new("sweep_probe");

#[metric(name = "snapshot_sweep_temperature", metadata = { acq_group = "sweep_probe" })]
static SWEEP_TEMPERATURE: metriken::GaugeGroup = metriken::GaugeGroup::new(64);

/// A sweep stamped by its writer reaches the group's window, and a bound of
/// 1 on a 64-slot array is 1 member, not 63 `None`s.
#[test]
fn a_stamped_sweep_window_reaches_the_group_and_the_bound_holds() {
    SWEEP_GROUP.set_member_bound(1);
    let guard = SWEEP_GROUP.acquire();
    let _ = SWEEP_TEMPERATURE.set(0, 42);
    guard.finish();
    let window = SWEEP_GROUP.window().unwrap();

    let groups = build(&mut builder());
    let group = find(&groups, "unattributed/sweep_probe");
    assert_eq!(group.window, Some(window));
    assert_eq!(group.schema.as_ref().unwrap().gauges.len(), 1);
    assert_eq!(group.gauges.len(), 1);
}

static SWEEP_DISCARD_GROUP: TestGroup = TestGroup::new("sweep_discard_probe");

#[metric(name = "snapshot_sweep_discard_temperature", metadata = { acq_group = "sweep_discard_probe" })]
static SWEEP_DISCARD_TEMPERATURE: metriken::GaugeGroup = metriken::GaugeGroup::new(1);

/// A discarded sweep leaves the previous window standing.
#[test]
fn a_discarded_sweep_leaves_the_previous_window_standing() {
    let guard = SWEEP_DISCARD_GROUP.acquire();
    let _ = SWEEP_DISCARD_TEMPERATURE.set(0, 55);
    guard.finish();
    let first = SWEEP_DISCARD_GROUP.window().unwrap();

    SWEEP_DISCARD_GROUP.acquire().discard();
    assert_eq!(SWEEP_DISCARD_GROUP.window(), Some(first));

    let groups = build(&mut builder());
    assert_eq!(
        find(&groups, "unattributed/sweep_discard_probe").window,
        Some(first)
    );
}

// --- the snapshot -----------------------------------------------------------------------

/// `ts + wall_offset` is `systemtime`, and the snapshot carries the process's
/// epoch and anchor.
#[test]
fn the_anchored_stamp_and_the_wall_clock_agree() {
    let stamp = Stamp::now();
    let snap = {
        let _one = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        builder().snapshot(
            stamp,
            Duration::from_millis(3),
            Vec::new(),
            [("source".to_string(), "test".to_string())].into(),
        )
    };
    let wall = snap
        .systemtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    assert_eq!(stamp.ts + stamp.wall_offset, wall);
    assert_eq!(snap.duration, Duration::from_millis(3));
    let md = |k: &str| snap.metadata.get(k).cloned();
    assert_eq!(md("source").as_deref(), Some("test"), "caller keys kept");
    assert_eq!(
        md("producer_epoch").as_deref(),
        Some(metriken::epoch::producer_epoch())
    );
    assert_eq!(
        md("clock_anchor_wall_ns"),
        Some(metriken::epoch::clock_anchor_wall_ns().to_string())
    );
    assert_eq!(md("ts"), Some(stamp.ts.to_string()));
    assert_eq!(md("wall_offset"), Some(stamp.wall_offset.to_string()));
}

// --- extra groups ----------------------------------------------------------------------

fn extra(namespace: &str, name: &str, values: &[(&str, u64)]) -> ExtraGroup {
    ExtraGroup {
        namespace: namespace.to_string(),
        name: name.to_string(),
        window: None,
        counters: values
            .iter()
            .map(|(n, v)| {
                (
                    MetricDesc {
                        name: format!("external/{n}"),
                        metadata: [("metric".to_string(), n.to_string())].into(),
                    },
                    Some(*v),
                )
            })
            .collect(),
        gauges: Vec::new(),
        histograms: Vec::new(),
    }
}

/// A caller's extra group is built every time, in the order given, and the
/// same input gives the same hash.
#[test]
fn an_extra_group_is_built_every_time_with_a_stable_hash() {
    let mut b = builder();
    let run = |b: &mut GroupBuilder<TestRouter>| {
        let _one = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        b.build_groups(vec![extra("external", "main", &[("a", 1), ("b", 2)])])
    };
    let s1 = run(&mut b);
    let s2 = run(&mut b);
    let (g1, g2) = (find(&s1, "external/main"), find(&s2, "external/main"));
    assert_eq!(g1.validate(), Ok(()));
    assert_eq!(g1.schema_hash, g2.schema_hash);
    assert!(
        !Arc::ptr_eq(g1.schema.as_ref().unwrap(), g2.schema.as_ref().unwrap()),
        "an extra group is never a cache hit"
    );
    assert_eq!(g2.counters, vec![Some(1), Some(2)]);
    assert!(g2.window.is_none());
}

static MERGE_WINDOW: Window = Window::new(10, 20);

#[metric(name = "snapshot_extra_merge_probe", metadata = { acq_group = "merge_probe" })]
static EXTRA_MERGE_PROBE: metriken::Counter = metriken::Counter::new();

/// Extra members for a group the registry also routes to are appended after
/// the registry members, on a hit build as well as the first.
#[test]
fn extra_members_join_a_routed_group_of_the_same_name() {
    struct MergeRouter;
    impl Router for MergeRouter {
        type Guard = metriken_exposition::group_builder::NoGuard;
        fn route<'a>(&'a self, metric: &'a MetricEntry) -> Option<Route<'a>> {
            (metric.name() == "snapshot_extra_merge_probe").then_some(Route {
                group: GroupId::new("merge", "main"),
                membership: Membership::All,
            })
        }
        fn acquire(&self, _: GroupId<'_>) -> Acquisition<Self::Guard> {
            Acquisition::Stamped(Some(MERGE_WINDOW))
        }
    }
    EXTRA_MERGE_PROBE.add(3);
    let mut b = GroupBuilder::new(MergeRouter);
    for _ in 0..2 {
        let groups = b.build_groups(vec![extra("merge", "main", &[("pushed", 9)])]);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.validate(), Ok(()));
        let schema = g.schema.as_ref().unwrap();
        assert_eq!(
            metric_name(&schema.counters[0]),
            Some("snapshot_extra_merge_probe")
        );
        assert_eq!(metric_name(&schema.counters[1]), Some("pushed"));
        assert_eq!(g.counters[1], Some(9));
        assert_eq!(g.window, Some(MERGE_WINDOW), "the routed group's window");
    }
}

// --- routers of other shapes ------------------------------------------------------------

#[metric(name = "split_a_one")]
static SPLIT_A_ONE: metriken::Counter = metriken::Counter::new();
#[metric(name = "split_a_two")]
static SPLIT_A_TWO: metriken::Gauge = metriken::Gauge::new();
#[metric(name = "split_b_one")]
static SPLIT_B_ONE: metriken::GaugeGroup = metriken::GaugeGroup::new(4);

/// Routes `split_a_*` and `split_b_*` to two groups and declines the rest.
struct SplitRouter;

impl Router for SplitRouter {
    type Guard = metriken_exposition::group_builder::NoGuard;

    fn route<'a>(&'a self, metric: &'a MetricEntry) -> Option<Route<'a>> {
        let name = if metric.name().starts_with("split_a_") {
            "a"
        } else if metric.name().starts_with("split_b_") {
            "b"
        } else {
            return None;
        };
        Some(Route {
            group: GroupId::new("split", name),
            membership: Membership::Prefix(2),
        })
    }
}

/// A router splits metrics into two groups; the cache hits both, and a
/// change to one rebuilds that one alone.
#[test]
fn a_router_splits_metrics_into_two_groups_and_each_caches_alone() {
    SPLIT_A_ONE.add(1);
    SPLIT_A_TWO.set(-2);
    SPLIT_B_ONE.set(0, 5);
    SPLIT_B_ONE.set(1, 6);
    SPLIT_B_ONE.set(2, 7);

    let mut b = GroupBuilder::new(SplitRouter);
    let s1 = b.build_groups(Vec::new());
    let names: Vec<&str> = s1.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, vec!["split/a", "split/b"]);
    assert_eq!(b.rebuilds(), 2, "a miss per group on the first build");

    let a = &s1[0];
    assert_eq!(a.counters, vec![Some(1)]);
    assert_eq!(a.gauges, vec![Some(-2)]);
    let bg = &s1[1];
    assert_eq!(bg.gauges, vec![Some(5), Some(6)], "the prefix of 2");

    SPLIT_A_ONE.add(1);
    let s2 = b.build_groups(Vec::new());
    assert_eq!(
        b.rebuilds(),
        2,
        "values changed, membership did not: both hit"
    );
    assert_eq!(s2[0].counters, vec![Some(2)]);
    assert!(Arc::ptr_eq(
        s1[0].schema.as_ref().unwrap(),
        s2[0].schema.as_ref().unwrap()
    ));

    SPLIT_B_ONE.set_metadata(1, [("disk".to_string(), "sda".to_string())].into());
    let s3 = b.build_groups(Vec::new());
    assert_eq!(b.rebuilds(), 3, "only the relabelled group rebuilds");
    assert!(Arc::ptr_eq(
        s1[0].schema.as_ref().unwrap(),
        s3[0].schema.as_ref().unwrap()
    ));
    assert_ne!(s1[1].schema_hash, s3[1].schema_hash);
    assert_eq!(
        s3[1].schema.as_ref().unwrap().gauges[1]
            .metadata
            .get("disk")
            .map(String::as_str),
        Some("sda")
    );
}

/// `ByName` names members by metric name, independent of registry position.
#[test]
fn members_can_be_named_by_metric_name() {
    let mut b = GroupBuilder::new(SplitRouter).with_names(ByName);
    let groups = b.build_groups(Vec::new());
    let a = find(&groups, "split/a").schema.as_ref().unwrap();
    assert_eq!(a.counters[0].name, "split_a_one");
    assert_eq!(a.gauges[0].name, "split_a_two");
    let bg = find(&groups, "split/b").schema.as_ref().unwrap();
    assert_eq!(bg.gauges[0].name, "split_b_one#0");
    assert_eq!(bg.gauges[1].name, "split_b_one#1");
}

// --- families ----------------------------------------------------------------------------

#[metric(name = "tenant_requests", metadata = { acq_group = "family_probe" })]
static TENANT_REQUESTS: metriken::CounterFamily = metriken::CounterFamily::new();

/// Only the family's group, through the default router.
fn family_group(b: &mut GroupBuilder<DefaultRouter>) -> GroupSnapshot {
    b.build_groups(Vec::new())
        .into_iter()
        .find(|g| g.name == "svc/family_probe")
        .expect("the family's group")
}

/// A family snapshots as one group whose members are its live members,
/// labels and `__uid__` included. Its schema is reused while membership is
/// unchanged (values moving do not count) and rebuilt when a member comes or
/// goes.
#[test]
fn a_family_is_one_group_whose_members_come_and_go() {
    let metrics = metriken::metrics();
    let entry = metrics
        .iter()
        .find(|m| m.name() == "tenant_requests")
        .unwrap();
    assert!(is_family(entry));
    drop(metrics);

    let mut b = GroupBuilder::new(DefaultRouter::new("svc"));

    let acme = TENANT_REQUESTS.member([("tenant", "acme")]);
    let globex = TENANT_REQUESTS.member([("tenant", "globex")]);
    acme.add(3);

    let g1 = family_group(&mut b);
    assert_eq!(g1.validate(), Ok(()));
    let schema1 = g1.schema.clone().unwrap();
    let tenants: Vec<&str> = schema1
        .counters
        .iter()
        .map(|d| d.metadata["tenant"].as_str())
        .collect();
    assert_eq!(tenants, vec!["acme", "globex"]);
    assert_eq!(
        g1.counters,
        vec![Some(3), Some(0)],
        "a member at zero is a member"
    );
    let uid_acme = schema1.counters[0].metadata[metriken::group::UID_LABEL].clone();
    assert!(!schema1.counters[0].metadata.contains_key("acq_group"));

    globex.increment();
    let g2 = family_group(&mut b);
    assert!(
        Arc::ptr_eq(&schema1, g2.schema.as_ref().unwrap()),
        "membership unchanged: the cached schema"
    );
    assert_eq!(g2.counters, vec![Some(3), Some(1)]);

    drop(acme);
    let g3 = family_group(&mut b);
    assert_eq!(g3.validate(), Ok(()));
    let schema3 = g3.schema.clone().unwrap();
    assert_eq!(schema3.counters.len(), 1);
    assert_eq!(schema3.counters[0].metadata["tenant"], "globex");
    assert_ne!(g1.schema_hash, g3.schema_hash);

    // The freed slot is taken by a new occupant with the same labels.
    let acme_again = TENANT_REQUESTS.member([("tenant", "acme")]);
    let g4 = family_group(&mut b);
    let schema4 = g4.schema.as_ref().unwrap();
    assert_eq!(schema4.counters.len(), 2);
    assert_eq!(
        schema4.counters[0].name, schema1.counters[0].name,
        "same slot"
    );
    assert_ne!(
        schema4.counters[0].metadata[metriken::group::UID_LABEL],
        uid_acme,
        "a new occupant"
    );
    assert_ne!(g4.schema_hash, g1.schema_hash);
    assert_eq!(g4.counters[0], Some(0));
    drop((globex, acme_again));

    let g5 = b
        .build_groups(Vec::new())
        .into_iter()
        .find(|g| g.name == "svc/family_probe");
    assert!(g5.is_none(), "a family with no members has no group");
}

#[metric(name = "tenant_connections")]
static TENANT_CONNECTIONS: metriken::GaugeFamily = metriken::GaugeFamily::new();

/// A gauge family works the same way.
#[test]
fn a_gauge_family_is_a_group_of_its_members() {
    let m = TENANT_CONNECTIONS.member([("tenant", "initech")]);
    m.set(-4);
    let mut b = GroupBuilder::new(DefaultRouter::new("svcg"));
    let groups = b.build_groups(Vec::new());
    let main = find(&groups, "svcg/main");
    let schema = main.schema.as_ref().unwrap();
    let at = schema
        .gauges
        .iter()
        .position(|d| metric_name(d) == Some("tenant_connections"))
        .expect("the member");
    assert_eq!(main.gauges[at], Some(-4));
    assert_eq!(schema.gauges[at].metadata["tenant"], "initech");
}
