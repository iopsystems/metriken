//! Prototype: `sum(irate(<counter>))` over a long table computed in one pass
//! over decoded columns, against the engine. Ignored; needs an archive:
//! `BATCH_RATE_ARCHIVE=long.dendro cargo test --release -p metriken-archive
//! --test batch_rate -- --ignored --nocapture`. `BATCH_RATE_TABLE` and
//! `BATCH_RATE_METRIC` name the table and counter (defaults
//! `cpu_usage/cpu_usage_task`, `task_cpu_usage`); `BATCH_RATE_STEP` is the
//! step in seconds (default 1).

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use arrow::array::{Array, Int64Array, UInt64Array};
use metriken_archive::{ArchiveReader, Catalog, DendroCatalog};
use metriken_query::{BufferPool, MetricsSource, QueryResult};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

const SPACING_PROBE: usize = 9;

#[derive(Clone, Copy)]
struct Sample {
    ts: u64,
    value: u64,
    window: (u64, u64),
}

#[derive(Clone, Copy)]
struct Seen {
    ts: u64,
    cum: f64,
    window: (u64, u64),
}

/// What an edge evaluated to: the interpolated cumulative and, when the
/// edge is described by real reads, its window.
#[derive(Clone, Copy)]
struct EdgeValue {
    cum: f64,
    window: Option<(f64, f64)>,
}

#[derive(Default)]
struct Series {
    probe: Vec<Sample>,
    typical: Option<u64>,
    pulled: usize,
    prev_value: Option<u64>,
    acc: f64,
    first_ts: Option<u64>,
    last: Option<Seen>,
    next_edge: usize,
    /// The value at edge `next_edge - 1`, when it was observed.
    prev: Option<EdgeValue>,
}

/// Per grid point, what `sum` accumulates.
struct Sums {
    v: Vec<f64>,
    lo: Vec<f64>,
    hi: Vec<f64>,
    count: Vec<u32>,
    bounded: Vec<bool>,
    interpolated: Vec<bool>,
}

struct Grid {
    /// Edge `j` is `first - step + j * step`; point `k` (k >= 1) is at edge
    /// `k` with its left edge at `k - 1`.
    first: u64,
    step: u64,
    edges: usize,
}

impl Grid {
    fn edge(&self, j: usize) -> u64 {
        self.first - self.step + j as u64 * self.step
    }
}

impl Series {
    fn push(&mut self, s: Sample, grid: &Grid, sums: &mut Sums) {
        if self.typical.is_none() {
            self.probe.push(s);
            if self.probe.len() == SPACING_PROBE {
                self.settle(grid, sums);
            }
            return;
        }
        self.process(s, grid, sums);
    }

    /// Fix the typical spacing from the probe and run its samples.
    fn settle(&mut self, grid: &Grid, sums: &mut Sums) {
        if self.typical.is_some() {
            return;
        }
        let mut gaps: Vec<u64> = self.probe.windows(2).map(|w| w[1].ts - w[0].ts).collect();
        gaps.sort_unstable();
        self.typical = Some(gaps.get(gaps.len() / 2).copied().unwrap_or(1).max(1));
        if self.probe.len() < 2 {
            self.probe.clear();
            return;
        }
        for s in std::mem::take(&mut self.probe) {
            self.process(s, grid, sums);
        }
    }

    fn process(&mut self, s: Sample, grid: &Grid, sums: &mut Sums) {
        if let Some(prev) = self.prev_value {
            self.acc += if s.value >= prev {
                (s.value - prev) as f64
            } else {
                s.value as f64
            };
        }
        self.prev_value = Some(s.value);
        let first = *self.first_ts.get_or_insert(s.ts);
        self.pulled += 1;
        let cur = Seen {
            ts: s.ts,
            cum: self.acc,
            window: s.window,
        };
        let typical = self.typical.unwrap_or(1);
        while self.next_edge < grid.edges && grid.edge(self.next_edge) <= s.ts {
            let e = grid.edge(self.next_edge);
            let value = if e < first {
                None
            } else if e == s.ts {
                Some(EdgeValue {
                    cum: cur.cum,
                    window: Some((cur.window.0 as f64, cur.window.1 as f64)),
                })
            } else if let Some(lo) = self.last {
                let frac = (e - lo.ts) as f64 / (s.ts - lo.ts) as f64;
                let cum = lo.cum + frac * (cur.cum - lo.cum);
                let gap = s.ts - lo.ts;
                let window = if self.pulled >= 2 && gap > typical.saturating_mul(2) {
                    None
                } else {
                    let b = lo.window.0 as f64 + frac * (cur.window.0 as f64 - lo.window.0 as f64);
                    let en = lo.window.1 as f64 + frac * (cur.window.1 as f64 - lo.window.1 as f64);
                    Some((b, en))
                };
                Some(EdgeValue { cum, window })
            } else {
                None
            };
            if let (Some(left), Some(right)) = (self.prev, value) {
                let k = self.next_edge;
                let step_s = grid.step as f64 / 1e9;
                let increase = right.cum - left.cum;
                let v = increase / step_s;
                let pair = left.window.zip(right.window);
                let interpolated = pair.is_none();
                let bounds = pair
                    .and_then(|((b_left, e_left), (b_hi, e_hi))| {
                        let elapsed_max = (e_hi - b_left) / 1e9;
                        let elapsed_min = (b_hi - e_left) / 1e9;
                        (elapsed_min > 0.0 && elapsed_max > 0.0)
                            .then(|| (increase / elapsed_max, increase / elapsed_min))
                    })
                    .map(|(lo, hi)| (lo.min(v), hi.max(v)));
                let (lo, hi) = bounds.unwrap_or((v, v));
                sums.v[k] += v;
                sums.lo[k] += lo;
                sums.hi[k] += hi;
                sums.count[k] += 1;
                sums.bounded[k] |= bounds.is_some();
                sums.interpolated[k] |= interpolated;
            }
            self.prev = value;
            self.next_edge += 1;
        }
        self.last = Some(cur);
    }
}

#[test]
#[ignore]
fn batch_rate() {
    let Ok(path) = std::env::var("BATCH_RATE_ARCHIVE") else {
        return;
    };
    let table = std::env::var("BATCH_RATE_TABLE")
        .unwrap_or_else(|_| "cpu_usage/cpu_usage_task".to_string());
    let metric = std::env::var("BATCH_RATE_METRIC").unwrap_or_else(|_| "task_cpu_usage".to_string());
    let step_s: f64 = std::env::var("BATCH_RATE_STEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    let query = format!("sum(irate({metric}[5s]))");

    // The engine, as the reference.
    let pool = BufferPool::new(256 << 20);
    let reader = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(std::path::Path::new(&path)).unwrap()),
        None,
        pool,
        None,
    )
    .unwrap()
    .remove(0)
    .1;
    let (lo, hi) = reader.time_range().unwrap();
    let t = Instant::now();
    let QueryResult::Matrix { result } = reader.query_range(&query, lo, hi, step_s).unwrap() else {
        panic!("not a matrix");
    };
    let engine_took = t.elapsed();
    let engine = &result[0];

    // The batch path, timed from opening the catalog.
    let t = Instant::now();
    let catalog = DendroCatalog::open(std::path::Path::new(&path)).unwrap();
    let source = catalog.sources().unwrap()[0].id;
    // Series identity is the occupant's labels: two occupant numbers with
    // the same labels are one series, as in the engine.
    let mut series_of: HashMap<u64, usize> = HashMap::new();
    let mut by_labels: BTreeMap<BTreeMap<String, String>, usize> = BTreeMap::new();
    let occupants = format!("{table}/occupants");
    for (seq, _) in catalog.segment_meta(source, &occupants).unwrap() {
        let bytes = catalog.segment_bytes(source, &occupants, seq).unwrap().unwrap();
        for (_, o) in metriken_segment::occupants::decode_segment(&bytes).unwrap() {
            let n = by_labels.len();
            let idx = *by_labels.entry(o.labels).or_insert(n);
            series_of.entry(o.occupant).or_insert(idx);
        }
    }
    let step = (step_s * 1e9) as u64;
    let first = engine.values[0].0;
    let first_ns = (first * 1e9).round() as u64;
    let last_ns = (engine.values.last().unwrap().0 * 1e9).round() as u64;
    let grid = Grid {
        first: first_ns,
        step,
        edges: ((last_ns - first_ns) / step) as usize + 2,
    };
    let mut sums = Sums {
        v: vec![0.0; grid.edges],
        lo: vec![0.0; grid.edges],
        hi: vec![0.0; grid.edges],
        count: vec![0; grid.edges],
        bounded: vec![false; grid.edges],
        interpolated: vec![false; grid.edges],
    };
    let max_occ = series_of.keys().copied().max().unwrap_or(0) as usize;
    let mut slot: Vec<u32> = vec![u32::MAX; max_occ + 1];
    for (o, s) in &series_of {
        slot[*o as usize] = *s as u32;
    }
    let mut series: Vec<Series> = (0..by_labels.len()).map(|_| Series::default()).collect();
    let mut decode_only = std::time::Duration::ZERO;
    let mut rows = 0usize;
    // Decode every segment in parallel, then run the series in order.
    let blobs: Vec<Vec<u8>> = catalog
        .segment_meta(source, &table)
        .unwrap()
        .into_iter()
        .map(|(seq, _)| catalog.segment_bytes(source, &table, seq).unwrap().unwrap())
        .collect();
    let td = Instant::now();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let decoded: Vec<std::sync::Mutex<Vec<arrow::record_batch::RecordBatch>>> =
        blobs.iter().map(|_| std::sync::Mutex::new(Vec::new())).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(bytes) = blobs.get(i) else { break };
                let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes.clone()))
                    .unwrap()
                    .with_batch_size(64 * 1024)
                    .build()
                    .unwrap();
                *decoded[i].lock().unwrap() = reader.map(|b| b.unwrap()).collect();
            });
        }
    });
    decode_only += td.elapsed();
    for batches in decoded {
        let batches = batches.into_inner().unwrap();
        for batch in batches {
            let col = |name: &str| batch.column(batch.schema().index_of(name).unwrap()).clone();
            let ts = col("timestamp");
            let ts = ts.as_any().downcast_ref::<UInt64Array>().unwrap();
            let occ = col("occupant");
            let occ = occ.as_any().downcast_ref::<UInt64Array>().unwrap();
            let val = col(&metric);
            let val = val.as_any().downcast_ref::<UInt64Array>().unwrap();
            let begin = col(":window_begin");
            let begin = begin.as_any().downcast_ref::<Int64Array>().unwrap();
            let width = col(":window_width");
            let width = width.as_any().downcast_ref::<UInt64Array>().unwrap();
            for r in 0..batch.num_rows() {
                if occ.is_null(r) || ts.is_null(r) || val.is_null(r) {
                    continue;
                }
                let base = ts.value(r);
                let window = if begin.is_null(r) || width.is_null(r) {
                    (base, base)
                } else {
                    let b = (base as i64).saturating_add(begin.value(r)).max(0) as u64;
                    (b, b.saturating_add(width.value(r)))
                };
                let Some(&s) = slot.get(occ.value(r) as usize) else {
                    continue;
                };
                if s == u32::MAX {
                    continue;
                }
                let s = s as usize;
                series[s].push(
                    Sample {
                        ts: base,
                        value: val.value(r),
                        window,
                    },
                    &grid,
                    &mut sums,
                );
                rows += 1;
            }
        }
    }
    for s in &mut series {
        s.settle(&grid, &mut sums);
    }
    let batch_took = t.elapsed();

    // Compare point by point.
    let (mut checked, mut bad) = (0usize, 0usize);
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0);
    for (i, (t, v)) in engine.values.iter().enumerate() {
        let k = (((t * 1e9).round() as u64 - first_ns) / step) as usize + 1;
        checked += 1;
        let ours_v = sums.v[k];
        let interp = sums.interpolated[k];
        let ours_b = (sums.bounded[k] && !interp).then(|| (sums.lo[k], sums.hi[k]));
        let theirs_b = engine.bands.as_ref().and_then(|b| b[i]);
        let theirs_i = engine
            .interpolated
            .as_ref()
            .is_some_and(|f| f[i]);
        let ok = sums.count[k] > 0
            && close(ours_v, *v)
            && interp == theirs_i
            && match (ours_b, theirs_b) {
                (Some((a, b)), Some((c, d))) => close(a, c) && close(b, d),
                (None, None) => true,
                _ => false,
            };
        if !ok {
            if bad < 5 {
                println!(
                    "BATCH mismatch at {t}: ours {ours_v} {ours_b:?} interp {interp}, engine {v} {theirs_b:?} interp {theirs_i}"
                );
            }
            bad += 1;
        }
    }
    let extra = (1..grid.edges)
        .filter(|k| sums.count[*k] > 0)
        .count()
        .saturating_sub(checked);
    println!(
        "BATCH {query} step {step_s}s: engine {engine_took:?}, batch {batch_took:?} (decode {decode_only:?}; {rows} rows, {} series); {checked} points, {bad} differ, {extra} extra",
        series.len()
    );
}
