use std::fs::File;
use std::io::{BufRead, BufReader, Seek};
use std::path::Path;

use parquet::errors::ParquetError;

use crate::snapshot::Snapshot;
use crate::{ParquetOptions, ParquetSchema};

/// Converts a file with metrics in msgpack format to a parquet file.
/// If successful, returns the number of rows written out to the parquet file.
pub fn msgpack_to_parquet(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<i64, ParquetError> {
    let mut reader = BufReader::new(File::open(input)?);
    let mut schema = ParquetSchema::new();

    // First pass to build the schema
    while !reader.fill_buf().unwrap().is_empty() {
        let s: Snapshot =
            rmp_serde::from_read(&mut reader).map_err(|x| ParquetError::External(Box::new(x)))?;
        schema.push(s);
    }
    let mut writer = schema.finalize(
        File::create(output)?,
        ParquetOptions::new().compression_level(3)?,
    )?;

    // Rewind file pointer and second pass for the actual metrics
    reader.rewind().unwrap();
    while !reader.fill_buf().unwrap().is_empty() {
        let s: Snapshot =
            rmp_serde::from_read(&mut reader).map_err(|x| ParquetError::External(Box::new(x)))?;
        writer.push(s)?;
    }
    let metadata = writer.finalize()?;

    Ok(metadata.num_rows)
}
