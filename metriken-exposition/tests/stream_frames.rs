//! GO check 1 of `docs/journal/2026-10-11-stream-route.md`: the frames a
//! [`StreamSubscription`] sends for a fixed sequence of passes equal, byte
//! for byte, the frames rezolus's agent sent for the same sequence
//! (`tests/fixtures/stream-frames/{wide,long}.bin`, recorded by rezolus at
//! f55709b8 from its `StreamState`).
//!
//! File format: one record per chunk the stream yields, the opening chunk
//! (preamble and handshake) first, then one per interval. Each record is a
//! little-endian `u32` byte length followed by the chunk's bytes.
//!
//! The wide passes here hold [`EncodedGroup`]s, where rezolus's held its
//! `AgentRow`s; both send the same stream, schema and payload.
//! metriken-storage's `stream_frames` test runs the wide sequence with
//! `AgentRow`s.

#![cfg(feature = "stream")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use metriken_exposition::stream::{
    EncodedGroup, EncodedLongGroup, EncodedStreamGroup, PassStamp, StreamRequest,
    StreamSubscription,
};
use metriken_exposition::{GroupSchema, MetricDesc};
use metriken_model::wal::{
    encode_wal_group_row, encode_wal_long_row, LongOccupant, WalGroupRow, WalLongRow,
};

const UUID: &str = "0f1e2d3c-4b5a-4968-8776-655443322110";
const ANCHOR: i64 = 1_700_000_000_000_000_000;
const SECOND: u64 = 1_000_000_000;
/// Wake times are offsets from this, half an interval past a boundary so
/// `seq` does not sit on an edge.
const BASE: u64 = 1_700_000_000_000_000_000 + SECOND / 2;

fn subscription() -> StreamSubscription {
    StreamSubscription::for_source(
        StreamRequest::default(),
        Duration::from_secs(1),
        UUID.to_string(),
        ANCHOR,
        [("source".to_string(), "rezolus".to_string())].into(),
        [(dendro::keys::PRODUCER_EPOCH.to_string(), UUID.to_string())].into(),
    )
}

fn schema(n: usize) -> GroupSchema {
    GroupSchema {
        counters: (0..n)
            .map(|i| MetricDesc {
                name: format!("0x{i}"),
                metadata: [("metric".to_string(), format!("counter_{i}"))].into(),
            })
            .collect(),
        gauges: Vec::new(),
        histograms: Vec::new(),
    }
}

/// One group of a pass: its stream, window, counter count, and the value its
/// counters start from.
struct G {
    stream: &'static str,
    window: Option<(u64, u64)>,
    n: usize,
    base: u64,
}

fn g(stream: &'static str, window: Option<(u64, u64)>, n: usize, base: u64) -> G {
    G {
        stream,
        window,
        n,
        base,
    }
}

fn wide_row(g: &G) -> EncodedGroup {
    let schema = schema(g.n);
    EncodedGroup {
        stream: g.stream.to_string(),
        window: g.window,
        schema_hash: schema.hash(),
        row: encode_wal_group_row(&WalGroupRow {
            schema_hash: schema.hash(),
            schema: None,
            window: g.window,
            counters: (0..g.n).map(|i| Some(g.base + i as u64)).collect(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        })
        .unwrap(),
        schema: Some(Arc::new(schema)),
    }
}

/// A windowed group in the long form, with `g.n` occupants of one counter
/// each. A windowless group is built wide, so the sequence mixes both forms.
fn long_row(g: &G) -> EncodedStreamGroup {
    if g.window.is_none() {
        return EncodedStreamGroup::Wide(wide_row(g));
    }
    let columns = schema(1);
    let keys: Vec<u64> = (0..g.n as u64).map(|i| 0x100 + i).collect();
    let row = encode_wal_long_row(&WalLongRow {
        schema_hash: columns.hash(),
        schema: None,
        window: g.window,
        occupants: keys
            .iter()
            .map(|&key| LongOccupant {
                occupant: key,
                counters: vec![Some(g.base + key)],
                gauges: Vec::new(),
                histograms: Vec::new(),
            })
            .collect(),
    })
    .unwrap();
    EncodedStreamGroup::Long(EncodedLongGroup {
        stream: g.stream.to_string(),
        window: g.window,
        columns_hash: columns.hash(),
        columns: Arc::new(columns),
        occupants: keys
            .iter()
            .map(|&key| {
                let labels: BTreeMap<String, String> = [("id".to_string(), key.to_string())].into();
                (key, Arc::new(labels))
            })
            .collect(),
        row,
    })
}

/// `(wall_ns, ts, wall_offset)`.
type Stamp = (u64, i64, i64);

/// The sequence, as `(wake time, pass)`:
///
/// 1. First pass: `cpu/a` and `cpu/b` windowed, `external/main` windowless.
///    All sent.
/// 2. The same pass again (same stamp): an empty frame.
/// 3. A new pass where `cpu/a`'s window did not advance and `cpu/b`'s did:
///    `cpu/b` and `external/main` sent.
/// 4. No pass: an empty frame.
/// 5. Two seconds later, so `seq` skips one: both windows advanced, and
///    `cpu/b` has one more counter (wide: a new schema; long: a new
///    occupant).
/// 6. The wall clock steps back half a second, same pass: an empty frame
///    whose `seq` is held at the previous one plus one.
fn sequence<R>(row: fn(&G) -> R) -> Vec<(u64, Option<(PassStamp, Vec<R>)>)> {
    let pass = |stamp: Stamp, groups: &[G]| {
        (
            PassStamp {
                wall_ns: stamp.0,
                ts: stamp.1,
                wall_offset: stamp.2,
            },
            groups.iter().map(row).collect::<Vec<R>>(),
        )
    };
    let s1: Stamp = (BASE + SECOND - 10_000, ANCHOR + 1_000_000, 7);
    let s2: Stamp = (BASE + 3 * SECOND - 10_000, ANCHOR + 2_000_000, 7);
    let s3: Stamp = (BASE + 6 * SECOND - 10_000, ANCHOR + 3_000_000, -3);
    let p1 = || {
        pass(
            s1,
            &[
                g("cpu/a", Some((100, 200)), 2, 10),
                g("cpu/b", Some((100, 210)), 2, 20),
                g("external/main", None, 1, 30),
            ],
        )
    };
    let p2 = pass(
        s2,
        &[
            g("cpu/a", Some((100, 200)), 2, 10),
            g("cpu/b", Some((210, 320)), 2, 21),
            g("external/main", None, 1, 31),
        ],
    );
    let p3 = || {
        pass(
            s3,
            &[
                g("cpu/a", Some((200, 400)), 2, 12),
                g("cpu/b", Some((320, 430)), 3, 22),
                g("external/main", None, 1, 32),
            ],
        )
    };
    vec![
        (BASE + SECOND, Some(p1())),
        (BASE + 2 * SECOND, Some(p1())),
        (BASE + 3 * SECOND, Some(p2)),
        (BASE + 4 * SECOND, None),
        (BASE + 6 * SECOND, Some(p3())),
        (BASE + 6 * SECOND - SECOND / 2 - 1, Some(p3())),
    ]
}

fn chunks<R>(row: fn(&G) -> R) -> Vec<Bytes>
where
    R: metriken_exposition::stream::StreamRow + Send + Sync + 'static,
{
    let mut sub = subscription();
    let mut out = vec![sub.opening().unwrap()];
    for (wall_ns, pass) in sequence(row) {
        let now = SystemTime::UNIX_EPOCH + Duration::from_nanos(wall_ns);
        out.push(sub.frame_at(pass.as_ref(), now).unwrap());
    }
    out
}

fn serialize(chunks: &[Bytes]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out
}

/// `(seq, streams)` of each rows frame.
fn decoded(chunks: &[Bytes]) -> Vec<(u64, Vec<String>)> {
    let body: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let mut reader = dendro::replicate::wire::FrameReader::new(std::io::Cursor::new(body))
        .expect("the preamble reads");
    let mut out = Vec::new();
    while let Some(frame) = reader.next_frame().expect("decodable") {
        if let dendro::replicate::Frame::Rows { seq, rows, .. } = frame {
            out.push((seq, rows.into_iter().map(|r| r.stream).collect()));
        }
    }
    out
}

fn check(name: &str, chunks: Vec<Bytes>, expected: &[(u64, &[&str])]) {
    let first = (BASE + SECOND) / SECOND;
    let expected: Vec<(u64, Vec<String>)> = expected
        .iter()
        .map(|(seq, streams)| (first + seq, streams.iter().map(|s| s.to_string()).collect()))
        .collect();
    assert_eq!(
        decoded(&chunks),
        expected,
        "{name}: frames by seq and stream"
    );

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/stream-frames")
        .join(format!("{name}.bin"));
    let fixture = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let bytes = serialize(&chunks);
    assert!(
        fixture == bytes,
        "{name}: frames differ from {} ({} bytes produced, {} in the fixture)",
        path.display(),
        bytes.len(),
        fixture.len()
    );
}

#[test]
fn wide_frames_match_the_fixture() {
    check(
        "wide",
        chunks(wide_row),
        &[
            (0, &["cpu/a", "cpu/b", "external/main"]),
            (1, &[]),
            (2, &["cpu/b", "external/main"]),
            (3, &[]),
            (5, &["cpu/a", "cpu/b", "external/main"]),
            (6, &[]),
        ],
    );
}

#[test]
fn long_frames_match_the_fixture() {
    check(
        "long",
        chunks(long_row),
        &[
            (
                0,
                &[
                    "cpu/a/occupants",
                    "cpu/a",
                    "cpu/b/occupants",
                    "cpu/b",
                    "external/main",
                ],
            ),
            (1, &[]),
            (2, &["cpu/b", "external/main"]),
            (3, &[]),
            (5, &["cpu/a", "cpu/b/occupants", "cpu/b", "external/main"]),
            (6, &[]),
        ],
    );
}

/// An empty frame from `empty_frame_at` is the frame `frame_at` sends for a
/// missing pass.
#[test]
fn empty_frame_at_is_frame_at_without_a_pass() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_nanos(BASE);
    let mut a = subscription();
    let mut b = subscription();
    assert_eq!(
        a.empty_frame_at(now).unwrap(),
        b.frame_at::<(PassStamp, Vec<EncodedGroup>)>(None, now)
            .unwrap()
    );
}
