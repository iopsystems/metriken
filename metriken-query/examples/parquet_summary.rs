//! Tally parquet column sizes by metric type.
//!
//! Walks one or more parquet files and prints per-file (and total) stats:
//!   - column counts split by counter / gauge / histogram
//!   - compressed and uncompressed byte sums per metric type
//!   - average uncompressed bytes per row for each type
//!
//! Usage:
//!     cargo run --release --example parquet_summary -- <path>...

use std::fs;
use std::path::PathBuf;

use parquet::file::reader::FileReader;
use parquet::file::serialized_reader::SerializedFileReader;

#[derive(Default, Clone, Copy)]
struct Stats {
    columns: u64,
    rows: u64,
    compressed: u64,
    uncompressed: u64,
}

impl Stats {
    fn add_column(&mut self, rows: u64, compressed: i64, uncompressed: i64) {
        self.columns += 1;
        // total rows is the same for every column; track the max so a
        // histogram column with N rows gets credited once.
        self.rows = self.rows.max(rows);
        self.compressed += compressed.max(0) as u64;
        self.uncompressed += uncompressed.max(0) as u64;
    }
}

fn fmt_bytes(b: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
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

fn summarize(path: &PathBuf) -> std::io::Result<()> {
    use arrow::datatypes::DataType;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let bytes = fs::read(path)?;
    let total_rows: u64 = {
        let r = SerializedFileReader::new(bytes::Bytes::from(bytes.clone())).expect("reader");
        r.metadata().file_metadata().num_rows() as u64
    };

    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes.clone()))
        .expect("builder");
    let arrow_schema = builder.schema().clone();

    // Map top-level column name → metric type, derived from the Arrow data
    // type.  Mirrors the loader in tsdb/mod.rs.
    let mut col_type: std::collections::HashMap<String, &'static str> = Default::default();
    for field in arrow_schema.fields() {
        if field.name() == "timestamp" {
            continue;
        }
        let kind = match field.data_type() {
            DataType::UInt64 => "counter",
            DataType::Int64 => "gauge",
            DataType::List(inner) if inner.data_type() == &DataType::UInt64 => "histogram",
            _ => "other",
        };
        col_type.insert(field.name().clone(), kind);
    }

    // Walk row-group metadata to sum compressed / uncompressed bytes per
    // top-level column (histograms have nested leaves under
    // `<col>.list.element` which we collapse here).
    let r = SerializedFileReader::new(bytes::Bytes::from(bytes)).expect("reader");
    let parquet_meta = r.metadata();

    let mut by_type: std::collections::HashMap<&'static str, Stats> = Default::default();
    let mut per_col_compressed: std::collections::HashMap<String, i64> = Default::default();
    let mut per_col_uncompressed: std::collections::HashMap<String, i64> = Default::default();

    for rg_idx in 0..parquet_meta.num_row_groups() {
        let rg = parquet_meta.row_group(rg_idx);
        for col_idx in 0..rg.num_columns() {
            let col = rg.column(col_idx);
            let top = col
                .column_path()
                .parts()
                .first()
                .cloned()
                .unwrap_or_default();
            *per_col_compressed.entry(top.clone()).or_default() += col.compressed_size();
            *per_col_uncompressed.entry(top).or_default() += col.uncompressed_size();
        }
    }

    for (col, kind) in &col_type {
        let comp = per_col_compressed.get(col).copied().unwrap_or(0);
        let uncomp = per_col_uncompressed.get(col).copied().unwrap_or(0);
        by_type
            .entry(kind)
            .or_default()
            .add_column(total_rows, comp, uncomp);
    }

    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    println!(
        "\n{fname}  ({} rows, {} columns)",
        total_rows,
        col_type.len()
    );
    println!(
        "  {:<10} {:>5} {:>10} {:>12} {:>12} {:>10}",
        "type", "cols", "rows", "compressed", "uncompressed", "u/row"
    );
    let mut kinds: Vec<&&str> = by_type.keys().collect();
    kinds.sort();
    for k in kinds {
        let s = &by_type[k];
        let per_row = if s.rows > 0 && s.columns > 0 {
            s.uncompressed as f64 / s.rows as f64 / s.columns as f64
        } else {
            0.0
        };
        println!(
            "  {:<10} {:>5} {:>10} {:>12} {:>12} {:>9.1}B",
            k,
            s.columns,
            s.rows,
            fmt_bytes(s.compressed),
            fmt_bytes(s.uncompressed),
            per_row,
        );
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: parquet_summary <path>...");
        std::process::exit(2);
    }
    for arg in &args {
        if let Err(e) = summarize(&PathBuf::from(arg)) {
            eprintln!("{arg}: {e}");
        }
    }
}
