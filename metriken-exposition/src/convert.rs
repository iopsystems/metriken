use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::path::Path;

use parquet::errors::ParquetError;

use crate::snapshot::Snapshot;
use crate::{ParquetOptions, ParquetSchema};

/// A struct for converting msgpack'd metriken snapshots into a parquet file.
#[derive(Clone, Debug, Default)]
pub struct MsgpackToParquet {
    parquet_options: ParquetOptions,
}

impl MsgpackToParquet {
    /// Returns a new `MsgpackToParquet` converter that uses the default
    /// conversion options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a `MsgpackToParquet` converted from the provided parquet options.
    pub fn with_options(options: ParquetOptions) -> Self {
        Self {
            parquet_options: options,
        }
    }

    /// Converts a file with metrics in msgpack format to a parquet file.
    /// Input and putput are file paths.
    /// If successful, returns the number of rows written out to the parquet
    /// file.
    pub fn convert_file_path(
        self,
        input: impl AsRef<Path>,
        output: impl AsRef<Path>,
    ) -> Result<i64, ParquetError> {
        self.convert_file_handle(File::open(input)?, File::create(output)?)
    }

    /// Converts a file with metrics in msgpack format to a parquet file.
    /// Input and putput are open file handles.
    /// If successful, returns the number of rows written out to the parquet
    /// file.
    pub fn convert_file_handle(
        self,
        reader: impl Read + Seek,
        writer: impl Write + Send,
    ) -> Result<i64, ParquetError> {
        let mut reader = BufReader::new(reader);
        let mut schema = ParquetSchema::new();

        // First pass to build the schema
        while !reader.fill_buf().unwrap().is_empty() {
            let s: Snapshot = rmp_serde::from_read(&mut reader)
                .map_err(|x| ParquetError::External(Box::new(x)))?;
            schema.push(s);
        }
        let mut writer = schema.finalize(writer, self.parquet_options)?;

        // Rewind file pointer and second pass for the actual metrics
        reader.rewind().unwrap();
        while !reader.fill_buf().unwrap().is_empty() {
            let s: Snapshot = rmp_serde::from_read(&mut reader)
                .map_err(|x| ParquetError::External(Box::new(x)))?;
            writer.push(s)?;
        }
        let metadata = writer.finalize()?;

        Ok(metadata.num_rows)
    }
}
