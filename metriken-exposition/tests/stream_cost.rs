//! What a build costs when a slot group's membership changes, long against
//! wide. Ignored by default; run with
//! `cargo test --release -p metriken-exposition --test stream_cost -- --ignored --nocapture`.
//!
//! 2,500 occupants over four counter groups and one gauge group, 16 leaving
//! and 16 arriving on a change tick: close to a per-task group under process
//! churn on a 32-core host.

#![cfg(feature = "msgpack")]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use metriken::group::SlotIdentity;
use metriken::{metric, MetricEntry};
use metriken_exposition::group_builder::{
    GroupBuilder, GroupId, Membership, NoGuard, Route, Router,
};

const SLOTS: usize = 4096;
const LIVE: usize = 2500;
const CHURN: usize = 16;
const TICKS: usize = 200;

#[metric(name = "cost_user", metadata = { acq_group = "tasks" })]
static USER: metriken::CounterGroup = metriken::CounterGroup::new(SLOTS);
#[metric(name = "cost_system", metadata = { acq_group = "tasks" })]
static SYSTEM: metriken::CounterGroup = metriken::CounterGroup::new(SLOTS);
#[metric(name = "cost_switches", metadata = { acq_group = "tasks" })]
static SWITCHES: metriken::CounterGroup = metriken::CounterGroup::new(SLOTS);
#[metric(name = "cost_wait", metadata = { acq_group = "tasks" })]
static WAIT: metriken::CounterGroup = metriken::CounterGroup::new(SLOTS);
#[metric(name = "cost_rss", metadata = { acq_group = "tasks" })]
static RSS: metriken::GaugeGroup = metriken::GaugeGroup::new(SLOTS);
static TASKS: SlotIdentity = SlotIdentity::new(&[&USER, &SYSTEM, &SWITCHES, &WAIT, &RSS]);

struct TasksRouter;

impl Router for TasksRouter {
    type Guard = NoGuard;

    fn route<'a>(&'a self, metric: &'a MetricEntry) -> Option<Route<'a>> {
        metric.name().starts_with("cost_").then_some(Route {
            group: GroupId::new("cost", "tasks"),
            membership: Membership::Slots,
        })
    }

    fn annotate(&self, _: &MetricEntry, metadata: &mut BTreeMap<String, String>) {
        metadata.insert("sampler".to_string(), "cost".to_string());
    }
}

fn assign(slot: usize, n: usize) {
    TASKS.assign(
        slot,
        BTreeMap::from([
            ("comm".to_string(), format!("worker-{}", n % 97)),
            ("pid".to_string(), (10_000 + n).to_string()),
            ("tid".to_string(), (20_000 + n).to_string()),
        ]),
    );
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort_unstable();
    v[v.len() / 2]
}

#[test]
#[ignore]
fn change_tick_against_unchanged_tick() {
    for slot in 0..LIVE {
        assign(slot, slot);
    }
    let mut next = LIVE;
    let values = |tick: u64| {
        for slot in 0..SLOTS {
            if TASKS.uid(slot).is_some() {
                USER.set(slot, tick + slot as u64);
                SYSTEM.set(slot, tick);
                SWITCHES.set(slot, tick * 2);
                WAIT.set(slot, tick * 3);
                RSS.set(slot, tick as i64);
            }
        }
    };
    let mut churn = |tick: usize| {
        for i in 0..CHURN {
            let gone = (tick * CHURN + i) % SLOTS;
            TASKS.release(gone);
            let slot = (gone + LIVE) % SLOTS;
            assign(slot, next);
            next += 1;
        }
    };

    for (label, long) in [("long", true), ("wide", false)] {
        let mut b = GroupBuilder::new(TasksRouter);
        let build = |b: &mut GroupBuilder<TasksRouter>| {
            let t = Instant::now();
            if long {
                std::hint::black_box(b.build_stream(Vec::new()));
            } else {
                std::hint::black_box(b.build_groups(Vec::new()));
            }
            t.elapsed()
        };
        build(&mut b);
        let (mut changed, mut unchanged) = (Vec::new(), Vec::new());
        for tick in 0..TICKS {
            values(tick as u64);
            unchanged.push(build(&mut b));
            churn(tick);
            values(tick as u64);
            changed.push(build(&mut b));
        }
        let (c, u) = (median(changed), median(unchanged));
        println!(
            "{label}: change tick {c:?}, unchanged tick {u:?}, ratio {:.2}",
            c.as_secs_f64() / u.as_secs_f64()
        );
    }
}
