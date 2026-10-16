//! Streaming `histogram_quantile` / `histogram_quantiles` pipeline.
//!
//! Used by both `histogram_quantile(q, m)` (standard PromQL, single
//! quantile) and `histogram_quantiles([qs], m)` (rezolus extension,
//! multiple quantiles in one walk) — they're the same operation with
//! N=1 vs N>1 and share this code path.
//!
//! The pipeline walks per-tick: pull the next snapshot from each
//! matching [`HistogramStream`], sum the per-series deltas into a
//! small reusable scratch buffer, compute the N quantiles for that
//! tick, append to the N output Vecs, and repeat. Peak resident is
//! one merged delta histogram (tens of KB at typical bucket density)
//! plus the N partial output Vecs — independent of input series
//! cardinality.
//!
//! Output ties to the existing [`MatrixSample`] JSON shape so the
//! boundary serializer is unchanged. The `Iterator` trait isn't a
//! fit (the per-tick summed `Ref` borrows from scratch that mutates
//! between ticks — the classic lending-iterator problem), so the
//! function is a straight loop rather than a generic chain.

use std::collections::HashMap;

use ::histogram::{Config, CumulativeROHistogramRef, Quantile, QuantilesResult};

use crate::histogram_stream::{HistogramRow, HistogramStream};
use crate::labels::Labels;
use crate::promql::streaming::{derive_group_labels, Band, GroupBy};
use crate::promql::{HistogramHeatmapResult, MatrixSample};
use crate::types::HistogramSnapshot;

/// One `apply_quantiles` output row: `(t_sec, value, optional bucket band)`.
type QuantileRow = (f64, f64, Option<Band>);

/// Per-group value-uncertainty bands, parallel to a group's values. Aliased to
/// keep `Option<GroupBands>` readable (clears clippy's `type_complexity`).
type GroupBands = Vec<Vec<Option<Band>>>;

/// A strided call's windows, `(anchor + (k - 1)·stride, anchor + k·stride]`
/// for integer `k`. A range query anchors them at multiples of the stride
/// (`anchor_ns` 0), so they fall at the same instants whatever range is
/// read; an instant query anchors them at its own time, so its window is the
/// stride ending there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stride {
    pub(crate) ns: u64,
    pub(crate) anchor_ns: u64,
    /// A window is emitted only when one of its rows is at or after this:
    /// a range's start, so no point falls before the range; 0 for an
    /// instant, whose one window always is.
    pub(crate) emit_from_ns: u64,
}

impl Stride {
    /// The window holding time `t`.
    pub(crate) fn window(&self, t: u64) -> i64 {
        let d = i128::from(t) - i128::from(self.anchor_ns);
        let s = i128::from(self.ns);
        (d.div_euclid(s) + i128::from(d.rem_euclid(s) != 0)) as i64
    }

    /// Where window `k` opens: rows after it belong to the window.
    pub(crate) fn opens_ns(&self, k: i64) -> u64 {
        let at = i128::from(self.anchor_ns) + (i128::from(k) - 1) * i128::from(self.ns);
        at.clamp(0, i128::from(u64::MAX)) as u64
    }
}

/// Assigns a strided call's rows to windows, in timestamp order. The window
/// holding `start_ns` is the first that can be emitted; rows before it supply only the
/// snapshot a delta is taken from. A window closes when a row's timestamp
/// falls in a later one, so rows of several series at one timestamp stay in
/// one window.
struct Windows {
    stride: Stride,
    first: i64,
    current: Option<i64>,
    last_t: u64,
    /// The open window has a row at or after `stride.emit_from_ns`.
    emits: bool,
}

impl Windows {
    fn new(stride: Stride, start_ns: u64) -> Self {
        Self {
            stride,
            first: stride.window(start_ns),
            current: None,
            last_t: 0,
            emits: false,
        }
    }

    /// For a row at `t`: whether it belongs to a window from the first on,
    /// and, when it opens a new one, whether the window it closes is emitted
    /// and if so the time of its last row. A closed window that is not
    /// emitted still has its rows' deltas, which the caller discards.
    fn row(&mut self, t: u64) -> (bool, Option<Option<u64>>) {
        let k = self.stride.window(t);
        if k < self.first {
            return (false, None);
        }
        let closed = match self.current {
            Some(c) if c != k => {
                let at = self.emits.then_some(self.last_t);
                self.emits = false;
                Some(at)
            }
            _ => None,
        };
        self.current = Some(k);
        self.last_t = t;
        self.emits |= t >= self.stride.emit_from_ns;
        (true, closed)
    }

    /// The time of the last row of the open window, if it is emitted.
    fn finish(&self) -> Option<u64> {
        self.current.filter(|_| self.emits).map(|_| self.last_t)
    }
}

// ─── impl HistogramStream ────────────────────────────────────────────────────

/// The histogram operators, over a source's histogram row stream.
pub(crate) trait HistogramOps: Sized {
    /// Compute quantiles over `[start_ns, end_ns]`. Returns one `MatrixSample`
    /// per requested quantile (labeled `quantile: <q>`).
    fn quantiles(
        self,
        quantiles_in: &[f64],
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
        metric_name: &str,
    ) -> Vec<MatrixSample>;

    fn mean(
        self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
        metric_name: &str,
    ) -> Vec<MatrixSample>;

    fn count(
        self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
        metric_name: &str,
    ) -> Vec<MatrixSample>;

    fn sum(
        self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
        metric_name: &str,
    ) -> Vec<MatrixSample>;

    fn irate(
        self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        metric_name: &str,
    ) -> Vec<MatrixSample>;

    fn heatmap(
        self,
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
    ) -> Option<HistogramHeatmapResult>;
}

impl HistogramOps for HistogramStream {
    fn quantiles(
        self,
        quantiles_in: &[f64],
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        quantiles_impl(self, quantiles_in, start_ns, end_ns, stride, metric_name)
    }

    fn mean(
        self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        reduce(self, group_by, start_ns, end_ns, stride, metric_name, |r| {
            let c = r.total_count();
            r.mean().map(|m| {
                // mean = sum / N; propagate the sum band divided by N.
                let band = if c > 0 {
                    let (lo, hi) = bucket_weighted_sum(r);
                    Some((lo / c as f64, hi / c as f64))
                } else {
                    None
                };
                (m, band)
            })
        })
    }

    fn count(
        self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        reduce(self, group_by, start_ns, end_ns, stride, metric_name, |r| {
            // count is exact — an integer tally, no bucket-resolution
            // uncertainty. No band.
            let c = r.total_count();
            (c > 0).then_some((c as f64, None))
        })
    }

    fn sum(
        self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        reduce(self, group_by, start_ns, end_ns, stride, metric_name, |r| {
            let c = r.total_count();
            if c == 0 {
                return None;
            }
            // Nominal sum = total_count · mean (midpoints); the band is the
            // bucket-resolution range [Σ count·start, Σ count·end].
            r.mean()
                .map(|m| (c as f64 * m, Some(bucket_weighted_sum(r))))
        })
    }

    fn irate(
        self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        irate_impl(self, group_by, start_ns, end_ns, metric_name)
    }

    fn heatmap(
        self,
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
    ) -> Option<HistogramHeatmapResult> {
        heatmap_impl(self, start_ns, end_ns, stride)
    }
}

// ─── Delta helpers ────────────────────────────────────────────────────────────

fn total_count(snap: &HistogramSnapshot) -> u64 {
    snap.count.last().copied().unwrap_or(0)
}

/// Bucket-resolution bounds on the sum of all observations in a histogram.
/// Each observation is known only to lie within its bucket `[start, end]`, so
/// the true sum lies within `[Σ count·start, Σ count·end]`. The nominal sum
/// (`total_count · mean`, where `mean` uses bucket midpoints) sits inside this
/// band by construction. Returns `(lo, hi)`.
fn bucket_weighted_sum(r: &CumulativeROHistogramRef<'_>) -> (f64, f64) {
    let mut lo = 0.0f64;
    let mut hi = 0.0f64;
    for bucket in r.iter() {
        let c = bucket.count() as f64;
        lo += c * bucket.start() as f64;
        hi += c * bucket.end() as f64;
    }
    (lo, hi)
}

/// Per-bucket counts gathered from row deltas: dense by bucket index, with
/// the buckets added to since the last flush, so a flush costs the buckets
/// touched rather than every bucket of the histogram. Indices at or past the
/// histogram's bucket count belong to no bucket and are ignored.
struct Accum {
    counts: Vec<u64>,
    touched: Vec<u32>,
    buckets: usize,
}

impl Accum {
    fn new(config: Config) -> Self {
        Self {
            counts: Vec::new(),
            touched: Vec::new(),
            buckets: config.total_buckets(),
        }
    }

    fn add(&mut self, idx: u32, c: u64) {
        let i = idx as usize;
        if i >= self.buckets {
            return;
        }
        if i >= self.counts.len() {
            self.counts.resize(i + 1, 0);
        }
        if self.counts[i] == 0 {
            self.touched.push(idx);
        }
        self.counts[i] = self.counts[i].saturating_add(c);
    }
}

/// Compute `(curr - prev)` per-bucket individual counts and accumulate
/// into `accum`. Counter resets (curr < prev for a bucket) are clamped
/// to zero via `saturating_sub`, mirroring counter irate/rate semantics.
/// Both snapshots are sparse cumulative with ascending, unique indices,
/// walked together.
fn accumulate_delta_into(prev: &HistogramSnapshot, curr: &HistogramSnapshot, accum: &mut Accum) {
    debug_assert!(prev.index.windows(2).all(|w| w[0] < w[1]));
    debug_assert!(curr.index.windows(2).all(|w| w[0] < w[1]));
    let mut i = 0;
    let mut prev_running = 0u64;
    let mut curr_running = 0u64;
    for (j, &idx) in curr.index.iter().enumerate() {
        let individual = curr.count[j].saturating_sub(curr_running);
        curr_running = curr.count[j];
        while i < prev.index.len() && prev.index[i] < idx {
            prev_running = prev.count[i];
            i += 1;
        }
        let before = if i < prev.index.len() && prev.index[i] == idx {
            let v = prev.count[i].saturating_sub(prev_running);
            prev_running = prev.count[i];
            i += 1;
            v
        } else {
            0
        };
        let delta = individual.saturating_sub(before);
        if delta > 0 {
            accum.add(idx, delta);
        }
    }
}

// ─── Internal implementations ────────────────────────────────────────────────

fn quantiles_impl(
    stream: HistogramStream,
    quantiles_in: &[f64],
    start_ns: u64,
    end_ns: u64,
    stride: Option<Stride>,
    metric_name: &str,
) -> Vec<MatrixSample> {
    let HistogramStream { meta, rows } = stream;
    let config = meta.config;
    let n = meta.series.len();
    if n == 0 {
        return vec![];
    }

    let quantile_keys: Vec<(usize, f64, Quantile)> = quantiles_in
        .iter()
        .enumerate()
        .filter_map(|(i, q)| Quantile::new(*q).ok().map(|qk| (i, *q, qk)))
        .collect();
    if quantile_keys.is_empty() {
        return vec![];
    }
    let quantile_floats: Vec<f64> = quantile_keys.iter().map(|(_, q, _)| *q).collect();

    // Per point: (ts_sec, value, Some([bucket.start, bucket.end])) — the value
    // quantization band from the bucket the quantile lands in.
    let mut outputs: Vec<Vec<QuantileRow>> = vec![Vec::new(); quantiles_in.len()];
    let mut prev_per_series: Vec<Option<HistogramSnapshot>> = vec![None; n];
    let mut tick_accum = Accum::new(config);
    let mut stride_accum = Accum::new(config);
    let mut scratch_idx: Vec<u32> = Vec::new();
    let mut scratch_cnt: Vec<u64> = Vec::new();
    let mut windows = stride.map(|s| Windows::new(s, start_ns));
    let mut current_t: Option<u64> = None;

    for HistogramRow {
        series_idx,
        timestamp: t,
        snapshot,
    } in rows
    {
        if t > end_ns {
            continue;
        }

        if let Some(w) = windows.as_mut() {
            let (inside, closed) = w.row(t);
            if let Some(emit) = closed {
                let any = flush_accum(&mut stride_accum, &mut scratch_idx, &mut scratch_cnt);
                if let (true, Some(at)) = (any, emit) {
                    apply_quantiles(
                        config,
                        &scratch_idx,
                        &scratch_cnt,
                        &quantile_floats,
                        &quantile_keys,
                        at,
                        &mut outputs,
                    );
                }
            }
            if inside {
                if let Some(prev) = &prev_per_series[series_idx] {
                    accumulate_delta_into(prev, &snapshot, &mut stride_accum);
                }
            }
            prev_per_series[series_idx] = Some(snapshot);
            continue;
        }

        // Emit previous tick on tick boundary.
        {
            if let Some(prev_t) = current_t {
                if prev_t != t
                    && prev_t >= start_ns
                    && flush_accum(&mut tick_accum, &mut scratch_idx, &mut scratch_cnt)
                {
                    apply_quantiles(
                        config,
                        &scratch_idx,
                        &scratch_cnt,
                        &quantile_floats,
                        &quantile_keys,
                        prev_t,
                        &mut outputs,
                    );
                }
            }
        }
        current_t = Some(t);

        if t >= start_ns {
            if let Some(prev) = &prev_per_series[series_idx] {
                accumulate_delta_into(prev, &snapshot, &mut tick_accum);
            }
        }
        prev_per_series[series_idx] = Some(snapshot);
    }

    // Final emit.
    if let Some(w) = &windows {
        if let Some(at) = w.finish() {
            if flush_accum(&mut stride_accum, &mut scratch_idx, &mut scratch_cnt) {
                apply_quantiles(
                    config,
                    &scratch_idx,
                    &scratch_cnt,
                    &quantile_floats,
                    &quantile_keys,
                    at,
                    &mut outputs,
                );
            }
        }
    } else if let Some(t) = current_t.filter(|&t| t >= start_ns) {
        if flush_accum(&mut tick_accum, &mut scratch_idx, &mut scratch_cnt) {
            apply_quantiles(
                config,
                &scratch_idx,
                &scratch_cnt,
                &quantile_floats,
                &quantile_keys,
                t,
                &mut outputs,
            );
        }
    }

    let mut samples = Vec::with_capacity(outputs.len());
    for (i, q) in quantiles_in.iter().enumerate() {
        let rows = std::mem::take(&mut outputs[i]);
        if rows.is_empty() {
            continue;
        }
        let values: Vec<(f64, f64)> = rows.iter().map(|(t, v, _)| (*t, *v)).collect();
        // Every quantile point carries its bucket band, so emit intervals.
        let intervals: Option<Vec<(f64, f64)>> = if rows.iter().all(|(_, _, b)| b.is_some()) {
            Some(rows.iter().map(|(_, _, b)| b.unwrap()).collect())
        } else {
            None
        };
        let mut metric: HashMap<String, String> = HashMap::new();
        metric.insert("__name__".to_string(), metric_name.to_string());
        metric.insert("quantile".to_string(), q.to_string());
        // Histogram quantiles carry a bucket band at every point or none, so
        // `bands` mirrors `intervals` here; there is no interpolation across a
        // hole for this producer.
        let bands = intervals
            .as_ref()
            .map(|iv| iv.iter().copied().map(Some).collect());
        samples.push(
            MatrixSample::new(metric, values)
                .with_intervals(intervals)
                .with_bands(bands),
        );
    }
    samples
}

#[allow(clippy::too_many_arguments)]
fn reduce(
    stream: HistogramStream,
    group_by: GroupBy<'_>,
    start_ns: u64,
    end_ns: u64,
    stride: Option<Stride>,
    metric_name: &str,
    // Returns (nominal value, optional value-uncertainty band from bucket
    // resolution). sum/mean carry a band; count is exact (None).
    reducer: impl Fn(&CumulativeROHistogramRef<'_>) -> Option<(f64, Option<(f64, f64)>)>,
) -> Vec<MatrixSample> {
    let HistogramStream { meta, rows } = stream;
    let config = meta.config;
    let n = meta.series.len();
    if n == 0 {
        return vec![];
    }

    let mut group_keys: Vec<Labels> = Vec::new();
    let series_group: Vec<usize> = meta
        .series
        .iter()
        .map(|labels| {
            let key = derive_group_labels(labels, group_by);
            assign_group_index(&mut group_keys, key)
        })
        .collect();
    let g_count = group_keys.len();

    let mut values_per_group: Vec<Vec<(f64, f64)>> = (0..g_count).map(|_| Vec::new()).collect();
    let mut bands_per_group: Vec<Vec<Option<(f64, f64)>>> =
        (0..g_count).map(|_| Vec::new()).collect();
    let mut prev_per_series: Vec<Option<HistogramSnapshot>> = vec![None; n];
    let mut accum_per_group: Vec<Accum> = (0..g_count).map(|_| Accum::new(config)).collect();
    let mut scratch_idx: Vec<u32> = Vec::new();
    let mut scratch_cnt: Vec<u64> = Vec::new();
    let mut windows = stride.map(|s| Windows::new(s, start_ns));
    let mut current_t: Option<u64> = None;

    for HistogramRow {
        series_idx,
        timestamp: t,
        snapshot,
    } in rows
    {
        if t > end_ns {
            continue;
        }

        if let Some(w) = windows.as_mut() {
            let (inside, closed) = w.row(t);
            if let Some(emit) = closed {
                for g in 0..g_count {
                    let any =
                        flush_accum(&mut accum_per_group[g], &mut scratch_idx, &mut scratch_cnt);
                    if let (true, Some(at)) = (any, emit) {
                        let r = CumulativeROHistogramRef::from_parts_unchecked(
                            config,
                            &scratch_idx,
                            &scratch_cnt,
                        );
                        if let Some((v, band)) = reducer(&r) {
                            values_per_group[g].push((at as f64 / 1e9, v));
                            bands_per_group[g].push(band);
                        }
                    }
                }
            }
            if inside {
                if let Some(prev) = &prev_per_series[series_idx] {
                    let g = series_group[series_idx];
                    accumulate_delta_into(prev, &snapshot, &mut accum_per_group[g]);
                }
            }
            prev_per_series[series_idx] = Some(snapshot);
            continue;
        }

        // On tick boundary: emit the previous tick then clear accumulators.
        {
            if let Some(prev_t) = current_t {
                if prev_t != t && prev_t >= start_ns {
                    for g in 0..g_count {
                        if flush_accum(&mut accum_per_group[g], &mut scratch_idx, &mut scratch_cnt)
                        {
                            let r = CumulativeROHistogramRef::from_parts_unchecked(
                                config,
                                &scratch_idx,
                                &scratch_cnt,
                            );
                            if let Some((v, band)) = reducer(&r) {
                                values_per_group[g].push((prev_t as f64 / 1e9, v));
                                bands_per_group[g].push(band);
                            }
                        }
                    }
                }
            }
        }
        current_t = Some(t);

        // Accumulate delta for this series into its group's accumulator.
        if t >= start_ns {
            if let Some(prev) = &prev_per_series[series_idx] {
                let g = series_group[series_idx];
                accumulate_delta_into(prev, &snapshot, &mut accum_per_group[g]);
            }
        }
        prev_per_series[series_idx] = Some(snapshot);
    }

    // Final emit for the last window or tick.
    let last = match &windows {
        Some(w) => w.finish(),
        None => current_t.filter(|&t| t >= start_ns),
    };
    if let Some(t) = last {
        {
            for g in 0..g_count {
                if flush_accum(&mut accum_per_group[g], &mut scratch_idx, &mut scratch_cnt) {
                    let r = CumulativeROHistogramRef::from_parts_unchecked(
                        config,
                        &scratch_idx,
                        &scratch_cnt,
                    );
                    if let Some((v, band)) = reducer(&r) {
                        values_per_group[g].push((t as f64 / 1e9, v));
                        bands_per_group[g].push(band);
                    }
                }
            }
        }
    }

    build_grouped_output(
        metric_name,
        group_keys,
        values_per_group,
        Some(bands_per_group),
    )
}

fn irate_impl(
    stream: HistogramStream,
    group_by: GroupBy<'_>,
    start_ns: u64,
    end_ns: u64,
    metric_name: &str,
) -> Vec<MatrixSample> {
    let HistogramStream { meta, rows } = stream;
    let n = meta.series.len();
    if n == 0 {
        return vec![];
    }

    let mut group_keys: Vec<Labels> = Vec::new();
    let series_group: Vec<usize> = meta
        .series
        .iter()
        .map(|labels| assign_group_index(&mut group_keys, derive_group_labels(labels, group_by)))
        .collect();
    let g_count = group_keys.len();

    let mut values_per_group: Vec<Vec<(f64, f64)>> = (0..g_count).map(|_| Vec::new()).collect();
    let mut prev_per_series: Vec<Option<HistogramSnapshot>> = vec![None; n];
    // prev_t_per_group: tracks the last timestamp at which a group had data,
    // mirroring the original per-group-timestamp semantics: the first tick with
    // data does NOT emit (no previous group timestamp to compute dt against).
    let mut prev_t_per_group: Vec<Option<u64>> = vec![None; g_count];
    let mut tick_count_per_group: Vec<u64> = vec![0; g_count];
    let mut tick_has_data: Vec<bool> = vec![false; g_count];
    let mut current_t: Option<u64> = None;

    for HistogramRow {
        series_idx,
        timestamp: t,
        snapshot,
    } in rows
    {
        if t > end_ns {
            continue;
        }

        // On tick boundary: emit the previous tick's data.
        if let Some(prev_t) = current_t {
            if prev_t != t {
                for g in 0..g_count {
                    if tick_has_data[g] {
                        if prev_t >= start_ns {
                            if let Some(prev_group_t) = prev_t_per_group[g] {
                                let dt_ns = prev_t.saturating_sub(prev_group_t);
                                if dt_ns > 0 {
                                    let rate = (tick_count_per_group[g] as f64).max(0.0)
                                        / (dt_ns as f64 / 1e9);
                                    values_per_group[g].push((prev_t as f64 / 1e9, rate));
                                }
                            }
                        }
                        prev_t_per_group[g] = Some(prev_t);
                    }
                    tick_count_per_group[g] = 0;
                    tick_has_data[g] = false;
                }
            }
        }
        current_t = Some(t);

        let g = series_group[series_idx];
        if let Some(prev) = &prev_per_series[series_idx] {
            let delta = total_count(&snapshot).saturating_sub(total_count(prev));
            tick_count_per_group[g] = tick_count_per_group[g].saturating_add(delta);
            tick_has_data[g] = true;
        }
        prev_per_series[series_idx] = Some(snapshot);
    }

    // Final tick.
    if let Some(t) = current_t.filter(|&t| t >= start_ns) {
        for g in 0..g_count {
            if tick_has_data[g] {
                if let Some(prev_group_t) = prev_t_per_group[g] {
                    let dt_ns = t.saturating_sub(prev_group_t);
                    if dt_ns > 0 {
                        let rate = (tick_count_per_group[g] as f64).max(0.0) / (dt_ns as f64 / 1e9);
                        values_per_group[g].push((t as f64 / 1e9, rate));
                    }
                }
            }
        }
    }

    build_grouped_output(metric_name, group_keys, values_per_group, None)
}

fn heatmap_impl(
    stream: HistogramStream,
    start_ns: u64,
    end_ns: u64,
    stride: Option<Stride>,
) -> Option<HistogramHeatmapResult> {
    let HistogramStream { meta, rows } = stream;
    let n = meta.series.len();
    if n == 0 {
        return None;
    }
    let config = meta.config;

    let all_bounds: Vec<u64> =
        ::histogram::Histogram::new(config.grouping_power(), config.max_value_power())
            .ok()?
            .iter()
            .map(|b| b.end())
            .collect();

    let mut prev_per_series: Vec<Option<HistogramSnapshot>> = vec![None; n];
    let mut tick_accum = Accum::new(config);
    let mut stride_accum = Accum::new(config);
    let mut windows = stride.map(|s| Windows::new(s, start_ns));
    let mut scratch_idx: Vec<u32> = Vec::new();
    let mut scratch_cnt: Vec<u64> = Vec::new();

    let mut timestamps: Vec<f64> = Vec::new();
    let mut data: Vec<(usize, usize, f64)> = Vec::new();
    let mut min_value = f64::MAX;
    let mut max_value = f64::MIN;
    let mut min_bucket_idx = usize::MAX;
    let mut max_bucket_idx = 0usize;

    let emit_tick = |t_ns: u64,
                     idx: &[u32],
                     cnt: &[u64],
                     timestamps: &mut Vec<f64>,
                     data: &mut Vec<(usize, usize, f64)>,
                     min_value: &mut f64,
                     max_value: &mut f64,
                     min_bucket_idx: &mut usize,
                     max_bucket_idx: &mut usize| {
        let time_idx = timestamps.len();
        timestamps.push(t_ns as f64 / 1e9);
        let mut prev = 0u64;
        for k in 0..idx.len() {
            let cumu = cnt[k];
            let individual = cumu - prev;
            prev = cumu;
            if individual == 0 {
                continue;
            }
            let bucket_idx = idx[k] as usize;
            let count_f64 = individual as f64;
            data.push((time_idx, bucket_idx, count_f64));
            *min_value = min_value.min(count_f64);
            *max_value = max_value.max(count_f64);
            *min_bucket_idx = (*min_bucket_idx).min(bucket_idx);
            *max_bucket_idx = (*max_bucket_idx).max(bucket_idx);
        }
    };

    let mut current_t: Option<u64> = None;

    for HistogramRow {
        series_idx,
        timestamp: t,
        snapshot,
    } in rows
    {
        if t > end_ns {
            continue;
        }

        if let Some(w) = windows.as_mut() {
            let (inside, closed) = w.row(t);
            if let Some(emit) = closed {
                let any = flush_accum(&mut stride_accum, &mut scratch_idx, &mut scratch_cnt);
                if let (true, Some(at)) = (any, emit) {
                    emit_tick(
                        at,
                        &scratch_idx,
                        &scratch_cnt,
                        &mut timestamps,
                        &mut data,
                        &mut min_value,
                        &mut max_value,
                        &mut min_bucket_idx,
                        &mut max_bucket_idx,
                    );
                }
            }
            if inside {
                if let Some(prev) = &prev_per_series[series_idx] {
                    accumulate_delta_into(prev, &snapshot, &mut stride_accum);
                }
            }
            prev_per_series[series_idx] = Some(snapshot);
            continue;
        }

        // On tick boundary: flush previous tick.
        {
            if let Some(prev_t) = current_t {
                if prev_t != t
                    && prev_t >= start_ns
                    && flush_accum(&mut tick_accum, &mut scratch_idx, &mut scratch_cnt)
                {
                    emit_tick(
                        prev_t,
                        &scratch_idx,
                        &scratch_cnt,
                        &mut timestamps,
                        &mut data,
                        &mut min_value,
                        &mut max_value,
                        &mut min_bucket_idx,
                        &mut max_bucket_idx,
                    );
                }
            }
        }
        current_t = Some(t);

        if t >= start_ns {
            if let Some(prev) = &prev_per_series[series_idx] {
                accumulate_delta_into(prev, &snapshot, &mut tick_accum);
            }
        }
        prev_per_series[series_idx] = Some(snapshot);
    }

    // Final emit.
    if let Some(w) = &windows {
        if let Some(at) = w.finish() {
            if flush_accum(&mut stride_accum, &mut scratch_idx, &mut scratch_cnt) {
                emit_tick(
                    at,
                    &scratch_idx,
                    &scratch_cnt,
                    &mut timestamps,
                    &mut data,
                    &mut min_value,
                    &mut max_value,
                    &mut min_bucket_idx,
                    &mut max_bucket_idx,
                );
            }
        }
    } else if let Some(t) = current_t.filter(|&t| t >= start_ns) {
        if flush_accum(&mut tick_accum, &mut scratch_idx, &mut scratch_cnt) {
            emit_tick(
                t,
                &scratch_idx,
                &scratch_cnt,
                &mut timestamps,
                &mut data,
                &mut min_value,
                &mut max_value,
                &mut min_bucket_idx,
                &mut max_bucket_idx,
            );
        }
    }

    if timestamps.is_empty() {
        return None;
    }

    if min_value == f64::MAX {
        min_value = 0.0;
    }
    if max_value == f64::MIN {
        max_value = 0.0;
    }

    let (bucket_bounds, data) = if !data.is_empty() && max_bucket_idx < all_bounds.len() {
        let bounds = all_bounds[min_bucket_idx..=max_bucket_idx].to_vec();
        let remapped = data
            .into_iter()
            .map(|(t, b, c)| (t, b - min_bucket_idx, c))
            .collect();
        (bounds, remapped)
    } else {
        (all_bounds, data)
    };

    Some(HistogramHeatmapResult {
        timestamps,
        bucket_bounds,
        data,
        min_value,
        max_value,
    })
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

/// Flush `accum` into scratch buffers as a running cumulative. Clears `accum`.
/// Returns `false` if the result is empty.
///
/// The cumulative is `u64`: one tick or stride summed over several series
/// can exceed `u32::MAX` observations, and a `u32` cumulative would have to
/// clip it, which moves every quantile above the clip point down.
fn flush_accum(accum: &mut Accum, scratch_idx: &mut Vec<u32>, scratch_cnt: &mut Vec<u64>) -> bool {
    scratch_idx.clear();
    scratch_cnt.clear();
    accum.touched.sort_unstable();
    let mut running: u64 = 0;
    for &idx in &accum.touched {
        let c = std::mem::take(&mut accum.counts[idx as usize]);
        if c == 0 {
            continue;
        }
        running = running.saturating_add(c);
        scratch_idx.push(idx);
        scratch_cnt.push(running);
    }
    accum.touched.clear();
    !scratch_idx.is_empty()
}

fn apply_quantiles(
    config: Config,
    idx: &[u32],
    cnt: &[u64],
    quantile_floats: &[f64],
    keys: &[(usize, f64, Quantile)],
    t_ns: u64,
    out: &mut [Vec<QuantileRow>],
) {
    let r = CumulativeROHistogramRef::from_parts_unchecked(config, idx, cnt);
    let q_result: Result<Option<QuantilesResult>, _> = r.quantiles(quantile_floats);
    if let Ok(Some(qr)) = q_result {
        let t_sec = t_ns as f64 / 1e9;
        for (out_idx, _q_f, q_key) in keys {
            if let Some(bucket) = qr.get(q_key) {
                // Nominal is bucket.end(); the value quantization band is the
                // bucket's full [start, end] range (nominal sits at the top edge).
                let band = (bucket.start() as f64, bucket.end() as f64);
                out[*out_idx].push((t_sec, band.1, Some(band)));
            }
        }
    }
}

fn assign_group_index(group_keys: &mut Vec<Labels>, key: Labels) -> usize {
    match group_keys.iter().position(|k| k == &key) {
        Some(i) => i,
        None => {
            group_keys.push(key);
            group_keys.len() - 1
        }
    }
}

fn build_grouped_output(
    metric_name: &str,
    group_keys: Vec<Labels>,
    values_per_group: Vec<Vec<(f64, f64)>>,
    // Per-group value-uncertainty bands, parallel to values_per_group. When
    // present and every point in a group has a band, the group emits
    // `intervals`; a group with any None point (or no bands at all) emits
    // `intervals: None`.
    bands_per_group: Option<GroupBands>,
) -> Vec<MatrixSample> {
    let mut samples = Vec::new();
    let mut bands_iter = bands_per_group.map(|b| b.into_iter());
    for (key, values) in group_keys.into_iter().zip(values_per_group) {
        let group_bands = bands_iter.as_mut().and_then(|it| it.next());
        if values.is_empty() {
            continue;
        }
        let intervals: Option<Vec<(f64, f64)>> = group_bands.and_then(|bands| {
            if !bands.is_empty() && bands.iter().all(|b| b.is_some()) {
                Some(bands.into_iter().map(|b| b.unwrap()).collect())
            } else {
                None
            }
        });
        let mut metric: HashMap<String, String> = HashMap::new();
        metric.insert("__name__".to_string(), metric_name.to_string());
        for (k, v) in key.inner {
            metric.insert(k, v);
        }
        // Histogram quantiles carry a bucket band at every point or none, so
        // `bands` mirrors `intervals` here; there is no interpolation across a
        // hole for this producer.
        let bands = intervals
            .as_ref()
            .map(|iv| iv.iter().copied().map(Some).collect());
        samples.push(
            MatrixSample::new(metric, values)
                .with_intervals(intervals)
                .with_bands(bands),
        );
    }
    samples
}

#[cfg(test)]
mod accum_tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A sparse cumulative snapshot from per-bucket counts.
    fn snapshot(counts: &BTreeMap<u32, u64>) -> HistogramSnapshot {
        let mut running = 0u64;
        let mut index = Vec::new();
        let mut count = Vec::new();
        for (&idx, &c) in counts {
            running += c;
            index.push(idx);
            count.push(running);
        }
        HistogramSnapshot { index, count }
    }

    /// Deltas walked from several snapshot pairs into one accumulator flush
    /// as the running sum of the per-bucket subtraction, in ascending bucket
    /// order. Buckets present in only one snapshot, growing, shrinking (a
    /// reset) and unchanged are all covered by the pseudo-random cases, and
    /// several pairs per flush put touched buckets out of order.
    #[test]
    fn the_delta_walk_matches_per_bucket_subtraction() {
        let config = histogram::Config::new(4, 10).unwrap();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut accum = Accum::new(config);
        for _ in 0..300 {
            let mut want: BTreeMap<u32, u64> = BTreeMap::new();
            for _ in 0..(next() % 4 + 1) {
                let mut a = BTreeMap::new();
                let mut b = BTreeMap::new();
                for _ in 0..(next() % 12) {
                    a.insert((next() % 40) as u32, next() % 9 + 1);
                }
                for _ in 0..(next() % 12) {
                    b.insert((next() % 40) as u32, next() % 9 + 1);
                }
                let (prev, curr) = (snapshot(&a), snapshot(&b));
                accumulate_delta_into(&prev, &curr, &mut accum);
                for (&k, &c) in &b {
                    let d = c.saturating_sub(a.get(&k).copied().unwrap_or(0));
                    if d > 0 {
                        *want.entry(k).or_insert(0) += d;
                    }
                }
            }
            let mut idx = Vec::new();
            let mut cnt = Vec::new();
            flush_accum(&mut accum, &mut idx, &mut cnt);
            let want_idx: Vec<u32> = want.keys().copied().collect();
            let want_cnt: Vec<u64> = want
                .values()
                .scan(0u64, |run, c| {
                    *run += c;
                    Some(*run)
                })
                .collect();
            assert_eq!((idx, cnt), (want_idx, want_cnt));
            assert!(accum.touched.is_empty() && accum.counts.iter().all(|c| *c == 0));
        }
    }

    /// An index past the histogram's buckets is ignored, not allocated for.
    #[test]
    fn an_index_past_the_buckets_is_ignored() {
        let config = histogram::Config::new(4, 10).unwrap();
        let mut accum = Accum::new(config);
        accum.add(u32::MAX, 5);
        accum.add(3, 2);
        assert!(accum.counts.len() <= config.total_buckets());
        let (mut idx, mut cnt) = (Vec::new(), Vec::new());
        assert!(flush_accum(&mut accum, &mut idx, &mut cnt));
        assert_eq!((idx, cnt), (vec![3], vec![2]));
    }
}
