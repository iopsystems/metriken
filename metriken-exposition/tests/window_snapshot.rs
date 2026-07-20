use metriken::{metric, Counter, Window, WindowedCounterGroup, WindowedLazyCounter};
use metriken_exposition::{Snapshot, SnapshotterBuilder};

#[metric(name = "expo_window_scalar_counter")]
static SCALAR: WindowedLazyCounter = WindowedLazyCounter::new(Counter::new);

#[metric(name = "expo_window_counter_group")]
static GROUP: WindowedCounterGroup = WindowedCounterGroup::new(2);

fn find_counter_window(mut snapshot: Snapshot, metric_name: &str) -> Option<Option<Window>> {
    snapshot
        .counters()
        .into_iter()
        .find(|c| c.metadata.get("metric").map(String::as_str) == Some(metric_name))
        .map(|c| c.window)
}

#[test]
fn scalar_counter_window_is_exposed() {
    SCALAR.set_with_window(123, Window::new(70, 90));
    let snapshot = SnapshotterBuilder::new().build().snapshot();
    let window = find_counter_window(snapshot, "expo_window_scalar_counter")
        .expect("scalar counter must be in snapshot");
    assert_eq!(window, Some(Window::new(70, 90)));
}

#[test]
fn counter_group_window_is_exposed() {
    GROUP.set_metadata(
        0,
        std::collections::HashMap::from([("slot".into(), "0".into())]),
    );
    GROUP.set_with_window(0, 5, Window::new(11, 22));
    let snapshot = SnapshotterBuilder::new().build().snapshot();
    let mut snapshot = snapshot;
    let window = snapshot
        .counters()
        .into_iter()
        .find(|c| {
            c.metadata.get("metric").map(String::as_str) == Some("expo_window_counter_group")
                && c.metadata.get("slot").map(String::as_str) == Some("0")
        })
        .map(|c| c.window)
        .expect("counter group entry must be in snapshot");
    assert_eq!(window, Some(Window::new(11, 22)));
}

#[metric(name = "expo_window_pair_consistency")]
static PAIR: WindowedLazyCounter = WindowedLazyCounter::new(Counter::new);

#[test]
fn snapshot_pairs_value_and_window_consistently() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    const ITERS: u64 = 200_000;
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let stop = stop.clone();
        thread::spawn(move || {
            for v in 1..=ITERS {
                PAIR.set_with_window(v, Window::new(v, v + 1));
            }
            stop.store(true, Ordering::Relaxed);
        })
    };
    let reader = thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let mut snapshot = SnapshotterBuilder::new().build().snapshot();
            for c in snapshot.counters() {
                if c.metadata.get("metric").map(String::as_str)
                    == Some("expo_window_pair_consistency")
                {
                    if let Some(w) = c.window {
                        assert_eq!(
                            w.begin_ns, c.value,
                            "exposition torn pair: value {} with {:?}",
                            c.value, w
                        );
                        assert_eq!(w.end_ns, c.value + 1);
                    }
                }
            }
        }
    });
    writer.join().unwrap();
    reader.join().unwrap();
}
