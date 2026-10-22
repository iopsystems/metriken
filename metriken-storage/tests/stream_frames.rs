//! GO check 1 of `docs/journal/2026-10-11-stream-route.md` with rezolus's own
//! wide pass type: `AgentRows` of `AgentRow`s through a `StreamSubscription`
//! give, byte for byte, the frames rezolus's agent sent for the same
//! sequence (metriken-exposition's `tests/fixtures/stream-frames/wide.bin`,
//! whose test describes the sequence and the file format).
#![cfg(all(feature = "rez", feature = "stream"))]

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use metriken_storage::rez::wire::{AgentRow, AgentRows};
use metriken_storage::schema::{GroupSchema, MetricDesc};
use metriken_storage::stream::{StreamRequest, StreamSubscription};
use metriken_storage::wal::{encode_wal_group_row, WalGroupRow};

const UUID: &str = "0f1e2d3c-4b5a-4968-8776-655443322110";
const ANCHOR: i64 = 1_700_000_000_000_000_000;
const SECOND: u64 = 1_000_000_000;
const BASE: u64 = 1_700_000_000_000_000_000 + SECOND / 2;

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
        gauge_histograms: Vec::new(),
    }
}

/// `(stream, window, counter count, first value)`.
type G = (&'static str, Option<(u64, u64)>, usize, u64);

fn row(&(stream, window, n, base): &G) -> AgentRow {
    let schema = schema(n);
    let row = encode_wal_group_row(&WalGroupRow {
        schema_hash: schema.hash(),
        schema: None,
        window,
        counters: (0..n).map(|i| Some(base + i as u64)).collect(),
        gauges: Vec::new(),
        histograms: Vec::new(),
        gauge_histograms: Vec::new(),
    })
    .unwrap();
    AgentRow {
        stream: stream.to_string(),
        window,
        schema_hash: schema.hash(),
        schema: Some(Arc::new(schema)),
        arity: (n as u32, 0, 0),
        approx_bytes: row.len() as u32,
        row,
    }
}

fn pass((wall_ns, ts, wall_offset): (u64, i64, i64), groups: &[G]) -> AgentRows {
    AgentRows {
        wall_ns,
        ts,
        wall_offset,
        duration_ns: 1_000,
        rows: groups.iter().map(row).collect(),
    }
}

#[test]
fn agent_rows_frames_match_the_wide_fixture() {
    let s1 = (BASE + SECOND - 10_000, ANCHOR + 1_000_000, 7);
    let s2 = (BASE + 3 * SECOND - 10_000, ANCHOR + 2_000_000, 7);
    let s3 = (BASE + 6 * SECOND - 10_000, ANCHOR + 3_000_000, -3);
    let g1: [G; 3] = [
        ("cpu/a", Some((100, 200)), 2, 10),
        ("cpu/b", Some((100, 210)), 2, 20),
        ("external/main", None, 1, 30),
    ];
    let g2: [G; 3] = [
        ("cpu/a", Some((100, 200)), 2, 10),
        ("cpu/b", Some((210, 320)), 2, 21),
        ("external/main", None, 1, 31),
    ];
    let g3: [G; 3] = [
        ("cpu/a", Some((200, 400)), 2, 12),
        ("cpu/b", Some((320, 430)), 3, 22),
        ("external/main", None, 1, 32),
    ];
    let sequence: Vec<(u64, Option<Arc<AgentRows>>)> = vec![
        (BASE + SECOND, Some(Arc::new(pass(s1, &g1)))),
        (BASE + 2 * SECOND, Some(Arc::new(pass(s1, &g1)))),
        (BASE + 3 * SECOND, Some(Arc::new(pass(s2, &g2)))),
        (BASE + 4 * SECOND, None),
        (BASE + 6 * SECOND, Some(Arc::new(pass(s3, &g3)))),
        (
            BASE + 6 * SECOND - SECOND / 2 - 1,
            Some(Arc::new(pass(s3, &g3))),
        ),
    ];

    let mut sub = StreamSubscription::for_source(
        StreamRequest::default(),
        Duration::from_secs(1),
        UUID.to_string(),
        ANCHOR,
        [("source".to_string(), "rezolus".to_string())].into(),
        [(dendro::keys::PRODUCER_EPOCH.to_string(), UUID.to_string())].into(),
    );
    let mut chunks = vec![sub.opening().unwrap()];
    for (wall_ns, pass) in &sequence {
        let now = SystemTime::UNIX_EPOCH + Duration::from_nanos(*wall_ns);
        chunks.push(sub.frame_at(pass.as_ref(), now).unwrap());
    }
    let mut bytes = Vec::new();
    for chunk in &chunks {
        bytes.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        bytes.extend_from_slice(chunk);
    }

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../metriken-exposition/tests/fixtures/stream-frames/wide.bin");
    let fixture = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(
        fixture == bytes,
        "frames differ from {} ({} bytes produced, {} in the fixture)",
        path.display(),
        bytes.len(),
        fixture.len()
    );
}
