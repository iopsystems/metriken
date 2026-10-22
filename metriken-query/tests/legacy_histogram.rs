//! Artifacts from the published releases read as counting histograms
//! through this crate's readers: a parquet file written by metriken-storage
//! 0.4.0 (`metric_type = histogram`) through `ParquetReader`, and msgpack
//! snapshots encoded by metriken-model 0.1.3 through
//! `MemoryStore::ingest_snapshot`. The fixtures and how they were made are
//! described in `metriken-storage/tests/legacy_histogram.rs`.

use std::path::{Path, PathBuf};

use metriken_query::{MetricsSource, ParquetReader, QueryResult};

const BASE_S: f64 = 1_700_000_000.0;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/legacy")
        .join(name)
}

fn values(result: QueryResult) -> Vec<(f64, f64)> {
    match result {
        QueryResult::Matrix { result } => {
            assert_eq!(result.len(), 1, "{result:?}");
            result.into_iter().next().unwrap().values
        }
        other => panic!("{other:?}"),
    }
}

fn one_a_tick() -> Vec<(f64, f64)> {
    (1..=3).map(|t| (BASE_S + t as f64, 1.0)).collect()
}

#[test]
fn a_legacy_parquet_file_reads_as_counting() {
    let reader = ParquetReader::open(fixture("legacy-0.4.0.parquet")).unwrap();
    assert_eq!(reader.histogram_names(), ["lat"]);
    assert!(MetricsSource::gauge_histogram_names(&reader).is_empty());
    let got = reader
        .query_range("histogram_count(lat)", BASE_S + 1.0, BASE_S + 3.0, 1.0)
        .unwrap();
    assert_eq!(values(got), one_a_tick());
}

#[cfg(feature = "ingest")]
#[test]
fn legacy_snapshots_ingest_as_counting() {
    let store = metriken_query::MemoryStore::builder()
        .sampling_interval_ms(1000)
        .build();
    let bytes = std::fs::read(fixture("legacy-v3.msgpack")).unwrap();
    let mut rest: &[u8] = &bytes;
    let mut n = 0;
    while !rest.is_empty() {
        let mut d = rmp_serde::Deserializer::new(&mut rest);
        let s: metriken_model::Snapshot = serde::Deserialize::deserialize(&mut d).unwrap();
        store.ingest_snapshot(s);
        n += 1;
    }
    assert_eq!(n, 4);
    assert_eq!(store.histogram_names(), ["lat"]);
    assert!(store.gauge_histogram_names().is_empty());
    let got = store
        .query_range("histogram_count(lat)", BASE_S + 1.0, BASE_S + 3.0, 1.0)
        .unwrap();
    assert_eq!(values(got), one_a_tick());
}
