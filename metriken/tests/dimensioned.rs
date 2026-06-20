use metriken::{DimensionedCounter, DimensionedGauge, DimensionedHistogram, MetricDimension};

#[derive(MetricDimension)]
enum CacheResult {
    Hit,
    Miss,
}

#[derive(MetricDimension)]
#[metric_dimension(name = "op")]
enum Operation {
    Read,
    Write,
    Delete,
}

#[test]
fn simple_enum_labels() {
    assert_eq!(CacheResult::COUNT, 2);
    assert_eq!(CacheResult::Hit.index(), 0);
    assert_eq!(CacheResult::Miss.index(), 1);

    let labels = CacheResult::Hit.labels();
    assert_eq!(labels.get("cache_result").unwrap(), "hit");

    let all = CacheResult::all_labels();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].get("cache_result").unwrap(), "hit");
    assert_eq!(all[1].get("cache_result").unwrap(), "miss");
}

#[test]
fn custom_name_override() {
    assert_eq!(Operation::COUNT, 3);
    let labels = Operation::Read.labels();
    assert_eq!(labels.get("op").unwrap(), "read");

    let all = Operation::all_labels();
    assert_eq!(all.len(), 3);
    assert_eq!(all[2].get("op").unwrap(), "delete");
}

#[test]
fn dimensioned_counter_with_derive() {
    static C: DimensionedCounter<CacheResult> = DimensionedCounter::new();

    C.increment(CacheResult::Hit);
    C.increment(CacheResult::Hit);
    C.increment(CacheResult::Miss);

    assert_eq!(C.value(CacheResult::Hit), Some(2));
    assert_eq!(C.value(CacheResult::Miss), Some(1));
}

#[test]
fn dimensioned_gauge_with_derive() {
    static G: DimensionedGauge<Operation> = DimensionedGauge::new();

    G.set(Operation::Read, 10);
    G.set(Operation::Write, 5);
    G.decrement(Operation::Read);

    assert_eq!(G.value(Operation::Read), Some(9));
    assert_eq!(G.value(Operation::Write), Some(5));
    // Delete was never written but backing is initialized after first write — should be Some(0)
    assert_eq!(G.value(Operation::Delete), Some(0));
}

#[test]
fn dimensioned_histogram_with_derive() {
    static H: DimensionedHistogram<Operation> = DimensionedHistogram::new(7, 64);

    H.increment(Operation::Read, 1000).unwrap();
    H.increment(Operation::Write, 2000).unwrap();

    assert!(H.load(Operation::Read).is_some());
    assert!(H.load(Operation::Write).is_some());
    // Delete was never written but the group backing is initialized — still Some (empty histogram)
    assert!(H.load(Operation::Delete).is_some());
}

#[test]
fn compound_tuple_dimension() {
    #[derive(MetricDimension)]
    enum Status {
        Ok,
        Err,
    }

    assert_eq!(<(CacheResult, Status) as MetricDimension>::COUNT, 4); // 2 * 2

    assert_eq!((CacheResult::Hit, Status::Ok).index(), 0); // 0 * 2 + 0
    assert_eq!((CacheResult::Hit, Status::Err).index(), 1); // 0 * 2 + 1
    assert_eq!((CacheResult::Miss, Status::Ok).index(), 2); // 1 * 2 + 0
    assert_eq!((CacheResult::Miss, Status::Err).index(), 3); // 1 * 2 + 1

    let all = <(CacheResult, Status) as MetricDimension>::all_labels();
    assert_eq!(all.len(), 4);
    assert_eq!(all[0].get("cache_result").unwrap(), "hit");
    assert_eq!(all[0].get("status").unwrap(), "ok");
    assert_eq!(all[3].get("cache_result").unwrap(), "miss");
    assert_eq!(all[3].get("status").unwrap(), "err");
}

#[test]
fn compound_tuple_counter() {
    #[derive(MetricDimension)]
    enum Status {
        Ok,
        Err,
    }

    static REQUESTS: DimensionedCounter<(CacheResult, Status)> = DimensionedCounter::new();

    REQUESTS.increment((CacheResult::Hit, Status::Ok));
    assert_eq!(REQUESTS.value((CacheResult::Hit, Status::Ok)), Some(1));
    // Miss/Err was never written but backing is initialized — should be Some(0)
    assert_eq!(REQUESTS.value((CacheResult::Miss, Status::Err)), Some(0));
}

#[test]
fn all_metadata_present_after_exposition() {
    use metriken::Metric;

    static C: DimensionedCounter<CacheResult> = DimensionedCounter::new();

    // Trigger metadata init via Metric::value() — simulates what exposition does
    if let Some(metriken::Value::CounterGroup(g)) = Metric::value(&C) {
        let snapshot = g.metadata_snapshot();
        assert_eq!(
            snapshot.len(),
            2,
            "both CacheResult variants must have metadata"
        );

        let hit = snapshot.iter().find(|(idx, _)| *idx == 0).map(|(_, m)| m);
        assert_eq!(hit.unwrap().get("cache_result").unwrap(), "hit");

        let miss = snapshot.iter().find(|(idx, _)| *idx == 1).map(|(_, m)| m);
        assert_eq!(miss.unwrap().get("cache_result").unwrap(), "miss");
    } else {
        panic!("expected Value::CounterGroup");
    }
}
