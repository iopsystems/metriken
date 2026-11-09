//! Float counters and gauges on the live path: V3 snapshots whose groups
//! hold `float_counters` and `float_gauges`, ingested by
//! `MemoryStore::ingest_snapshot` and queried as a `MetricsSource`. The
//! archive path is tested in metriken-storage's `tests/float_scalars.rs`,
//! which writes archives.
//!
//! What is checked, from `docs/journal/2026-11-06-float-scalars.md`: a
//! float counter is listed and evaluated as a counter and a float gauge as
//! a gauge (decision 1), fractions read back exactly, any decrease of a
//! float counter is a reset (decision 3), and a NaN or infinite sample is
//! dropped (decision 4).
#![cfg(feature = "ingest")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use metriken_model::{GroupSchema, GroupSnapshot, MetricDesc, Snapshot, SnapshotV3};
use metriken_query::{MemoryStore, MetricsSource, QueryResult};

const T0: u64 = 1000;
const TICKS: u64 = 12;

fn desc(name: &str, metric: &str) -> MetricDesc {
    MetricDesc {
        name: name.to_string(),
        metadata: [("metric".to_string(), metric.to_string())].into(),
    }
}

/// A counter in seconds, 0.1 a tick, reset to 0.05 at tick 6.
fn busy(t: u64) -> f64 {
    if t < 6 {
        0.125 + 0.1 * t as f64
    } else {
        0.05 + 0.1 * (t - 6) as f64
    }
}

/// A gauge of fractions, not dyadic.
fn limit(t: u64) -> f64 {
    0.25 - 0.001 * t as f64
}

/// Tick `t`'s snapshot: a group of a float counter and a float gauge, and
/// a group whose counter `turns_total` is an integer before tick 4 and a
/// float from it.
fn snapshot(t: u64) -> Snapshot {
    let floats = Arc::new(GroupSchema {
        float_counters: vec![desc("0", "busy_seconds_total")],
        float_gauges: vec![desc("1", "cpu_limit")],
        ..GroupSchema::default()
    });
    let turns = Arc::new(if t < 4 {
        GroupSchema {
            counters: vec![desc("0", "turns_total")],
            ..GroupSchema::default()
        }
    } else {
        GroupSchema {
            float_counters: vec![desc("0", "turns_total")],
            ..GroupSchema::default()
        }
    });
    let group = |name: &str, schema: &Arc<GroupSchema>| GroupSnapshot {
        name: name.to_string(),
        schema_hash: schema.hash(),
        schema: Some(Arc::clone(schema)),
        window: None,
        counters: Vec::new(),
        gauges: Vec::new(),
        histograms: Vec::new(),
        gauge_histograms: Vec::new(),
        float_counters: Vec::new(),
        float_gauges: Vec::new(),
    };
    let mut f = group("prom/node", &floats);
    f.float_counters = vec![Some(busy(t))];
    f.float_gauges = vec![Some(limit(t))];
    let mut g = group("prom/turns", &turns);
    if t < 4 {
        g.counters = vec![Some(10 * t)];
    } else {
        g.float_counters = vec![Some(10.0 * t as f64 + 0.5)];
    }
    Snapshot::V3(SnapshotV3 {
        systemtime: SystemTime::UNIX_EPOCH + Duration::from_secs(T0 + t),
        duration: Duration::from_millis(1),
        metadata: HashMap::new(),
        groups: vec![f, g],
    })
}

fn store() -> MemoryStore {
    let store = MemoryStore::builder().sampling_interval_ms(1000).build();
    for t in 0..TICKS {
        store.ingest_snapshot(snapshot(t));
    }
    store
}

/// `query`'s one series as `(tick, value)`.
fn one(store: &MemoryStore, query: &str) -> Vec<(u64, f64)> {
    let result = store
        .query_range(query, T0 as f64, (T0 + TICKS - 1) as f64, 1.0)
        .unwrap_or_else(|e| panic!("{query}: {e}"));
    let QueryResult::Matrix { result, .. } = result else {
        panic!("{query}: not a matrix")
    };
    assert_eq!(result.len(), 1, "{query}: {result:?}");
    result[0]
        .values
        .iter()
        .map(|(t, v)| (*t as u64 - T0, *v))
        .collect()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-12
}

#[test]
fn a_float_counter_and_gauge_are_listed_as_a_counter_and_a_gauge() {
    let store = store();
    for name in ["busy_seconds_total", "turns_total"] {
        assert!(store.has_counter(name), "{name}");
        assert!(!store.has_gauge(name), "{name}");
    }
    assert!(store.has_gauge("cpu_limit"));
    assert!(!store.has_counter("cpu_limit"));
    assert_eq!(store.gauge_names(), ["cpu_limit"]);
}

/// A float gauge reads back as stored, fraction included.
#[test]
fn a_float_gauge_reads_back_exactly() {
    let store = store();
    let got: Vec<(u64, u64)> = one(&store, "cpu_limit")
        .into_iter()
        .map(|(t, v)| (t, v.to_bits()))
        .collect();
    let want: Vec<(u64, u64)> = (0..TICKS).map(|t| (t, limit(t).to_bits())).collect();
    assert_eq!(got, want);
    let avg = one(&store, "avg_over_time(cpu_limit[1s])");
    for (t, v) in avg.into_iter().filter(|(t, _)| *t > 0) {
        assert!(close(v, (limit(t) + limit(t - 1)) / 2.0), "{t}: {v}");
    }
}

/// A float counter's rate is its fractional increase; the decrease at tick
/// 6 is a reset, so the increase there is the value after it.
#[test]
fn a_float_counters_rate_resets_on_a_decrease() {
    let store = store();
    let irate = one(&store, "irate(busy_seconds_total[2s])");
    assert_eq!(irate.len(), TICKS as usize - 1);
    for (t, v) in irate {
        let want = if t == 6 { busy(6) } else { 0.1 };
        assert!(close(v, want), "{t}: {v} {want}");
    }
}

/// A live series that turns from integer to float rates continuously
/// across the change.
#[test]
fn a_counter_that_turns_float_rates_across_the_change() {
    let store = store();
    let irate = one(&store, "irate(turns_total[2s])");
    let want: Vec<(u64, f64)> = (1..TICKS)
        .map(|t| (t, if t == 4 { 10.5 } else { 10.0 }))
        .collect();
    assert_eq!(irate, want);
}

/// The non-finite readings a producer other than metriken-recorder can
/// send, by tick: NaN at 3, +Inf at 5, -Inf at 8.
fn non_finite(t: u64) -> Option<f64> {
    match t {
        3 => Some(f64::NAN),
        5 => Some(f64::INFINITY),
        8 => Some(f64::NEG_INFINITY),
        _ => None,
    }
}

/// A non-finite float sample is dropped at ingest: a gauge reads its
/// previous value there (the lookback finds no sample at that tick), and a
/// counter's rate after it is finite and unchanged.
#[test]
fn a_non_finite_float_sample_is_dropped() {
    let schema = Arc::new(GroupSchema {
        float_counters: vec![desc("0", "odd_total")],
        float_gauges: vec![desc("1", "odd_level")],
        ..GroupSchema::default()
    });
    let store = MemoryStore::builder().sampling_interval_ms(1000).build();
    for t in 0..TICKS {
        let bad = non_finite(t);
        store.ingest_snapshot(Snapshot::V3(SnapshotV3 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_secs(T0 + t),
            duration: Duration::from_millis(1),
            metadata: HashMap::new(),
            groups: vec![GroupSnapshot {
                name: "prom/odd".to_string(),
                schema_hash: schema.hash(),
                schema: Some(Arc::clone(&schema)),
                window: None,
                counters: Vec::new(),
                gauges: Vec::new(),
                histograms: Vec::new(),
                gauge_histograms: Vec::new(),
                float_counters: vec![Some(bad.unwrap_or(10.0 * t as f64))],
                float_gauges: vec![Some(bad.unwrap_or(t as f64 + 0.5))],
            }],
        }));
    }
    let level = one(&store, "odd_level");
    let want: Vec<(u64, f64)> = (0..TICKS)
        .map(|t| {
            let held = (0..=t).rev().find(|&u| non_finite(u).is_none()).unwrap();
            (t, held as f64 + 0.5)
        })
        .collect();
    assert_eq!(level, want);
    let irate = one(&store, "irate(odd_total[3s])");
    assert_eq!(irate, (1..TICKS).map(|t| (t, 10.0)).collect::<Vec<_>>());
}
