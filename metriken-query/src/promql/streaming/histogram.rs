//! Streaming `histogram_quantiles` pipeline.
//!
//! Used by both `histogram_quantile(q, m)` (standard PromQL, single
//! quantile) and `histogram_quantiles([qs], m)` (rezolus extension,
//! multiple quantiles in one walk) — they're the same operation
//! with N=1 vs N>1 and share this code path.
//!
//! The eager path materialises an entire summed [`HistogramSeries`]
//! before walking it for quantile extraction:
//!
//! 1. `tsdb.histograms()` clones every matching label-keyed series.
//! 2. `collection.sum()` merges all of them into one new
//!    [`HistogramSeries`] containing every timestamp.
//! 3. `summed.percentiles(...)` walks that merged series and emits
//!    one output `UntypedSeries` per quantile.
//!
//! Steps 1–2 dominate peak: the merged series is `O(timestamps ×
//! avg_buckets)` resident, plus the per-query collection clone
//! (`O(series × timestamps × avg_buckets)`).
//!
//! The streaming version inverts the loop: walk per-tick, sum the
//! per-series deltas into a small reusable scratch buffer, compute
//! the N quantiles for that tick, append to the N output Vecs, and
//! repeat. Peak resident is ~one merged delta histogram (tens of KB
//! at typical bucket density) plus the N partial output Vecs —
//! independent of input series cardinality.
//!
//! Output ties to the existing `MatrixSample` JSON shape so the
//! boundary serializer is unchanged. The `Iterator` trait isn't
//! useful here (the per-tick summed Ref borrows from scratch that
//! mutates between ticks — the classic lending-iterator problem),
//! so the function is a straight loop rather than a generic chain.

use std::collections::{BTreeMap, HashMap};

use ::histogram::{Config, CumulativeROHistogram32Ref, Quantile, QuantilesResult};

use crate::promql::MatrixSample;
use crate::tsdb::{HistogramCollection, Labels};

/// Compute quantiles of `metric{filter}` over the time range
/// `[start_ns, end_ns]`. When `stride_ns` is `Some`, deltas are
/// accumulated into stride-sized windows before quantile extraction,
/// matching the eager engine's `iter_strided` semantics.
///
/// Returns one [`MatrixSample`] per requested quantile, with metric
/// labels `{__name__: metric_name, quantile: q}` — the standard
/// PromQL convention used by `histogram_quantile`.
pub fn quantiles(
    collection: &HistogramCollection,
    label_filter: &Labels,
    quantiles_in: &[f64],
    start_ns: u64,
    end_ns: u64,
    stride_ns: Option<u64>,
    metric_name: &str,
) -> Vec<MatrixSample> {
    // One peekable iterator per matching series. Holding all S
    // simultaneously costs S × (one Ref + one cursor) ≈ tens of bytes
    // per series; the actual histogram data stays in the TSDB.
    let mut iters: Vec<_> = collection
        .iter()
        .filter(|(labels, _)| label_filter.inner.is_empty() || labels.matches(label_filter))
        .map(|(_, series)| series.iter().peekable())
        .collect();

    if iters.is_empty() {
        return vec![];
    }

    // Every series shares the same `Config` (loader/ingest enforce
    // it). Pick it from whichever series has at least one delta.
    let config: Option<Config> = iters
        .iter_mut()
        .filter_map(|it| it.peek().map(|(_, r)| r.config()))
        .next();
    let Some(config) = config else {
        return vec![];
    };

    // Pre-resolve quantile keys once — `Quantile::new` validates
    // each value is in `[0, 1]`. Anything that fails to construct is
    // dropped, matching the eager path's `if let Ok(...)` shape.
    // We keep the original f64 for the `quantiles()` call (which
    // takes a `&[f64]`) and the `Quantile` for the result lookup.
    let quantile_keys: Vec<(usize, f64, Quantile)> = quantiles_in
        .iter()
        .enumerate()
        .filter_map(|(i, q)| Quantile::new(*q).ok().map(|qk| (i, *q, qk)))
        .collect();
    if quantile_keys.is_empty() {
        return vec![];
    }
    let quantile_floats: Vec<f64> = quantile_keys.iter().map(|(_, q, _)| *q).collect();

    // Output accumulators — one Vec per requested quantile.
    let mut outputs: Vec<Vec<(f64, f64)>> = vec![Vec::new(); quantiles_in.len()];

    // Reusable scratch: a sorted bucket index → cumulative count
    // pair, rebuilt per emission. We use parallel `Vec<u32>`s so
    // `CumulativeROHistogram32Ref::from_parts_unchecked` can borrow
    // them directly without allocating.
    let mut scratch_idx: Vec<u32> = Vec::new();
    let mut scratch_cnt: Vec<u32> = Vec::new();

    // Stride accumulator (only populated when `stride_ns` is Some).
    // Using a BTreeMap mirrors the eager `StrideIter` exactly so
    // bucket ordering and saturation behaviour match.
    let mut stride_accum: BTreeMap<u32, u64> = BTreeMap::new();
    let mut stride_last_emit: Option<u64> = None;
    let mut stride_end_time: u64 = 0;

    loop {
        // Find min ts across all series whose next sample is in
        // `[start_ns, end_ns]`. Past-end is treated as exhausted to
        // bound the loop without depending on series length.
        let mut min_ts: Option<u64> = None;
        for it in iters.iter_mut() {
            while let Some(&(ts, _)) = it.peek() {
                if ts > end_ns {
                    // Drop the rest of this series — we won't visit
                    // any of its remaining samples.
                    it.next();
                    continue;
                }
                min_ts = Some(min_ts.map_or(ts, |m| m.min(ts)));
                break;
            }
        }
        let Some(t) = min_ts else { break };

        if t < start_ns {
            // Skip pre-start samples cleanly.
            for it in iters.iter_mut() {
                if matches!(it.peek(), Some(&(ts, _)) if ts == t) {
                    it.next();
                }
            }
            continue;
        }

        // Pull every series that has a sample at `t`. The Refs
        // borrow into the source HistogramSeries' flat buffers — no
        // allocation. Held in a small Vec for the merge below.
        let mut at_t: Vec<CumulativeROHistogram32Ref<'_>> = Vec::new();
        for it in iters.iter_mut() {
            if matches!(it.peek(), Some(&(ts, _)) if ts == t) {
                let (_, r) = it.next().expect("peek matched");
                at_t.push(r);
            }
        }

        if let Some(stride) = stride_ns {
            // Stride mode: fold every tick into the running BTreeMap.
            for r in &at_t {
                accumulate_into(&mut stride_accum, r);
            }
            stride_end_time = t;

            // Anchor: the very first observed tick starts the
            // window but doesn't emit. Subsequent ticks emit whenever
            // `stride` ns have elapsed since the last emit.
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
        } else {
            // No-stride mode: emit one quantile sample per input tick.
            match at_t.len() {
                1 => {
                    apply_quantiles_ref(&at_t[0], &quantile_floats, &quantile_keys, t, &mut outputs)
                }
                _ => {
                    // Multi-series fold into scratch. Cost is
                    // O(sum of bucket counts across S series) per
                    // tick; uses fresh Vecs each time but they're
                    // small enough to be invisible vs the eager path.
                    merge_into(&at_t, &mut scratch_idx, &mut scratch_cnt);
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
        }
    }

    // Drain the final stride window if non-empty.
    if stride_ns.is_some()
        && !stride_accum.is_empty()
        && flush_accum(&mut stride_accum, &mut scratch_idx, &mut scratch_cnt)
    {
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

    // Build the matrix samples in the input quantile order.
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

/// Decompose `r`'s running cumulative back into per-bucket
/// individual counts and add them into `accum`. Mirrors the
/// per-bucket walk in the eager `StrideIter::next`.
fn accumulate_into(accum: &mut BTreeMap<u32, u64>, r: &CumulativeROHistogram32Ref<'_>) {
    let idx = r.index();
    let cnt = r.count();
    let mut prev = 0u32;
    for k in 0..idx.len() {
        let cumu = cnt[k];
        let individual = cumu - prev;
        prev = cumu;
        if individual > 0 {
            *accum.entry(idx[k]).or_insert(0) += individual as u64;
        }
    }
}

/// Flush `accum` into the parallel `(idx, cnt)` scratch buffers,
/// rebuilding the running cumulative. Returns false if the result
/// is empty (no quantile to emit).
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

/// Two-pointer merge of every Ref in `refs` into the parallel
/// `(idx, cnt)` scratch buffers. The result is the bucket-wise
/// individual-count sum re-encoded as a running cumulative.
///
/// O(sum of bucket counts) per call.
fn merge_into(
    refs: &[CumulativeROHistogram32Ref<'_>],
    scratch_idx: &mut Vec<u32>,
    scratch_cnt: &mut Vec<u32>,
) {
    let mut accum: BTreeMap<u32, u64> = BTreeMap::new();
    for r in refs {
        accumulate_into(&mut accum, r);
    }
    scratch_idx.clear();
    scratch_cnt.clear();
    let mut running: u64 = 0;
    for (idx, c) in accum {
        if c == 0 {
            continue;
        }
        running = running.saturating_add(c);
        let clipped = running.min(u32::MAX as u64) as u32;
        scratch_idx.push(idx);
        scratch_cnt.push(clipped);
    }
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
    apply_quantiles_ref(&r, quantile_floats, keys, t_ns, out);
}

fn apply_quantiles_ref(
    r: &CumulativeROHistogram32Ref<'_>,
    quantile_floats: &[f64],
    keys: &[(usize, f64, Quantile)],
    t_ns: u64,
    out: &mut [Vec<(f64, f64)>],
) {
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

/// Streaming `histogram_heatmap(metric{filter}[, stride])`.
///
/// Same per-tick walk as [`quantiles`] (sum across input series's
/// per-timestamp deltas into reusable scratch), but for each tick
/// emits one `(time_idx, bucket_idx, count)` triple per non-zero
/// bucket directly into the output `HistogramHeatmapResult` instead
/// of running the quantile reducer.
///
/// Time-range filtering happens during the walk (skip ticks outside
/// `[start_ns, end_ns]`); bucket-range trimming happens in a single
/// second pass over the collected triples.
///
/// The output is intrinsically 2-D so a `Vec` of triples gets
/// materialised either way; the streaming-side win is avoiding the
/// `tsdb.histograms()` clone and the `collection.sum()` merged
/// `HistogramSeries` allocation that the eager path used to build.
pub fn heatmap(
    collection: &HistogramCollection,
    label_filter: &Labels,
    start_ns: u64,
    end_ns: u64,
    stride_ns: Option<u64>,
) -> Option<crate::promql::HistogramHeatmapResult> {
    let mut iters: Vec<_> = collection
        .iter()
        .filter(|(labels, _)| label_filter.inner.is_empty() || labels.matches(label_filter))
        .map(|(_, series)| series.iter().peekable())
        .collect();

    if iters.is_empty() {
        return None;
    }

    let config: Option<Config> = iters
        .iter_mut()
        .filter_map(|it| it.peek().map(|(_, r)| r.config()))
        .next();
    let config = config?;

    // Bucket bounds come from the series' shared Config — we use an
    // empty Histogram with the same configuration so the Y-axis
    // covers every bucket (not just the non-zero ones we observe).
    let all_bounds: Vec<u64> =
        ::histogram::Histogram::new(config.grouping_power(), config.max_value_power())
            .ok()?
            .iter()
            .map(|b| b.end())
            .collect();

    // Per-tick scratch + stride accumulator (mirrors `quantiles`).
    let mut scratch_idx: Vec<u32> = Vec::new();
    let mut scratch_cnt: Vec<u32> = Vec::new();
    let mut stride_accum: BTreeMap<u32, u64> = BTreeMap::new();
    let mut stride_last_emit: Option<u64> = None;
    let mut stride_end_time: u64 = 0;

    // Output accumulators.
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
        // Decompose cumulative back to per-bucket individual counts.
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

        if t < start_ns {
            for it in iters.iter_mut() {
                if matches!(it.peek(), Some(&(ts, _)) if ts == t) {
                    it.next();
                }
            }
            continue;
        }

        let mut at_t: Vec<CumulativeROHistogram32Ref<'_>> = Vec::new();
        for it in iters.iter_mut() {
            if matches!(it.peek(), Some(&(ts, _)) if ts == t) {
                let (_, r) = it.next().expect("peek matched");
                at_t.push(r);
            }
        }

        if let Some(stride) = stride_ns {
            for r in &at_t {
                accumulate_into(&mut stride_accum, r);
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
        } else {
            match at_t.len() {
                1 => emit_tick(
                    t,
                    at_t[0].index(),
                    at_t[0].count(),
                    &mut timestamps,
                    &mut data,
                    &mut min_value,
                    &mut max_value,
                    &mut min_bucket_idx,
                    &mut max_bucket_idx,
                ),
                _ => {
                    merge_into(&at_t, &mut scratch_idx, &mut scratch_cnt);
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
        }
    }

    if stride_ns.is_some()
        && !stride_accum.is_empty()
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

    // Trim bucket_bounds to the active range and remap bucket indices.
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

    Some(crate::promql::HistogramHeatmapResult {
        timestamps,
        bucket_bounds,
        data,
        min_value,
        max_value,
    })
}
