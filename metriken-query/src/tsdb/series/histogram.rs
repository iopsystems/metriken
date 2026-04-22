use std::collections::{BTreeMap, BTreeSet};

use ::histogram::{CumulativeROHistogram, Histogram, Quantile, SampleQuantiles};

use super::*;

/// Represents a series of histogram readings.
///
/// Histograms are stored as [`CumulativeROHistogram`]s, a read-only cumulative
/// form that keeps only non-zero buckets in columnar form. This is
/// substantially smaller than the dense [`Histogram`] representation when the
/// underlying distribution is sparse, and the cumulative layout keeps quantile
/// queries cheap.
#[derive(Default, Clone)]
pub struct HistogramSeries {
    inner: BTreeMap<u64, CumulativeROHistogram>,
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

    pub fn insert(&mut self, timestamp: u64, value: CumulativeROHistogram) {
        self.inner.insert(timestamp, value);
    }

    /// Returns the time bounds (min, max) in nanoseconds, or None if empty.
    pub fn time_bounds(&self) -> Option<(u64, u64)> {
        let min = *self.inner.keys().next()?;
        let max = *self.inner.keys().next_back()?;
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

        let (&first_time, first_hist) = self.inner.first_key_value().unwrap();
        let mut prev = first_hist;
        let mut prev_time = first_time;

        let mut result = vec![UntypedSeries::default(); percentiles.len()];

        for (time, curr) in self.inner.iter().skip(1) {
            // With a stride, skip snapshots until we've covered the stride window.
            if let Some(stride) = stride_ns {
                if *time - prev_time < stride {
                    continue;
                }
            }

            let delta = match delta(prev, curr) {
                Some(d) => d,
                None => {
                    prev = curr;
                    prev_time = *time;
                    continue;
                }
            };

            if let Ok(Some(q_results)) = delta.quantiles(percentiles) {
                for (id, q) in percentiles.iter().enumerate() {
                    if let Ok(quantile) = Quantile::new(*q) {
                        if let Some(bucket) = q_results.get(&quantile) {
                            result[id].inner.insert(*time, bucket.end() as f64);
                        }
                    }
                }
            }

            prev = curr;
            prev_time = *time;
        }

        Some(result)
    }

    /// Returns bucket data suitable for rendering as a heatmap.
    /// Y-axis is bucket index (latency range), X-axis is time, color is count.
    pub fn heatmap(&self, stride_ns: Option<u64>) -> Option<HistogramHeatmapData> {
        if self.inner.len() < 2 {
            return None;
        }

        let mut result = HistogramHeatmapData::default();
        let mut min_value = f64::MAX;
        let mut max_value = f64::MIN;

        let (&first_time, first_hist) = self.inner.first_key_value().unwrap();
        let mut prev = first_hist;
        let mut prev_time = first_time;

        // Bucket boundaries come from the histogram config, which is identical
        // across the series.  Collect them once from an empty Histogram with
        // the same configuration so the Y-axis covers every bucket (not only
        // the non-zero ones we actually observe).
        let config = first_hist.config();
        if let Ok(empty) = Histogram::new(config.grouping_power(), config.max_value_power()) {
            result.bucket_bounds = empty.iter().map(|b| b.end()).collect();
        } else {
            return None;
        }

        for (time, curr) in self.inner.iter().skip(1) {
            // With a stride, skip snapshots until we've covered the stride window.
            if let Some(stride) = stride_ns {
                if *time - prev_time < stride {
                    continue;
                }
            }

            let delta = match delta(prev, curr) {
                Some(d) => d,
                None => {
                    prev = curr;
                    prev_time = *time;
                    continue;
                }
            };
            let time_index = result.timestamps.len();

            // Store timestamp in seconds
            result.timestamps.push(*time as f64 / 1_000_000_000.0);

            // Emit only the non-zero buckets of the delta — CumulativeRO only
            // stores those, so this is also a zero-copy walk.
            for (i, bucket) in delta.iter().enumerate() {
                let bucket_index = delta.index()[i] as usize;
                let count_f64 = bucket.count() as f64;
                result.data.push((time_index, bucket_index, count_f64));
                min_value = min_value.min(count_f64);
                max_value = max_value.max(count_f64);
            }

            prev = curr;
            prev_time = *time;
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
}

impl Add<&HistogramSeries> for HistogramSeries {
    type Output = HistogramSeries;
    fn add(self, other: &HistogramSeries) -> Self::Output {
        let mut result = self.clone();

        for (time, histogram) in other.inner.iter() {
            if let Some(h) = result.inner.get_mut(time) {
                if let Some(sum) = wrapping_add(h, histogram) {
                    *h = sum;
                }
                // Skip mismatched histograms rather than panicking
            } else {
                result.inner.insert(*time, histogram.clone());
            }
        }

        result
    }
}

/// Decompose a `CumulativeROHistogram` into `bucket_index -> individual_count`.
fn individual_counts(h: &CumulativeROHistogram) -> BTreeMap<u32, u64> {
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

/// Combine two decomposed histograms with a per-bucket binary operator and
/// rebuild a `CumulativeROHistogram`.  Returns `None` if the configs differ.
fn combine<F>(
    prev: &CumulativeROHistogram,
    curr: &CumulativeROHistogram,
    op: F,
) -> Option<CumulativeROHistogram>
where
    F: Fn(u64, u64) -> u64,
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
    let mut running: u64 = 0;

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

    CumulativeROHistogram::from_parts(prev.config(), index, count).ok()
}

/// Delta between two snapshots: `curr - prev` per bucket (wrapping).
fn delta(
    prev: &CumulativeROHistogram,
    curr: &CumulativeROHistogram,
) -> Option<CumulativeROHistogram> {
    combine(prev, curr, |p, c| c.wrapping_sub(p))
}

/// Merge (sum) two snapshots bucket-wise.  Used when summing series at the
/// same timestamp across label sets.
fn wrapping_add(
    a: &CumulativeROHistogram,
    b: &CumulativeROHistogram,
) -> Option<CumulativeROHistogram> {
    combine(a, b, |x, y| x.wrapping_add(y))
}
