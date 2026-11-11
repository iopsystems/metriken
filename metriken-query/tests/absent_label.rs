//! A label a series does not carry matches as the empty string, as in
//! Prometheus, through each storage backend's own filter path: a parquet
//! file, a segmented archive and the in-memory store.

use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use metriken_query::{
    MemoryStore, MetricsSource, ParquetReader, QueryResult, SegmentedParquetReader,
};
use metriken_storage::BufferPool;
use parquet::arrow::ArrowWriter;

const S: u64 = 1_000_000_000;
const ROWS: u64 = 5;

/// The series of gauge `x`: each label set with its constant value. Two
/// carry `l`, two do not.
const SERIES: [(&[(&str, &str)], i64); 4] = [
    (&[("l", "a")], 1),
    (&[("l", "b")], 2),
    (&[], 3),
    (&[("m", "z")], 4),
];

/// Each query with the values of the series it selects, in order.
const CASES: [(&str, &[i64]); 7] = [
    (r#"x{l=""}"#, &[3, 4]),
    (r#"x{l!=""}"#, &[1, 2]),
    (r#"x{l=~"a|"}"#, &[1, 3, 4]),
    (r#"x{l!~".*"}"#, &[]),
    (r#"x{l!="a"}"#, &[2, 3, 4]),
    // With `or`: a matcher both branches share (`m=""`) is part of the
    // read's filter, and each branch is applied to what it reads.
    (r#"x{l="", m="" or l="b", m=""}"#, &[2, 3]),
    (r#"x{l="" or l="a"}"#, &[1, 3, 4]),
];

/// A parquet table holding [`SERIES`], one gauge column each, `ROWS` rows
/// one second apart.
fn table() -> Vec<u8> {
    let mut fields = vec![Field::new("timestamp", DataType::UInt64, false)];
    let mut columns: Vec<ArrayRef> = vec![Arc::new(UInt64Array::from(
        (1..=ROWS).map(|i| i * S).collect::<Vec<_>>(),
    ))];
    for (i, (labels, value)) in SERIES.iter().enumerate() {
        let mut meta = std::collections::HashMap::new();
        meta.insert("metric".to_string(), "x".to_string());
        meta.insert("metric_type".to_string(), "gauge".to_string());
        for (k, v) in *labels {
            meta.insert(k.to_string(), v.to_string());
        }
        fields.push(Field::new(format!("x__{i}"), DataType::Int64, true).with_metadata(meta));
        columns.push(Arc::new(Int64Array::from(vec![*value; ROWS as usize])));
    }
    let schema = Arc::new(Schema::new(fields));
    let mut buf = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), None).unwrap();
    writer
        .write(&RecordBatch::try_new(schema, columns).unwrap())
        .unwrap();
    writer.close().unwrap();
    buf
}

/// The values of the series `query` selects, sorted.
fn selected(source: &dyn MetricsSource, query: &str) -> Vec<i64> {
    let result = source
        .query_range(query, 1.0, ROWS as f64, 1.0)
        .unwrap_or_else(|e| panic!("{query}: {e}"));
    let QueryResult::Matrix { result, .. } = result else {
        panic!("{query}: not a matrix");
    };
    let mut values: Vec<i64> = result
        .iter()
        .map(|m| m.values.last().expect("a point").1 as i64)
        .collect();
    values.sort();
    values
}

fn check(backend: &str, source: &dyn MetricsSource) {
    for (query, want) in CASES {
        assert_eq!(selected(source, query), want, "{backend}: {query}");
    }
}

#[test]
fn parquet_matches_an_absent_label_as_empty() {
    check("parquet", &ParquetReader::open_bytes(table()).unwrap());
}

#[test]
fn a_segmented_archive_matches_an_absent_label_as_empty() {
    let pool = BufferPool::new(16 << 20);
    let reader = SegmentedParquetReader::open_bytes_with_pool(vec![table()], pool).unwrap();
    check("segmented", &reader);
}

#[test]
fn memory_matches_an_absent_label_as_empty() {
    let store = MemoryStore::builder().sampling_interval_ms(1000).build();
    for (labels, value) in SERIES {
        let labels: Vec<(&str, &str)> = labels.to_vec();
        store
            .insert_gauge_series(
                "x",
                metriken_query::Labels::from(labels.as_slice()),
                (1..=ROWS).map(|i| i * S).collect(),
                vec![value; ROWS as usize],
                None,
            )
            .unwrap();
    }
    check("memory", &store);
}
