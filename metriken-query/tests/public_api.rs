//! Tests that exercise the public API as a DOWNSTREAM crate sees it.
//!
//! Deliberately not in `integration.rs`, which is gated behind the
//! `fixtures` feature and so is skipped by a default `cargo test`. These
//! assert things an in-crate unit test structurally cannot: `#[non_exhaustive]`
//! and private fields do not apply within the defining crate, so only an
//! external crate can prove a type is still constructible.

/// `EnvPoint` is constructible from OUTSIDE the crate.
///
/// This lives here rather than in the unit tests because `#[non_exhaustive]`
/// does not apply within the defining crate — an in-crate test would pass even
/// if the constructors were missing or private, which is precisely the mistake
/// this guards. Marking the struct non-exhaustive removed literal construction
/// for downstreams, so `new` + the `with_*` builders have to be enough on their
/// own, and only an integration test proves it.
#[test]
fn env_point_is_constructible_by_a_downstream() {
    use metriken_query::EnvPoint;

    let plain = EnvPoint::new(1.0, 2.0, 3.0, 4.0, 5.0, 6.0);
    assert_eq!(plain.median, 4.0);
    assert_eq!((plain.unc_lo, plain.unc_hi), (None, None));
    assert!(!plain.interpolated);

    let full = EnvPoint::new(1.0, 2.0, 3.0, 4.0, 5.0, 6.0)
        .with_band(Some((3.5, 4.5)))
        .with_interpolated(true);
    assert_eq!((full.unc_lo, full.unc_hi), (Some(3.5), Some(4.5)));
    assert!(full.interpolated);

    // Clearing the band puts both edges back to None together — they are
    // documented as parallel, so neither can be set without the other.
    assert_eq!(
        {
            let p = full.with_band(None);
            (p.unc_lo, p.unc_hi)
        },
        (None, None),
    );
}

/// Histogram functions over a tick that holds more than `u32::MAX`
/// observations, through `MemoryStore::query_range`.
///
/// The streaming path once summed each tick's deltas into a `u32`
/// cumulative clipped at `u32::MAX`: 5e9 observations counted as
/// 4,294,967,295, and every quantile whose rank lay above the clip landed in
/// the last bucket below it. The series here puts 4e9 observations in bucket
/// 100 and 1e9 in bucket 200, so rank 0.85 · 5e9 = 4.25e9 is in bucket 200
/// only when the cumulative is not clipped.
#[test]
fn histogram_functions_count_ticks_above_u32_max() {
    use metriken_query::{MemoryStore, QueryResult};
    use metriken_storage::HistogramSnapshot;

    const S: u64 = 1_000_000_000;
    const LOW: u64 = 4_000_000_000;
    const HIGH: u64 = 1_000_000_000;

    let config = histogram::Config::new(3, 64).unwrap();
    let bucket = |i: usize| {
        let b = histogram::Histogram::with_config(&config)
            .iter()
            .nth(i)
            .unwrap();
        (b.start() as f64, b.end() as f64)
    };
    let (lo100, hi100) = bucket(100);
    let (lo200, hi200) = bucket(200);

    let matrix = |r: Result<QueryResult, _>| match r.unwrap() {
        QueryResult::Matrix { result } => result,
        other => panic!("expected a matrix, got {other:?}"),
    };
    let close = |a: f64, b: f64| (a - b).abs() <= b.abs() * 1e-12;

    // `ticks` cumulative snapshots after an empty first one; the last holds
    // LOW in bucket 100 and HIGH in bucket 200.
    let store_with = |ticks: u64| {
        let store = MemoryStore::builder().sampling_interval_ms(1000).build();
        let mut snaps = vec![HistogramSnapshot {
            index: vec![],
            count: vec![],
        }];
        for k in 1..=ticks {
            let (low, high) = (LOW * k / ticks, HIGH * k / ticks);
            snaps.push(HistogramSnapshot {
                index: vec![100, 200],
                count: vec![low, low + high],
            });
        }
        let ts = (0..=ticks).map(|k| (1000 + k) * S).collect();
        store
            .insert_histogram_series("lat", [("op", "read")], config, ts, snaps)
            .unwrap();
        store
    };

    // One tick of 5e9 observations.
    let store = store_with(1);
    let q = matrix(store.query_range("histogram_quantiles([0.85], lat)", 1000.0, 1001.0, 1.0));
    assert_eq!(q[0].values, vec![(1001.0, hi200)]);
    assert_eq!(q[0].intervals, Some(vec![(lo200, hi200)]));

    let count = matrix(store.query_range("histogram_count(lat)", 1000.0, 1001.0, 1.0));
    assert_eq!(count[0].values, vec![(1001.0, (LOW + HIGH) as f64)]);

    let sum_lo = LOW as f64 * lo100 + HIGH as f64 * lo200;
    let sum_hi = LOW as f64 * hi100 + HIGH as f64 * hi200;
    let sum = matrix(store.query_range("histogram_sum(lat)", 1000.0, 1001.0, 1.0));
    let (lo, hi) = sum[0].intervals.as_ref().unwrap()[0];
    assert!(
        close(lo, sum_lo) && close(hi, sum_hi),
        "sum band [{lo}, {hi}]"
    );

    let n = (LOW + HIGH) as f64;
    let mean = matrix(store.query_range("histogram_mean(lat)", 1000.0, 1001.0, 1.0));
    let (lo, hi) = mean[0].intervals.as_ref().unwrap()[0];
    assert!(
        close(lo, sum_lo / n) && close(hi, sum_hi / n),
        "mean band [{lo}, {hi}]"
    );
    assert!(close(mean[0].values[0].1 * n, sum[0].values[0].1));

    let QueryResult::HistogramHeatmap { result } = store
        .query_range("histogram_heatmap(lat)", 1000.0, 1001.0, 1.0)
        .unwrap()
    else {
        panic!("expected a heatmap");
    };
    let mut cells: Vec<f64> = result.data.iter().map(|(_, _, c)| *c).collect();
    cells.sort_by(f64::total_cmp);
    assert_eq!(cells, vec![HIGH as f64, LOW as f64]);
    assert_eq!(result.max_value, LOW as f64);

    // Two ticks of 2.5e9 each, below `u32::MAX` apiece; a 2s stride sums
    // them into one 5e9 cumulative.
    let store = store_with(2);
    let q = matrix(store.query_range("histogram_quantiles([0.85], lat, 2)", 1000.0, 1002.0, 1.0));
    assert_eq!(q[0].values, vec![(1002.0, hi200)]);
    let count = matrix(store.query_range("histogram_count(lat, 2)", 1000.0, 1002.0, 1.0));
    assert_eq!(count[0].values, vec![(1002.0, n)]);
    let QueryResult::HistogramHeatmap { result } = store
        .query_range("histogram_heatmap(lat, 2)", 1000.0, 1002.0, 1.0)
        .unwrap()
    else {
        panic!("expected a heatmap");
    };
    assert_eq!(result.max_value, LOW as f64);
    assert_eq!(result.min_value, HIGH as f64);
}
