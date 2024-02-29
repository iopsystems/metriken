use std::fs::File;
use std::io::{BufRead, Seek};
use std::path::Path;

use crate::snapshot::Snapshot;
use crate::{ParquetOptions, ParquetWriter};
use parquet::errors::ParquetError;

pub fn msgpack_to_parquet(input: &Path, output: String) -> Result<i64, ParquetError> {
    let mut reader = std::io::BufReader::new(File::open(input)?);
    let options = ParquetOptions::builder().compression_level(3)?.build();
    let mut writer = ParquetWriter::new(File::create(output)?, options);

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
