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

use std::collections::{HashMap, VecDeque};

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
    /// A window is emitted only when its stamp (its close, or its last row
    /// for the last window) is at or after this: a range's start, so no
    /// point falls before the range; 0 for an instant, whose one window
    /// always is.
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

    /// Where window `k` closes: its last instant.
    fn closes_ns(&self, k: i64) -> u64 {
        self.opens_ns(k.saturating_add(1))
    }
}

/// The fixed-point scale of a strided window's counts: a window's share of
/// a row's delta is in units of `1 / SHARE_SCALE` observations, so a window
/// can hold part of one. Quantiles and means do not depend on the scale;
/// counts, sums and heatmap cells are divided by it. A window's cumulative
/// count saturates at 2^48 observations.
const SHARE_SCALE: u64 = 1 << 16;

/// The most windows a strided call holds open. Past it the oldest is
/// emitted, and a later share for it counts in the oldest still open.
const MAX_OPEN_WINDOWS: usize = 256;

/// `floor(a · b / c)` for `b <= c`, without overflow while `a < 2^80`.
fn mul_div_floor(a: u128, b: u128, c: u128) -> u128 {
    (a / c) * b + (a % c) * b / c
}

/// A strided call's open windows, and each row's delta shared among the
/// windows its interval overlaps, in proportion to the overlap. A row at `t`
/// whose series' previous row is at `p` adds `d` observations to a bucket
/// over `(p, t]`; a window overlapping `(p, t]` by `o` gets `d · o / (t − p)`
/// of them, to `1 / SHARE_SCALE` of an observation. The shares sum to the
/// delta. Windows before the one holding the range's start get nothing.
///
/// A window closes once every series still reporting has a row after its
/// close, so a series read at another phase or cadence adds its share
/// before the window is emitted. A series is still reporting while it has
/// two rows and its latest is less than two of its own row intervals old.
/// A share for a window already closed, from a series that missed rows,
/// counts in the oldest window still open. A closed window is stamped at
/// its close, the last window at its last row; it is emitted when that
/// stamp is at or after the stride's `emit_from_ns`. At most
/// [`MAX_OPEN_WINDOWS`] are open at once.
struct Splitter {
    stride: Stride,
    /// The window holding the range's start: the first that can be emitted.
    first: i64,
    /// Windows up to this one are closed.
    closed_to: i64,
    /// The latest window a row has fallen in.
    newest: i64,
    /// Open windows in order, each with one accumulator per group.
    open: VecDeque<(i64, Vec<Accum>)>,
    spare: Vec<Vec<Accum>>,
    groups: usize,
    config: Config,
    /// Per series: its latest row's time and the interval before it (0
    /// until it has two rows).
    latest: Vec<Option<(u64, u64)>>,
    last_t: u64,
    deltas: Vec<(u32, u64)>,
    /// `settled` at the latest `t` it was taken for. Rows at one `t` only
    /// raise it, so within a `t` the first value holds as a lower bound.
    settled_at: Option<(u64, u64)>,
    /// The most windows open at once.
    #[cfg(test)]
    most_open: usize,
}

impl Splitter {
    fn new(stride: Stride, start_ns: u64, series: usize, groups: usize, config: Config) -> Self {
        let first = stride.window(start_ns);
        Self {
            stride,
            first,
            closed_to: first - 1,
            newest: first - 1,
            open: VecDeque::new(),
            spare: Vec::new(),
            groups,
            config,
            latest: vec![None; series],
            last_t: 0,
            deltas: Vec::new(),
            settled_at: None,
            #[cfg(test)]
            most_open: 0,
        }
    }

    /// The accumulators of window `k`, opened if need be; a closed window's
    /// are the oldest open window's.
    fn accums(&mut self, k: i64) -> &mut [Accum] {
        let k = k.max(self.closed_to + 1);
        let pos = self.open.partition_point(|(j, _)| *j < k);
        if self.open.get(pos).map(|(j, _)| *j) != Some(k) {
            let (groups, config) = (self.groups, self.config);
            let accums = self
                .spare
                .pop()
                .unwrap_or_else(|| (0..groups).map(|_| Accum::new(config)).collect());
            self.open.insert(pos, (k, accums));
            #[cfg(test)]
            {
                self.most_open = self.most_open.max(self.open.len());
            }
        }
        &mut self.open[pos].1
    }

    /// The time up to which no series still reporting at `t` can add: the
    /// oldest latest row among them.
    fn settled(&mut self, t: u64) -> u64 {
        if let Some((at, settled)) = self.settled_at {
            if at == t {
                return settled;
            }
        }
        let settled = self
            .latest
            .iter()
            .flatten()
            .filter(|(at, dt)| *dt > 0 && t.saturating_sub(*at) < dt.saturating_mul(2))
            .map(|(at, _)| *at)
            .fold(t, u64::min);
        self.settled_at = Some((t, settled));
        settled
    }

    /// Closes the open windows whose close is at or before `to`, oldest
    /// first. `emit` gets each one's stamp, its close or `cap` if that is
    /// earlier, when the window is emitted, and its accumulators, which it
    /// must drain.
    fn close_through(
        &mut self,
        to: u64,
        cap: u64,
        emit: &mut impl FnMut(Option<u64>, &mut [Accum]),
    ) {
        while self
            .open
            .front()
            .is_some_and(|(j, _)| self.stride.closes_ns(*j) <= to)
        {
            let (j, mut accums) = self.open.pop_front().unwrap();
            let at = self.stride.closes_ns(j).min(cap);
            emit((at >= self.stride.emit_from_ns).then_some(at), &mut accums);
            self.closed_to = j;
            self.spare.push(accums);
        }
    }

    /// Emits the oldest open windows while more than [`MAX_OPEN_WINDOWS`]
    /// are open.
    fn close_excess(&mut self, cap: u64, emit: &mut impl FnMut(Option<u64>, &mut [Accum])) {
        while self.open.len() > MAX_OPEN_WINDOWS {
            let oldest = self.stride.closes_ns(self.open[0].0);
            self.close_through(oldest, cap, emit);
        }
    }

    /// A row of series `series`, in `group`, at `t`, with the series'
    /// previous snapshot, if any.
    fn row(
        &mut self,
        series: usize,
        group: usize,
        prev: Option<&HistogramSnapshot>,
        t: u64,
        curr: &HistogramSnapshot,
        emit: &mut impl FnMut(Option<u64>, &mut [Accum]),
    ) {
        let before = self.latest[series].map(|(at, _)| at);
        self.latest[series] = Some((t, before.map_or(0, |at| t.saturating_sub(at))));
        let kt = self.stride.window(t);
        if kt < self.first {
            return;
        }
        self.last_t = t;
        if let (Some(p), Some(prev)) = (before, prev) {
            self.share(group, p, prev, t, curr, emit);
        }
        if kt > self.newest {
            self.newest = kt;
            // A window closing at `t` waits for a row after `t`: other
            // series may still have rows at `t`.
            let to = self.settled(t).min(t.saturating_sub(1));
            self.close_through(to, t, emit);
        }
    }

    /// Adds the delta from `prev` at `p` to `curr` at `t` to the windows
    /// `(p, t]` overlaps. Windows it fills before the last close as they are
    /// filled when no series still reporting can add to them, so a long
    /// interval holds one window open rather than every one it spans.
    fn share(
        &mut self,
        group: usize,
        p: u64,
        prev: &HistogramSnapshot,
        t: u64,
        curr: &HistogramSnapshot,
        emit: &mut impl FnMut(Option<u64>, &mut [Accum]),
    ) {
        let kt = self.stride.window(t);
        let kp = self.stride.window(p);
        if kp >= kt || p >= t {
            let accum = &mut self.accums(kt)[group];
            delta_walk(prev, curr, |idx, d| {
                accum.add(idx, d.saturating_mul(SHARE_SCALE))
            });
            self.close_excess(t, emit);
            return;
        }
        let mut deltas = std::mem::take(&mut self.deltas);
        deltas.clear();
        delta_walk(prev, curr, |idx, d| deltas.push((idx, d)));
        let dt = u128::from(t - p);
        let from = kp.max(self.first);
        let settled = if kt - from > 1 { self.settled(t) } else { 0 };
        for k in from..=kt {
            let upto = u128::from(self.stride.closes_ns(k).min(t) - p);
            let before = u128::from(self.stride.opens_ns(k).max(p) - p);
            let accum = &mut self.accums(k)[group];
            for &(idx, d) in &deltas {
                let d = u128::from(d) * u128::from(SHARE_SCALE);
                let share = mul_div_floor(d, upto, dt) - mul_div_floor(d, before, dt);
                if share > 0 {
                    accum.add(idx, u64::try_from(share).unwrap_or(u64::MAX));
                }
            }
            if k < kt {
                self.close_through(settled.min(self.stride.closes_ns(k)), t, emit);
            }
            self.close_excess(t, emit);
        }
        self.deltas = deltas;
    }

    /// Closes every window.
    fn finish(&mut self, emit: &mut impl FnMut(Option<u64>, &mut [Accum])) {
        let last_t = self.last_t;
        self.close_through(u64::MAX, last_t, emit);
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
        reduce(
            self,
            group_by,
            start_ns,
            end_ns,
            stride,
            metric_name,
            |r, _| {
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
            },
        )
    }

    fn count(
        self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        reduce(
            self,
            group_by,
            start_ns,
            end_ns,
            stride,
            metric_name,
            |r, scale| {
                // count has no bucket-resolution uncertainty, so no band.
                let c = r.total_count();
                (c > 0).then_some((c as f64 / scale, None))
            },
        )
    }

    fn sum(
        self,
        group_by: GroupBy<'_>,
        start_ns: u64,
        end_ns: u64,
        stride: Option<Stride>,
        metric_name: &str,
    ) -> Vec<MatrixSample> {
        reduce(
            self,
            group_by,
            start_ns,
            end_ns,
            stride,
            metric_name,
            |r, scale| {
                let c = r.total_count();
                if c == 0 {
                    return None;
                }
                // Nominal sum = total_count · mean (midpoints); the band is the
                // bucket-resolution range [Σ count·start, Σ count·end].
                let (lo, hi) = bucket_weighted_sum(r);
                r.mean()
                    .map(|m| (c as f64 * m / scale, Some((lo / scale, hi / scale))))
            },
        )
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
    delta_walk(prev, curr, |idx, d| accum.add(idx, d));
}

/// Calls `f` with each bucket's `(curr - prev)` individual count, where it
/// is positive, in ascending bucket order (see [`accumulate_delta_into`]).
fn delta_walk(prev: &HistogramSnapshot, curr: &HistogramSnapshot, mut f: impl FnMut(u32, u64)) {
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
            f(idx, delta);
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
    let mut scratch_idx: Vec<u32> = Vec::new();
    let mut scratch_cnt: Vec<u64> = Vec::new();
    let mut splitter = stride.map(|s| Splitter::new(s, start_ns, n, 1, config));
    let (mut window_idx, mut window_cnt) = (Vec::new(), Vec::new());
    let mut emit_window =
        |at: Option<u64>, accums: &mut [Accum], outputs: &mut Vec<Vec<QuantileRow>>| {
            if flush_accum(&mut accums[0], &mut window_idx, &mut window_cnt) {
                if let Some(at) = at {
                    apply_quantiles(
                        config,
                        &window_idx,
                        &window_cnt,
                        &quantile_floats,
                        &quantile_keys,
                        at,
                        outputs,
                    );
                }
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

        if let Some(sp) = splitter.as_mut() {
            let prev = prev_per_series[series_idx].as_ref();
            sp.row(series_idx, 0, prev, t, &snapshot, &mut |at, accums| {
                emit_window(at, accums, &mut outputs)
            });
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
    if let Some(sp) = splitter.as_mut() {
        sp.finish(&mut |at, accums| emit_window(at, accums, &mut outputs));
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
    // The counts are in units of `1 / scale` observations (the second
    // argument): `SHARE_SCALE` in a strided window, 1 in a tick.
    reducer: impl Fn(&CumulativeROHistogramRef<'_>, f64) -> Option<(f64, Option<(f64, f64)>)>,
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
    let mut splitter = stride.map(|s| Splitter::new(s, start_ns, n, g_count, config));
    let (mut window_idx, mut window_cnt) = (Vec::new(), Vec::new());
    let mut emit_window =
        |at: Option<u64>,
         accums: &mut [Accum],
         values_per_group: &mut Vec<Vec<(f64, f64)>>,
         bands_per_group: &mut Vec<Vec<Option<(f64, f64)>>>| {
            for (g, accum) in accums.iter_mut().enumerate() {
                if !flush_accum(accum, &mut window_idx, &mut window_cnt) {
                    continue;
                }
                let Some(at) = at else { continue };
                let r = CumulativeROHistogramRef::from_parts_unchecked(
                    config,
                    &window_idx,
                    &window_cnt,
                );
                if let Some((v, band)) = reducer(&r, SHARE_SCALE as f64) {
                    values_per_group[g].push((at as f64 / 1e9, v));
                    bands_per_group[g].push(band);
                }
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

        if let Some(sp) = splitter.as_mut() {
            let prev = prev_per_series[series_idx].as_ref();
            sp.row(
                series_idx,
                series_group[series_idx],
                prev,
                t,
                &snapshot,
                &mut |at, accums| {
                    emit_window(at, accums, &mut values_per_group, &mut bands_per_group)
                },
            );
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
                            if let Some((v, band)) = reducer(&r, 1.0) {
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
    if let Some(sp) = splitter.as_mut() {
        sp.finish(&mut |at, accums| {
            emit_window(at, accums, &mut values_per_group, &mut bands_per_group)
        });
    } else if let Some(t) = current_t.filter(|&t| t >= start_ns) {
        {
            for g in 0..g_count {
                if flush_accum(&mut accum_per_group[g], &mut scratch_idx, &mut scratch_cnt) {
                    let r = CumulativeROHistogramRef::from_parts_unchecked(
                        config,
                        &scratch_idx,
                        &scratch_cnt,
                    );
                    if let Some((v, band)) = reducer(&r, 1.0) {
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
    let mut splitter = stride.map(|s| Splitter::new(s, start_ns, n, 1, config));
    let mut scratch_idx: Vec<u32> = Vec::new();
    let mut scratch_cnt: Vec<u64> = Vec::new();

    let mut timestamps: Vec<f64> = Vec::new();
    let mut data: Vec<(usize, usize, f64)> = Vec::new();
    let mut min_value = f64::MAX;
    let mut max_value = f64::MIN;
    let mut min_bucket_idx = usize::MAX;
    let mut max_bucket_idx = 0usize;

    let emit_tick = |t_ns: u64,
                     scale: f64,
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
            let count_f64 = individual as f64 / scale;
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

        if let Some(sp) = splitter.as_mut() {
            let prev = prev_per_series[series_idx].as_ref();
            sp.row(series_idx, 0, prev, t, &snapshot, &mut |at, accums| {
                if flush_accum(&mut accums[0], &mut scratch_idx, &mut scratch_cnt) {
                    if let Some(at) = at {
                        emit_tick(
                            at,
                            SHARE_SCALE as f64,
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
            });
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
                        1.0,
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
    if let Some(sp) = splitter.as_mut() {
        sp.finish(&mut |at, accums| {
            if flush_accum(&mut accums[0], &mut scratch_idx, &mut scratch_cnt) {
                if let Some(at) = at {
                    emit_tick(
                        at,
                        SHARE_SCALE as f64,
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
        });
    } else if let Some(t) = current_t.filter(|&t| t >= start_ns) {
        if flush_accum(&mut tick_accum, &mut scratch_idx, &mut scratch_cnt) {
            emit_tick(
                t,
                1.0,
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

    /// A row after a long gap fills the windows across it one at a time:
    /// a day's gap at a 1 s stride keeps at most two windows open, and the
    /// shares still sum to the delta.
    #[test]
    fn a_long_gap_holds_few_windows_open() {
        let config = histogram::Config::new(4, 10).unwrap();
        let stride = Stride {
            ns: 1_000_000_000,
            anchor_ns: 0,
            emit_from_ns: 0,
        };
        let mut sp = Splitter::new(stride, 1_000_000_000_000, 1, 1, config);
        let (mut idx, mut cnt) = (Vec::new(), Vec::new());
        let mut total = 0u64;
        let mut emit = |_: Option<u64>, accums: &mut [Accum]| {
            if flush_accum(&mut accums[0], &mut idx, &mut cnt) {
                total += cnt.last().copied().unwrap_or(0);
            }
        };
        let s = 1_000_000_000u64;
        let snaps = [
            snapshot(&BTreeMap::from([(3, 1)])),
            snapshot(&BTreeMap::from([(3, 2)])),
            snapshot(&BTreeMap::from([(3, 86_402)])),
        ];
        let times = [1000 * s, 1001 * s, (1001 + 86_400) * s];
        for i in 0..3usize {
            let prev = i.checked_sub(1).map(|p| &snaps[p]);
            sp.row(0, 0, prev, times[i], &snaps[i], &mut emit);
        }
        sp.finish(&mut emit);
        assert!(sp.most_open <= 2, "{} windows open", sp.most_open);
        assert_eq!(total, 86_401 * SHARE_SCALE);
    }

    /// A slow series holding windows open behind a fast one keeps at most
    /// `MAX_OPEN_WINDOWS` open, and the shares still sum to the deltas.
    #[test]
    fn open_windows_are_capped() {
        let config = histogram::Config::new(4, 10).unwrap();
        let s = 1_000_000_000u64;
        let stride = Stride {
            ns: s,
            anchor_ns: 0,
            emit_from_ns: 0,
        };
        let mut sp = Splitter::new(stride, 1000 * s, 2, 1, config);
        let (mut idx, mut cnt) = (Vec::new(), Vec::new());
        let mut total = 0u64;
        let mut emit = |_: Option<u64>, accums: &mut [Accum]| {
            if flush_accum(&mut accums[0], &mut idx, &mut cnt) {
                total += cnt.last().copied().unwrap_or(0);
            }
        };
        let snap = |n: u64| snapshot(&BTreeMap::from([(3, n)]));
        // The slow series reads at 0 s and 1000 s: an interval of 1000 s,
        // so it still reports for 2000 s and holds windows from 1000 s.
        sp.row(1, 0, None, 0, &snap(1), &mut emit);
        sp.row(1, 0, Some(&snap(1)), 1000 * s, &snap(2), &mut emit);
        let mut prev = snap(0);
        sp.row(0, 0, None, 1000 * s, &prev, &mut emit);
        for i in 1..=1000u64 {
            let curr = snap(i);
            sp.row(0, 0, Some(&prev), (1000 + i) * s, &curr, &mut emit);
            prev = curr;
        }
        sp.finish(&mut emit);
        assert!(
            sp.most_open <= MAX_OPEN_WINDOWS + 1,
            "{} open",
            sp.most_open
        );
        // The slow series' delta at 1000 s opens window 1000 only partly in
        // range: (999, 1000] is its last second, 1/1000 of one observation.
        let slow = SHARE_SCALE - SHARE_SCALE * 999 / 1000;
        assert_eq!(total, 1000 * SHARE_SCALE + slow);
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
