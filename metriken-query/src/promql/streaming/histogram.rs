use std::collections::{BTreeMap, HashMap};

use ::histogram::{Config, CumulativeROHistogram32Ref, Quantile, QuantilesResult};

use crate::promql::streaming::{derive_group_labels, GroupBy};
use crate::promql::{HistogramHeatmapResult, MatrixSample};
use crate::types::{Histogram as HistogramSeries, HistogramSnapshot, Histograms};

// ─── Iterator adapter ────────────────────────────────────────────────────────

/// Yields `(timestamp, &HistogramSnapshot)` pairs from a `Histogram`'s
/// flat storage — one raw cumulative snapshot per timestamp.
struct HistogramIter<'a> {
    timestamps: &'a [u64],
    snapshots: &'a [HistogramSnapshot],
    pos: usize,
}

impl<'a> HistogramIter<'a> {
    fn new(h: &'a HistogramSeries) -> Self {
        Self { timestamps: &h.timestamps, snapshots: &h.snapshots, pos: 0 }
    }
}

impl<'a> Iterator for HistogramIter<'a> {
    type Item = (u64, &'a HistogramSnapshot);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.timestamps.len() {
            return None;
        }
        let ts = self.timestamps[self.pos];
        let snap = &self.snapshots[self.pos];
        self.pos += 1;
        Some((ts, snap))
    }
}

// ─── impl Histograms ─────────────────────────────────────────────────────────

impl Histograms {
    /// Compute quantiles over `[start_ns, end_ns]`. Returns one `MatrixSample`
    /// per requested quantile (labeled `quantile: <q>`).
    pub fn quantiles(
        &self,
        quantiles_in: &[f64],
        start_ns: u64,
        end_ns: u64,
        stride_ns: Option<u64>,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        quantiles_impl(&self.series, quantiles_in, start_ns, end_ns, stride_ns, metric_name)
    }

    pub fn mean(
        &self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        stride_ns: Option<u64>,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        reduce(&self.series, group_by, start_ns, end_ns, stride_ns, metric_name, |r| r.mean())
    }

    pub fn count(
        &self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        stride_ns: Option<u64>,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        reduce(
            &self.series,
            group_by,
            start_ns,
            end_ns,
            stride_ns,
            metric_name,
            |r| {
                let c = r.total_count();
                (c > 0).then_some(c as f64)
            },
        )
    }

    pub fn sum(
        &self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        stride_ns: Option<u64>,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        reduce(
            &self.series,
            group_by,
            start_ns,
            end_ns,
            stride_ns,
            metric_name,
            |r| {
                let c = r.total_count();
                if c == 0 {
                    return None;
                }
                r.mean().map(|m| c as f64 * m)
            },
        )
    }

    pub fn irate(
        &self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        irate_impl(&self.series, group_by, start_ns, end_ns, metric_name)
    }

    pub fn heatmap(
        &self,
        start_ns: u64,
        end_ns: u64,
        stride_ns: Option<u64>,
    ) -> Option<HistogramHeatmapResult> {
        heatmap_impl(&self.series, start_ns, end_ns, stride_ns)
    }
}

// ─── Delta helpers ────────────────────────────────────────────────────────────

fn total_count(snap: &HistogramSnapshot) -> u64 {
    snap.count.last().copied().unwrap_or(0)
}

/// Decompose a sparse cumulative snapshot into a per-bucket individual count map.
fn decompose_to_individual(snap: &HistogramSnapshot) -> BTreeMap<u32, u64> {
    let mut out = BTreeMap::new();
    let mut prev = 0u64;
    for k in 0..snap.index.len() {
        let c = snap.count[k];
        let individual = c.saturating_sub(prev);
        if individual > 0 {
            out.insert(snap.index[k], individual);
        }
        prev = c;
    }
    out
}

/// Compute `(curr - prev)` per-bucket individual counts and accumulate
/// into `accum`. Counter resets (curr < prev for a bucket) are clamped
/// to zero via `saturating_sub`, mirroring counter irate/rate semantics.
fn accumulate_delta_into(prev: &HistogramSnapshot, curr: &HistogramSnapshot, accum: &mut BTreeMap<u32, u64>) {
    let prev_map = decompose_to_individual(prev);
    let curr_map = decompose_to_individual(curr);
    for (&idx, &c) in &curr_map {
        let p = prev_map.get(&idx).copied().unwrap_or(0);
        let delta = c.saturating_sub(p);
        if delta > 0 {
            *accum.entry(idx).or_insert(0) += delta;
        }
    }
}

// ─── Internal implementations ────────────────────────────────────────────────

fn quantiles_impl(
    series: &[HistogramSeries],
    quantiles_in: &[f64],
    start_ns: u64,
    end_ns: u64,
    stride_ns: Option<u64>,
    metric_name: &str,
) -> Vec<MatrixSample> {
    let n = series.len();
    let mut iters: Vec<_> = series.iter().map(|h| HistogramIter::new(h).peekable()).collect();
    if n == 0 {
        return vec![];
    }

    let config: Option<Config> = series.iter().find(|h| !h.snapshots.is_empty()).map(|h| h.config);
    let Some(config) = config else {
        return vec![];
    };

    let quantile_keys: Vec<(usize, f64, Quantile)> = quantiles_in
        .iter()
        .enumerate()
        .filter_map(|(i, q)| Quantile::new(*q).ok().map(|qk| (i, *q, qk)))
        .collect();
    if quantile_keys.is_empty() {
        return vec![];
    }
    let quantile_floats: Vec<f64> = quantile_keys.iter().map(|(_, q, _)| *q).collect();

    let mut outputs: Vec<Vec<(f64, f64)>> = vec![Vec::new(); quantiles_in.len()];
    let mut prev_per_iter: Vec<Option<HistogramSnapshot>> = vec![None; n];
    let mut tick_accum: BTreeMap<u32, u64> = BTreeMap::new();
    let mut stride_accum: BTreeMap<u32, u64> = BTreeMap::new();
    let mut stride_last_emit: Option<u64> = None;
    let mut stride_end_time: u64 = 0;
    let mut scratch_idx: Vec<u32> = Vec::new();
    let mut scratch_cnt: Vec<u32> = Vec::new();

    loop {
        let mut min_ts: Option<u64> = None;
        for it in iters.iter_mut() {
            while let Some(&(ts, _)) = it.peek() {
                if ts > end_ns {
                    it.next();
                    continue;
                }
                min_ts = Some(min_ts.map_or(ts, |m| m.min(ts)));
                break;
            }
        }
        let Some(t) = min_ts else { break };

        tick_accum.clear();

        for (i, it) in iters.iter_mut().enumerate() {
            if matches!(it.peek(), Some(&(ts, _)) if ts == t) {
                let (_, snap) = it.next().expect("peek matched");
                if t >= start_ns {
                    if let Some(prev) = &prev_per_iter[i] {
                        accumulate_delta_into(prev, snap, &mut tick_accum);
                    }
                }
                prev_per_iter[i] = Some(snap.clone());
            }
        }

        if t < start_ns {
            continue;
        }

        if let Some(stride) = stride_ns {
            for (&idx, &c) in &tick_accum {
                *stride_accum.entry(idx).or_insert(0) += c;
            }
            stride_end_time = t;
            let last = match stride_last_emit {
                Some(t) => t,
                None => {
                    stride_last_emit = Some(t);
                    continue;
                }
            };
            if t >= last && t - last >= stride {
                if flush_accum(&mut stride_accum, &mut scratch_idx, &mut scratch_cnt) {
                    apply_quantiles(
                        config,
                        &scratch_idx,
                        &scratch_cnt,
                        &quantile_floats,
                        &quantile_keys,
                        stride_end_time,
                        &mut outputs,
                    );
                }
                stride_last_emit = Some(t);
            }
        } else if flush_accum(&mut tick_accum, &mut scratch_idx, &mut scratch_cnt) {
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

    if stride_ns.is_some() && flush_accum(&mut stride_accum, &mut scratch_idx, &mut scratch_cnt) {
        apply_quantiles(
            config,
            &scratch_idx,
            &scratch_cnt,
            &quantile_floats,
            &quantile_keys,
            stride_end_time,
            &mut outputs,
        );
    }

    let mut samples = Vec::with_capacity(outputs.len());
    for (i, q) in quantiles_in.iter().enumerate() {
        let values = std::mem::take(&mut outputs[i]);
        if values.is_empty() {
            continue;
        }
        let mut metric: HashMap<String, String> = HashMap::new();
        metric.insert("__name__".to_string(), metric_name.to_string());
        metric.insert("quantile".to_string(), q.to_string());
        samples.push(MatrixSample { metric, values });
    }
    samples
}

#[allow(clippy::too_many_arguments)]
fn reduce(
    series: &[HistogramSeries],
    group_by: GroupBy<'_>,
    start_ns: u64,
    end_ns: u64,
    stride_ns: Option<u64>,
    metric_name: &str,
    reducer: impl Fn(&CumulativeROHistogram32Ref<'_>) -> Option<f64>,
) -> Vec<MatrixSample> {
    use crate::labels::Labels;
    let mut group_keys: Vec<Labels> = Vec::new();
    let mut series_group: Vec<usize> = Vec::new();
    let n = series.len();
    let mut iters: Vec<_> = series
        .iter()
        .map(|h| {
            let key = derive_group_labels(&h.labels, group_by);
            series_group.push(assign_group_index(&mut group_keys, key));
            HistogramIter::new(h).peekable()
        })
        .collect();
    let g_count = group_keys.len();
    if n == 0 {
        return vec![];
    }

    let config: Option<Config> = series.iter().find(|h| !h.snapshots.is_empty()).map(|h| h.config);
    let Some(config) = config else {
        return vec![];
    };

    let mut values_per_group: Vec<Vec<(f64, f64)>> = (0..g_count).map(|_| Vec::new()).collect();
    let mut prev_per_iter: Vec<Option<HistogramSnapshot>> = vec![None; n];
    let mut scratch_idx: Vec<u32> = Vec::new();
    let mut scratch_cnt: Vec<u32> = Vec::new();
    let mut accum_per_group: Vec<BTreeMap<u32, u64>> =
        (0..g_count).map(|_| BTreeMap::new()).collect();
    let mut stride_last_emit: Option<u64> = None;
    let mut stride_end_time: u64 = 0;

    loop {
        let mut min_ts: Option<u64> = None;
        for it in iters.iter_mut() {
            while let Some(&(ts, _)) = it.peek() {
                if ts > end_ns {
                    it.next();
                    continue;
                }
                min_ts = Some(min_ts.map_or(ts, |m| m.min(ts)));
                break;
            }
        }
        let Some(t) = min_ts else { break };

        if stride_ns.is_none() {
            for accum in accum_per_group.iter_mut() {
                accum.clear();
            }
        }

        for (i, it) in iters.iter_mut().enumerate() {
            if matches!(it.peek(), Some(&(ts, _)) if ts == t) {
                let (_, snap) = it.next().expect("peek matched");
                if t >= start_ns {
                    if let Some(prev) = &prev_per_iter[i] {
                        accumulate_delta_into(prev, snap, &mut accum_per_group[series_group[i]]);
                    }
                }
                prev_per_iter[i] = Some(snap.clone());
            }
        }

        if t < start_ns {
            continue;
        }

        if let Some(stride) = stride_ns {
            stride_end_time = t;
            let last = match stride_last_emit {
                Some(t) => t,
                None => {
                    stride_last_emit = Some(t);
                    continue;
                }
            };
            if t >= last && t - last >= stride {
                for g in 0..g_count {
                    if flush_accum(&mut accum_per_group[g], &mut scratch_idx, &mut scratch_cnt) {
                        let r = CumulativeROHistogram32Ref::from_parts_unchecked(
                            config,
                            &scratch_idx,
                            &scratch_cnt,
                        );
                        if let Some(v) = reducer(&r) {
                            values_per_group[g].push((stride_end_time as f64 / 1e9, v));
                        }
                    }
                }
                stride_last_emit = Some(t);
            }
        } else {
            for g in 0..g_count {
                if flush_accum(&mut accum_per_group[g], &mut scratch_idx, &mut scratch_cnt) {
                    let r = CumulativeROHistogram32Ref::from_parts_unchecked(
                        config,
                        &scratch_idx,
                        &scratch_cnt,
                    );
                    if let Some(v) = reducer(&r) {
                        values_per_group[g].push((t as f64 / 1e9, v));
                    }
                }
            }
        }
    }

    if stride_ns.is_some() {
        for g in 0..g_count {
            if flush_accum(&mut accum_per_group[g], &mut scratch_idx, &mut scratch_cnt) {
                let r = CumulativeROHistogram32Ref::from_parts_unchecked(
                    config,
                    &scratch_idx,
                    &scratch_cnt,
                );
                if let Some(v) = reducer(&r) {
                    values_per_group[g].push((stride_end_time as f64 / 1e9, v));
                }
            }
        }
    }

    build_grouped_output(metric_name, group_keys, values_per_group)
}

fn irate_impl(
    series: &[HistogramSeries],
    group_by: GroupBy<'_>,
    start_ns: u64,
    end_ns: u64,
    metric_name: &str,
) -> Vec<MatrixSample> {
    use crate::labels::Labels;
    let mut group_keys: Vec<Labels> = Vec::new();
    let mut series_group: Vec<usize> = Vec::new();
    let n = series.len();
    let mut iters: Vec<_> = series
        .iter()
        .map(|h| {
            let key = derive_group_labels(&h.labels, group_by);
            series_group.push(assign_group_index(&mut group_keys, key));
            HistogramIter::new(h).peekable()
        })
        .collect();
    let g_count = group_keys.len();
    if n == 0 {
        return vec![];
    }

    let mut values_per_group: Vec<Vec<(f64, f64)>> = (0..g_count).map(|_| Vec::new()).collect();
    let mut prev_per_iter: Vec<Option<HistogramSnapshot>> = vec![None; n];
    let mut prev_t_per_group: Vec<Option<u64>> = vec![None; g_count];
    let mut tick_count_per_group: Vec<u64> = vec![0; g_count];
    let mut tick_has_data: Vec<bool> = vec![false; g_count];

    loop {
        let mut min_ts: Option<u64> = None;
        for it in iters.iter_mut() {
            while let Some(&(ts, _)) = it.peek() {
                if ts > end_ns {
                    it.next();
                    continue;
                }
                min_ts = Some(min_ts.map_or(ts, |m| m.min(ts)));
                break;
            }
        }
        let Some(t) = min_ts else { break };

        for c in tick_count_per_group.iter_mut() {
            *c = 0;
        }
        for h in tick_has_data.iter_mut() {
            *h = false;
        }

        for (i, it) in iters.iter_mut().enumerate() {
            if matches!(it.peek(), Some(&(ts, _)) if ts == t) {
                let (_, snap) = it.next().expect("peek matched");
                let g = series_group[i];
                if let Some(prev) = &prev_per_iter[i] {
                    let delta = total_count(snap).saturating_sub(total_count(prev));
                    tick_count_per_group[g] = tick_count_per_group[g].saturating_add(delta);
                    tick_has_data[g] = true;
                }
                prev_per_iter[i] = Some(snap.clone());
            }
        }

        if t < start_ns {
            for (g, &has) in tick_has_data.iter().enumerate() {
                if has {
                    prev_t_per_group[g] = Some(t);
                }
            }
            continue;
        }

        for g in 0..g_count {
            if !tick_has_data[g] {
                continue;
            }
            if let Some(prev) = prev_t_per_group[g] {
                let dt_ns = t.saturating_sub(prev);
                if dt_ns > 0 {
                    let rate =
                        (tick_count_per_group[g] as f64).max(0.0) / (dt_ns as f64 / 1e9);
                    values_per_group[g].push((t as f64 / 1e9, rate));
                }
            }
            prev_t_per_group[g] = Some(t);
        }
    }

    build_grouped_output(metric_name, group_keys, values_per_group)
}

fn heatmap_impl(
    series: &[HistogramSeries],
    start_ns: u64,
    end_ns: u64,
    stride_ns: Option<u64>,
) -> Option<HistogramHeatmapResult> {
    let n = series.len();
    let mut iters: Vec<_> = series.iter().map(|h| HistogramIter::new(h).peekable()).collect();
    if n == 0 {
        return None;
    }

    let config: Option<Config> = series.iter().find(|h| !h.snapshots.is_empty()).map(|h| h.config);
    let config = config?;

    let all_bounds: Vec<u64> =
        ::histogram::Histogram::new(config.grouping_power(), config.max_value_power())
            .ok()?
            .iter()
            .map(|b| b.end())
            .collect();

    let mut prev_per_iter: Vec<Option<HistogramSnapshot>> = vec![None; n];
    let mut tick_accum: BTreeMap<u32, u64> = BTreeMap::new();
    let mut stride_accum: BTreeMap<u32, u64> = BTreeMap::new();
    let mut stride_last_emit: Option<u64> = None;
    let mut stride_end_time: u64 = 0;
    let mut scratch_idx: Vec<u32> = Vec::new();
    let mut scratch_cnt: Vec<u32> = Vec::new();

    let mut timestamps: Vec<f64> = Vec::new();
    let mut data: Vec<(usize, usize, f64)> = Vec::new();
    let mut min_value = f64::MAX;
    let mut max_value = f64::MIN;
    let mut min_bucket_idx = usize::MAX;
    let mut max_bucket_idx = 0usize;

    let emit_tick = |t_ns: u64,
                     idx: &[u32],
                     cnt: &[u32],
                     timestamps: &mut Vec<f64>,
                     data: &mut Vec<(usize, usize, f64)>,
                     min_value: &mut f64,
                     max_value: &mut f64,
                     min_bucket_idx: &mut usize,
                     max_bucket_idx: &mut usize| {
        let time_idx = timestamps.len();
        timestamps.push(t_ns as f64 / 1e9);
        let mut prev = 0u32;
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

    loop {
        let mut min_ts: Option<u64> = None;
        for it in iters.iter_mut() {
            while let Some(&(ts, _)) = it.peek() {
                if ts > end_ns {
                    it.next();
                    continue;
                }
                min_ts = Some(min_ts.map_or(ts, |m| m.min(ts)));
                break;
            }
        }
        let Some(t) = min_ts else { break };

        tick_accum.clear();

        for (i, it) in iters.iter_mut().enumerate() {
            if matches!(it.peek(), Some(&(ts, _)) if ts == t) {
                let (_, snap) = it.next().expect("peek matched");
                if t >= start_ns {
                    if let Some(prev) = &prev_per_iter[i] {
                        accumulate_delta_into(prev, snap, &mut tick_accum);
                    }
                }
                prev_per_iter[i] = Some(snap.clone());
            }
        }

        if t < start_ns {
            continue;
        }

        if let Some(stride) = stride_ns {
            for (&idx, &c) in &tick_accum {
                *stride_accum.entry(idx).or_insert(0) += c;
            }
            stride_end_time = t;
            let last = match stride_last_emit {
                Some(t) => t,
                None => {
                    stride_last_emit = Some(t);
                    continue;
                }
            };
            if t >= last
                && t - last >= stride
                && flush_accum(&mut stride_accum, &mut scratch_idx, &mut scratch_cnt)
            {
                emit_tick(
                    stride_end_time,
                    &scratch_idx,
                    &scratch_cnt,
                    &mut timestamps,
                    &mut data,
                    &mut min_value,
                    &mut max_value,
                    &mut min_bucket_idx,
                    &mut max_bucket_idx,
                );
                stride_last_emit = Some(t);
            }
        } else if flush_accum(&mut tick_accum, &mut scratch_idx, &mut scratch_cnt) {
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

    if stride_ns.is_some() && flush_accum(&mut stride_accum, &mut scratch_idx, &mut scratch_cnt) {
        emit_tick(
            stride_end_time,
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

fn flush_accum(
    accum: &mut BTreeMap<u32, u64>,
    scratch_idx: &mut Vec<u32>,
    scratch_cnt: &mut Vec<u32>,
) -> bool {
    scratch_idx.clear();
    scratch_cnt.clear();
    let mut running: u64 = 0;
    for (idx, c) in accum.iter() {
        if *c == 0 {
            continue;
        }
        running = running.saturating_add(*c);
        let clipped = running.min(u32::MAX as u64) as u32;
        scratch_idx.push(*idx);
        scratch_cnt.push(clipped);
    }
    accum.clear();
    !scratch_idx.is_empty()
}

fn apply_quantiles(
    config: Config,
    idx: &[u32],
    cnt: &[u32],
    quantile_floats: &[f64],
    keys: &[(usize, f64, Quantile)],
    t_ns: u64,
    out: &mut [Vec<(f64, f64)>],
) {
    let r = CumulativeROHistogram32Ref::from_parts_unchecked(config, idx, cnt);
    let q_result: Result<Option<QuantilesResult>, _> = r.quantiles(quantile_floats);
    if let Ok(Some(qr)) = q_result {
        let t_sec = t_ns as f64 / 1e9;
        for (out_idx, _q_f, q_key) in keys {
            if let Some(bucket) = qr.get(q_key) {
                out[*out_idx].push((t_sec, bucket.end() as f64));
            }
        }
    }
}

fn assign_group_index(group_keys: &mut Vec<crate::labels::Labels>, key: crate::labels::Labels) -> usize {
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
    group_keys: Vec<crate::labels::Labels>,
    values_per_group: Vec<Vec<(f64, f64)>>,
) -> Vec<MatrixSample> {
    let mut samples = Vec::new();
    for (key, values) in group_keys.into_iter().zip(values_per_group) {
        if values.is_empty() {
            continue;
        }
        let mut metric: HashMap<String, String> = HashMap::new();
        metric.insert("__name__".to_string(), metric_name.to_string());
        for (k, v) in key.inner {
            metric.insert(k, v);
        }
        samples.push(MatrixSample { metric, values });
    }
    samples
}
