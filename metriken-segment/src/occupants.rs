//! The occupant stream: which labels each occupant number of a long table
//! stands for.
//!
//! A [long](crate::long) table names its rows by occupant number only. The labels live
//! in a parquet stream beside it, `<table>/occupants`: one row per occupant
//! at the tick it is first seen, and one per live occupant at every
//! restatement, so retention never drops a live occupant's labels. See
//! rezolus's `docs/journal/2026-09-25-dendro-archive-layout.md`, "The
//! occupant stream".
//!
//! The stream's format, for writers and readers alike: the WAL row and the
//! segment encoding. `metriken-query`'s `long::OccupantLabels` puts the
//! labels on a long table's series.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, StringBuilder, UInt64Array, UInt64Builder};
use arrow::datatypes::{DataType, Field, Schema, UInt64Type};
use arrow::record_batch::RecordBatch;
use serde::{Deserialize, Serialize};

/// The suffix that makes a stream a table's occupant stream.
pub const SUFFIX: &str = "/occupants";

/// Field metadata key saying how a `UInt64` label column maps back to the
/// label's text: [`DECIMAL`] or [`HEX16`].
const ENCODING_KEY: &str = "label_encoding";
const DECIMAL: &str = "decimal";
/// Sixteen lowercase hex digits, the format rezolus's agent writes
/// `__uid__` in.
const HEX16: &str = "hex16";

/// `table`'s occupant stream.
pub fn stream_of(table: &str) -> String {
    format!("{table}{SUFFIX}")
}

/// The table an occupant stream belongs to, if `stream` is one.
pub fn table_of(stream: &str) -> Option<&str> {
    stream.strip_suffix(SUFFIX)
}

/// One occupant and its labels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occupant {
    pub occupant: u64,
    pub labels: BTreeMap<String, String>,
}

/// A WAL row of an occupant stream: the occupants first seen or restated
/// at one tick, msgpack.
pub fn encode_wal_row(occupants: &[Occupant]) -> Vec<u8> {
    rmp_serde::to_vec(occupants).expect("occupant rows serialize")
}

pub fn decode_wal_row(row: &[u8]) -> Result<Vec<Occupant>, String> {
    rmp_serde::from_slice(row).map_err(|e| format!("decoding an occupant WAL row: {e}"))
}

/// How a label column is stored.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Decimal,
    Hex16,
    Text,
}

fn is_hex16(v: &str) -> bool {
    v.len() == 16
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_decimal(v: &str) -> bool {
    v.parse::<u64>().is_ok_and(|n| n.to_string() == v)
}

/// The narrowest encoding that reproduces every value of a column exactly.
fn encoding_of<'a>(values: impl Iterator<Item = &'a str> + Clone) -> Encoding {
    if values.clone().all(is_decimal) {
        Encoding::Decimal
    } else if values.clone().all(is_hex16) {
        Encoding::Hex16
    } else {
        Encoding::Text
    }
}

/// One segment of an occupant stream: `rows` are `(timestamp, occupant)`,
/// in the order they are to be stored.
///
/// Columns are `timestamp`, `occupant`, and one per label key, in key
/// order. A label column is `UInt64` when every value converts exactly
/// (decimal, or the agent's 16-digit hex), and says which in its field
/// metadata; otherwise `Utf8`. A label an occupant lacks is null.
pub fn encode_segment(
    rows: &[(u64, &Occupant)],
    props: parquet::file::properties::WriterProperties,
) -> Result<Vec<u8>, String> {
    let keys: BTreeSet<&str> = rows
        .iter()
        .flat_map(|(_, o)| o.labels.keys().map(String::as_str))
        .collect();
    let mut fields = vec![
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("occupant", DataType::UInt64, false),
    ];
    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from_iter_values(rows.iter().map(|(t, _)| *t))),
        Arc::new(UInt64Array::from_iter_values(
            rows.iter().map(|(_, o)| o.occupant),
        )),
    ];
    for key in keys {
        let values = rows
            .iter()
            .filter_map(|(_, o)| o.labels.get(key).map(String::as_str));
        match encoding_of(values) {
            Encoding::Text => {
                let mut b = StringBuilder::new();
                for (_, o) in rows {
                    b.append_option(o.labels.get(key));
                }
                fields.push(Field::new(key, DataType::Utf8, true));
                columns.push(Arc::new(b.finish()));
            }
            enc => {
                let mut b = UInt64Builder::new();
                for (_, o) in rows {
                    b.append_option(o.labels.get(key).map(|v| match enc {
                        Encoding::Hex16 => u64::from_str_radix(v, 16).expect("checked hex16"),
                        _ => v.parse().expect("checked decimal"),
                    }));
                }
                let name = if enc == Encoding::Hex16 {
                    HEX16
                } else {
                    DECIMAL
                };
                fields.push(
                    Field::new(key, DataType::UInt64, true).with_metadata(HashMap::from([(
                        ENCODING_KEY.to_string(),
                        name.to_string(),
                    )])),
                );
                columns.push(Arc::new(b.finish()));
            }
        }
    }
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(Arc::clone(&schema), columns).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let mut w =
        parquet::arrow::ArrowWriter::try_new(&mut buf, schema, Some(crate::format::stamped(props)))
            .map_err(|e| e.to_string())?;
    w.write(&batch).map_err(|e| e.to_string())?;
    w.close().map_err(|e| e.to_string())?;
    Ok(buf)
}

/// Every row of an occupant stream segment, as `(timestamp, occupant)`.
pub fn decode_segment(bytes: &[u8]) -> Result<Vec<(u64, Occupant)>, String> {
    let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::copy_from_slice(bytes),
    )
    .map_err(|e| e.to_string())?;
    crate::format::check(builder.metadata().file_metadata().key_value_metadata())?;
    let reader = builder.build().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| e.to_string())?;
        let schema = batch.schema();
        let ts = batch
            .column_by_name("timestamp")
            .and_then(|c| c.as_primitive_opt::<UInt64Type>())
            .ok_or("occupant segment has no UInt64 timestamp")?;
        let occ = batch
            .column_by_name("occupant")
            .and_then(|c| c.as_primitive_opt::<UInt64Type>())
            .ok_or("occupant segment has no UInt64 occupant")?;
        let labels: Vec<(String, ArrayRef, Option<String>)> = schema
            .fields()
            .iter()
            .enumerate()
            .filter(|(_, f)| f.name() != "timestamp" && f.name() != "occupant")
            .map(|(i, f)| {
                (
                    f.name().clone(),
                    Arc::clone(batch.column(i)),
                    f.metadata().get(ENCODING_KEY).cloned(),
                )
            })
            .collect();
        for row in 0..batch.num_rows() {
            let mut l = BTreeMap::new();
            for (key, col, enc) in &labels {
                if col.is_null(row) {
                    continue;
                }
                let v = match col.data_type() {
                    DataType::Utf8 => col.as_string::<i32>().value(row).to_string(),
                    DataType::UInt64 => {
                        let n = col.as_primitive::<UInt64Type>().value(row);
                        match enc.as_deref() {
                            Some(HEX16) => format!("{n:016x}"),
                            _ => n.to_string(),
                        }
                    }
                    other => return Err(format!("occupant label {key} is {other}")),
                };
                l.insert(key.clone(), v);
            }
            out.push((
                ts.value(row),
                Occupant {
                    occupant: occ.value(row),
                    labels: l,
                },
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props() -> parquet::file::properties::WriterProperties {
        parquet::file::properties::WriterProperties::builder()
            .set_compression(parquet::basic::Compression::UNCOMPRESSED)
            .build()
    }

    fn occ(n: u64, pairs: &[(&str, &str)]) -> Occupant {
        Occupant {
            occupant: n,
            labels: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn a_segment_round_trips_every_label_exactly() {
        let a = occ(
            0,
            &[
                ("__uid__", "00ab5eaa0a667340"),
                ("comm", "redis-server"),
                ("pid", "42"),
                ("tgid", "42"),
                ("cgroup", "/system.slice/redis.service"),
            ],
        );
        // No __uid__, a pid with a leading zero (not canonical decimal, so
        // the column must stay text to reproduce it), and an extra key.
        let b = occ(7, &[("comm", "worker"), ("pid", "0043"), ("name", "x")]);
        let rows = [(10u64, &a), (20, &b), (30, &a)];
        let bytes = encode_segment(&rows, props()).unwrap();
        let back = decode_segment(&bytes).unwrap();
        assert_eq!(
            back,
            vec![(10, a.clone()), (20, b.clone()), (30, a.clone())]
        );
    }

    #[test]
    fn integer_columns_are_stored_as_integers() {
        let a = occ(0, &[("__uid__", "a92be5eaa0a66734"), ("pid", "1")]);
        let bytes = encode_segment(&[(1, &a)], props()).unwrap();
        let meta = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
            bytes::Bytes::from(bytes),
        )
        .unwrap();
        let schema = meta.schema();
        assert_eq!(
            schema.field_with_name("__uid__").unwrap().data_type(),
            &DataType::UInt64
        );
        assert_eq!(
            schema.field_with_name("pid").unwrap().data_type(),
            &DataType::UInt64
        );
    }

    #[test]
    fn wal_rows_round_trip() {
        let rows = vec![occ(3, &[("comm", "a")]), occ(4, &[])];
        assert_eq!(decode_wal_row(&encode_wal_row(&rows)).unwrap(), rows);
    }
}
