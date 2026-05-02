//! Generate the canonical Parquet fixture files. Run from the workspace root:
//!
//!     cargo run -p metriken-query-fixtures --bin generate
//!
//! Output goes to `metriken-query-fixtures/fixtures/`. Files are committed,
//! so re-running should be a no-op on a clean tree (the builder is
//! deterministic by construction).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use metriken_query_fixtures::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Default: write next to this crate's manifest.
            Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
        });
    std::fs::create_dir_all(&out_dir)?;
    println!("writing fixtures to {}", out_dir.display());

    counter_basic(&out_dir)?;
    counter_reset(&out_dir)?;
    counter_multi_label(&out_dir)?;
    counter_near_overflow(&out_dir)?;
    gauge_basic(&out_dir)?;
    gauge_multi_source(&out_dir)?;
    histogram_basic(&out_dir)?;
    histogram_reset(&out_dir)?;
    histogram_empty_period(&out_dir)?;
    sampling_interval_500ms(&out_dir)?;
    rezolus_minimal(&out_dir)?;
    softirq_multi_kind(&out_dir)?;

    println!("done");
    Ok(())
}

/// One counter, single label set, strictly monotonic at 1Hz over 10 seconds.
/// Baseline `irate`/`rate` correctness target.
fn counter_basic(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let points: Vec<(u64, u64)> = (0..=10).map(|s| (ts_secs(s), s * 100)).collect();
    FixtureBuilder::new()
        .file_metadata("source", "fixture-counter-basic")
        .add_counter(CounterSeries {
            column_name: "requests".into(),
            metric: "requests".into(),
            labels: BTreeMap::new(),
            points,
        })
        .write(&dir.join("counter_basic.parquet"))
}

/// Counter that decreases mid-series. Mirrors the existing reset test at
/// `metriken-query/src/promql/tests.rs:406-431` — values [100, 200, 300, 50,
/// 150] at t=1000..1004s. PromQL semantics: pair (300→50) contributes 50,
/// pair (50→150) contributes 100.
fn counter_reset(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let points = vec![
        (ts_secs(1000), 100),
        (ts_secs(1001), 200),
        (ts_secs(1002), 300),
        (ts_secs(1003), 50),
        (ts_secs(1004), 150),
    ];
    FixtureBuilder::new()
        .file_metadata("source", "fixture-counter-reset")
        .add_counter(CounterSeries {
            column_name: "requests".into(),
            metric: "requests".into(),
            labels: BTreeMap::new(),
            points,
        })
        .write(&dir.join("counter_reset.parquet"))
}

/// Same metric name, four label permutations. Exercises `sum by (id)` /
/// `without (id)` / matchers like `{state="user"}`.
fn counter_multi_label(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = FixtureBuilder::new().file_metadata("source", "fixture-counter-multi-label");
    let mut col = 0u32;
    for id in ["0", "1"] {
        for state in ["user", "system"] {
            // Each (id, state) accumulates at a slightly different rate so
            // sum-by aggregations have non-trivial expected values.
            let rate: u64 = match (id, state) {
                ("0", "user") => 100,
                ("0", "system") => 30,
                ("1", "user") => 80,
                ("1", "system") => 20,
                _ => 0,
            };
            let points: Vec<(u64, u64)> = (0..=10).map(|s| (ts_secs(s), s * rate)).collect();
            builder = builder.add_counter(CounterSeries {
                column_name: format!("cpu_usage__{col}"),
                metric: "cpu_usage".into(),
                labels: labels(&[("id", id), ("state", state)]),
                points,
            });
            col += 1;
        }
    }
    builder.write(&dir.join("counter_multi_label.parquet"))
}

/// Gauge metric with multiple `source` label values per metric, mirroring
/// the shape Rezolus's service-extension parquets use (`cachecannon`,
/// `valkey`, `vllm`, …). Drives the Group A `gauge_bare_with_labels`
/// catalogue entry, which selects a single source out of a multi-series
/// metric via a `WHERE source = '...'` predicate.
fn gauge_multi_source(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = FixtureBuilder::new().file_metadata("source", "fixture-gauge-multi-source");
    let mut col = 0u32;
    // `target_rate` (gauge) per source × instance — three series so the
    // source-only filter still keeps multiple series and the source+instance
    // filter narrows to one. Mirrors cachecannon/valkey shape where the same
    // metric name is exposed by every service capture.
    for (source, instance, value) in [
        ("cachecannon", "0", 100i64),
        ("cachecannon", "1", 110),
        ("valkey", "0", 200),
    ] {
        let points: Vec<(u64, i64)> =
            (0..=10).map(|s| (ts_secs(s), value + (s as i64))).collect();
        builder = builder.add_gauge(GaugeSeries {
            column_name: format!("target_rate__{col}"),
            metric: "target_rate".into(),
            labels: labels(&[("source", source), ("instance", instance)]),
            points,
        });
        col += 1;
    }
    builder.write(&dir.join("gauge_multi_source.parquet"))
}

/// Counter increments by 1 per second starting near u64::MAX-5, so the series
/// crosses the boundary. Exercises width-of-arithmetic on subtraction.
fn counter_near_overflow(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let base: u64 = u64::MAX - 5;
    let points: Vec<(u64, u64)> = (0..=4).map(|s| (ts_secs(s), base + s)).collect();
    FixtureBuilder::new()
        .file_metadata("source", "fixture-counter-near-overflow")
        .add_counter(CounterSeries {
            column_name: "saturating_counter".into(),
            metric: "saturating_counter".into(),
            labels: BTreeMap::new(),
            points,
        })
        .write(&dir.join("counter_near_overflow.parquet"))
}

/// One gauge with a steady value, one with a sawtooth, one with negatives.
/// Plus a `memory_available` companion to `memory_total` so the
/// `memory_util_pct` entry can pair them in a JOIN.
fn gauge_basic(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let steady: Vec<(u64, i64)> = (0..=10).map(|s| (ts_secs(s), 1024)).collect();
    let sawtooth: Vec<(u64, i64)> = (0..=10)
        .map(|s| (ts_secs(s), ((s as i64) % 4) * 10))
        .collect();
    let negative: Vec<(u64, i64)> = (0..=10).map(|s| (ts_secs(s), (s as i64) - 5)).collect();
    // memory_available drifts down then up — non-trivial used-memory % so
    // the golden snapshot is informative, not all-zeros.
    let memory_avail: Vec<(u64, i64)> =
        (0..=10).map(|s| (ts_secs(s), 1024 - (s as i64) * 32)).collect();

    FixtureBuilder::new()
        .file_metadata("source", "fixture-gauge-basic")
        .add_gauge(GaugeSeries {
            column_name: "memory_total".into(),
            metric: "memory_total".into(),
            labels: BTreeMap::new(),
            points: steady,
        })
        .add_gauge(GaugeSeries {
            column_name: "memory_available".into(),
            metric: "memory_available".into(),
            labels: BTreeMap::new(),
            points: memory_avail,
        })
        .add_gauge(GaugeSeries {
            column_name: "queue_depth".into(),
            metric: "queue_depth".into(),
            labels: BTreeMap::new(),
            points: sawtooth,
        })
        .add_gauge(GaugeSeries {
            column_name: "temperature_offset".into(),
            metric: "temperature_offset".into(),
            labels: BTreeMap::new(),
            points: negative,
        })
        .write(&dir.join("gauge_basic.parquet"))
}

/// Histogram with cumulative event counts increasing every second. The first
/// snapshot is a primer (no prev to difference against), so callers see 10
/// usable delta snapshots from this 11-point series. Uses the standard
/// rezolus (gp=4, mvp=16) configuration.
fn histogram_basic(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let gp = REZOLUS_HISTOGRAM_GP;
    let mvp = REZOLUS_HISTOGRAM_MVP;

    let mut points = Vec::new();
    let mut cumu = empty_buckets(gp, mvp);
    points.push((ts_secs(0), Some(cumu.clone())));
    for s in 1..=10 {
        // Each second adds two events at fixed values. 100ns is in an early
        // bucket; 10000ns is in a much later bucket.
        cumu = add_events(&cumu, gp, mvp, &[(100, 1), (10_000, 1)])?;
        points.push((ts_secs(s), Some(cumu.clone())));
    }

    FixtureBuilder::new()
        .file_metadata("source", "fixture-histogram-basic")
        .add_histogram(HistogramSeries {
            column_name: "request_latency".into(),
            metric: "request_latency".into(),
            labels: BTreeMap::new(),
            grouping_power: gp,
            max_value_power: mvp,
            points,
        })
        .write(&dir.join("histogram_basic.parquet"))
}

/// Histogram cumulative counts decrease at one timestamp (e.g. agent restart
/// or counter wrap). The loader at `tsdb/series/histogram.rs:266-271` should
/// substitute an empty delta for that period.
fn histogram_reset(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let gp = REZOLUS_HISTOGRAM_GP;
    let mvp = REZOLUS_HISTOGRAM_MVP;

    // Build cumulative buckets: t=0 empty, then accumulate, then drop.
    let mut buckets_seq = Vec::new();
    let mut cumu = empty_buckets(gp, mvp);
    buckets_seq.push(cumu.clone());
    cumu = add_events(&cumu, gp, mvp, &[(100, 5)])?; // t=1: 5 events
    buckets_seq.push(cumu.clone());
    cumu = add_events(&cumu, gp, mvp, &[(100, 5)])?; // t=2: 10 events total
    buckets_seq.push(cumu.clone());
    // Reset: cumulative drops below previous.
    let reset = empty_buckets(gp, mvp);
    buckets_seq.push(reset.clone()); // t=3: reset to 0
    let mut after = reset;
    after = add_events(&after, gp, mvp, &[(100, 3)])?;
    buckets_seq.push(after); // t=4: 3 events post-reset

    let points: Vec<(u64, Option<Vec<u64>>)> = buckets_seq
        .into_iter()
        .enumerate()
        .map(|(i, b)| (ts_secs(i as u64), Some(b)))
        .collect();

    FixtureBuilder::new()
        .file_metadata("source", "fixture-histogram-reset")
        .add_histogram(HistogramSeries {
            column_name: "request_latency".into(),
            metric: "request_latency".into(),
            labels: BTreeMap::new(),
            grouping_power: gp,
            max_value_power: mvp,
            points,
        })
        .write(&dir.join("histogram_reset.parquet"))
}

/// Histogram with a quiet period: same cumulative value across two adjacent
/// timestamps, so the delta is the empty histogram. The loader keeps the
/// timestamp (per `tsdb/mod.rs:579-626`) so quantile/heatmap math sees a
/// genuine "no events this period" entry.
fn histogram_empty_period(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let gp = REZOLUS_HISTOGRAM_GP;
    let mvp = REZOLUS_HISTOGRAM_MVP;

    let mut points = Vec::new();
    let mut cumu = empty_buckets(gp, mvp);
    points.push((ts_secs(0), Some(cumu.clone())));
    cumu = add_events(&cumu, gp, mvp, &[(100, 1)])?;
    points.push((ts_secs(1), Some(cumu.clone())));
    // t=2 same as t=1 → empty delta
    points.push((ts_secs(2), Some(cumu.clone())));
    cumu = add_events(&cumu, gp, mvp, &[(100, 1)])?;
    points.push((ts_secs(3), Some(cumu.clone())));

    FixtureBuilder::new()
        .file_metadata("source", "fixture-histogram-empty-period")
        .add_histogram(HistogramSeries {
            column_name: "request_latency".into(),
            metric: "request_latency".into(),
            labels: BTreeMap::new(),
            grouping_power: gp,
            max_value_power: mvp,
            points,
        })
        .write(&dir.join("histogram_empty_period.parquet"))
}

/// File-level kv `sampling_interval_ms = 500`. Timestamps are at clean 0.5s
/// boundaries, so the loader's snap-to-interval logic at `tsdb/mod.rs:52-58`
/// is a no-op; this fixture exists so the harness can verify the kv is
/// honored end-to-end.
fn sampling_interval_500ms(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let points: Vec<(u64, u64)> = (0..=10).map(|s| (s * 500_000_000, s * 50)).collect();
    FixtureBuilder::new()
        .sampling_interval_ms(500)
        .file_metadata("source", "fixture-sampling-interval-500ms")
        .add_counter(CounterSeries {
            column_name: "requests".into(),
            metric: "requests".into(),
            labels: BTreeMap::new(),
            points,
        })
        .write(&dir.join("sampling_interval_500ms.parquet"))
}

/// Mini Rezolus-shaped fixture: cpu_usage (counter, two label dims),
/// memory_total (gauge), syscall_latency (histogram). Just enough surface
/// for the canonical Rezolus query templates to run without touching the
/// large checked-in Rezolus parquet files.
fn rezolus_minimal(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = FixtureBuilder::new().file_metadata("source", "fixture-rezolus-minimal");

    // cpu_usage{id, state} for two CPUs × {user, system}, monotonic.
    let mut col = 0u32;
    for id in ["0", "1"] {
        for state in ["user", "system"] {
            let rate: u64 = match state {
                "user" => 700_000_000,   // ns/s
                "system" => 100_000_000, // ns/s
                _ => 0,
            };
            let points: Vec<(u64, u64)> = (0..=20).map(|s| (ts_secs(s), s * rate)).collect();
            builder = builder.add_counter(CounterSeries {
                column_name: format!("cpu_usage__{col}"),
                metric: "cpu_usage".into(),
                labels: labels(&[("id", id), ("state", state)]),
                points,
            });
            col += 1;
        }
    }

    // Memory gauge with a slow drift.
    let mem_points: Vec<(u64, i64)> = (0..=20)
        .map(|s| (ts_secs(s), 1_073_741_824 + (s as i64) * 1024))
        .collect();
    builder = builder.add_gauge(GaugeSeries {
        column_name: "memory_total".into(),
        metric: "memory_total".into(),
        labels: BTreeMap::new(),
        points: mem_points,
    });

    // syscall_latency histogram, two events per second, fixed values.
    let gp = REZOLUS_HISTOGRAM_GP;
    let mvp = REZOLUS_HISTOGRAM_MVP;
    let mut hist_points = Vec::new();
    let mut cumu = empty_buckets(gp, mvp);
    hist_points.push((ts_secs(0), Some(cumu.clone())));
    for s in 1..=20 {
        cumu = add_events(&cumu, gp, mvp, &[(100, 1), (10_000, 1)])?;
        hist_points.push((ts_secs(s), Some(cumu.clone())));
    }
    builder = builder.add_histogram(HistogramSeries {
        column_name: "syscall_latency".into(),
        metric: "syscall_latency".into(),
        labels: BTreeMap::new(),
        grouping_power: gp,
        max_value_power: mvp,
        points: hist_points,
    });

    // BPF self-monitoring counters — one column per `sampler`, two samplers,
    // monotonic. Drives the Group J Rezolus self-monitoring catalogue
    // entries (rezolus_bpf_run_time + rezolus_bpf_run_count).
    let mut col = 0u32;
    for sampler in ["cpu", "syscall"] {
        let run_time_rate: u64 = 50_000_000; // ns/s of BPF time
        let run_count_rate: u64 = 1_000; // events/s
        let run_time: Vec<(u64, u64)> = (0..=20)
            .map(|s| (ts_secs(s), s * run_time_rate))
            .collect();
        let run_count: Vec<(u64, u64)> = (0..=20)
            .map(|s| (ts_secs(s), s * run_count_rate))
            .collect();
        builder = builder.add_counter(CounterSeries {
            column_name: format!("rezolus_bpf_run_time__{col}"),
            metric: "rezolus_bpf_run_time".into(),
            labels: labels(&[("sampler", sampler)]),
            points: run_time,
        });
        builder = builder.add_counter(CounterSeries {
            column_name: format!("rezolus_bpf_run_count__{col}"),
            metric: "rezolus_bpf_run_count".into(),
            labels: labels(&[("sampler", sampler)]),
            points: run_count,
        });
        col += 1;
    }

    // Per-CPU performance counters used by the IPC / IPNS dashboard
    // queries. Two CPUs × five counters (instructions, cycles, tsc, aperf,
    // mperf), all monotonic. cpu_aperf/cpu_mperf are equal so the
    // aperf/mperf ratio is exactly 1.0 — easy to reason about in goldens.
    let mut col = 0u32;
    let perf_rates: &[(&str, u64, u64)] = &[
        ("cpu_instructions", 6_000_000_000, 5_000_000_000), // CPU0 / CPU1
        ("cpu_cycles", 3_000_000_000, 2_500_000_000),
        ("cpu_tsc", 3_000_000_000, 3_000_000_000),
        ("cpu_aperf", 3_000_000_000, 3_000_000_000),
        ("cpu_mperf", 3_000_000_000, 3_000_000_000),
    ];
    for (metric, r0, r1) in perf_rates {
        for (id, rate) in ["0", "1"].iter().zip([*r0, *r1]) {
            let points: Vec<(u64, u64)> =
                (0..=20).map(|s| (ts_secs(s), s * rate)).collect();
            builder = builder.add_counter(CounterSeries {
                column_name: format!("{metric}__{col}"),
                metric: (*metric).to_string(),
                labels: labels(&[("id", id)]),
                points,
            });
            col += 1;
        }
    }
    // cpu_cores is a steady-value gauge (equal to the number of CPUs in the
    // fixture). Drives the trailing `/ cpu_cores` divisor in the IPNS query.
    let cores_points: Vec<(u64, i64)> = (0..=20).map(|s| (ts_secs(s), 2)).collect();
    builder = builder.add_gauge(GaugeSeries {
        column_name: "cpu_cores".into(),
        metric: "cpu_cores".into(),
        labels: BTreeMap::new(),
        points: cores_points,
    });

    builder.write(&dir.join("rezolus_minimal.parquet"))
}

/// Multi-label softirq counter, mirroring the Rezolus dashboard's softirq
/// shape: one metric (`softirq`) with labels `(id, kind)`. Three kinds × two
/// CPUs = six series, all monotonic at differentiated rates so per-`kind`
/// aggregations have non-trivial expected values. Drives the templated
/// catalogue entry `softirq_irate_total_by_kind`.
fn softirq_multi_kind(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = FixtureBuilder::new().file_metadata("source", "fixture-softirq-multi-kind");
    let mut col = 0u32;
    for id in ["0", "1"] {
        for kind in ["hi", "net_rx", "timer"] {
            // Per-(kind, id) accumulation rates keep each series distinguishable.
            let rate: u64 = match (kind, id) {
                ("hi", "0") => 50,
                ("hi", "1") => 30,
                ("net_rx", "0") => 1000,
                ("net_rx", "1") => 800,
                ("timer", "0") => 200,
                ("timer", "1") => 150,
                _ => 0,
            };
            let points: Vec<(u64, u64)> = (0..=10).map(|s| (ts_secs(s), s * rate)).collect();
            builder = builder.add_counter(CounterSeries {
                column_name: format!("softirq__{col}"),
                metric: "softirq".into(),
                labels: labels(&[("id", id), ("kind", kind)]),
                points,
            });
            col += 1;
        }
    }
    // softirq_time — ns-of-CPU-time-spent counter, same (id, kind) shape as
    // `softirq` but with much larger per-second deltas (ns scale). Drives the
    // time-percentage Group-F catalogue entries.
    let mut col = 0u32;
    for id in ["0", "1"] {
        for kind in ["hi", "net_rx", "timer"] {
            // ns/s of CPU time. Pick rates that don't all collapse to the
            // same fraction so the per-id and total entries are distinguishable.
            let rate_ns: u64 = match (kind, id) {
                ("hi", "0") => 200_000,
                ("hi", "1") => 100_000,
                ("net_rx", "0") => 5_000_000,
                ("net_rx", "1") => 4_000_000,
                ("timer", "0") => 1_000_000,
                ("timer", "1") => 800_000,
                _ => 0,
            };
            let points: Vec<(u64, u64)> = (0..=10).map(|s| (ts_secs(s), s * rate_ns)).collect();
            builder = builder.add_counter(CounterSeries {
                column_name: format!("softirq_time__{col}"),
                metric: "softirq_time".into(),
                labels: labels(&[("id", id), ("kind", kind)]),
                points,
            });
            col += 1;
        }
    }
    builder.write(&dir.join("softirq_multi_kind.parquet"))
}
