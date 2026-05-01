use std::collections::BTreeMap;
use std::ops::Range;

use ::histogram::{
    Config, CumulativeROHistogram, CumulativeROHistogram32, CumulativeROHistogram32Ref,
};

/// Represents a series of histogram readings stored in a flat CSR layout.
///
/// Bucket data for every snapshot in the series is concatenated into a
/// single `indices` / `counts` pair of buffers.  `offsets[i]` marks where
/// snapshot `i`'s range begins; the range ends at `offsets[i+1]` (or
/// `indices.len()` for the last snapshot).  Per-snapshot fixed overhead
/// is **12 B** (8 B timestamp + 4 B offset) — down from 56 B in the
/// per-snapshot Vec layout, and 88 B in the pre-hoist layout.
///
/// `Config` is held once at the series level — every snapshot shares it,
/// so per-snapshot storage was redundant.
///
/// Each snapshot's bucket data is the per-period delta between the
/// previous raw cumulative snapshot and the snapshot at this timestamp:
/// sorted ascending non-zero bucket indices, with `counts` being the
/// running cumulative (prefix sum) across only those non-zero buckets.
/// Per-period deltas comfortably fit in `u32` for any realistic sampling
/// interval, which halves the stored count footprint vs `u64`.
///
/// Stride-window queries anchor at the first stored delta's timestamp.
/// The very first raw snapshot is dropped (no predecessor, no delta) so
/// for regularly-sampled inputs every stride bin is shifted by one
/// sampling interval relative to the original cumulative-form behavior —
/// equivalent to rendering a null at the leading edge of the time axis.
#[derive(Default, Clone)]
pub struct HistogramSeries {
    config: Option<Config>,
    /// Sorted ascending timestamps; `timestamps[i]` is snapshot `i`'s ts.
    timestamps: Vec<u64>,
    /// Start index into `indices`/`counts` for each snapshot.  Snapshot
    /// `i` covers `offsets[i] .. offsets.get(i+1).unwrap_or(indices.len())`.
    offsets: Vec<u32>,
    /// Concatenation of every snapshot's non-zero bucket indices.
    indices: Vec<u32>,
    /// Concatenation of every snapshot's running-cumulative counts (one
    /// per index).  Each snapshot's slice is locally a prefix sum,
    /// independent of any other snapshot's slice.
    counts: Vec<u32>,
}

/// Borrowed view into a single snapshot's bucket data.
struct DeltaView<'a> {
    index: &'a [u32],
    count: &'a [u32],
}

impl HistogramSeries {
    pub fn is_empty(&self) -> bool {
        self.timestamps.is_empty()
    }

    fn len(&self) -> usize {
        self.timestamps.len()
    }

    /// Range into `indices`/`counts` for snapshot `i`.
    fn snapshot_range(&self, i: usize) -> Range<usize> {
        let start = self.offsets[i] as usize;
        let end = self
            .offsets
            .get(i + 1)
            .copied()
            .map(|v| v as usize)
            .unwrap_or(self.indices.len());
        start..end
    }

    /// Borrowed view of snapshot `i`'s bucket data.
    fn delta_at(&self, i: usize) -> DeltaView<'_> {
        let r = self.snapshot_range(i);
        DeltaView {
            index: &self.indices[r.clone()],
            count: &self.counts[r],
        }
    }

    /// Append a snapshot's bucket data at the end of the flat buffers.
    fn push_snapshot(&mut self, ts: u64, idx: &[u32], cnt: &[u32]) {
        self.timestamps.push(ts);
        self.offsets.push(self.indices.len() as u32);
        self.indices.extend_from_slice(idx);
        self.counts.extend_from_slice(cnt);
    }

    /// Replace the last snapshot's bucket data in place.  Used when an
    /// `insert` collides with the most recent timestamp.
    fn replace_last_snapshot(&mut self, idx: &[u32], cnt: &[u32]) {
        let last = self.timestamps.len() - 1;
        let r = self.snapshot_range(last);
        self.indices.splice(r.clone(), idx.iter().copied());
        self.counts.splice(r, cnt.iter().copied());
    }

    /// Replace the bucket data at snapshot `i` in place.
    fn replace_snapshot(&mut self, i: usize, idx: &[u32], cnt: &[u32]) {
        let r = self.snapshot_range(i);
        let old_len = r.end - r.start;
        let new_len = idx.len();
        self.indices.splice(r.clone(), idx.iter().copied());
        self.counts.splice(r, cnt.iter().copied());
        if new_len != old_len {
            let delta = new_len as i64 - old_len as i64;
            for off in &mut self.offsets[i + 1..] {
                *off = (*off as i64 + delta) as u32;
            }
        }
    }

    /// Insert a new snapshot at position `i`.
    fn insert_snapshot(&mut self, i: usize, ts: u64, idx: &[u32], cnt: &[u32]) {
        let off = if i < self.offsets.len() {
            self.offsets[i] as usize
        } else {
            self.indices.len()
        };
        self.timestamps.insert(i, ts);
        self.offsets.insert(i, off as u32);
        self.indices
            .splice(off..off, idx.iter().copied())
            .for_each(drop);
        self.counts
            .splice(off..off, cnt.iter().copied())
            .for_each(drop);
        let inserted_len = idx.len() as u32;
        for o in &mut self.offsets[i + 1..] {
            *o += inserted_len;
        }
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

        let idx = value.index();
        let cnt = value.count();

        if let Some(&last_ts) = self.timestamps.last() {
            if timestamp > last_ts {
                self.push_snapshot(timestamp, idx, cnt);
                return;
            }
            if timestamp == last_ts {
                self.replace_last_snapshot(idx, cnt);
                return;
            }
        } else {
            self.push_snapshot(timestamp, idx, cnt);
            return;
        }
        match self.timestamps.binary_search(&timestamp) {
            Ok(pos) => self.replace_snapshot(pos, idx, cnt),
            Err(pos) => self.insert_snapshot(pos, timestamp, idx, cnt),
        }
    }

    /// Returns the time bounds (min, max) in nanoseconds, or None if empty.
    pub fn time_bounds(&self) -> Option<(u64, u64)> {
        let min = *self.timestamps.first()?;
        let max = *self.timestamps.last()?;
        Some((min, max))
    }

    /// Iterate the stored per-period delta histograms in timestamp order.
    /// Each entry is a borrowed `CumulativeROHistogram32Ref` over the
    /// series' flat buffers — zero allocation per snapshot.
    ///
    /// An empty `Ref` (`is_empty()` true) means no events occurred in the
    /// interval, or the delta could not be represented.
    pub fn iter(&self) -> impl Iterator<Item = (u64, CumulativeROHistogram32Ref<'_>)> + '_ {
        let config = self.config;
        (0..self.len()).filter_map(move |i| {
            let cfg = config?;
            let view = self.delta_at(i);
            // The slices were validated when each snapshot was inserted
            // (via `from_parts` on the Owned hist) so `from_parts_unchecked`
            // is sound here and avoids a re-validation walk.
            let r = CumulativeROHistogram32Ref::from_parts_unchecked(cfg, view.index, view.count);
            Some((self.timestamps[i], r))
        })
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

        let entries: Vec<(u64, bool, u64)> = series
            .iter()
            .map(|(t, h)| (t, h.is_empty(), h.total_count()))
            .collect();
        assert_eq!(entries.len(), 3, "every timestamp must be preserved");

        let entry_at = |ts: u64| {
            entries
                .iter()
                .find(|(t, _, _)| *t == ts)
                .copied()
                .expect("ts present")
        };
        let (_, e2_empty, _) = entry_at(2_000_000_000);
        assert!(e2_empty, "no-events delta must be explicit empty");

        let (_, e3_empty, e3_count) = entry_at(3_000_000_000);
        assert!(!e3_empty);
        assert_eq!(e3_count, 4);

        let (_, e4_empty, _) = entry_at(4_000_000_000);
        assert!(e4_empty, "reset delta must be explicit empty");
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
    fn out_of_order_insert_shifts_offsets_correctly() {
        // Insert at end, then in the middle; verify the iteration order
        // stays sorted and bucket payloads round-trip.
        let c = cfg();
        let s_prev = build_cumu(c, &[(0, 0)]);
        let mut series = HistogramSeries::default();
        series.insert(
            1_000,
            delta_to_32_or_empty(&s_prev, &build_cumu(c, &[(10, 1)])),
        );
        series.insert(
            3_000,
            delta_to_32_or_empty(&s_prev, &build_cumu(c, &[(10, 2), (20, 5)])),
        );
        series.insert(
            2_000,
            delta_to_32_or_empty(&s_prev, &build_cumu(c, &[(20, 3)])),
        );

        let entries: Vec<(u64, u64)> = series.iter().map(|(t, h)| (t, h.total_count())).collect();
        let times: Vec<u64> = entries.iter().map(|(t, _)| *t).collect();
        assert_eq!(times, vec![1_000, 2_000, 3_000]);
        assert_eq!(entries[0].1, 1);
        assert_eq!(entries[1].1, 3);
        assert_eq!(entries[2].1, 7);
    }
}
