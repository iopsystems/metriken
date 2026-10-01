//! `GroupBuilder::build_stream`: which groups come out long, that a long
//! group holds the same values under the same labels as its wide form, and
//! what a change of occupant costs.
//!
//! Every test uses groups of its own, since tests share one registry.

#![cfg(feature = "msgpack")]

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use metriken::group::{SlotIdentity, SlotMetadata, UID_LABEL};
use metriken::{metric, MetricEntry, Window};
use metriken_exposition::group_builder::{
    Acquisition, ExtraGroup, GroupBuilder, GroupId, LongGroupSnapshot, Membership, NoGuard, Route,
    Router, StreamGroup, GROUP_METADATA_KEY,
};
use metriken_exposition::{GroupSnapshot, MetricDesc};

const NS: &str = "stream";

/// Groups named `slots_*` take their members from slot metadata; any other
/// declared group takes every slot; an undeclared metric goes to `main` with
/// value-derived membership. `windowed` has a fixed stamped window.
struct TestRouter;

const WINDOW: Window = Window {
    begin_ns: 1_000,
    end_ns: 2_000,
};

impl Router for TestRouter {
    type Guard = NoGuard;

    fn route<'a>(&'a self, metric: &'a MetricEntry) -> Option<Route<'a>> {
        match metric.metadata().get(GROUP_METADATA_KEY) {
            Some(name) => Some(Route {
                group: GroupId::new(NS, name),
                membership: if name.starts_with("slots_") {
                    Membership::Slots
                } else {
                    Membership::All
                },
            }),
            None => Some(Route {
                group: GroupId::new(NS, "main"),
                membership: Membership::Present,
            }),
        }
    }

    fn acquire(&self, group: GroupId<'_>) -> Acquisition<NoGuard> {
        if group.name == "windowed" {
            Acquisition::Stamped(Some(WINDOW))
        } else {
            Acquisition::Windowless
        }
    }

    fn window(&self, group: GroupId<'_>) -> Option<Window> {
        (group.name == "windowed").then_some(WINDOW)
    }

    fn annotate(&self, _: &MetricEntry, metadata: &mut BTreeMap<String, String>) {
        metadata.insert("sampler".to_string(), NS.to_string());
    }
}

/// One build at a time: the builders share the registry and its slot
/// metadata.
static BUILD_LOCK: Mutex<()> = Mutex::new(());

fn stream(b: &mut GroupBuilder<TestRouter>) -> Vec<StreamGroup> {
    b.build_stream(Vec::new())
}

fn long<'s>(groups: &'s [StreamGroup], name: &str) -> &'s LongGroupSnapshot {
    match groups.iter().find(|g| g.name() == format!("{NS}/{name}")) {
        Some(StreamGroup::Long(g)) => g,
        Some(StreamGroup::Wide(_)) => panic!("`{name}` came out wide"),
        None => panic!("`{name}` is absent"),
    }
}

fn is_wide(groups: &[StreamGroup], name: &str) -> bool {
    matches!(
        groups.iter().find(|g| g.name() == format!("{NS}/{name}")),
        Some(StreamGroup::Wide(_))
    )
}

fn wide<'s>(groups: &'s [GroupSnapshot], name: &str) -> &'s GroupSnapshot {
    groups
        .iter()
        .find(|g| g.name == format!("{NS}/{name}"))
        .unwrap_or_else(|| panic!("`{name}` is absent"))
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A series' full label set and value, from a wide group: each member's
/// metadata.
fn wide_series(g: &GroupSnapshot) -> BTreeMap<BTreeMap<String, String>, String> {
    let schema = g.schema.as_ref().unwrap();
    let mut out = BTreeMap::new();
    let mut add = |d: &MetricDesc, v: String| {
        out.insert(d.metadata.clone(), v);
    };
    for (d, v) in schema.counters.iter().zip(&g.counters) {
        add(d, format!("{v:?}"));
    }
    for (d, v) in schema.gauges.iter().zip(&g.gauges) {
        add(d, format!("{v:?}"));
    }
    out
}

/// The same from a long group: a column's metadata merged with each
/// occupant's labels.
fn long_series(g: &LongGroupSnapshot) -> BTreeMap<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for o in &g.occupants {
        let mut add = |d: &MetricDesc, v: String| {
            let mut l = d.metadata.clone();
            l.extend(o.labels.iter().map(|(k, v)| (k.clone(), v.clone())));
            out.insert(l, v);
        };
        for (d, v) in g.columns.counters.iter().zip(&o.counters) {
            add(d, format!("{v:?}"));
        }
        for (d, v) in g.columns.gauges.iter().zip(&o.gauges) {
            add(d, format!("{v:?}"));
        }
    }
    out
}

// --- what comes out long ------------------------------------------------------------

#[metric(name = "stream_tasks_cpu", metadata = { acq_group = "slots_tasks" })]
static TASK_CPU: metriken::CounterGroup = metriken::CounterGroup::new(64);
#[metric(name = "stream_tasks_switches", metadata = { acq_group = "slots_tasks", unit = "count" })]
static TASK_SWITCHES: metriken::CounterGroup = metriken::CounterGroup::new(64);
#[metric(name = "stream_tasks_rss", metadata = { acq_group = "slots_tasks" })]
static TASK_RSS: metriken::GaugeGroup = metriken::GaugeGroup::new(64);
static TASKS: SlotIdentity = SlotIdentity::new(&[&TASK_CPU, &TASK_SWITCHES, &TASK_RSS]);

/// A slot group comes out long and holds the values its wide form holds,
/// under the same full label sets.
#[test]
fn a_long_group_holds_what_its_wide_form_holds() {
    let _one = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (slot, comm) in [(5usize, "redis"), (1, "nginx"), (9, "sshd")] {
        TASKS.assign(slot, labels(&[("comm", comm)]));
        TASK_CPU.set(slot, slot as u64 * 10);
        TASK_SWITCHES.set(slot, slot as u64);
        TASK_RSS.set(slot, slot as i64 * 100);
    }

    let groups = stream(&mut GroupBuilder::new(TestRouter));
    let g = long(&groups, "slots_tasks");
    let wide_groups = GroupBuilder::new(TestRouter).build_groups(Vec::new());
    assert_eq!(
        long_series(g),
        wide_series(wide(&wide_groups, "slots_tasks"))
    );

    assert_eq!(g.columns.counters.len(), 2);
    assert_eq!(g.columns.gauges.len(), 1);
    assert!(g.columns.histograms.is_empty());
    assert_eq!(g.columns_hash, g.columns.hash());
    for d in g.columns.counters.iter().chain(&g.columns.gauges) {
        assert!(!d.metadata.contains_key("id"), "{d:?}");
        assert!(!d.metadata.contains_key("comm"), "{d:?}");
        assert!(!d.metadata.contains_key(GROUP_METADATA_KEY), "{d:?}");
    }
    let slots: Vec<&str> = g
        .occupants
        .iter()
        .map(|o| o.labels["id"].as_str())
        .collect();
    assert_eq!(slots, ["1", "5", "9"], "slot order");

    for slot in [1, 5, 9] {
        TASKS.release(slot);
    }
}

#[metric(name = "stream_mixed_slots", metadata = { acq_group = "mixed" })]
static MIXED_SLOTS: metriken::CounterGroup = metriken::CounterGroup::new(4);
#[metric(name = "stream_mixed_scalar", metadata = { acq_group = "mixed" })]
static MIXED_SCALAR: metriken::Counter = metriken::Counter::new();
#[metric(name = "stream_extra_slots", metadata = { acq_group = "extra" })]
static EXTRA_SLOTS: metriken::CounterGroup = metriken::CounterGroup::new(4);

/// A group with a metric that is not a counter group or a gauge group comes
/// out wide, and so does a group `extra` adds members to.
#[test]
fn a_mixed_group_and_an_extended_group_come_out_wide() {
    let _one = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    MIXED_SLOTS.set(0, 1);
    MIXED_SCALAR.add(1);
    EXTRA_SLOTS.set(0, 1);
    let groups = GroupBuilder::new(TestRouter).build_stream(vec![ExtraGroup {
        namespace: NS.to_string(),
        name: "extra".to_string(),
        window: None,
        counters: vec![(
            MetricDesc {
                name: "pushed".to_string(),
                metadata: labels(&[("metric", "pushed")]),
            },
            Some(1),
        )],
        gauges: Vec::new(),
        histograms: Vec::new(),
    }]);
    assert!(is_wide(&groups, "mixed"));
    assert!(is_wide(&groups, "extra"));
    let names: Vec<&str> = groups.iter().map(StreamGroup::name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "sorted by name");
}

#[metric(name = "stream_cpus_busy", metadata = { acq_group = "windowed" })]
static CPU_BUSY: metriken::CounterGroup = metriken::CounterGroup::new(4);

/// A group of fixed slots with no slot metadata comes out long: each slot
/// is an occupant labelled by its `id`, and the group keeps its window.
#[test]
fn fixed_slots_are_occupants_and_the_window_is_kept() {
    let _one = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for cpu in 0..4 {
        CPU_BUSY.set(cpu, cpu as u64 + 1);
    }
    let mut b = GroupBuilder::new(TestRouter);
    let first = stream(&mut b);
    let g = long(&first, "windowed");
    assert_eq!(g.window, Some(WINDOW));
    assert_eq!(g.occupants.len(), 4);
    assert_eq!(*g.occupants[2].labels, labels(&[("id", "2")]));
    assert_eq!(g.occupants[2].counters, vec![Some(3)]);

    let keys: HashSet<u64> = g.occupants.iter().map(|o| o.key).collect();
    assert_eq!(keys.len(), 4, "distinct keys");
    let again = stream(&mut b);
    let same: Vec<u64> = long(&again, "windowed")
        .occupants
        .iter()
        .map(|o| o.key)
        .collect();
    let before: Vec<u64> = g.occupants.iter().map(|o| o.key).collect();
    assert_eq!(same, before, "a slot's key is stable while its labels are");
}

#[metric(name = "stream_present_slots")]
static PRESENT_SLOTS: metriken::CounterGroup = metriken::CounterGroup::new(8);

/// In a group whose membership follows values, a slot reading zero is not
/// an occupant.
#[test]
fn a_zero_counter_is_not_an_occupant_when_membership_follows_values() {
    let _one = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    PRESENT_SLOTS.set(3, 7);
    let groups = stream(&mut GroupBuilder::new(TestRouter));
    let g = long(&groups, "main");
    let ids: Vec<&str> = g
        .occupants
        .iter()
        .filter(|o| o.counters.iter().any(Option::is_some))
        .map(|o| o.labels["id"].as_str())
        .collect();
    assert!(ids.contains(&"3"), "{ids:?}");
    assert!(
        !g.occupants.iter().any(|o| o.labels["id"] == "4"),
        "slot 4 reads zero"
    );
}

// --- what a change of occupant costs ---------------------------------------------

#[metric(name = "stream_churn_cpu", metadata = { acq_group = "slots_churn" })]
static CHURN_CPU: metriken::CounterGroup = metriken::CounterGroup::new(16);
static CHURN: SlotIdentity = SlotIdentity::new(&[&CHURN_CPU]);

/// Keys are assigned from 0 in the order occupants appear. A departure and
/// an arrival keep the group's columns and the other occupants' labels and
/// keys as they were, and give the new occupant the next key, even in a
/// reused slot with the same labels.
#[test]
fn a_change_of_occupant_keeps_the_columns_and_the_other_occupants() {
    let _one = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let stay = CHURN.assign(1, labels(&[("comm", "stay")]));
    let go = CHURN.assign(2, labels(&[("comm", "go")]));
    CHURN_CPU.set(1, 1);
    CHURN_CPU.set(2, 2);

    let mut b = GroupBuilder::new(TestRouter);
    let before = stream(&mut b);
    let g0 = long(&before, "slots_churn").clone();
    let rebuilds = b.rebuilds();
    assert_eq!(g0.occupants[0].key, 0);
    assert_eq!(g0.occupants[1].key, 1);
    assert_eq!(g0.occupants[0].labels[UID_LABEL], stay);
    assert_eq!(g0.occupants[1].labels[UID_LABEL], go);

    CHURN.release(2);
    let back = CHURN.assign(2, labels(&[("comm", "go")]));
    CHURN_CPU.set(2, 3);
    let after = stream(&mut b);
    let g1 = long(&after, "slots_churn");

    assert!(Arc::ptr_eq(&g0.columns, &g1.columns), "columns kept");
    assert_eq!(g0.columns_hash, g1.columns_hash);
    assert_eq!(b.rebuilds(), rebuilds, "nothing rebuilt");
    assert!(
        Arc::ptr_eq(&g0.occupants[0].labels, &g1.occupants[0].labels),
        "the occupant that stayed keeps its labels"
    );
    assert_eq!(g1.occupants[0].key, g0.occupants[0].key);
    assert_ne!(back, go);
    assert_eq!(g1.occupants[1].key, 2, "a new occupant, the next key");
    assert_eq!(g1.occupants[1].labels[UID_LABEL], back);
    assert_eq!(g1.occupants[1].counters, vec![Some(3)]);

    CHURN.release(1);
    CHURN.release(2);
}

#[metric(name = "stream_relabel_cpu", metadata = { acq_group = "slots_relabel" })]
static RELABEL_CPU: metriken::CounterGroup = metriken::CounterGroup::new(4);

/// A slot whose metadata is replaced without a uid is a new occupant with a
/// new key and new labels.
#[test]
fn a_relabelled_slot_without_a_uid_is_a_new_occupant() {
    let _one = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SlotMetadata::set_metadata(
        &RELABEL_CPU,
        0,
        [("dev".to_string(), "sda".to_string())].into(),
    );
    RELABEL_CPU.set(0, 1);
    let mut b = GroupBuilder::new(TestRouter);
    let first = stream(&mut b);
    let k0 = long(&first, "slots_relabel").occupants[0].key;

    SlotMetadata::set_metadata(
        &RELABEL_CPU,
        0,
        [("dev".to_string(), "sdb".to_string())].into(),
    );
    let second = stream(&mut b);
    let o = &long(&second, "slots_relabel").occupants[0];
    assert_ne!(o.key, k0);
    assert_eq!(*o.labels, labels(&[("dev", "sdb"), ("id", "0")]));
}
