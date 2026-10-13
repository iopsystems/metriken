//! Reading a producer's replication stream from bytes, with no transport.
//!
//! A producer serves `Frame::{Handshake, Rows}` on `/metrics/stream`
//! (`metriken-exposition`'s `stream`). [`FrameDecoder`] turns the body's
//! chunks into frames, and [`StreamSubscriber`] folds one interval's frames
//! into the rows a recording stages ([`Applied`], then [`Applied::passes`]).
//!
//! # Why not dendro's `Subscriber`
//!
//! dendro ships one. It owns a dendro `Writer`, while a recording writes
//! through metriken-storage's `ArchiveWriter`, which maps the producer's
//! occupant keys to its own occupant numbers
//! (`SourceRecorder::stage_streamed`). So the frames are read and checked
//! here, their rows decoded with `StreamDecoder`, and metriken-storage writes
//! the archive.
//!
//! # No identity index
//!
//! Identity travels in the rows, `__uid__` included. A current producer sends
//! no `Frame::Index` and stamps every rows frame with dendro's
//! `NO_INDEX_STATE`. An older one (a rezolus agent before 6.0) still sends
//! index frames and names its own index state; both are ignored, since the
//! same labels arrive in the rows.

use std::collections::BTreeMap;

use dendro::archive::WalRow;
use dendro::replicate::Frame;

/// What one interval's frames amounted to.
#[derive(Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Applied {
    /// The interval index the producer stamped. Not a frame count: a gap says
    /// an interval the subscriber asked for produced no reading.
    pub seq: u64,
    /// The interval's rows, ready for the writer.
    pub rows: Vec<WalRow>,
    /// Whether `seq` jumped, meaning intervals produced no frame at all.
    pub gap: bool,
}

/// One pass off the stream: the producer's stamp and its rows, in the order
/// they arrived.
#[derive(Debug)]
#[non_exhaustive]
pub struct StreamPass {
    pub ts: u64,
    pub wall_offset: i64,
    pub rows: Vec<WalRow>,
}

impl Applied {
    /// The rows as passes, one [`StreamPass`] per distinct stamp.
    ///
    /// For a well-formed interval that is exactly one: the producer stamps a
    /// whole pass once. Grouping rather than assuming keeps a relay that
    /// batches several passes into one frame from landing them all on the
    /// first pass's stamp. By `ts` alone, not `(ts, wall_offset)`: two passes
    /// at one `ts` would write one row's worth of key however their wall
    /// offsets differ. The first row's `wall_offset` stands for the pass.
    ///
    /// A negative stamp is refused: the archive's `ts` is unsigned, and a
    /// producer that sent one is not one to guess for.
    pub fn passes(self) -> Result<Vec<StreamPass>, String> {
        let mut by_stamp: BTreeMap<i64, (i64, Vec<WalRow>)> = BTreeMap::new();
        for row in self.rows {
            if row.ts < 0 {
                return Err(format!(
                    "stream {} stamped a row at {} ns, before the epoch",
                    row.stream, row.ts
                ));
            }
            by_stamp
                .entry(row.ts)
                .or_insert_with(|| (row.wall_offset, Vec::new()))
                .1
                .push(row);
        }
        Ok(by_stamp
            .into_iter()
            .map(|(ts, (wall_offset, rows))| StreamPass {
                ts: ts as u64,
                wall_offset,
                rows,
            })
            .collect())
    }

    /// The wall clock of the newest row, `ts + wall_offset` on the
    /// producer's clock, or `None` for an interval with no rows.
    pub fn newest_wall_ns(&self) -> Option<u64> {
        self.rows
            .iter()
            .map(|r| r.ts.saturating_add(r.wall_offset).max(0) as u64)
            .max()
    }
}

/// What a stream's handshake says about its producer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Handshake {
    /// The producer epoch: changes when, and only when, every cumulative
    /// counter restarted from zero.
    pub uuid: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
    /// The producer's anchor, nanoseconds since the Unix epoch. Row
    /// timestamps are on this timeline, so a recording that discarded it
    /// could not place its own rows in wall time. 0 when the handshake
    /// carried none (dendro's field is signed; a negative anchor reads as
    /// none too).
    pub clock_anchor_wall_ns: u64,
}

impl Handshake {
    /// A handshake from a producer with epoch `uuid`, anchored at
    /// `clock_anchor_wall_ns` (0 for none). The recorder builds these from
    /// the stream; a caller builds one to test code that consumes
    /// [`StreamEvent::Connected`].
    ///
    /// [`StreamEvent::Connected`]: crate::StreamEvent::Connected
    pub fn new(
        uuid: Option<String>,
        labels: BTreeMap<String, String>,
        metadata: BTreeMap<String, String>,
        clock_anchor_wall_ns: u64,
    ) -> Self {
        Self {
            uuid,
            labels,
            metadata,
            clock_anchor_wall_ns,
        }
    }

    /// The epoch, when the handshake named a non-empty one.
    pub fn epoch(&self) -> Option<&str> {
        self.uuid.as_deref().filter(|u| !u.is_empty())
    }
}

/// Accumulates one connection's frames.
#[derive(Debug, Default)]
pub struct StreamSubscriber {
    handshake: Option<Handshake>,
    last_seq: Option<u64>,
}

impl StreamSubscriber {
    pub fn new() -> Self {
        Self::default()
    }

    /// The handshake, once it has arrived.
    pub fn handshake(&self) -> Option<&Handshake> {
        self.handshake.as_ref()
    }

    /// Fold one interval's frames in: everything up to and including its
    /// `Rows`.
    pub fn apply(&mut self, frames: Vec<Frame>) -> Result<Applied, String> {
        let mut out = Applied::default();

        for frame in frames {
            match frame {
                Frame::Handshake {
                    uuid,
                    labels,
                    metadata,
                    clock_anchor_wall_ns,
                    ..
                } => {
                    // A second handshake on one connection would mean the
                    // producer restarted underneath us, which is a new source
                    // and a new timeline rather than more of this one.
                    if let Some(existing) = &self.handshake {
                        if existing.uuid != uuid {
                            return Err(format!(
                                "the stream changed source mid-connection ({:?} -> {uuid:?}); \
                                 its rows are on a different timeline and cannot be appended \
                                 to this recording",
                                existing.uuid
                            ));
                        }
                    }
                    self.handshake = Some(Handshake {
                        uuid,
                        labels,
                        metadata,
                        clock_anchor_wall_ns: u64::try_from(clock_anchor_wall_ns).unwrap_or(0),
                    });
                }

                // An older producer sends an identity index. Its labels are
                // also in the rows' schemas, which is where a `.dendro` takes
                // identity from, so the frame is dropped.
                Frame::Index { .. } => {}

                // `index_state` is not checked: a current producer always
                // names `NO_INDEX_STATE`, and an older one's state refers to
                // an index this subscriber does not keep.
                Frame::Rows { seq, rows, .. } => {
                    if let Some(last) = self.last_seq {
                        out.gap = seq > last.saturating_add(1);
                    }
                    self.last_seq = Some(seq);
                    out.seq = seq;
                    out.rows.extend(rows);
                }

                // Segments and clock offsets are an archive publisher's
                // frames; a live producer has no archive to send them from.
                other => {
                    return Err(format!(
                        "unexpected frame on a producer stream: {}",
                        frame_kind(&other)
                    ))
                }
            }
        }

        Ok(out)
    }
}

/// Names a frame for an error message. `Frame` is `#[non_exhaustive]`, so a
/// kind added to dendro reaches the catch-all, and [`StreamSubscriber::apply`]
/// refuses it at runtime.
pub(crate) fn frame_kind(frame: &Frame) -> &'static str {
    match frame {
        Frame::Handshake { .. } => "handshake",
        Frame::Index { .. } => "index",
        Frame::Rows { .. } => "rows",
        Frame::Segment { .. } => "segment",
        Frame::ClockOffset { .. } => "clock offset",
        _ => "kind this recorder was not written for",
    }
}

/// Frames out of a byte stream that arrives in pieces.
///
/// An HTTP response body is a stream of chunks that respects no frame
/// boundary: one chunk may hold three frames, or a third of one. This holds
/// the remainder between chunks and yields whole frames as they complete,
/// using the length prefix dendro's encoder writes. It decodes with
/// `decode_payload`, which refuses a frame kind the linked dendro does not
/// know.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    preamble_read: bool,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a chunk and take whatever frames it completed. An empty result is
    /// normal: a chunk that does not finish a frame produces nothing.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, String> {
        use dendro::replicate::wire;

        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();

        if !self.preamble_read {
            let want = wire::MAGIC.len() + 2;
            if self.buf.len() < want {
                return Ok(out);
            }
            if &self.buf[..wire::MAGIC.len()] != wire::MAGIC {
                // The likely cause is an endpoint serving something else
                // entirely, and a decoder that went on would report
                // malformed frames forever instead of this.
                return Err(
                    "the stream does not begin with dendro's replication magic; this is \
                     not a replication stream"
                        .to_string(),
                );
            }
            let version =
                u16::from_le_bytes([self.buf[wire::MAGIC.len()], self.buf[wire::MAGIC.len() + 1]]);
            if version != wire::PROTOCOL_VERSION {
                return Err(format!(
                    "the producer sent replication protocol version {version}; this recorder \
                     implements version {}",
                    wire::PROTOCOL_VERSION
                ));
            }
            self.buf.drain(..want);
            self.preamble_read = true;
        }

        loop {
            const PREFIX: usize = dendro::replicate::wire::LENGTH_PREFIX_BYTES;
            if self.buf.len() < PREFIX {
                break;
            }
            let len =
                u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if len > wire::MAX_FRAME_BYTES {
                // A corrupt or hostile prefix would otherwise be an
                // allocation of up to 4 GiB.
                return Err(format!(
                    "a frame declares {len} bytes, past the {} byte limit",
                    wire::MAX_FRAME_BYTES
                ));
            }
            if self.buf.len() < PREFIX + len {
                break;
            }
            let frame = wire::decode_payload(&self.buf[PREFIX..PREFIX + len])
                .map_err(|e| format!("undecodable replication frame: {e}"))?;
            self.buf.drain(..PREFIX + len);
            out.push(frame);
        }

        Ok(out)
    }

    /// Bytes held back because they do not yet complete a frame. A stream
    /// that ended with some is a truncated stream.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &str = "cpu_usage/cpu_usage_task";

    fn handshake() -> Frame {
        Frame::Handshake {
            source: 0,
            uuid: Some("epoch-1".to_string()),
            labels: [("source".to_string(), "rezolus".to_string())]
                .into_iter()
                .collect(),
            metadata: BTreeMap::new(),
            clock_anchor_wall_ns: 1_700_000_000_000_000_000,
            complete: false,
        }
    }

    fn rows_frame(seq: u64, n: usize) -> Frame {
        Frame::Rows {
            source: 0,
            seq,
            index_state: dendro::replicate::NO_INDEX_STATE,
            rows: (0..n)
                .map(|i| WalRow {
                    stream: STREAM.to_string(),
                    ts: 1_000 + i as i64,
                    wall_offset: 0,
                    row: vec![0x93, 0x01],
                })
                .collect(),
        }
    }

    fn encoded_stream(frames: &[Frame]) -> Vec<u8> {
        let mut bytes = Vec::new();
        dendro::replicate::wire::write_preamble(&mut bytes).unwrap();
        for f in frames {
            dendro::replicate::wire::encode_frame(f, &mut bytes).unwrap();
        }
        bytes
    }

    #[test]
    fn frames_survive_being_split_at_every_byte() {
        let sent = vec![handshake(), rows_frame(1, 2), rows_frame(2, 0)];
        let bytes = encoded_stream(&sent);
        let mut decoder = FrameDecoder::new();
        let mut got = Vec::new();
        for b in &bytes {
            got.extend(decoder.push(&[*b]).unwrap());
        }
        assert_eq!(got, sent);
        assert_eq!(decoder.pending(), 0, "nothing held back at the end");
    }

    #[test]
    fn frames_survive_arriving_all_at_once() {
        let sent = vec![handshake(), rows_frame(1, 1)];
        let mut decoder = FrameDecoder::new();
        assert_eq!(decoder.push(&encoded_stream(&sent)).unwrap(), sent);
    }

    #[test]
    fn a_partial_frame_yields_nothing_and_is_not_an_error() {
        let bytes = encoded_stream(&[handshake()]);
        let mut decoder = FrameDecoder::new();
        let half = bytes.len() / 2;
        assert!(decoder.push(&bytes[..half]).unwrap().is_empty());
        assert!(decoder.pending() > 0);
        assert_eq!(decoder.push(&bytes[half..]).unwrap().len(), 1);
    }

    #[test]
    fn a_stream_that_is_not_a_replication_stream_says_so() {
        let mut decoder = FrameDecoder::new();
        let err = decoder
            .push(b"\x93\x01\x02 this is msgpack, not frames")
            .expect_err("must refuse");
        assert!(err.contains("not a replication stream"), "{err}");
    }

    #[test]
    fn an_absurd_frame_length_is_refused_rather_than_allocated() {
        let mut bytes = Vec::new();
        dendro::replicate::wire::write_preamble(&mut bytes).unwrap();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut decoder = FrameDecoder::new();
        let err = decoder.push(&bytes).expect_err("must refuse");
        assert!(err.contains("past the"), "{err}");
    }

    #[test]
    fn an_interval_keeps_its_rows_and_the_anchor() {
        let mut sub = StreamSubscriber::new();
        let applied = sub.apply(vec![handshake(), rows_frame(7, 3)]).unwrap();
        assert_eq!(applied.rows.len(), 3);
        assert_eq!(applied.seq, 7);
        assert_eq!(
            sub.handshake().unwrap().clock_anchor_wall_ns,
            1_700_000_000_000_000_000
        );
    }

    #[test]
    fn an_older_producers_index_frames_are_ignored() {
        let mut sub = StreamSubscriber::new();
        let applied = sub
            .apply(vec![
                handshake(),
                Frame::Index {
                    source: 0,
                    stream: STREAM.to_string(),
                    ts: 1_000,
                    kind: dendro::replicate::IndexKind::Full,
                    state: (0xdead, 0xbeef),
                    blob: vec![0x93, 0x01],
                },
                Frame::Rows {
                    source: 0,
                    seq: 1,
                    index_state: (0xdead, 0xbeef),
                    rows: vec![WalRow {
                        stream: STREAM.to_string(),
                        ts: 1_000,
                        wall_offset: 0,
                        row: vec![0x93, 0x01],
                    }],
                },
            ])
            .unwrap();
        assert_eq!(applied.rows.len(), 1);
    }

    #[test]
    fn a_jump_in_seq_is_reported_as_a_gap() {
        let mut sub = StreamSubscriber::new();
        sub.apply(vec![handshake(), rows_frame(1, 1)]).unwrap();
        assert!(!sub.apply(vec![rows_frame(2, 1)]).unwrap().gap);
        assert!(sub.apply(vec![rows_frame(9, 1)]).unwrap().gap);
    }

    #[test]
    fn a_source_change_mid_connection_is_refused() {
        let mut sub = StreamSubscriber::new();
        sub.apply(vec![handshake()]).unwrap();
        let Frame::Handshake {
            source,
            labels,
            metadata,
            clock_anchor_wall_ns,
            complete,
            ..
        } = handshake()
        else {
            unreachable!()
        };
        let restarted = Frame::Handshake {
            source,
            uuid: Some("epoch-2".to_string()),
            labels,
            metadata,
            clock_anchor_wall_ns,
            complete,
        };
        let err = sub.apply(vec![restarted]).expect_err("must refuse");
        assert!(err.contains("different timeline"), "{err}");
    }

    #[test]
    fn a_segment_frame_on_a_producer_stream_is_refused() {
        let mut sub = StreamSubscriber::new();
        let err = sub
            .apply(vec![
                handshake(),
                Frame::Segment {
                    source: 0,
                    stream: STREAM.to_string(),
                    meta: dendro::archive::SegmentMeta {
                        rows: 0,
                        first_ts: 0,
                        last_ts: 0,
                    },
                    bytes: Vec::new(),
                    caller_index: None,
                },
            ])
            .expect_err("must refuse");
        assert!(err.contains("segment"), "{err}");
    }

    fn wal_row(stream: &str, ts: i64, wall_offset: i64) -> WalRow {
        WalRow {
            stream: stream.to_string(),
            ts,
            wall_offset,
            row: vec![0x90],
        }
    }

    #[test]
    fn an_interval_becomes_one_pass_at_the_producers_stamp() {
        let applied = Applied {
            seq: 3,
            rows: vec![wal_row(STREAM, 5_000, 7), wal_row("b/two", 5_000, 7)],
            gap: false,
        };
        let passes = applied.passes().unwrap();
        assert_eq!(passes.len(), 1, "one pass, not one per row");
        assert_eq!((passes[0].ts, passes[0].wall_offset), (5_000, 7));
        assert_eq!(passes[0].rows[1].stream, "b/two", "in arrival order");
    }

    #[test]
    fn rows_at_two_stamps_become_two_passes() {
        let applied = Applied {
            rows: vec![wal_row(STREAM, 5_000, 0), wal_row(STREAM, 6_000, 0)],
            ..Applied::default()
        };
        let passes = applied.passes().unwrap();
        assert_eq!(
            passes.iter().map(|p| p.ts).collect::<Vec<_>>(),
            vec![5_000, 6_000]
        );
    }

    #[test]
    fn rows_at_one_stamp_are_one_pass_whatever_their_wall_offsets() {
        let applied = Applied {
            rows: vec![wal_row(STREAM, 5_000, 0), wal_row("b/two", 5_000, 9)],
            ..Applied::default()
        };
        let passes = applied.passes().unwrap();
        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0].wall_offset, 0, "the first row's stands");
    }

    #[test]
    fn a_stamp_before_the_epoch_is_refused() {
        let rows = Applied {
            rows: vec![wal_row(STREAM, -1, 0)],
            ..Applied::default()
        };
        let err = rows.passes().expect_err("a negative row stamp");
        assert!(
            err.contains(STREAM) && err.contains("before the epoch"),
            "{err}"
        );
    }
}
