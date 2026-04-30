//! Compare in-memory representation cost of the metriken-query TSDB before
//! and after the histogram delta + sorted-Vec optimizations.
//!
//! Usage:
//!
//!     cargo run --release --example memory_compare -- new <path-to-parquet>
//!     cargo run --release --example memory_compare -- old <path-to-parquet>
//!
//! Run each mode in its own process so that allocator caching from one form
//! doesn't pollute the RSS reading for the other.  A representative input
//! is the Rezolus `cachecannon.parquet` viewer sample:
//!
//!     curl -L -o /tmp/cachecannon.parquet \
//!       https://github.com/iopsystems/rezolus/raw/refs/heads/main/site/viewer/data/cachecannon.parquet
//!
//! Each run prints the parquet file size, the count of loaded series and the
//! resident-set delta (RSS after load minus RSS before load).  Subtracting
//! the `new` delta from the `old` delta gives the savings.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::time::Instant;

use arrow::array::{Int64Array, ListArray, UInt64Array};
use arrow::datatypes::DataType;
use bytes::Bytes;
use histogram::{CumulativeROHistogram, Histogram};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use metriken_query::tsdb::{Labels, Tsdb};

fn read_rss_kib() -> u64 {
    // /proc/self/statm: size resident shared text lib data dt (in pages)
    let s = match fs::read_to_string("/proc/self/statm") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 2 {
        return 0;
    }
    let pages: u64 = parts[1].parse().unwrap_or(0);
    pages * 4 // statm reports in pages; assume 4 KiB on Linux
}

fn fmt_bytes(b: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if b >= GIB {
        format!("{:.2} GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.2} MiB", b as f64 / MIB as f64)
    } else if b >= KIB {
        format!("{:.1} KiB", b as f64 / KIB as f64)
    } else {
        format!("{b} B")
    }
}

/// Stand-in for the pre-optimization TSDB representation: BTreeMap-backed
/// time-keyed storage with `u64` cumulative-since-start histograms.
#[derive(Default)]
struct OldTsdb {
    counters: HashMap<String, HashMap<Labels, BTreeMap<u64, u64>>>,
    gauges: HashMap<String, HashMap<Labels, BTreeMap<u64, i64>>>,
    histograms: HashMap<String, HashMap<Labels, BTreeMap<u64, CumulativeROHistogram>>>,
}

impl OldTsdb {
    fn series_count(&self) -> usize {
        let counters: usize = self.counters.values().map(|m| m.len()).sum();
        let gauges: usize = self.gauges.values().map(|m| m.len()).sum();
        let histograms: usize = self.histograms.values().map(|m| m.len()).sum();
        counters + gauges + histograms
    }
}

/// Re-implementation of the original `Tsdb::load_from_bytes` histogram path,
/// preserving the prior representation (cumulative-since-start `u64`
/// histograms keyed by `BTreeMap<u64, _>`).  Used solely by this example to
/// measure the memory delta against the new representation.
fn load_old(bytes: Bytes) -> OldTsdb {
    let mut data = OldTsdb::default();

    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes).expect("parquet reader");
    let reader = builder.build().expect("build reader");

    for batch in reader.into_iter().flatten() {
        let schema = batch.schema().clone();
        let mut timestamps: BTreeMap<usize, u64> = BTreeMap::new();

        for (id, field) in schema.fields().iter().enumerate() {
            if field.name() == "timestamp" {
                let column = batch.column(id);
                let values = column
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .expect("timestamp not u64");
                for (id, v) in values.iter().enumerate() {
                    if let Some(v) = v {
                        timestamps.insert(id, v);
                    }
                }
                break;
            }
        }

        for (id, field) in schema.fields().iter().enumerate() {
            if field.name() == "timestamp" {
                continue;
            }
            let mut meta = field.metadata().clone();
            let name = if let Some(n) = meta.get("metric").cloned() {
                n
            } else {
                let col_name = field.name();
                col_name
                    .strip_suffix(":buckets")
                    .unwrap_or(col_name)
                    .to_string()
            };
            let grouping_power: Option<u8> =
                meta.remove("grouping_power").and_then(|v| v.parse().ok());
            let max_value_power: Option<u8> =
                meta.remove("max_value_power").and_then(|v| v.parse().ok());

            let mut labels = Labels::default();
            for (k, v) in meta.iter() {
                match k.as_str() {
                    "metric" | "metric_type" | "unit" => continue,
                    _ => {
                        labels.inner.insert(k.to_string(), v.to_string());
                    }
                }
            }

            let column = batch.column(id);
            match column.data_type() {
                DataType::UInt64 => {
                    let series = data
                        .counters
                        .entry(name)
                        .or_default()
                        .entry(labels)
                        .or_default();
                    let values = column.as_any().downcast_ref::<UInt64Array>().unwrap();
                    for (i, v) in values.iter().enumerate() {
                        if let (Some(v), Some(ts)) = (v, timestamps.get(&i)) {
                            series.insert(*ts, v);
                        }
                    }
                }
                DataType::Int64 => {
                    let series = data
                        .gauges
                        .entry(name)
                        .or_default()
                        .entry(labels)
                        .or_default();
                    let values = column.as_any().downcast_ref::<Int64Array>().unwrap();
                    for (i, v) in values.iter().enumerate() {
                        if let (Some(v), Some(ts)) = (v, timestamps.get(&i)) {
                            series.insert(*ts, v);
                        }
                    }
                }
                DataType::List(field_type) => {
                    if field_type.data_type() != &DataType::UInt64 {
                        continue;
                    }
                    let (Some(gp), Some(mvp)) = (grouping_power, max_value_power) else {
                        continue;
                    };
                    let series = data
                        .histograms
                        .entry(name)
                        .or_default()
                        .entry(labels)
                        .or_default();
                    let list_array = column.as_any().downcast_ref::<ListArray>().unwrap();
                    for (i, value) in list_array.iter().enumerate() {
                        if let (Some(list_value), Some(ts)) = (value, timestamps.get(&i)) {
                            let arr = list_value.as_any().downcast_ref::<UInt64Array>().unwrap();
                            let buckets: Vec<u64> = arr.iter().flatten().collect();
                            if let Ok(h) = Histogram::from_buckets(gp, mvp, buckets) {
                                series.insert(*ts, CumulativeROHistogram::from(&h));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    data
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| {
        eprintln!("usage: memory_compare <old|new> <parquet-path>");
        std::process::exit(2);
    });
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: memory_compare <old|new> <parquet-path>");
        std::process::exit(2);
    });

    let raw = fs::read(&path).expect("read parquet");
    let file_size = raw.len() as u64;
    println!("input: {path} ({})", fmt_bytes(file_size));

    let baseline = read_rss_kib();
    println!("baseline RSS: {} KiB", baseline);

    let bytes = Bytes::from(raw);
    let start = Instant::now();

    match mode.as_str() {
        "old" => {
            let tsdb = load_old(bytes);
            let elapsed = start.elapsed();
            let series_count = tsdb.series_count();
            let after = read_rss_kib();
            println!("--- OLD representation ---");
            println!("series loaded: {series_count}");
            println!("load time: {elapsed:.2?}");
            println!(
                "RSS after load: {} KiB (+{} KiB / +{})",
                after,
                after - baseline,
                fmt_bytes((after - baseline) * 1024)
            );
            std::hint::black_box(&tsdb);
        }
        "new" => {
            let tsdb = Tsdb::load_from_bytes(bytes).expect("load");
            let elapsed = start.elapsed();
            let count_label_sets =
                |names: Vec<&str>, fetch: &dyn Fn(&str) -> Option<Vec<Labels>>| -> usize {
                    names
                        .into_iter()
                        .map(|n| fetch(n).map(|v| v.len()).unwrap_or(0))
                        .sum()
                };
            let series_count = count_label_sets(tsdb.counter_names(), &|n| tsdb.counter_labels(n))
                + count_label_sets(tsdb.gauge_names(), &|n| tsdb.gauge_labels(n))
                + count_label_sets(tsdb.histogram_names(), &|n| tsdb.histogram_labels(n));
            let after = read_rss_kib();
            println!("--- NEW representation ---");
            println!("series loaded: {series_count}");
            println!("load time: {elapsed:.2?}");
            println!(
                "RSS after load: {} KiB (+{} KiB / +{})",
                after,
                after - baseline,
                fmt_bytes((after - baseline) * 1024)
            );
            std::hint::black_box(&tsdb);
        }
        other => {
            eprintln!("unknown mode {other:?}, expected 'old' or 'new'");
            std::process::exit(2);
        }
    }
}
