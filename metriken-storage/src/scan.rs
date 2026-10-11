//! A counter's decoded columns, as a source hands them to the batched rate
//! path (`batch_rate::grid_rates`).
//!
//! A [`CounterScan`] names the matched series and returns the touched
//! segments in time order, a chunk at a time. Each [`ScanSegment`] is the
//! segment's arrow batches and, per batch and read column, which series each
//! row is. The engine reads the arrays in place.

use crate::labels::Labels;
use crate::parquet::BatchColumns;

/// A matched series.
pub struct ScanSeries {
    /// Its labels after relabelling.
    pub labels: Labels,
    /// Whether its first column carries acquisition-window columns.
    pub windowed: bool,
}

/// Which series each row of a read column is.
pub enum RowSeries {
    /// Every row is this series (a wide column).
    One(u32),
    /// Row `r` is series `v[r]`, or none when `u32::MAX` (a long column, by
    /// occupant); empty for a column with neither.
    PerRow(Vec<u32>),
}

/// A read column's schema positions in its segment.
pub struct ScanColumn {
    pub values: usize,
    pub begin: Option<usize>,
    pub width: Option<usize>,
}

/// One segment's decoded columns.
pub struct ScanSegment {
    pub columns: BatchColumns,
    pub cols: Vec<ScanColumn>,
    /// Per batch, per entry of `cols`.
    pub series: Vec<Vec<RowSeries>>,
}

/// Up to `n` segments, decoded.
pub struct ScanChunk {
    /// The chunk's segments that the store still has, in time order.
    pub segments: Vec<ScanSegment>,
    /// The earliest catalog start of every segment after this chunk; `None`
    /// after the last chunk or when one of them has no span.
    pub rest_start: Option<u64>,
}

/// A segment could not be read. The engine discards the scan and evaluates
/// the query per series.
#[derive(Debug)]
pub struct ScanError;

/// Where a scan's chunks come from.
pub trait ChunkReader {
    /// The next up to `n` segments, or `None` when none remain.
    fn next_chunk(&mut self, n: usize) -> Result<Option<ScanChunk>, ScanError>;
}

/// A counter's matched series and its decoded segments. A reader returns
/// segments in time order, and a series has at most one column per segment,
/// so each series' samples arrive in increasing time; `SeriesRate` requires
/// it.
pub struct CounterScan<'a> {
    pub series: Vec<ScanSeries>,
    reader: Box<dyn ChunkReader + 'a>,
}

impl<'a> CounterScan<'a> {
    pub fn new(series: Vec<ScanSeries>, reader: Box<dyn ChunkReader + 'a>) -> Self {
        Self { series, reader }
    }

    /// Decodes up to `n` of the remaining segments.
    pub fn next_chunk(&mut self, n: usize) -> Result<Option<ScanChunk>, ScanError> {
        self.reader.next_chunk(n)
    }
}
