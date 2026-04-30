//! Inspect histogram column metadata in one or more parquet files.
//!
//! Prints per-file histogram column counts grouped by
//! `(grouping_power, max_value_power)` so you can see how dense each
//! file's histograms are configured.

use std::collections::BTreeMap;
use std::path::PathBuf;

use arrow::datatypes::DataType;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: inspect_histograms <path>...");
        std::process::exit(2);
    }
    for p in args {
        let path = PathBuf::from(&p);
        let bytes = std::fs::read(&path).expect("read");
        let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes))
            .expect("parquet builder");
        let schema = builder.schema().clone();
        let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
        for field in schema.fields() {
            if !matches!(
                field.data_type(),
                DataType::List(inner) if inner.data_type() == &DataType::UInt64
            ) {
                continue;
            }
            let m = field.metadata();
            let gp = m
                .get("grouping_power")
                .cloned()
                .unwrap_or_else(|| "?".into());
            let mvp = m
                .get("max_value_power")
                .cloned()
                .unwrap_or_else(|| "?".into());
            *counts.entry((gp, mvp)).or_default() += 1;
        }
        let total: usize = counts.values().copied().sum();
        println!("\n{}  ({} histogram columns)", p, total);
        for ((gp, mvp), n) in &counts {
            println!("  gp={:>3}  mvp={:>3}  count={:>5}", gp, mvp, n);
        }
    }
}
