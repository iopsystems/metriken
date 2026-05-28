use std::collections::BinaryHeap;

use crate::labels::Labels;
use crate::types::HistogramSnapshot;

pub(crate) struct HistogramStreamMeta {
    pub config: ::histogram::Config,
    /// Labels for each series, indexed by `HistogramRow::series_idx`.
    pub series: Vec<Labels>,
}

pub(crate) struct HistogramRow {
    /// Index into `HistogramStreamMeta::series`.
    pub series_idx: usize,
    pub timestamp: u64,
    pub snapshot: HistogramSnapshot,
}

pub(crate) struct HistogramStream {
    pub meta: HistogramStreamMeta,
    /// Rows in ascending `(timestamp, series_idx)` order.
    pub rows: Box<dyn Iterator<Item = HistogramRow> + Send>,
}

impl HistogramStream {
    /// K-way merge of multiple streams for the same metric across different
    /// parquet files. Normalises `series_idx` values to a unified series list.
    /// Returns `None` if `streams` is empty.
    pub fn merge(streams: Vec<HistogramStream>) -> Option<HistogramStream> {
        if streams.is_empty() {
            return None;
        }
        if streams.len() == 1 {
            return streams.into_iter().next();
        }

        // Build unified series list; record per-stream remapping tables.
        let config = streams[0].meta.config;
        let mut global_series: Vec<Labels> = Vec::new();
        // remap[stream_idx][local_series_idx] = global_series_idx
        let mut remap: Vec<Vec<usize>> = Vec::with_capacity(streams.len());
        for stream in &streams {
            let mut local_remap = Vec::with_capacity(stream.meta.series.len());
            for labels in &stream.meta.series {
                let global_idx = global_series
                    .iter()
                    .position(|g| g == labels)
                    .unwrap_or_else(|| {
                        let idx = global_series.len();
                        global_series.push(labels.clone());
                        idx
                    });
                local_remap.push(global_idx);
            }
            remap.push(local_remap);
        }

        // Wrap each stream's iterator to remap series_idx to global index.
        let mut iters: Vec<Box<dyn Iterator<Item = HistogramRow> + Send>> = streams
            .into_iter()
            .enumerate()
            .map(|(si, stream)| {
                let local_remap = remap[si].clone();
                let remapped: Box<dyn Iterator<Item = HistogramRow> + Send> =
                    Box::new(stream.rows.map(move |mut row| {
                        row.series_idx = local_remap[row.series_idx];
                        row
                    }));
                remapped
            })
            .collect();

        // Prime the heap with the first row from each iterator.
        // Heap is a min-heap keyed by (timestamp, series_idx).
        struct Entry {
            timestamp: u64,
            series_idx: usize,
            iter_idx: usize,
            row: HistogramRow,
        }
        impl Eq for Entry {}
        impl PartialEq for Entry {
            fn eq(&self, other: &Self) -> bool {
                self.timestamp == other.timestamp && self.series_idx == other.series_idx
            }
        }
        impl Ord for Entry {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                // Reverse so BinaryHeap becomes a min-heap.
                other
                    .timestamp
                    .cmp(&self.timestamp)
                    .then(other.series_idx.cmp(&self.series_idx))
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
                    series_idx: row.series_idx,
                    iter_idx,
                    row,
                });
            }
        }

        let merged: Box<dyn Iterator<Item = HistogramRow> + Send> = Box::new(
            std::iter::from_fn(move || {
                let entry = heap.pop()?;
                // Advance that iterator and push its next row.
                if let Some(next_row) = iters[entry.iter_idx].next() {
                    heap.push(Entry {
                        timestamp: next_row.timestamp,
                        series_idx: next_row.series_idx,
                        iter_idx: entry.iter_idx,
                        row: next_row,
                    });
                }
                Some(entry.row)
            }),
        );

        Some(HistogramStream {
            meta: HistogramStreamMeta { config, series: global_series },
            rows: merged,
        })
    }
}
