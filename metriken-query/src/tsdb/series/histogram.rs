use std::collections::BTreeMap;

use ::histogram::{Config, CumulativeROHistogram, CumulativeROHistogram32, Histogram, Quantile};

use super::*;

/// Represents a series of histogram readings.
///
/// Each entry is a per-period delta in `BucketsDelta` form: parallel
/// `Vec<u32>`s of sorted non-zero bucket indices and the corresponding
/// running cumulative counts across only those buckets.  The single
/// `histogram::Config` is hoisted out to the series — every snapshot in a
/// series shares the same configuration, so storing it per-snapshot was
/// pure overhead.
///
/// Per-snapshot footprint is now 56 B (was 88 B): two `Vec<u32>` headers
/// plus a `u64` timestamp.  See
/// `docs/histogram-series-memory-optimization-plan.md` for the next-step
/// CSR flattening that drops this further to 12 B per snapshot.
///
/// Per-period deltas comfortably fit in `u32` for any realistic sampling
/// interval, which halves the stored count footprint vs `u64`.
/// Pre-differencing on load also removes the per-pair `delta()` work from
/// the hot query paths.
///
/// Entries are kept as a sorted `Vec<(timestamp_ns, BucketsDelta)>` ordered
/// by timestamp.  Insertion is O(1) at the end (the load-time pattern) and
/// falls back to a binary-search insert otherwise.
///
/// Stride-window queries anchor at the first stored delta's timestamp.  The
/// very first raw snapshot is dropped (no predecessor, no delta) so for
/// regularly-sampled inputs every stride bin is shifted by one sampling
/// interval relative to the original cumulative-form behavior — equivalent to
/// rendering a null at the leading edge of the time axis.
#[derive(Default, Clone)]
pub struct HistogramSeries {
    config: Option<Config>,
    inner: Vec<(u64, BucketsDelta)>,
}

/// Per-snapshot delta payload: sorted ascending non-zero bucket indices and
/// the matching running cumulative counts (prefix sum) for those buckets.
#[derive(Default, Clone)]
pub(crate) struct BucketsDelta {
    index: Vec<u32>,
    count: Vec<u32>,
}

impl BucketsDelta {
    fn from_hist(h: &CumulativeROHistogram32) -> Self {
        Self {
            index: h.index().to_vec(),
            count: h.count().to_vec(),
        }
    }
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
        // First insert defines the series Config; subsequent mismatches
        // shouldn't happen (loader/ingest enforce a single Config per
        // series) so we treat it as a no-op rather than panicking.
        match self.config {
            None => self.config = Some(value.config()),
            Some(cfg) if cfg != value.config() => return,
            _ => {}
        }

        let delta = BucketsDelta::from_hist(&value);

        if let Some((last_ts, _)) = self.inner.last() {
            if timestamp > *last_ts {
                self.inner.push((timestamp, delta));
                return;
            }
            if timestamp == *last_ts {
                self.inner.last_mut().unwrap().1 = delta;
                return;
            }
        } else {
            self.inner.push((timestamp, delta));
            return;
        }
        match self.inner.binary_search_by_key(&timestamp, |(t, _)| *t) {
            Ok(idx) => self.inner[idx].1 = delta,
            Err(idx) => self.inner.insert(idx, (timestamp, delta)),
        }
    }

    /// Returns the time bounds (min, max) in nanoseconds, or None if empty.
    pub fn time_bounds(&self) -> Option<(u64, u64)> {
        let min = self.inner.first()?.0;
        let max = self.inner.last()?.0;
        Some((min, max))
    }

    /// Iterate the stored per-period delta histograms in timestamp order.
    /// Each entry's `CumulativeROHistogram32` is the delta between the
    /// previous raw snapshot and the snapshot at this timestamp; an empty
    /// histogram (`is_empty()` true) means no events occurred in the
    /// interval, or the delta could not be represented.
    pub fn iter(&self) -> impl Iterator<Item = (u64, CumulativeROHistogram32)> + '_ {
        let config = self.config;
        self.inner.iter().filter_map(move |(t, delta)| {
            let cfg = config?;
            CumulativeROHistogram32::from_parts(cfg, delta.index.clone(), delta.count.clone())
                .ok()
                .map(|h| (*t, h))
        })
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

        let config = self.config?;

        let mut result = HistogramHeatmapData::default();
        let mut min_value = f64::MAX;
        let mut max_value = f64::MIN;

        // Bucket boundaries come from the series' shared Config — collect
        // them once from an empty Histogram with the same configuration so
        // the Y-axis covers every bucket (not only the non-zero ones we
        // actually observe).
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
        let config = match self.config {
            Some(cfg) => cfg,
            None => return Box::new(std::iter::empty()),
        };
        match stride_ns {
            None => Box::new(self.inner.iter().filter_map(move |(t, delta)| {
                CumulativeROHistogram32::from_parts(
                    config,
                    delta.index.clone(),
                    delta.count.clone(),
                )
                .ok()
                .map(|h| (*t, h))
            })),
            Some(stride) => Box::new(StrideIter::new(self.inner.iter(), stride, config)),
        }
    }
}

/// Sums consecutive per-period deltas until at least `stride` nanoseconds have
/// elapsed since the last emitted bin, then yields the accumulated delta
/// histogram timestamped at the last snapshot in the window.
struct StrideIter<'a, I: Iterator<Item = &'a (u64, BucketsDelta)>> {
    iter: I,
    stride: u64,
    last_emit: Option<u64>,
    accum: BTreeMap<u32, u64>,
    accum_end_time: u64,
    config: Config,
}

impl<'a, I: Iterator<Item = &'a (u64, BucketsDelta)>> StrideIter<'a, I> {
    fn new(iter: I, stride: u64, config: Config) -> Self {
        Self {
            iter,
            stride,
            last_emit: None,
            accum: BTreeMap::new(),
            accum_end_time: 0,
            config,
        }
    }

    fn flush(&mut self) -> Option<(u64, CumulativeROHistogram32)> {
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
        CumulativeROHistogram32::from_parts(self.config, index, count)
            .ok()
            .map(|h| (end_time, h))
    }
}

impl<'a, I: Iterator<Item = &'a (u64, BucketsDelta)>> Iterator for StrideIter<'a, I> {
    type Item = (u64, CumulativeROHistogram32);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (time, delta) = match self.iter.next() {
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

            // Walk the delta's individual bucket counts (decompose the running
            // cumulative on the fly) into the accumulator.
            let mut prev = 0u32;
            for (i, &idx) in delta.index.iter().enumerate() {
                let cumu = delta.count[i];
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
        // Configs must match; mismatch is a "should never happen" path —
        // fall back to `self` rather than panicking.
        let config = match (self.config, other.config) {
            (Some(a), Some(b)) if a == b => Some(a),
            (Some(_), None) => self.config,
            (None, Some(_)) => other.config,
            (None, None) => None,
            _ => return self,
        };

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
                    let merged = combine_slices(
                        &a[i].1.index,
                        &a[i].1.count,
                        &b[j].1.index,
                        &b[j].1.count,
                        |x, y| x.wrapping_add(y),
                    );
                    out.push((a[i].0, merged));
                    i += 1;
                    j += 1;
                }
            }
        }
        out.extend_from_slice(&a[i..]);
        out.extend_from_slice(&b[j..]);

        HistogramSeries { config, inner: out }
    }
}

/// Two-pointer merge over two already-sorted index/count pairs.  Decomposes
/// each side's running cumulative into per-bucket individual counts on the
/// fly, applies `op`, and rebuilds an output `BucketsDelta` whose `count`
/// is the running cumulative across the merged bucket set.
///
/// O(N + M); replaces the previous BTreeMap/BTreeSet two-pass implementation.
fn combine_slices(
    a_idx: &[u32],
    a_cnt: &[u32],
    b_idx: &[u32],
    b_cnt: &[u32],
    op: impl Fn(u32, u32) -> u32,
) -> BucketsDelta {
    let mut out_index = Vec::with_capacity(a_idx.len() + b_idx.len());
    let mut out_count = Vec::with_capacity(a_idx.len() + b_idx.len());
    let mut running: u32 = 0;
    let mut a_prev: u32 = 0;
    let mut b_prev: u32 = 0;
    let (mut i, mut j) = (0usize, 0usize);

    while i < a_idx.len() && j < b_idx.len() {
        match a_idx[i].cmp(&b_idx[j]) {
            std::cmp::Ordering::Less => {
                let av = a_cnt[i] - a_prev;
                a_prev = a_cnt[i];
                let d = op(av, 0);
                if d > 0 {
                    running = running.wrapping_add(d);
                    out_index.push(a_idx[i]);
                    out_count.push(running);
                }
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                let bv = b_cnt[j] - b_prev;
                b_prev = b_cnt[j];
                let d = op(0, bv);
                if d > 0 {
                    running = running.wrapping_add(d);
                    out_index.push(b_idx[j]);
                    out_count.push(running);
                }
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let av = a_cnt[i] - a_prev;
                let bv = b_cnt[j] - b_prev;
                a_prev = a_cnt[i];
                b_prev = b_cnt[j];
                let d = op(av, bv);
                if d > 0 {
                    running = running.wrapping_add(d);
                    out_index.push(a_idx[i]);
                    out_count.push(running);
                }
                i += 1;
                j += 1;
            }
        }
    }
    while i < a_idx.len() {
        let av = a_cnt[i] - a_prev;
        a_prev = a_cnt[i];
        let d = op(av, 0);
        if d > 0 {
            running = running.wrapping_add(d);
            out_index.push(a_idx[i]);
            out_count.push(running);
        }
        i += 1;
    }
    while j < b_idx.len() {
        let bv = b_cnt[j] - b_prev;
        b_prev = b_cnt[j];
        let d = op(0, bv);
        if d > 0 {
            running = running.wrapping_add(d);
            out_index.push(b_idx[j]);
            out_count.push(running);
        }
        j += 1;
    }

    BucketsDelta {
        index: out_index,
        count: out_count,
    }
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

    let mut indices: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
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

/// Build an empty delta histogram for the given config.  Used by the loader
/// and ingest path to record an explicit empty entry at a timestamp where a
/// non-empty delta could not be produced (overflow, reset, config mismatch,
/// or a null parquet row), so that offset-based lookups remain aligned with
/// the shared timestamp axis.
pub(crate) fn empty_delta_32(config: ::histogram::Config) -> CumulativeROHistogram32 {
    CumulativeROHistogram32::from_parts(config, Vec::new(), Vec::new())
        .expect("empty index/count vectors always validate")
}

/// Like [`delta_to_32`] but never drops the timestamp: when the delta cannot
/// be represented (config mismatch, u32 overflow, counter reset producing
/// wrap-around) the result is an empty histogram with `prev`'s config.  The
/// caller can still distinguish "no events" (empty delta) from "lots of
/// events" via `total_count()`, but every observed snapshot pair produces a
/// stored entry.
pub(crate) fn delta_to_32_or_empty(
    prev: &CumulativeROHistogram,
    curr: &CumulativeROHistogram,
) -> CumulativeROHistogram32 {
    delta_to_32(prev, curr).unwrap_or_else(|| empty_delta_32(prev.config()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::histogram::{Config, Histogram};

    /// Build a `CumulativeROHistogram` (u64) from an `(index, count)` pair —
    /// indices are bucket positions, counts are total events in each bucket.
    fn build_cumu(cfg: Config, events: &[(u32, u64)]) -> CumulativeROHistogram {
        let mut h = Histogram::with_config(&cfg);
        let raw = h.as_mut_slice();
        for &(idx, count) in events {
            raw[idx as usize] = count;
        }
        CumulativeROHistogram::from(&h)
    }

    fn cfg() -> Config {
        Config::new(4, 16).expect("valid config")
    }

    #[test]
    fn delta_of_identical_snapshots_is_empty() {
        let c = cfg();
        let prev = build_cumu(c, &[(10, 5), (20, 3)]);
        let curr = build_cumu(c, &[(10, 5), (20, 3)]);
        let d = delta_to_32(&prev, &curr).expect("identical snapshots produce a delta");
        assert!(d.is_empty(), "delta of identical snapshots should be empty");
        assert_eq!(d.total_count(), 0);
    }

    #[test]
    fn delta_of_strictly_increasing_snapshots_is_nonempty() {
        let c = cfg();
        let prev = build_cumu(c, &[(10, 5), (20, 3)]);
        let curr = build_cumu(c, &[(10, 7), (20, 4), (30, 1)]);
        let d = delta_to_32(&prev, &curr).expect("delta should compute");
        assert!(!d.is_empty());
        assert_eq!(d.total_count(), 4); // (7-5) + (4-3) + (1-0)
    }

    #[test]
    fn delta_to_32_or_empty_falls_back_on_reset() {
        // Counter reset: curr is strictly less than prev.  delta_to_32
        // returns None (overflow on wrapping_sub); _or_empty must still
        // produce an empty histogram so the caller can record the
        // timestamp.
        let c = cfg();
        let prev = build_cumu(c, &[(10, 1_000_000), (20, 500_000)]);
        let curr = build_cumu(c, &[(10, 100), (20, 50)]);
        assert!(
            delta_to_32(&prev, &curr).is_none(),
            "reset should overflow u32"
        );
        let d = delta_to_32_or_empty(&prev, &curr);
        assert!(d.is_empty(), "reset must produce an explicit empty");
        assert_eq!(d.config(), c);
    }

    #[test]
    fn series_preserves_every_timestamp_across_empty_and_reset_deltas() {
        // Three transitions: identical (empty), normal, reset (empty).
        // After loading, the series must have 3 entries — one per
        // observed snapshot pair — even though two of them are empty.
        let c = cfg();
        let s1 = build_cumu(c, &[(10, 5), (20, 3)]);
        let s2 = build_cumu(c, &[(10, 5), (20, 3)]); // identical, no events
        let s3 = build_cumu(c, &[(10, 8), (20, 4)]); // +3 in bucket 10, +1 in bucket 20
        let s4 = build_cumu(c, &[(10, 1), (20, 0)]); // counter reset

        let mut series = HistogramSeries::default();
        series.insert(2_000_000_000, delta_to_32_or_empty(&s1, &s2));
        series.insert(3_000_000_000, delta_to_32_or_empty(&s2, &s3));
        series.insert(4_000_000_000, delta_to_32_or_empty(&s3, &s4));

        let entries: Vec<(u64, CumulativeROHistogram32)> = series.iter().collect();
        assert_eq!(entries.len(), 3, "every timestamp must be preserved");

        let entry_at = |ts: u64| {
            entries
                .iter()
                .find(|(t, _)| *t == ts)
                .map(|(_, h)| h.clone())
                .expect("ts present")
        };
        let e2 = entry_at(2_000_000_000);
        assert!(e2.is_empty(), "no-events delta must be explicit empty");

        let e3 = entry_at(3_000_000_000);
        assert!(!e3.is_empty());
        assert_eq!(e3.total_count(), 4);

        let e4 = entry_at(4_000_000_000);
        assert!(e4.is_empty(), "reset delta must be explicit empty");
    }

    #[test]
    fn time_bounds_cover_empty_entries() {
        // Empty deltas still occupy timestamps and must show up in
        // time_bounds() — otherwise consumers that align time axes by
        // bounds would clip them off.
        let c = cfg();
        let s1 = build_cumu(c, &[(10, 5)]);
        let s2 = build_cumu(c, &[(10, 5)]);

        let mut series = HistogramSeries::default();
        series.insert(1_000_000_000, delta_to_32_or_empty(&s1, &s2));
        series.insert(5_000_000_000, delta_to_32_or_empty(&s1, &s2));

        assert_eq!(series.time_bounds(), Some((1_000_000_000, 5_000_000_000)));
    }

    #[test]
    fn add_merges_disjoint_and_overlapping_timestamps() {
        // Two series with one shared timestamp and one each unique to it;
        // the shared-timestamp delta should sum bucket-wise.
        let c = cfg();
        let s_prev = build_cumu(c, &[(10, 0)]);

        let mut a = HistogramSeries::default();
        a.insert(
            1_000,
            delta_to_32_or_empty(&s_prev, &build_cumu(c, &[(10, 5)])),
        );
        a.insert(
            2_000,
            delta_to_32_or_empty(&s_prev, &build_cumu(c, &[(10, 3)])),
        );

        let mut b = HistogramSeries::default();
        b.insert(
            2_000,
            delta_to_32_or_empty(&s_prev, &build_cumu(c, &[(10, 7), (20, 1)])),
        );
        b.insert(
            3_000,
            delta_to_32_or_empty(&s_prev, &build_cumu(c, &[(20, 4)])),
        );

        let merged = a + &b;
        let entries: Vec<(u64, CumulativeROHistogram32)> = merged.iter().collect();
        assert_eq!(entries.len(), 3);
        // 2_000 is the overlap: bucket 10 gets 3+7=10, bucket 20 gets 0+1=1.
        let merged_2k = entries
            .iter()
            .find(|(t, _)| *t == 2_000)
            .map(|(_, h)| h.clone())
            .unwrap();
        assert_eq!(merged_2k.total_count(), 11);
    }
}
