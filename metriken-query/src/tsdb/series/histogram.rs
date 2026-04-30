use std::collections::{BTreeMap, BTreeSet};

use ::histogram::{CumulativeROHistogram, CumulativeROHistogram32, Histogram, Quantile};

use super::*;

/// Represents a series of histogram readings.
///
/// Each entry is a per-period delta histogram in [`CumulativeROHistogram32`]
/// form: cumulative counts (a running prefix sum) across the non-zero buckets
/// of one snapshot, but those counts represent the delta between two
/// consecutive raw snapshots rather than a running cumulative across the whole
/// process lifetime. Per-period deltas comfortably fit in `u32` for any
/// realistic sampling interval, which halves the stored count footprint.
/// Pre-differencing on load also removes the per-pair `delta()` work from the
/// hot query paths.
///
/// Entries are kept as a sorted `Vec<(timestamp_ns, delta)>` ordered by
/// timestamp.  Insertion is O(1) at the end (the load-time pattern) and falls
/// back to a binary-search insert otherwise.
///
/// Stride-window queries anchor at the first stored delta's timestamp.  The
/// very first raw snapshot is dropped (no predecessor, no delta) so for
/// regularly-sampled inputs every stride bin is shifted by one sampling
/// interval relative to the original cumulative-form behavior — equivalent to
/// rendering a null at the leading edge of the time axis.
#[derive(Default, Clone)]
pub struct HistogramSeries {
    inner: Vec<(u64, CumulativeROHistogram32)>,
}

/// Data for rendering a histogram as a latency heatmap
#[derive(Default, Clone)]
pub struct HistogramHeatmapData {
    /// Timestamps in seconds
    pub timestamps: Vec<f64>,
    /// Bucket boundaries (end values) for Y-axis labels
    pub bucket_bounds: Vec<u64>,
    /// Heatmap data as [time_index, bucket_index, count]
    pub data: Vec<(usize, usize, f64)>,
    /// Minimum count value (for color scaling)
    pub min_value: f64,
    /// Maximum count value (for color scaling)
    pub max_value: f64,
}

impl HistogramSeries {
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn insert(&mut self, timestamp: u64, value: CumulativeROHistogram32) {
        if let Some((last_ts, _)) = self.inner.last() {
            if timestamp > *last_ts {
                self.inner.push((timestamp, value));
                return;
            }
            if timestamp == *last_ts {
                self.inner.last_mut().unwrap().1 = value;
                return;
            }
        } else {
            self.inner.push((timestamp, value));
            return;
        }
        match self.inner.binary_search_by_key(&timestamp, |(t, _)| *t) {
            Ok(idx) => self.inner[idx].1 = value,
            Err(idx) => self.inner.insert(idx, (timestamp, value)),
        }
    }

    /// Returns the time bounds (min, max) in nanoseconds, or None if empty.
    pub fn time_bounds(&self) -> Option<(u64, u64)> {
        let min = self.inner.first()?.0;
        let max = self.inner.last()?.0;
        Some((min, max))
    }

    pub fn percentiles(
        &self,
        percentiles: &[f64],
        stride_ns: Option<u64>,
    ) -> Option<Vec<UntypedSeries>> {
        if self.is_empty() {
            return None;
        }

        let mut result = vec![UntypedSeries::default(); percentiles.len()];

        for (time, delta) in self.iter_strided(stride_ns) {
            if let Ok(Some(q_results)) = delta.quantiles(percentiles) {
                for (id, q) in percentiles.iter().enumerate() {
                    if let Ok(quantile) = Quantile::new(*q) {
                        if let Some(bucket) = q_results.get(&quantile) {
                            result[id].insert(time, bucket.end() as f64);
                        }
                    }
                }
            }
        }

        Some(result)
    }

    /// Returns bucket data suitable for rendering as a heatmap.
    /// Y-axis is bucket index (latency range), X-axis is time, color is count.
    pub fn heatmap(&self, stride_ns: Option<u64>) -> Option<HistogramHeatmapData> {
        if self.inner.is_empty() {
            return None;
        }

        let mut result = HistogramHeatmapData::default();
        let mut min_value = f64::MAX;
        let mut max_value = f64::MIN;

        // Bucket boundaries come from the histogram config, which is identical
        // across the series. Collect them once from an empty Histogram with the
        // same configuration so the Y-axis covers every bucket (not only the
        // non-zero ones we actually observe).
        let config = self.inner.first().unwrap().1.config();
        if let Ok(empty) = Histogram::new(config.grouping_power(), config.max_value_power()) {
            result.bucket_bounds = empty.iter().map(|b| b.end()).collect();
        } else {
            return None;
        }

        for (time, delta) in self.iter_strided(stride_ns) {
            let time_index = result.timestamps.len();

            // Store timestamp in seconds
            result.timestamps.push(time as f64 / 1_000_000_000.0);

            // Emit only the non-zero buckets of the delta — CumulativeRO only
            // stores those, so this is also a zero-copy walk.
            for (i, bucket) in delta.iter().enumerate() {
                let bucket_index = delta.index()[i] as usize;
                let count_f64 = bucket.count() as f64;
                result.data.push((time_index, bucket_index, count_f64));
                min_value = min_value.min(count_f64);
                max_value = max_value.max(count_f64);
            }
        }

        if result.timestamps.is_empty() {
            return None;
        }

        // Handle edge cases
        if min_value == f64::MAX {
            min_value = 0.0;
        }
        if max_value == f64::MIN {
            max_value = 0.0;
        }

        result.min_value = min_value;
        result.max_value = max_value;

        Some(result)
    }

    /// Iterate the stored per-period deltas, optionally bucketing them into
    /// stride windows.  Without a stride each entry is yielded individually;
    /// with a stride, deltas are summed across windows of `stride_ns`
    /// nanoseconds and yielded once per window (timestamped at the latest
    /// snapshot in the window).
    fn iter_strided<'a>(
        &'a self,
        stride_ns: Option<u64>,
    ) -> Box<dyn Iterator<Item = (u64, CumulativeROHistogram32)> + 'a> {
        match stride_ns {
            None => Box::new(self.inner.iter().map(|(t, h)| (*t, h.clone()))),
            Some(stride) => Box::new(StrideIter::new(self.inner.iter(), stride)),
        }
    }
}

/// Sums consecutive per-period deltas until at least `stride` nanoseconds have
/// elapsed since the last emitted bin, then yields the accumulated delta
/// histogram timestamped at the last snapshot in the window.
struct StrideIter<'a, I: Iterator<Item = &'a (u64, CumulativeROHistogram32)>> {
    iter: I,
    stride: u64,
    last_emit: Option<u64>,
    accum: BTreeMap<u32, u64>,
    accum_config: Option<::histogram::Config>,
    accum_end_time: u64,
}

impl<'a, I: Iterator<Item = &'a (u64, CumulativeROHistogram32)>> StrideIter<'a, I> {
    fn new(iter: I, stride: u64) -> Self {
        Self {
            iter,
            stride,
            last_emit: None,
            accum: BTreeMap::new(),
            accum_config: None,
            accum_end_time: 0,
        }
    }

    fn flush(&mut self) -> Option<(u64, CumulativeROHistogram32)> {
        let config = self.accum_config.take()?;
        let mut index = Vec::with_capacity(self.accum.len());
        let mut count = Vec::with_capacity(self.accum.len());
        let mut running: u64 = 0;
        for (idx, c) in std::mem::take(&mut self.accum) {
            if c == 0 {
                continue;
            }
            running = running.saturating_add(c);
            // Saturate at u32::MAX on overflow rather than wrapping — produces
            // a clipped but monotonic cumulative that `from_parts` will accept.
            let clipped = running.min(u32::MAX as u64) as u32;
            index.push(idx);
            count.push(clipped);
        }
        let end_time = self.accum_end_time;
        self.last_emit = Some(end_time);
        if index.is_empty() {
            return None;
        }
        CumulativeROHistogram32::from_parts(config, index, count)
            .ok()
            .map(|h| (end_time, h))
    }
}

impl<'a, I: Iterator<Item = &'a (u64, CumulativeROHistogram32)>> Iterator for StrideIter<'a, I> {
    type Item = (u64, CumulativeROHistogram32);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (time, hist) = match self.iter.next() {
                Some(pair) => (pair.0, &pair.1),
                None => {
                    // Drain any final partial accumulator.
                    if !self.accum.is_empty() {
                        if let Some(emit) = self.flush() {
                            return Some(emit);
                        }
                    }
                    return None;
                }
            };

            // Initialize / validate config.
            match &self.accum_config {
                None => self.accum_config = Some(hist.config()),
                Some(cfg) => {
                    if *cfg != hist.config() {
                        // Config change — drop accumulator and start fresh
                        // from this snapshot.
                        self.accum.clear();
                        self.accum_config = Some(hist.config());
                        self.last_emit = None;
                    }
                }
            }

            // Add this delta's individual bucket counts into the accumulator.
            let mut prev = 0u32;
            for (i, &idx) in hist.index().iter().enumerate() {
                let cumu = hist.count()[i];
                let individual = cumu - prev;
                prev = cumu;
                if individual > 0 {
                    *self.accum.entry(idx).or_insert(0) += individual as u64;
                }
            }
            self.accum_end_time = time;

            // Anchor at the first observed delta — the first raw cumulative
            // snapshot has no predecessor and so produced no stored delta,
            // which means the first stride bin is implicitly dropped.  For
            // regularly-sampled series each remaining bin is shifted by one
            // sampling interval relative to a hypothetical anchor at the
            // first raw snapshot, which is invisible at typical render
            // resolutions.
            let last = match self.last_emit {
                Some(t) => t,
                None => {
                    self.last_emit = Some(time);
                    continue;
                }
            };
            if time >= last && time - last >= self.stride {
                if let Some(emit) = self.flush() {
                    return Some(emit);
                }
            }
        }
    }
}

impl Add<&HistogramSeries> for HistogramSeries {
    type Output = HistogramSeries;
    fn add(self, other: &HistogramSeries) -> Self::Output {
        let a = &self.inner;
        let b = &other.inner;
        let mut out = Vec::with_capacity(a.len() + b.len());
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            match a[i].0.cmp(&b[j].0) {
                std::cmp::Ordering::Less => {
                    out.push(a[i].clone());
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    out.push(b[j].clone());
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    if let Some(sum) = wrapping_add(&a[i].1, &b[j].1) {
                        out.push((a[i].0, sum));
                    } else {
                        // Mismatched configs — skip rather than panicking;
                        // keep this side's value so the bin isn't dropped.
                        out.push(a[i].clone());
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
        out.extend_from_slice(&a[i..]);
        out.extend_from_slice(&b[j..]);

        HistogramSeries { inner: out }
    }
}

/// Decompose a `CumulativeROHistogram32` into `bucket_index -> individual_count`.
fn individual_counts(h: &CumulativeROHistogram32) -> BTreeMap<u32, u32> {
    let index = h.index();
    let count = h.count();
    let mut out = BTreeMap::new();
    let mut prev = 0u32;
    for (i, &idx) in index.iter().enumerate() {
        let c = count[i];
        out.insert(idx, c - prev);
        prev = c;
    }
    out
}

/// Combine two decomposed histograms with a per-bucket binary operator and
/// rebuild a `CumulativeROHistogram32`.  Returns `None` if the configs differ
/// or the resulting cumulative is not strictly non-decreasing (e.g. after
/// wraparound).
fn combine<F>(
    prev: &CumulativeROHistogram32,
    curr: &CumulativeROHistogram32,
    op: F,
) -> Option<CumulativeROHistogram32>
where
    F: Fn(u32, u32) -> u32,
{
    if prev.config() != curr.config() {
        return None;
    }

    let p = individual_counts(prev);
    let c = individual_counts(curr);

    let mut indices: BTreeSet<u32> = BTreeSet::new();
    indices.extend(p.keys().copied());
    indices.extend(c.keys().copied());

    let mut index = Vec::new();
    let mut count = Vec::new();
    let mut running: u32 = 0;

    for idx in indices {
        let pv = p.get(&idx).copied().unwrap_or(0);
        let cv = c.get(&idx).copied().unwrap_or(0);
        let d = op(pv, cv);
        if d > 0 {
            running = running.wrapping_add(d);
            index.push(idx);
            count.push(running);
        }
    }

    CumulativeROHistogram32::from_parts(prev.config(), index, count).ok()
}

/// Merge (sum) two delta snapshots bucket-wise.  Used when summing series at
/// the same timestamp across label sets.
fn wrapping_add(
    a: &CumulativeROHistogram32,
    b: &CumulativeROHistogram32,
) -> Option<CumulativeROHistogram32> {
    combine(a, b, |x, y| x.wrapping_add(y))
}

/// Compute the per-period delta between two consecutive cumulative-since-start
/// snapshots and narrow to `CumulativeROHistogram32`.  Used by the loader to
/// pre-difference snapshots at ingest time.  Returns `None` if the configs
/// differ, the delta cannot be represented, or any per-bucket cumulative
/// exceeds `u32::MAX`.
pub(crate) fn delta_to_32(
    prev: &CumulativeROHistogram,
    curr: &CumulativeROHistogram,
) -> Option<CumulativeROHistogram32> {
    if prev.config() != curr.config() {
        return None;
    }

    let p = u64_individual_counts(prev);
    let c = u64_individual_counts(curr);

    let mut indices: BTreeSet<u32> = BTreeSet::new();
    indices.extend(p.keys().copied());
    indices.extend(c.keys().copied());

    let mut index = Vec::new();
    let mut count = Vec::new();
    let mut running: u64 = 0;

    for idx in indices {
        let pv = p.get(&idx).copied().unwrap_or(0);
        let cv = c.get(&idx).copied().unwrap_or(0);
        let d = cv.wrapping_sub(pv);
        if d > 0 {
            running = running.wrapping_add(d);
            if running > u32::MAX as u64 {
                return None;
            }
            index.push(idx);
            count.push(running as u32);
        }
    }

    CumulativeROHistogram32::from_parts(prev.config(), index, count).ok()
}

fn u64_individual_counts(h: &CumulativeROHistogram) -> BTreeMap<u32, u64> {
    let index = h.index();
    let count = h.count();
    let mut out = BTreeMap::new();
    let mut prev = 0u64;
    for (i, &idx) in index.iter().enumerate() {
        let c = count[i];
        out.insert(idx, c - prev);
        prev = c;
    }
    out
}
