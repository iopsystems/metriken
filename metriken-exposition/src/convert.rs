use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::path::Path;

use parquet::errors::ParquetError;

use crate::snapshot::Snapshot;
use crate::{ParquetOptions, ParquetSchema};

#[derive(Clone, Debug)]
pub struct MsgpackToParquet {
    compression_level: i32,
}

impl Default for MsgpackToParquet {
    fn default() -> Self {
        Self {
            compression_level: 3,
        }
    }
}

impl MsgpackToParquet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn compression_level(mut self, level: i32) -> Self {
        self.compression_level = level;
        self
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
        let mut schema = ParquetSchema::new(None);

        // First pass to build the schema
        while !reader.fill_buf().unwrap().is_empty() {
            let s: Snapshot = rmp_serde::from_read(&mut reader)
                .map_err(|x| ParquetError::External(Box::new(x)))?;
            schema.push(s);
        }
        let mut writer = schema.finalize(
            writer,
            ParquetOptions::new().compression_level(self.compression_level)?,
        )?;

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
