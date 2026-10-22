//! Artifacts written by the published metriken-storage 0.4.0, whose
//! counting histogram columns say `metric_type = histogram`, read as counting
//! histograms: an archive through `ArchiveReader`, a parquet file through
//! the parquet reader, and a raw msgpack recording through `convert`, which
//! now writes `counter_histogram`.
//!
//! `tests/fixtures/legacy/` was written by a scratch program against the
//! crates.io releases (metriken-storage 0.4.0, metriken-exposition 0.24.0):
//! `ops` (a counter, +10 a tick), `mem_free` (a gauge, 1000 − t) and `lat`
//! (a counting histogram, one more observation of 2 each tick), 1 s apart
//! from 1700000000 s; 12 ticks in the archive, 4 in the msgpack files.
#![cfg(feature = "write")]

use std::path::{Path, PathBuf};

use metriken_query::{MetricsSource, QueryResult};
use metriken_storage::{ArchiveReader, BufferPool, DataSource, DendroCatalog};

const BASE_S: f64 = 1_700_000_000.0;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/legacy")
        .join(name)
}

fn counts(result: QueryResult) -> Vec<(f64, f64)> {
    match result {
        QueryResult::Matrix { result } => {
            assert_eq!(result.len(), 1, "{result:?}");
            result.into_iter().next().unwrap().values
        }
        other => panic!("{other:?}"),
    }
}

/// One observation a tick, read as the difference between rows.
fn one_a_tick(from: u64, to: u64) -> Vec<(f64, f64)> {
    (from..=to).map(|t| (BASE_S + t as f64, 1.0)).collect()
}

#[test]
fn a_legacy_archive_reads_its_histogram_as_counting() {
    // Opened from a copy: opening an archive may write beside it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.dendro");
    std::fs::copy(fixture("legacy-0.4.0.dendro"), &path).unwrap();
    let catalog = DendroCatalog::open(&path).unwrap();
    let mut recordings =
        ArchiveReader::from_catalog(Box::new(catalog), None, BufferPool::new(64 << 20), None)
            .unwrap();
    let reader = recordings.remove(0).1;
    assert_eq!(ArchiveReader::histogram_names(&reader), ["lat"]);
    assert!(ArchiveReader::gauge_histogram_names(&reader).is_empty());
    let got = reader
        .query_range("histogram_count(lat)", BASE_S + 1.0, BASE_S + 11.0, 1.0)
        .unwrap();
    assert_eq!(counts(got), one_a_tick(1, 11));
}

#[test]
fn a_legacy_parquet_file_reads_its_histogram_as_counting() {
    let bytes = std::fs::read(fixture("legacy-0.4.0.parquet")).unwrap();
    let source = metriken_storage::parquet::MultiParquetSource::open_bytes_with_pool(
        bytes,
        BufferPool::new(1 << 20),
    )
    .unwrap();
    assert_eq!(source.histogram_names(), ["lat"]);
    assert!(source.gauge_histogram_names().is_empty());
}

/// `convert` of a legacy recording writes the counting histogram as
/// `counter_histogram`, which reads as counting.
#[cfg(feature = "convert")]
#[test]
fn convert_of_a_legacy_recording_writes_counter_histogram() {
    use parquet::file::reader::FileReader;
    let mut out = Vec::new();
    metriken_storage::convert::MsgpackToParquet::new()
        .convert_file_handle(
            std::fs::File::open(fixture("legacy-v2.msgpack")).unwrap(),
            &mut out,
        )
        .unwrap();
    let file =
        parquet::file::reader::SerializedFileReader::new(bytes::Bytes::from(out.clone())).unwrap();
    let schema = parquet::arrow::parquet_to_arrow_schema(
        file.metadata().file_metadata().schema_descr(),
        file.metadata().file_metadata().key_value_metadata(),
    )
    .unwrap();
    let lat = schema.field_with_name("2:buckets").unwrap();
    assert_eq!(lat.metadata()["metric_type"], "counter_histogram");
    let source = metriken_storage::parquet::MultiParquetSource::open_bytes_with_pool(
        out,
        BufferPool::new(1 << 20),
    )
    .unwrap();
    assert_eq!(source.histogram_names(), ["lat"]);
}

/// A list column's `metric_type` decides its kind: absent, `histogram` and
/// `counter_histogram` are counting; `gauge_histogram` is a gauge
/// histogram; anything else is not read.
#[test]
fn a_list_columns_kind_is_its_metric_type() {
    use std::collections::HashMap;
    use std::sync::Arc;

    use arrow::array::{Array, ArrayRef, ListBuilder, UInt64Array, UInt64Builder};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;

    let kinds: [(&str, Option<&str>); 6] = [
        ("absent", None),
        ("legacy", Some("histogram")),
        ("counting", Some("counter_histogram")),
        ("gauge", Some("gauge_histogram")),
        ("sparse", Some("sparse_histogram")),
        ("future", Some("rate_histogram")),
    ];
    let buckets = histogram::Config::new(0, 4).unwrap().total_buckets();
    let mut fields = vec![Field::new("timestamp", DataType::UInt64, false)];
    let mut arrays: Vec<ArrayRef> = vec![Arc::new(UInt64Array::from(vec![1_000u64]))];
    for (metric, kind) in kinds {
        let mut md: HashMap<String, String> = [
            ("metric", metric),
            ("grouping_power", "0"),
            ("max_value_power", "4"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        if let Some(kind) = kind {
            md.insert("metric_type".to_string(), kind.to_string());
        }
        let mut b = ListBuilder::new(UInt64Builder::new());
        for _ in 0..buckets {
            b.values().append_value(1);
        }
        b.append(true);
        let list = b.finish();
        fields.push(
            Field::new(format!("{metric}:buckets"), list.data_type().clone(), true)
                .with_metadata(md),
        );
        arrays.push(Arc::new(list));
    }
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
    let mut bytes = Vec::new();
    let props = metriken_storage::format::stamped_for(
        parquet::file::properties::WriterProperties::builder().build(),
        &schema,
    );
    let mut w = parquet::arrow::ArrowWriter::try_new(&mut bytes, schema, Some(props)).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    let source = metriken_storage::parquet::MultiParquetSource::open_bytes_with_pool(
        bytes,
        BufferPool::new(1 << 20),
    )
    .unwrap();
    assert_eq!(source.histogram_names(), ["absent", "counting", "legacy"]);
    assert_eq!(source.gauge_histogram_names(), ["gauge"]);
}
