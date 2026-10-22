use std::collections::BinaryHeap;

use crate::labels::Labels;
use crate::types::HistogramSnapshot;

/// Which reading convention a histogram column has: cumulative counts that a
/// reader differences, or a gauge histogram's state that it must not.
/// Selects the column set a reader's histogram plumbing works over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistKind {
    Counting,
    Gauge,
}

pub struct HistogramStreamMeta {
    pub config: ::histogram::Config,
    /// Labels for each series, indexed by `HistogramRow::series_idx`.
    pub series: Vec<Labels>,
}

pub struct HistogramRow {
    /// Index into `HistogramStreamMeta::series`.
    pub series_idx: usize,
    pub timestamp: u64,
    pub snapshot: HistogramSnapshot,
}

pub struct HistogramStream {
    pub meta: HistogramStreamMeta,
    /// Rows in ascending `(timestamp, series_idx)` order.
    pub rows: Box<dyn Iterator<Item = HistogramRow> + Send>,
}

impl HistogramStream {
    /// K-way merge of multiple streams for the same metric across different
    /// parquet files. Normalises `series_idx` values to a unified series list.
    /// Returns `None` if `streams` is empty.
    ///
    /// Streams of different bucket configurations are rebucketed to the
    /// coarsest of them ([`common_config`]).
    pub fn merge(streams: Vec<HistogramStream>) -> Option<HistogramStream> {
        merge_streams(streams, None)
    }

    /// [`merge`](Self::merge) for streams read at different instants (two
    /// recordings), re-stamped onto the rows of `streams[clock]`: a row of
    /// another stream takes the time of the clock's first row at or after
    /// it, so the rows of every stream that fall between two clock rows
    /// share the later one's time.
    ///
    /// The histogram functions pool, at each row time, the deltas of every
    /// series with a row there, and two recordings' rows fall at different
    /// times. On the clock's times, each pooled delta is every series' increase
    /// since the previous clock row, give or take the part of one row
    /// interval of a faster stream that straddles a clock row. A series with
    /// several rows between two clock rows contributes each delta, which sum
    /// to its increase over the interval. Pick the slowest stream as the
    /// clock, so no clock interval is shorter than another stream's rows.
    /// Rows before the clock's first row, and inside a hole in it (an
    /// interval more than twice the one before), keep their own times:
    /// there the clock stream has nothing to pool with them. Rows within one
    /// clock interval after its last row are left out (the clock's next row
    /// is past the read, so they would form a tick of their own streams
    /// alone), and rows after that keep their own times.
    pub fn merge_on_clock(streams: Vec<HistogramStream>, clock: usize) -> Option<HistogramStream> {
        merge_streams(streams, Some(clock))
    }

    /// This stream with every row's buckets under `to`, which must be
    /// [`common_config`] of this stream's configuration and others (no
    /// coarser bucket of `to` splits one of this stream's).
    pub fn rebucketed(self, to: ::histogram::Config) -> HistogramStream {
        let from = self.meta.config;
        if from == to {
            return self;
        }
        HistogramStream {
            meta: HistogramStreamMeta {
                config: to,
                series: self.meta.series,
            },
            rows: Box::new(self.rows.map(move |mut row| {
                row.snapshot = rebucket(row.snapshot, &from, &to);
                row
            })),
        }
    }
}

/// The bucket configuration a merge decodes every stream under: the
/// smallest grouping power and the largest max value power among them. A
/// bucket of a configuration with a higher grouping power lies inside one
/// bucket of a lower one (a lower grouping power splits each power of two
/// into fewer parts), and a larger max value power only adds buckets at the
/// top, so every stream's buckets map into this one's without splitting.
pub fn common_config(
    configs: impl IntoIterator<Item = ::histogram::Config>,
) -> Option<::histogram::Config> {
    let mut grouping = None::<u8>;
    let mut max_power = None::<u8>;
    for c in configs {
        grouping = Some(grouping.map_or(c.grouping_power(), |g| g.min(c.grouping_power())));
        max_power = Some(max_power.map_or(c.max_value_power(), |m| m.max(c.max_value_power())));
    }
    ::histogram::Config::new(grouping?, max_power?).ok()
}

/// The lowest value of bucket `index` under `config` (as the `histogram`
/// crate computes it).
fn lower_bound(config: &::histogram::Config, index: u32) -> u64 {
    let g_pow = config.grouping_power() as u64;
    let index = index as u64;
    let g = index >> g_pow;
    let h = index - g * (1 << g_pow);
    if g < 1 {
        h
    } else {
        (1 << (g_pow + g - 1)) + (1 << (g - 1)) * h
    }
}

/// The bucket of `value` under `config` (as the `histogram` crate computes
/// it), clamped to the last bucket.
fn bucket_of(config: &::histogram::Config, value: u64) -> u32 {
    let grouping = config.grouping_power() as u32;
    let cutoff_power = grouping + 1;
    let cutoff_value = 1u64 << cutoff_power;
    let last = config.total_buckets() as u32 - 1;
    if value < cutoff_value {
        return (value as u32).min(last);
    }
    let power = 63 - value.leading_zeros();
    let log_bin = power - cutoff_power;
    let offset = (value - (1 << power)) >> (power - grouping);
    let lower_bin_count = cutoff_value as u32;
    let divisions = 1u32 << grouping;
    (lower_bin_count + log_bin * divisions + offset as u32).min(last)
}

/// `snapshot`'s buckets under `to`, from `from`. Indices stay ascending.
/// A snapshot's counts are cumulative over its buckets, so buckets that land
/// in one take the last one's count, which already includes the others.
fn rebucket(
    snapshot: HistogramSnapshot,
    from: &::histogram::Config,
    to: &::histogram::Config,
) -> HistogramSnapshot {
    let mut out = HistogramSnapshot {
        index: Vec::with_capacity(snapshot.index.len()),
        count: Vec::with_capacity(snapshot.count.len()),
    };
    for (i, c) in snapshot.index.into_iter().zip(snapshot.count) {
        let j = bucket_of(to, lower_bound(from, i));
        match out.index.last() {
            Some(&last) if last == j => *out.count.last_mut().expect("parallel") = c,
            _ => {
                out.index.push(j);
                out.count.push(c);
            }
        }
    }
    out
}

fn merge_streams(streams: Vec<HistogramStream>, clock: Option<usize>) -> Option<HistogramStream> {
    if streams.is_empty() {
        return None;
    }
    if streams.len() == 1 {
        return streams.into_iter().next();
    }
    let config = common_config(streams.iter().map(|s| s.meta.config))?;

    // Build unified series list; record per-stream remapping tables.
    let mut global_series: Vec<Labels> = Vec::new();
    let mut index: std::collections::HashMap<Labels, usize> = std::collections::HashMap::new();
    // remap[stream_idx][local_series_idx] = global_series_idx
    let mut remap: Vec<Vec<usize>> = Vec::with_capacity(streams.len());
    for stream in &streams {
        let mut local_remap = Vec::with_capacity(stream.meta.series.len());
        for labels in &stream.meta.series {
            let global_idx = *index.entry(labels.clone()).or_insert_with(|| {
                global_series.push(labels.clone());
                global_series.len() - 1
            });
            local_remap.push(global_idx);
        }
        remap.push(local_remap);
    }

    // Wrap each stream's iterator to remap series_idx to the global index
    // and its buckets to the common configuration.
    let mut iters: Vec<Box<dyn Iterator<Item = HistogramRow> + Send>> = streams
        .into_iter()
        .enumerate()
        .map(|(si, stream)| {
            let local_remap = remap[si].clone();
            let from = stream.meta.config;
            let remapped: Box<dyn Iterator<Item = HistogramRow> + Send> =
                Box::new(stream.rows.map(move |mut row| {
                    row.series_idx = local_remap[row.series_idx];
                    if from != config {
                        row.snapshot = rebucket(row.snapshot, &from, &config);
                    }
                    row
                }));
            remapped
        })
        .collect();

    // Prime the heap with the first row from each iterator. Heap is a
    // min-heap keyed by (timestamp, clock row last, series_idx): a row of
    // another stream at a clock row's time belongs to that clock row's
    // interval, so it pops first.
    struct Entry {
        timestamp: u64,
        is_clock: bool,
        series_idx: usize,
        iter_idx: usize,
        row: HistogramRow,
    }
    impl Entry {
        fn key(&self) -> (u64, bool, usize) {
            (self.timestamp, self.is_clock, self.series_idx)
        }
    }
    impl Eq for Entry {}
    impl PartialEq for Entry {
        fn eq(&self, other: &Self) -> bool {
            self.key() == other.key()
        }
    }
    impl Ord for Entry {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            // Reverse so BinaryHeap becomes a min-heap.
            other.key().cmp(&self.key())
        }
    }
    impl PartialOrd for Entry {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    let mut heap: BinaryHeap<Entry> = BinaryHeap::new();
    for (iter_idx, iter) in iters.iter_mut().enumerate() {
        if let Some(row) = iter.next() {
            heap.push(Entry {
                timestamp: row.timestamp,
                is_clock: clock == Some(iter_idx),
                series_idx: row.series_idx,
                iter_idx,
                row,
            });
        }
    }
    // The clock's next row time: its entry in the heap, while it has one.
    let clock_head = move |heap: &BinaryHeap<Entry>| -> Option<u64> {
        let clock = clock?;
        heap.iter()
            .filter(|e| e.iter_idx == clock)
            .map(|e| e.timestamp)
            .min()
    };
    // The clock's last row popped, and the gap before it.
    let mut last_clock: Option<u64> = None;
    let mut clock_gap: Option<u64> = None;

    let merged: Box<dyn Iterator<Item = HistogramRow> + Send> =
        Box::new(std::iter::from_fn(move || loop {
            let entry = heap.pop()?;
            // Advance that iterator and push its next row.
            // iter_idx is always valid: set at heap-priming time, iters is never resized.
            if let Some(next_row) = iters[entry.iter_idx].next() {
                heap.push(Entry {
                    timestamp: next_row.timestamp,
                    is_clock: entry.is_clock,
                    series_idx: next_row.series_idx,
                    iter_idx: entry.iter_idx,
                    row: next_row,
                });
            }
            let mut row = entry.row;
            if entry.is_clock {
                // A clock of several series has a row of each at one time.
                if last_clock != Some(row.timestamp) {
                    clock_gap = last_clock.map(|l| row.timestamp.saturating_sub(l));
                    last_clock = Some(row.timestamp);
                }
            } else if let Some(last) = last_clock {
                match clock_head(&heap) {
                    Some(head) => {
                        // Within the clock's span, and not across a hole in
                        // it (an interval more than twice the one before):
                        // there the clock stream read nothing, and the row
                        // keeps its own time.
                        let gap = head.saturating_sub(last);
                        if clock_gap.is_none_or(|before| gap <= before.saturating_mul(2)) {
                            row.timestamp = row.timestamp.max(head);
                        }
                    }
                    // In the interval after the clock's last row: its next
                    // row is past the read (or it ended), so the row has no
                    // tick to join. Pooled at its own time it would be a tick
                    // of the other streams alone; it is left out. Further
                    // on, the clock stream has ended and the row keeps its
                    // own time.
                    None => {
                        if clock_gap.is_some_and(|gap| row.timestamp.saturating_sub(last) <= gap) {
                            continue;
                        }
                    }
                }
            }
            return Some(row);
        }));

    Some(HistogramStream {
        meta: HistogramStreamMeta {
            config,
            series: global_series,
        },
        rows: merged,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bucket the `histogram` crate puts `value` in under `(g, m)`.
    fn crate_index(g: u8, m: u8, value: u64) -> u32 {
        let mut h = ::histogram::Histogram::new(g, m).unwrap();
        h.increment(value).unwrap();
        ::histogram::SparseHistogram::from(&h).index()[0]
    }

    fn values() -> impl Iterator<Item = u64> {
        (0..64u64)
            .chain((6..20).flat_map(|p| [(1u64 << p) - 1, 1 << p, (1 << p) + 3, (3 << p) / 2]))
            .chain((0..200u64).map(|k| k * 5237 % (1 << 20)))
    }

    /// Rebucketing a value's bucket to a coarser configuration lands it
    /// where the `histogram` crate would have put the value.
    #[test]
    fn a_bucket_moves_to_the_coarser_bucket_holding_its_values() {
        let from = ::histogram::Config::new(7, 20).unwrap();
        for to in [(3, 20), (3, 24), (7, 24), (0, 21)] {
            let to_config = ::histogram::Config::new(to.0, to.1).unwrap();
            for v in values() {
                let snapshot = HistogramSnapshot {
                    index: vec![crate_index(7, 20, v)],
                    count: vec![1],
                };
                let got = rebucket(snapshot, &from, &to_config);
                assert_eq!(got.index, vec![crate_index(to.0, to.1, v)], "{v} to {to:?}");
            }
        }
    }

    /// Buckets that land in one coarser bucket hold the observations of all
    /// of them, once: the counts are cumulative over the buckets, so the
    /// merged bucket's count is the last one's, not their sum.
    #[test]
    fn buckets_that_merge_count_their_observations_once() {
        let from = ::histogram::Config::new(2, 16).unwrap();
        let to = ::histogram::Config::new(0, 16).unwrap();
        // 3 observations of 5 and 1 of 6, in two buckets at grouping power
        // 2 and one ([4, 7]) at 0; then 2 of 9, in [8, 15] at both.
        let snapshot = HistogramSnapshot {
            index: vec![
                crate_index(2, 16, 5),
                crate_index(2, 16, 6),
                crate_index(2, 16, 9),
            ],
            count: vec![3, 4, 6],
        };
        let got = rebucket(snapshot, &from, &to);
        assert_eq!(got.index, [crate_index(0, 16, 5), crate_index(0, 16, 9)]);
        assert_eq!(got.count, [4, 6]);
    }

    #[test]
    fn the_common_configuration_is_the_coarsest_and_widest() {
        let c = common_config([
            ::histogram::Config::new(7, 20).unwrap(),
            ::histogram::Config::new(3, 16).unwrap(),
        ])
        .unwrap();
        assert_eq!((c.grouping_power(), c.max_value_power()), (3, 20));
    }

    fn stream(rows: Vec<(u64, u64)>, label: &str, config: (u8, u8)) -> HistogramStream {
        let config = ::histogram::Config::new(config.0, config.1).unwrap();
        HistogramStream {
            meta: HistogramStreamMeta {
                config,
                series: vec![Labels::from([("source", label)])],
            },
            rows: Box::new(rows.into_iter().map(|(t, c)| HistogramRow {
                series_idx: 0,
                timestamp: t,
                snapshot: HistogramSnapshot {
                    index: vec![1],
                    count: vec![c],
                },
            })),
        }
    }

    /// A clock of two series has two rows at each time; the second is the
    /// same clock row, not an interval of zero after the first.
    #[test]
    fn a_clock_of_several_series_restamps_as_one() {
        let rows: Vec<(u64, u64)> = (10..=50).step_by(10).map(|t| (t, t)).collect();
        let mut clock = stream(rows.clone(), "slow", (4, 16));
        clock.meta.series.push(Labels::from([("source", "slow2")]));
        let both: Vec<HistogramRow> = rows
            .iter()
            .flat_map(|&(t, c)| {
                (0..2).map(move |series_idx| HistogramRow {
                    series_idx,
                    timestamp: t,
                    snapshot: HistogramSnapshot {
                        index: vec![1],
                        count: vec![c],
                    },
                })
            })
            .collect();
        clock.rows = Box::new(both.into_iter());
        let fast = stream(
            (5..=50).step_by(5).map(|t| (t, t)).collect(),
            "fast",
            (4, 16),
        );
        let merged = HistogramStream::merge_on_clock(vec![clock, fast], 0).unwrap();
        let fast_times: Vec<u64> = merged
            .rows
            .filter(|r| r.series_idx == 2)
            .map(|r| r.timestamp)
            .collect();
        assert_eq!(fast_times, vec![5, 10, 20, 20, 30, 30, 40, 40, 50, 50]);
    }

    /// Rows of another stream take the time of the clock's next row, so a
    /// tick of the merged stream holds every stream's rows since the
    /// previous clock row. Before the clock's first row and inside a hole
    /// in it, they keep their own times.
    #[test]
    fn rows_are_restamped_onto_the_clock() {
        // The clock reads at 10, 20, 30, then not until 70 (a hole), 80.
        let clock = stream(
            [10, 20, 30, 70, 80].iter().map(|&t| (t, t)).collect(),
            "slow",
            (4, 16),
        );
        let fast = stream(
            (5..=100).step_by(5).map(|t| (t, t)).collect(),
            "fast",
            (4, 16),
        );
        let merged = HistogramStream::merge_on_clock(vec![clock, fast], 0).unwrap();
        let rows: Vec<(u64, usize)> = merged.rows.map(|r| (r.timestamp, r.series_idx)).collect();
        let fast_times: Vec<u64> = rows.iter().filter(|r| r.1 == 1).map(|r| r.0).collect();
        assert_eq!(
            fast_times,
            vec![
                5, 10, // before the clock's first row, and at it
                20, 20, // 15, 20
                30, 30, // 25, 30
                35, 40, 45, 50, 55, 60, 65, 70, // the hole: 30 to 70 is 4x the gap before
                80, 80, // 75, 80
                // 85 and 90 are within the interval after the clock's last
                // row (10): left out. 95 on, the clock has ended.
                95, 100,
            ],
            "{rows:?}"
        );
        assert!(rows.windows(2).all(|w| w[0].0 <= w[1].0), "{rows:?}");
    }

    /// Streams of different bucket configurations are merged under the
    /// coarsest rather than left out.
    #[test]
    fn streams_of_different_configurations_are_merged() {
        let a = stream(vec![(1, 1)], "a", (7, 20));
        let b = stream(vec![(1, 1)], "b", (3, 20));
        let merged = HistogramStream::merge(vec![a, b]).unwrap();
        assert_eq!(merged.meta.config.grouping_power(), 3);
        assert_eq!(merged.rows.count(), 2);
    }
}
