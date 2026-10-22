//! Gauge histograms, read as state: at each evaluation time, each series'
//! latest row within its staleness window, summed bucket by bucket across
//! the series a result pools. No value is a difference between two rows.
//!
//! A gauge histogram's row is the distribution of something now (free
//! extents per size, queue lengths across queues), not observations since
//! the previous row, so the counting histograms' per-row and per-stride
//! deltas (`super::histogram`) do not apply. The rules are those of
//! `docs/journal/2026-10-21-gauge-histogram.md`:
//!
//! - evaluation times are a gauge's: the grid `start + k·step` (the start
//!   snapped down to a step boundary in grid mode), the instant itself, or
//!   [`QueryOptions::eval_timestamps`](crate::QueryOptions::eval_timestamps);
//! - a series has a value at `t` when its latest row at or before `t` is
//!   within `max(step, interval)` of it, as a gauge is held;
//! - the qualifying series' states are summed bucket by bucket, per group
//!   for `histogram_count`/`sum`/`mean` with `by`/`without`, all together
//!   for the quantiles and the heatmap. Streams of different bucket
//!   configurations were already rebucketed to the coarsest common one when
//!   the source merged them (`HistogramStream::merge`).
//! - an empty state (series present, no items) is a count and a sum of 0
//!   and has no quantile and no mean.

use ::histogram::{Config, CumulativeROHistogramRef};

use super::histogram::{
    apply_quantiles, assign_group_index, bucket_weighted_sum, build_grouped_output, flush_accum,
    quantile_keys, quantile_samples, Accum, QuantileRow,
};
use super::{derive_group_labels, GroupBy};
use crate::histogram_stream::HistogramStream;
use crate::labels::Labels;
use crate::promql::{HistogramHeatmapResult, MatrixSample};
use crate::types::HistogramSnapshot;

/// What a call computes from the summed state at each time.
#[derive(Clone, Copy)]
pub(crate) enum Reduce {
    Count,
    Sum,
    Mean,
}

/// The evaluation times of a range: `eval` within `[start_ns, end_ns]` when
/// given, else `start_ns + k·step_ns` up to `end_ns`, or `start_ns` alone for
/// a zero step or a range of one point. The caller snaps `start_ns` as the
/// gauge grid does.
pub(crate) fn times(start_ns: u64, end_ns: u64, step_ns: u64, eval: Option<&[u64]>) -> Vec<u64> {
    if let Some(points) = eval {
        return points
            .iter()
            .copied()
            .filter(|t| (start_ns..=end_ns).contains(t))
            .collect();
    }
    if step_ns == 0 || end_ns <= start_ns {
        return vec![start_ns];
    }
    let n = (end_ns - start_ns) / step_ns + 1;
    (0..n).map(|k| start_ns + k * step_ns).collect()
}

/// Walk `stream`'s rows along `times`, keeping each series' latest row, and
/// call `at` at each time with the per-group sums of the states of the
/// series whose latest row is within `staleness_ns`, and which groups had
/// one. The accumulators must be drained by `at`.
fn walk(
    stream: HistogramStream,
    times: &[u64],
    staleness_ns: u64,
    series_group: &[usize],
    groups: usize,
    mut at: impl FnMut(u64, &mut [Accum], &[bool]),
) {
    let HistogramStream { meta, rows } = stream;
    let config = meta.config;
    let mut rows = rows.peekable();
    let mut latest: Vec<Option<(u64, HistogramSnapshot)>> = vec![None; meta.series.len()];
    let mut accums: Vec<Accum> = (0..groups).map(|_| Accum::new(config)).collect();
    let mut present = vec![false; groups];
    for &t in times {
        while let Some(row) = rows.next_if(|r| r.timestamp <= t) {
            latest[row.series_idx] = Some((row.timestamp, row.snapshot));
        }
        present.fill(false);
        for (series, row) in latest.iter().enumerate() {
            let Some((ts, snapshot)) = row else { continue };
            if t - ts > staleness_ns {
                continue;
            }
            let g = series_group[series];
            present[g] = true;
            add_state(snapshot, &mut accums[g]);
        }
        at(t, &mut accums, &present);
    }
}

/// Add a state (cumulative sparse buckets) into `accum` as per-bucket
/// counts.
fn add_state(snapshot: &HistogramSnapshot, accum: &mut Accum) {
    let mut below = 0u64;
    for (&idx, &cumulative) in snapshot.index.iter().zip(&snapshot.count) {
        let count = cumulative.saturating_sub(below);
        below = cumulative;
        if count > 0 {
            accum.add(idx, count);
        }
    }
}

/// `quantiles` of the summed state of every series at each of `times`, one
/// `MatrixSample` per quantile, labelled as the counting form labels them.
pub(crate) fn quantiles(
    stream: HistogramStream,
    quantiles_in: &[f64],
    times: &[u64],
    staleness_ns: u64,
    metric_name: &str,
) -> Vec<MatrixSample> {
    let config = stream.meta.config;
    let (keys, floats) = quantile_keys(quantiles_in);
    if keys.is_empty() || stream.meta.series.is_empty() {
        return Vec::new();
    }
    let series_group = vec![0; stream.meta.series.len()];
    let mut outputs: Vec<Vec<QuantileRow>> = vec![Vec::new(); quantiles_in.len()];
    let (mut idx, mut cnt) = (Vec::new(), Vec::new());
    walk(
        stream,
        times,
        staleness_ns,
        &series_group,
        1,
        |t, accums, _| {
            if flush_accum(&mut accums[0], &mut idx, &mut cnt) {
                apply_quantiles(config, &idx, &cnt, &floats, &keys, t, &mut outputs);
            }
        },
    );
    quantile_samples(quantiles_in, outputs, metric_name)
}

/// `histogram_count`, `histogram_sum` or `histogram_mean` of the summed
/// state of each group's series at each of `times`.
pub(crate) fn reduce(
    stream: HistogramStream,
    reduce: Reduce,
    group_by: GroupBy<'_>,
    times: &[u64],
    staleness_ns: u64,
    metric_name: &str,
) -> Vec<MatrixSample> {
    let config = stream.meta.config;
    let mut group_keys: Vec<Labels> = Vec::new();
    let series_group: Vec<usize> = stream
        .meta
        .series
        .iter()
        .map(|labels| assign_group_index(&mut group_keys, derive_group_labels(labels, group_by)))
        .collect();
    let groups = group_keys.len();
    let mut values: Vec<Vec<(f64, f64)>> = vec![Vec::new(); groups];
    let mut bands: Vec<Vec<Option<(f64, f64)>>> = vec![Vec::new(); groups];
    let (mut idx, mut cnt) = (Vec::new(), Vec::new());
    walk(
        stream,
        times,
        staleness_ns,
        &series_group,
        groups,
        |t, accums, present| {
            for g in 0..groups {
                let held = flush_accum(&mut accums[g], &mut idx, &mut cnt);
                if !present[g] {
                    continue;
                }
                let point = if held {
                    let r = CumulativeROHistogramRef::from_parts_unchecked(config, &idx, &cnt);
                    value(&r, reduce)
                } else {
                    // Present with no items: a count and a sum of zero, and no
                    // mean.
                    match reduce {
                        Reduce::Count => Some((0.0, None)),
                        Reduce::Sum => Some((0.0, Some((0.0, 0.0)))),
                        Reduce::Mean => None,
                    }
                };
                if let Some((v, band)) = point {
                    values[g].push((t as f64 / 1e9, v));
                    bands[g].push(band);
                }
            }
        },
    );
    build_grouped_output(metric_name, group_keys, values, Some(bands))
}

/// One reduction of a non-empty summed state: the value and its bucket
/// band, as the counting form reports them.
fn value(r: &CumulativeROHistogramRef<'_>, reduce: Reduce) -> Option<(f64, Option<(f64, f64)>)> {
    let c = r.total_count();
    match reduce {
        Reduce::Count => Some((c as f64, None)),
        Reduce::Sum => {
            let (lo, hi) = bucket_weighted_sum(r);
            r.mean().map(|m| (c as f64 * m, Some((lo, hi))))
        }
        Reduce::Mean => {
            let (lo, hi) = bucket_weighted_sum(r);
            r.mean()
                .map(|m| (m, (c > 0).then(|| (lo / c as f64, hi / c as f64))))
        }
    }
}

/// The summed state of every series at each of `times`, as a heatmap: a
/// column per time at which any series has a value (an empty state is a
/// column with no cells), a cell per non-empty bucket.
pub(crate) fn heatmap(
    stream: HistogramStream,
    times: &[u64],
    staleness_ns: u64,
) -> Option<HistogramHeatmapResult> {
    let config: Config = stream.meta.config;
    if stream.meta.series.is_empty() {
        return None;
    }
    let all_bounds: Vec<u64> =
        ::histogram::Histogram::new(config.grouping_power(), config.max_value_power())
            .ok()?
            .iter()
            .map(|b| b.end())
            .collect();
    let series_group = vec![0; stream.meta.series.len()];
    let mut timestamps: Vec<f64> = Vec::new();
    let mut data: Vec<(usize, usize, f64)> = Vec::new();
    let (mut idx, mut cnt) = (Vec::new(), Vec::new());
    walk(
        stream,
        times,
        staleness_ns,
        &series_group,
        1,
        |t, accums, present| {
            let held = flush_accum(&mut accums[0], &mut idx, &mut cnt);
            if !present[0] {
                return;
            }
            let column = timestamps.len();
            timestamps.push(t as f64 / 1e9);
            if !held {
                return;
            }
            let mut below = 0u64;
            for (&bucket, &cumulative) in idx.iter().zip(&cnt) {
                let count = cumulative - below;
                below = cumulative;
                if count > 0 {
                    data.push((column, bucket as usize, count as f64));
                }
            }
        },
    );
    if timestamps.is_empty() {
        return None;
    }
    let min_value = data.iter().map(|d| d.2).fold(f64::MAX, f64::min);
    let max_value = data.iter().map(|d| d.2).fold(f64::MIN, f64::max);
    let lo_bucket = data.iter().map(|d| d.1).min();
    let hi_bucket = data.iter().map(|d| d.1).max();
    let (bucket_bounds, data) = match (lo_bucket, hi_bucket) {
        (Some(lo), Some(hi)) if hi < all_bounds.len() => (
            all_bounds[lo..=hi].to_vec(),
            data.into_iter().map(|(t, b, c)| (t, b - lo, c)).collect(),
        ),
        _ => (all_bounds, data),
    };
    Some(HistogramHeatmapResult {
        timestamps,
        bucket_bounds,
        data,
        min_value: if min_value == f64::MAX {
            0.0
        } else {
            min_value
        },
        max_value: if max_value == f64::MIN {
            0.0
        } else {
            max_value
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::histogram_stream::{HistogramRow, HistogramStreamMeta};

    const S: u64 = 1_000_000_000;

    fn snap(buckets: &[(u32, u64)]) -> HistogramSnapshot {
        let mut running = 0;
        HistogramSnapshot {
            index: buckets.iter().map(|b| b.0).collect(),
            count: buckets
                .iter()
                .map(|b| {
                    running += b.1;
                    running
                })
                .collect(),
        }
    }

    fn stream(rows: Vec<(usize, u64, HistogramSnapshot)>, series: usize) -> HistogramStream {
        HistogramStream {
            meta: HistogramStreamMeta {
                config: Config::new(0, 16).unwrap(),
                series: (0..series)
                    .map(|i| {
                        let mut l = Labels::default();
                        l.inner.insert("dev".to_string(), format!("d{i}"));
                        l
                    })
                    .collect(),
            },
            rows: Box::new(rows.into_iter().map(|(series_idx, timestamp, snapshot)| {
                HistogramRow {
                    series_idx,
                    timestamp,
                    snapshot,
                }
            })),
        }
    }

    /// Two equal states in a row read as that state twice, not as an empty
    /// delta; a state that shrinks in one bucket and grows in another reads
    /// as the new state, not as the growth alone.
    #[test]
    fn a_count_is_the_state_not_a_difference() {
        let rows = vec![
            (0, 10 * S, snap(&[(1, 100), (3, 10)])),
            (0, 11 * S, snap(&[(1, 100), (3, 10)])),
            (0, 12 * S, snap(&[(1, 90), (3, 15)])),
        ];
        let got = reduce(
            stream(rows, 1),
            Reduce::Count,
            GroupBy::Include(&[]),
            &times(10 * S, 12 * S, S, None),
            S,
            "free",
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].values, [(10.0, 110.0), (11.0, 110.0), (12.0, 105.0)]);
    }

    /// Series sum bucket-wise; a series is held a staleness window past its
    /// latest row and not beyond.
    #[test]
    fn series_sum_and_go_stale() {
        let rows = vec![
            (0, 10 * S, snap(&[(1, 5)])),
            (1, 10 * S, snap(&[(1, 7)])),
            (0, 11 * S, snap(&[(1, 5)])),
            (0, 12 * S, snap(&[(1, 5)])),
        ];
        let got = reduce(
            stream(rows, 2),
            Reduce::Count,
            GroupBy::Include(&[]),
            &times(10 * S, 13 * S, S, None),
            S,
            "free",
        );
        // d1's only row (10 s) is held to 11 s; d0 has no row after 12 s.
        assert_eq!(
            got[0].values,
            [(10.0, 12.0), (11.0, 12.0), (12.0, 5.0), (13.0, 5.0)]
        );
    }

    #[test]
    fn an_empty_state_has_a_count_of_zero_and_no_quantile() {
        let rows = vec![(0, 10 * S, snap(&[]))];
        let s = || stream(vec![(0, 10 * S, snap(&[]))], 1);
        let count = reduce(
            s(),
            Reduce::Count,
            GroupBy::Include(&[]),
            &[10 * S],
            S,
            "free",
        );
        assert_eq!(count[0].values, [(10.0, 0.0)]);
        assert!(reduce(
            s(),
            Reduce::Mean,
            GroupBy::Include(&[]),
            &[10 * S],
            S,
            "free"
        )
        .is_empty());
        assert!(quantiles(stream(rows, 1), &[0.5], &[10 * S], S, "free").is_empty());
    }

    /// The quantile is the state's, from the bucket it lands in: of 3 items
    /// of size 1 and 1 of size 4 to 7, the median is in bucket 1 and the max
    /// in bucket 3.
    #[test]
    fn quantiles_of_the_summed_state() {
        let rows = vec![
            (0, 10 * S, snap(&[(1, 2)])),
            (1, 10 * S, snap(&[(1, 1), (3, 1)])),
        ];
        let got = quantiles(stream(rows, 2), &[0.5, 1.0], &[10 * S], S, "free");
        let by_q: Vec<(String, f64)> = got
            .iter()
            .map(|m| (m.metric["quantile"].clone(), m.values[0].1))
            .collect();
        assert_eq!(by_q, [("0.5".to_string(), 1.0), ("1".to_string(), 7.0)]);
    }

    #[test]
    fn a_heatmap_column_is_the_state() {
        let rows = vec![
            (0, 10 * S, snap(&[(1, 3), (3, 1)])),
            (0, 11 * S, snap(&[(1, 2), (3, 2)])),
        ];
        let h = heatmap(stream(rows, 1), &times(10 * S, 11 * S, S, None), S).unwrap();
        assert_eq!(h.timestamps, [10.0, 11.0]);
        assert_eq!(h.bucket_bounds, [1, 3, 7]);
        assert_eq!(h.data, [(0, 0, 3.0), (0, 2, 1.0), (1, 0, 2.0), (1, 2, 2.0)]);
    }
}
