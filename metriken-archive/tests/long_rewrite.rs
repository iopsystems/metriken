//! Prototype: rewrite one wide table of a dendro archive through the writer,
//! long and wide, to compare how they query. Ignored; needs an archive:
//! `LONG_REWRITE_SRC=in.dendro LONG_REWRITE_TABLE=cpu_usage/cpu_usage_task
//! LONG_REWRITE_OUT=dir cargo test --release -p metriken-archive --all-features
//! --test long_rewrite -- --ignored --nocapture`. Writes `dir/long.dendro` and
//! `dir/wide.dendro`, each holding only that table.
#![cfg(feature = "write")]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use metriken_archive::{ArchiveWriter, Catalog, DendroCatalog, WriterConfig};
use metriken_exposition::{GroupSchema, GroupSnapshot, MetricDesc, Snapshot, SnapshotV3};
use metriken_storage::table::{read_table_parquet, Values};

fn rewrite(src: &DendroCatalog, table: &str, out: &Path, long_groups: bool) {
    let source = &src.sources().unwrap()[0];
    let config = WriterConfig {
        long_groups,
        ..WriterConfig::default()
    };
    let mut writer = ArchiveWriter::create(out, config).unwrap();
    let mut recorder = writer
        .add_source(
            source.labels.clone(),
            source.metadata.clone(),
            source.clock_anchor_wall_ns,
        )
        .unwrap();
    let (mut last, mut rows, mut previous_hash) = (0u64, 0usize, None);
    for (seq, _) in src.segment_meta(source.id, table).unwrap() {
        let bytes = src.segment_bytes(source.id, table, seq).unwrap().unwrap();
        let t = read_table_parquet(table.to_string(), bytes).unwrap();
        let mut schema = GroupSchema::default();
        let mut values = Vec::new();
        for c in &t.columns {
            let Values::Counter(v) = &c.values else {
                panic!("prototype handles counter columns only: {}", c.name);
            };
            schema.counters.push(MetricDesc {
                name: c.name.clone(),
                metadata: c.metadata.clone().into_iter().collect(),
            });
            values.push(v);
        }
        let schema = Arc::new(schema);
        let hash = schema.hash();
        for (row, ts) in t.timestamps.iter().enumerate() {
            let window = t
                .table_window
                .as_ref()
                .and_then(|w| w[row])
                .map(|w| metriken::Window::new(w.begin_ns, w.end_ns));
            let group = GroupSnapshot {
                name: table.to_string(),
                schema_hash: hash,
                schema: (previous_hash != Some(hash)).then(|| Arc::clone(&schema)),
                window,
                counters: values.iter().map(|v| v[row]).collect(),
                gauges: Vec::new(),
                histograms: Vec::new(),
            };
            previous_hash = Some(hash);
            let snapshot = Snapshot::V3(SnapshotV3 {
                systemtime: UNIX_EPOCH + Duration::from_nanos(*ts),
                duration: Duration::ZERO,
                metadata: Default::default(),
                groups: vec![group],
            });
            let wall_offset = t.wall_offsets.get(row).copied().unwrap_or(0);
            let staged = recorder.stage(&snapshot, *ts, wall_offset).unwrap();
            writer.commit(vec![staged]).unwrap();
            recorder.maybe_seal().unwrap();
            last = *ts;
            rows += 1;
        }
    }
    recorder.finalize((last, 0)).unwrap();
    writer.join().unwrap();
    println!(
        "REWRITE {} rows, long={long_groups}, {} bytes",
        rows,
        std::fs::metadata(out).unwrap().len()
    );
}

#[test]
#[ignore]
fn long_rewrite() {
    let (Ok(src), Ok(out)) = (
        std::env::var("LONG_REWRITE_SRC"),
        std::env::var("LONG_REWRITE_OUT"),
    ) else {
        return;
    };
    let table = std::env::var("LONG_REWRITE_TABLE")
        .unwrap_or_else(|_| "cpu_usage/cpu_usage_task".to_string());
    let catalog = DendroCatalog::open(Path::new(&src)).unwrap();
    for long in [true, false] {
        let path = Path::new(&out).join(if long { "long.dendro" } else { "wide.dendro" });
        let _ = std::fs::remove_file(&path);
        let t = Instant::now();
        rewrite(&catalog, &table, &path, long);
        println!("REWRITE long={long} took {:?}", t.elapsed());
    }
}
