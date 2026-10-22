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
    /// The row spacing, in seconds, of the table it is read from, where a
    /// source knows it (`LabeledSource::with_interval`).
    pub interval: Option<f64>,
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
    /// The series (index ranges into the scan's series) that no later chunk
    /// holds rows of: those of a scan [`interleave`] joined that has ended.
    /// Empty for a single scan, whose series all end with its last chunk.
    /// An interleave of interleaves reports only its own children that
    /// ended, not the lists of its children's chunks: the series of an
    /// inner scan that ended early are reported when that whole inner scan
    /// ends. A late report costs only memory (the batch path holds points
    /// until then); no answer depends on it.
    pub ended: Vec<std::ops::Range<u32>>,
}

/// A segment could not be read. The engine discards the scan and evaluates
/// the query per series.
#[derive(Debug)]
pub struct ScanError;

/// What a reader knows of one series' rows in the segments it has not
/// returned yet ([`ChunkReader::later_rows`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaterRows {
    /// No unread segment is planned to read the series, so none holds a
    /// row of it.
    None,
    /// An unread segment is planned to read the series. It can still hold
    /// no row of it (a wide column whose cells are null there).
    Possible,
    /// The reader does not say.
    Unknown,
}

/// Where a scan's chunks come from.
pub trait ChunkReader {
    /// The next up to `n` segments, or `None` when none remain.
    fn next_chunk(&mut self, n: usize) -> Result<Option<ScanChunk>, ScanError>;

    /// Whether the segments not returned yet can hold rows of `series` (an
    /// index into the scan's series). The batch path ends a series that
    /// falls silent in a grouped display read only when none can follow.
    /// [`LaterRows::Unknown`] unless the reader says.
    fn later_rows(&self, series: u32) -> LaterRows {
        let _ = series;
        LaterRows::Unknown
    }
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

    /// The matched series and the chunk reader, for a caller that wraps
    /// the reader.
    pub fn into_parts(self) -> (Vec<ScanSeries>, Box<dyn ChunkReader + 'a>) {
        (self.series, self.reader)
    }

    /// Decodes up to `n` of the remaining segments.
    pub fn next_chunk(&mut self, n: usize) -> Result<Option<ScanChunk>, ScanError> {
        self.reader.next_chunk(n)
    }

    /// See [`ChunkReader::later_rows`].
    pub fn later_rows(&self, series: u32) -> LaterRows {
        self.reader.later_rows(series)
    }
}

/// Where a joined scan's next chunk starts.
#[derive(Clone, Copy)]
enum Start {
    /// No chunk read yet.
    Unread,
    /// The earliest catalog start of its unread segments, when known.
    At(Option<u64>),
    /// No chunk left.
    Done,
}

struct Child<'a> {
    scan: CounterScan<'a>,
    /// Its first series' index in the joined scan, and how many it has.
    offset: u32,
    len: u32,
    next_start: Start,
}

/// Several scans read as one (see [`interleave`]), in time order across
/// them: each chunk is the next of the scan whose unread segments start
/// earliest (a scan not read yet first), its series moved to their place in
/// the joined scan, and the series of the scans that have ended since the
/// last chunk listed in it (`ScanChunk::ended`). The batch path flushes a
/// grouped display read up to the earliest start of what is unread
/// (`ScanChunk::rest_start`); reading one scan to its end before the next
/// would hold every group until the last scan.
struct Interleaved<'a> {
    children: Vec<Child<'a>>,
    /// Series of scans that have ended, not yet reported in a chunk.
    ended: Vec<std::ops::Range<u32>>,
}

impl ChunkReader for Interleaved<'_> {
    fn next_chunk(&mut self, n: usize) -> Result<Option<ScanChunk>, ScanError> {
        loop {
            let next = self
                .children
                .iter()
                .enumerate()
                .filter_map(|(i, c)| match c.next_start {
                    Start::Done => None,
                    Start::Unread => Some((i, 0)),
                    Start::At(at) => Some((i, at.unwrap_or(0))),
                })
                .min_by_key(|(_, at)| *at)
                .map(|(i, _)| i);
            let Some(i) = next else {
                return Ok(None);
            };
            let child = &mut self.children[i];
            let Some(mut chunk) = child.scan.next_chunk(n)? else {
                child.next_start = Start::Done;
                self.ended.push(child.offset..child.offset + child.len);
                continue;
            };
            // `None` is the last chunk, or a segment without a span; a read
            // that returns nothing next settles which.
            child.next_start = Start::At(chunk.rest_start);
            let offset = child.offset;
            if offset > 0 {
                for segment in &mut chunk.segments {
                    for batch in &mut segment.series {
                        for rows in batch {
                            match rows {
                                RowSeries::One(s) => *s += offset,
                                RowSeries::PerRow(v) => {
                                    for s in v.iter_mut().filter(|s| **s != u32::MAX) {
                                        *s += offset;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            chunk.ended = std::mem::take(&mut self.ended);
            // The earliest start of anything unread, in every scan; not
            // known while a scan is unread or does not say.
            chunk.rest_start = self
                .children
                .iter()
                .map(|c| match c.next_start {
                    Start::Done => Some(None),
                    Start::Unread => None,
                    Start::At(at) => at.map(Some),
                })
                .try_fold(None::<u64>, |min, at| {
                    let at = at?;
                    Some(match (min, at) {
                        (Some(m), Some(a)) => Some(m.min(a)),
                        (m, a) => m.or(a),
                    })
                })
                .flatten();
            return Ok(Some(chunk));
        }
    }

    /// The answer of the scan holding `series`.
    fn later_rows(&self, series: u32) -> LaterRows {
        self.children
            .iter()
            .find(|c| (c.offset..c.offset + c.len).contains(&series))
            .map_or(LaterRows::Unknown, |c| c.scan.later_rows(series - c.offset))
    }
}

/// `scans` as one scan: their series in order, each scan's after the
/// previous', and their chunks interleaved in time order, each chunk the
/// next of the scan whose unread segments start earliest (a scan not read
/// yet first), its series indices moved to their place in the joined scan.
pub fn interleave<'a>(scans: Vec<CounterScan<'a>>) -> CounterScan<'a> {
    let mut series = Vec::new();
    let mut children = Vec::with_capacity(scans.len());
    for mut scan in scans {
        let offset = series.len() as u32;
        let len = scan.series.len() as u32;
        series.append(&mut scan.series);
        children.push(Child {
            scan,
            offset,
            len,
            next_start: Start::Unread,
        });
    }
    CounterScan::new(
        series,
        Box::new(Interleaved {
            children,
            ended: Vec::new(),
        }),
    )
}
