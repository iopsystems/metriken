use std::fs::File;
use std::io::{BufRead, Seek};

use crate::snapshot::Snapshot;
use crate::ParquetWriter;
use parquet::errors::ParquetError;

pub fn msgpack_to_parquet(input: String, output: String) -> Result<i64, ParquetError> {
    let mut reader = std::io::BufReader::new(File::open(input)?);
    let mut writer = ParquetWriter::try_new(File::create(output)?, true)?;

    // First pass to build the schema
    while !reader.fill_buf().unwrap().is_empty() {
        let s: Snapshot =
            rmp_serde::from_read(&mut reader).map_err(|x| ParquetError::External(Box::new(x)))?;
        writer.process_snapshot_schema(s)?;
    }
    writer.finalize_schema()?;

    // Rewind file pointer and second pass for the actual metrics
    reader.rewind().unwrap();
    while !reader.fill_buf().unwrap().is_empty() {
        let s: Snapshot =
            rmp_serde::from_read(&mut reader).map_err(|x| ParquetError::External(Box::new(x)))?;
        writer.process_snapshot(s)?;
    }
    let metadata = writer.finalize()?;

    Ok(metadata.num_rows)
}
