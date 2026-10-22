//! GO check 6 of `docs/journal/2026-10-11-stream-route.md`, the long half: a
//! `CounterHistogramGroup` sent long by `GroupBuilder::build_stream` and decoded by
//! `StreamDecoder` gives each member's buckets, under its labels; the writer
//! stages the decoded rows. The wide half is metriken-exposition's
//! `histogram_group` test.
#![cfg(all(feature = "stream", feature = "write"))]

use std::collections::{BTreeMap, HashMap};

use dendro::replicate::Frame;
use metriken::metric;
use metriken_exposition::group_builder::{DefaultRouter, GroupBuilder};
use metriken_storage::stream::{EncodedStreamGroup, FrameProducer, SchemaCache};
use metriken_storage::{
    ArchiveReader, ArchiveWriter, BufferPool, DendroCatalog, StreamDecoder, StreamedGroup,
    WriterConfig,
};

#[metric(name = "stream_hg_latency", metadata = { acq_group = "latency" })]
static LATENCY: metriken::CounterHistogramGroup = metriken::CounterHistogramGroup::new(4, 4, 10);

#[test]
fn a_histogram_group_sent_long_decodes_to_the_same_buckets() {
    LATENCY.set_metadata(0, HashMap::from([("op".to_string(), "read".to_string())]));
    LATENCY.set_metadata(3, HashMap::from([("op".to_string(), "write".to_string())]));
    for v in [1, 5, 9, 200] {
        LATENCY.increment(0, v).unwrap();
    }
    for v in [3, 600] {
        LATENCY.increment(3, v).unwrap();
        // No metadata: not a member.
        LATENCY.increment(1, v).unwrap();
    }

    let mut builder = GroupBuilder::new(DefaultRouter::new("t"));
    let mut schemas = SchemaCache::new();
    let encoded: Vec<EncodedStreamGroup> = builder
        .build_stream(Vec::new())
        .iter()
        .map(|g| EncodedStreamGroup::encode(g, &mut schemas).unwrap())
        .collect();
    assert!(
        encoded
            .iter()
            .any(|g| matches!(g, EncodedStreamGroup::Long(g) if g.stream == "t/latency")),
        "the histogram group is sent long"
    );

    let mut producer = FrameProducer::for_source(
        "00000000-0000-4000-8000-000000000000".to_string(),
        0,
        BTreeMap::new(),
        BTreeMap::new(),
    );
    let Frame::Rows { rows, .. } = producer.interval(&encoded, 1_000, 0, 1) else {
        panic!("a rows frame")
    };
    let decoded = StreamDecoder::new().decode(rows).unwrap();

    let labels: HashMap<u64, BTreeMap<String, String>> = decoded
        .iter()
        .filter_map(|g| match g {
            StreamedGroup::Occupants { table, occupants } if table == "t/latency" => Some(
                occupants
                    .iter()
                    .map(|o| (o.occupant, o.labels.clone()))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    let row = decoded
        .iter()
        .find_map(|g| match g {
            StreamedGroup::Long { name, row } if name == "t/latency" => Some(row),
            _ => None,
        })
        .expect("the long row");

    let columns = row
        .schema
        .as_ref()
        .expect("the first row anchors its columns");
    assert_eq!(columns.histograms.len(), 1);
    assert_eq!(
        columns.histograms[0]
            .metadata
            .get("metric")
            .map(String::as_str),
        Some("stream_hg_latency")
    );

    let mut seen = Vec::new();
    for o in &row.occupants {
        let labels = &labels[&o.occupant];
        let idx: usize = labels["id"].parse().unwrap();
        let (gp, mvp, buckets) = o.histograms[0].as_ref().expect("a loaded member");
        assert_eq!((*gp, *mvp), (4, 10));
        assert_eq!(
            buckets.as_slice(),
            LATENCY.load(idx).unwrap().as_slice(),
            "slot {idx}"
        );
        seen.push((idx, labels.get("op").cloned()));
    }
    seen.sort();
    assert_eq!(
        seen,
        [
            (0, Some("read".to_string())),
            (3, Some("write".to_string()))
        ]
    );

    // The writer stages the decoded rows, and the archive reads them back as
    // one histogram series per member.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hg.dendro");
    let mut writer = ArchiveWriter::create(&path, WriterConfig::default()).unwrap();
    let mut recording = writer
        .add_source(
            [("source".to_string(), "t".to_string())].into(),
            BTreeMap::new(),
            1_000,
        )
        .unwrap();
    let staged = recording.stage_streamed(decoded, 1_000, 0).unwrap();
    writer.commit(vec![staged]).unwrap();
    recording.finalize((1_000, 0)).unwrap();
    writer.join().unwrap();

    let mut recordings = ArchiveReader::from_catalog(
        Box::new(DendroCatalog::open(&path).unwrap()),
        None,
        BufferPool::new(16 << 20),
        None,
    )
    .unwrap();
    let reader = recordings.remove(0).1;
    assert!(
        reader
            .histogram_names()
            .contains(&"stream_hg_latency".to_string()),
        "{:?}",
        reader.histogram_names()
    );
    let mut ops: Vec<Option<String>> = reader
        .histogram_labels("stream_hg_latency")
        .into_iter()
        .map(|l| l.get("op").cloned())
        .collect();
    ops.sort();
    assert_eq!(ops, [Some("read".to_string()), Some("write".to_string())]);
}
