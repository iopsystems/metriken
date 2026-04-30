use super::*;

/// A time-keyed series of `f64` values, stored as a sorted `Vec<(u64, f64)>`.
///
/// Timestamps are kept in strictly ascending order; insertion is fast at the
/// end of the series (the common load-time pattern) and falls back to a
/// binary-search insert otherwise.  Duplicate timestamps overwrite.
#[derive(Default, Clone)]
pub struct UntypedSeries {
    inner: Vec<(u64, f64)>,
}

impl UntypedSeries {
    /// Build directly from a `Vec` whose timestamps are already sorted in
    /// strictly ascending order with no duplicates.  No checking is performed.
    pub(crate) fn from_sorted(inner: Vec<(u64, f64)>) -> Self {
        Self { inner }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, (u64, f64)> {
        self.inner.iter()
    }

    /// Slice covering all samples whose timestamp falls in `[start, end]`
    /// (inclusive on both ends).
    pub fn range(&self, start: u64, end: u64) -> &[(u64, f64)] {
        let lo = self.inner.partition_point(|&(t, _)| t < start);
        let hi = self.inner.partition_point(|&(t, _)| t <= end);
        &self.inner[lo..hi]
    }

    /// The latest sample whose timestamp is `<= ts`, or `None` if no such
    /// sample exists.
    pub fn last_at_or_before(&self, ts: u64) -> Option<(u64, f64)> {
        let hi = self.inner.partition_point(|&(t, _)| t <= ts);
        if hi == 0 {
            None
        } else {
            Some(self.inner[hi - 1])
        }
    }

    /// First sample (earliest timestamp), or None if empty.
    pub fn first(&self) -> Option<(u64, f64)> {
        self.inner.first().copied()
    }

    /// Last sample (latest timestamp), or None if empty.
    pub fn last(&self) -> Option<(u64, f64)> {
        self.inner.last().copied()
    }

    /// Insert or overwrite at `ts`.  Maintains sorted order.
    pub fn insert(&mut self, ts: u64, val: f64) {
        if let Some(&(last_ts, _)) = self.inner.last() {
            if ts > last_ts {
                self.inner.push((ts, val));
                return;
            }
            if ts == last_ts {
                self.inner.last_mut().unwrap().1 = val;
                return;
            }
        } else {
            self.inner.push((ts, val));
            return;
        }
        match self.inner.binary_search_by_key(&ts, |&(t, _)| t) {
            Ok(idx) => self.inner[idx].1 = val,
            Err(idx) => self.inner.insert(idx, (ts, val)),
        }
    }

    /// Add `val` at `ts` if absent; otherwise add to the existing value.
    pub fn add_at(&mut self, ts: u64, val: f64) {
        if let Some(&(last_ts, _)) = self.inner.last() {
            if ts > last_ts {
                self.inner.push((ts, val));
                return;
            }
            if ts == last_ts {
                self.inner.last_mut().unwrap().1 += val;
                return;
            }
        } else {
            self.inner.push((ts, val));
            return;
        }
        match self.inner.binary_search_by_key(&ts, |&(t, _)| t) {
            Ok(idx) => self.inner[idx].1 += val,
            Err(idx) => self.inner.insert(idx, (ts, val)),
        }
    }

    fn divide_scalar(mut self, divisor: f64) -> Self {
        for (_, v) in self.inner.iter_mut() {
            *v /= divisor;
        }
        self
    }

    /// Element-wise divide on the intersection of timestamps; non-matching
    /// timestamps are dropped from the result.
    fn divide(self, other: &UntypedSeries) -> Self {
        Self::merge_intersect(&self.inner, &other.inner, |a, b| a / b)
    }

    fn multiply_scalar(mut self, multiplier: f64) -> Self {
        for (_, v) in self.inner.iter_mut() {
            *v *= multiplier;
        }
        self
    }

    fn multiply(self, other: &UntypedSeries) -> Self {
        Self::merge_intersect(&self.inner, &other.inner, |a, b| a * b)
    }

    /// Two-pointer merge that keeps only matching timestamps and applies `op`.
    fn merge_intersect(a: &[(u64, f64)], b: &[(u64, f64)], op: impl Fn(f64, f64) -> f64) -> Self {
        let mut out = Vec::with_capacity(a.len().min(b.len()));
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            match a[i].0.cmp(&b[j].0) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    out.push((a[i].0, op(a[i].1, b[j].1)));
                    i += 1;
                    j += 1;
                }
            }
        }
        Self::from_sorted(out)
    }

    /// Two-pointer merge that keeps the union of timestamps.  When both sides
    /// have a value at a given timestamp, `op` combines them; otherwise the
    /// existing value is kept.
    fn merge_union(a: &[(u64, f64)], b: &[(u64, f64)], op: impl Fn(f64, f64) -> f64) -> Self {
        let mut out = Vec::with_capacity(a.len() + b.len());
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            match a[i].0.cmp(&b[j].0) {
                std::cmp::Ordering::Less => {
                    out.push(a[i]);
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    out.push(b[j]);
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    out.push((a[i].0, op(a[i].1, b[j].1)));
                    i += 1;
                    j += 1;
                }
            }
        }
        out.extend_from_slice(&a[i..]);
        out.extend_from_slice(&b[j..]);
        Self::from_sorted(out)
    }

    pub fn as_data(&self) -> Vec<Vec<f64>> {
        let mut times = Vec::with_capacity(self.inner.len());
        let mut values = Vec::with_capacity(self.inner.len());

        for (time, value) in self.inner.iter() {
            // convert time to unix epoch float seconds
            times.push(*time as f64 / 1000000000.0);
            values.push(*value);
        }

        vec![times, values]
    }
}

impl Add<UntypedSeries> for UntypedSeries {
    type Output = UntypedSeries;

    fn add(self, other: UntypedSeries) -> Self::Output {
        self.add(&other)
    }
}

impl Add<&UntypedSeries> for UntypedSeries {
    type Output = UntypedSeries;

    fn add(self, other: &UntypedSeries) -> Self::Output {
        UntypedSeries::merge_union(&self.inner, &other.inner, |a, b| a + b)
    }
}

impl Div<UntypedSeries> for UntypedSeries {
    type Output = UntypedSeries;
    fn div(self, other: UntypedSeries) -> <Self as Div<UntypedSeries>>::Output {
        self.divide(&other)
    }
}

impl Div<&UntypedSeries> for UntypedSeries {
    type Output = UntypedSeries;
    fn div(self, other: &UntypedSeries) -> <Self as Div<UntypedSeries>>::Output {
        self.divide(other)
    }
}

impl Div<f64> for UntypedSeries {
    type Output = UntypedSeries;
    fn div(self, other: f64) -> <Self as Div<UntypedSeries>>::Output {
        self.divide_scalar(other)
    }
}

impl Mul<UntypedSeries> for UntypedSeries {
    type Output = UntypedSeries;
    fn mul(self, other: UntypedSeries) -> <Self as Mul<UntypedSeries>>::Output {
        self.multiply(&other)
    }
}

impl Mul<&UntypedSeries> for UntypedSeries {
    type Output = UntypedSeries;
    fn mul(self, other: &UntypedSeries) -> <Self as Mul<UntypedSeries>>::Output {
        self.multiply(other)
    }
}

impl Mul<f64> for UntypedSeries {
    type Output = UntypedSeries;
    fn mul(self, other: f64) -> <Self as Mul<UntypedSeries>>::Output {
        self.multiply_scalar(other)
    }
}
