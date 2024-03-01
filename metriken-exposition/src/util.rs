use std::fs::File;
use std::io::{BufRead, BufReader, Seek};
use std::path::Path;

use parquet::errors::ParquetError;

use crate::snapshot::Snapshot;
use crate::{ParquetOptions, ParquetSchema};

pub fn msgpack_to_parquet(input: &Path, output: &Path) -> Result<i64, ParquetError> {
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
        ParquetOptions::builder().compression_level(3)?.build(),
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
