//! Building a [long](crate::long) segment: one row per (tick, occupant), one
//! column per metric, the occupant number in `occupant`, and the footer
//! markers `metriken-query` reads a long segment by.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, Int64Array, ListBuilder, UInt64Array, UInt64Builder};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::file::metadata::KeyValue;

use crate::long::{
    encode_occupant_ranges, LAYOUT_KEY, LAYOUT_LONG, OCCUPANTS_KEY, OCCUPANT_COLUMN,
};
use crate::schema::{GroupSchema, MetricDesc};
use crate::table::{
    segment_writer_props, WALL_OFFSET_COLUMN, WINDOW_BEGIN_COLUMN, WINDOW_WIDTH_COLUMN,
};
use crate::wal::LongOccupant;
use crate::window::Window;

/// One metric's column in a long table.
enum LongValues {
    Counter(Vec<Option<u64>>),
    Gauge(Vec<Option<i64>>),
    /// `(grouping_power, max_value_power, buckets)`.
    Histogram(Vec<Option<(u8, u8, Vec<u64>)>>),
}

struct LongColumn {
    name: String,
    metadata: HashMap<String, String>,
    values: LongValues,
}

impl LongColumn {
    fn pad(&mut self, to: usize) {
        match &mut self.values {
            LongValues::Counter(v) => v.resize(to, None),
            LongValues::Gauge(v) => v.resize(to, None),
            LongValues::Histogram(v) => v.resize(to, None),
        }
    }
}

/// A growing long table. Rows arrive a tick at a time, one per occupant
/// present; a metric absent for an occupant is null in that row. Columns
/// are keyed by metric name, so a schema change mid-segment (a metric added
/// or removed) pads the same way `GroupTableBuilder` does.
pub struct LongTableBuilder {
    timestamps: Vec<u64>,
    wall_offsets: Vec<i64>,
    windows: Vec<Option<Window>>,
    occupants: Vec<u64>,
    order: Vec<String>,
    columns: HashMap<String, LongColumn>,
}

impl Default for LongTableBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl LongTableBuilder {
    pub fn new() -> Self {
        Self {
            timestamps: Vec::new(),
            wall_offsets: Vec::new(),
            windows: Vec::new(),
            occupants: Vec::new(),
            order: Vec::new(),
            columns: HashMap::new(),
        }
    }

    /// Rows pushed so far.
    pub fn rows(&self) -> usize {
        self.timestamps.len()
    }

    fn column(
        &mut self,
        desc: &MetricDesc,
        metric_type: &str,
        empty: LongValues,
    ) -> &mut LongColumn {
        if !self.columns.contains_key(&desc.name) {
            let mut metadata: HashMap<String, String> = desc
                .metadata
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            metadata
                .entry("metric_type".to_string())
                .or_insert_with(|| metric_type.to_string());
            self.order.push(desc.name.clone());
            self.columns.insert(
                desc.name.clone(),
                LongColumn {
                    name: desc.name.clone(),
                    metadata,
                    values: empty,
                },
            );
        }
        self.columns.get_mut(&desc.name).expect("inserted above")
    }

    /// One tick: a row per occupant, the metric set described by `schema`
    /// (the fixed descriptors of the metric columns) and each occupant's
    /// values in `schema`'s order.
    pub fn push_tick(
        &mut self,
        ts: u64,
        wall_offset_ns: i64,
        window: Option<Window>,
        schema: &GroupSchema,
        occupants: &[LongOccupant],
    ) {
        for occ in occupants {
            let row = self.timestamps.len();
            self.timestamps.push(ts);
            self.wall_offsets.push(wall_offset_ns);
            self.windows.push(window);
            self.occupants.push(occ.occupant);
            for (desc, v) in schema.counters.iter().zip(&occ.counters) {
                let col = self.column(desc, "counter", LongValues::Counter(Vec::new()));
                col.pad(row);
                if let LongValues::Counter(vs) = &mut col.values {
                    vs.push(*v);
                }
            }
            for (desc, v) in schema.gauges.iter().zip(&occ.gauges) {
                let col = self.column(desc, "gauge", LongValues::Gauge(Vec::new()));
                col.pad(row);
                if let LongValues::Gauge(vs) = &mut col.values {
                    vs.push(*v);
                }
            }
            for (desc, v) in schema.histograms.iter().zip(&occ.histograms) {
                let col = self.column(desc, "histogram", LongValues::Histogram(Vec::new()));
                if let Some((gp, mvp, _)) = v {
                    col.metadata
                        .entry("grouping_power".to_string())
                        .or_insert_with(|| gp.to_string());
                    col.metadata
                        .entry("max_value_power".to_string())
                        .or_insert_with(|| mvp.to_string());
                }
                col.pad(row);
                if let LongValues::Histogram(vs) = &mut col.values {
                    vs.push(v.clone());
                }
            }
        }
    }

    /// Encode the table as a long segment. With `sort`, rows are ordered by
    /// `(occupant, timestamp)`, which lets a single-occupant read decode only
    /// its pages; otherwise they stay in arrival order.
    pub fn finish(self, sort: bool) -> Result<Vec<u8>, crate::table::Error> {
        self.finish_with(sort, segment_writer_props())
    }

    /// [`finish`](Self::finish) with the caller's writer properties, for a
    /// writer that seals with another codec. The layout markers are added to
    /// whatever key-value metadata `props` carries.
    pub fn finish_with(
        mut self,
        sort: bool,
        props: parquet::file::properties::WriterProperties,
    ) -> Result<Vec<u8>, crate::table::Error> {
        let rows = self.timestamps.len();
        for name in &self.order {
            self.columns.get_mut(name).expect("ordered").pad(rows);
        }
        let mut order: Vec<usize> = (0..rows).collect();
        if sort {
            order.sort_by_key(|&i| (self.occupants[i], self.timestamps[i]));
        }
        let pick = |v: &[u64]| -> Vec<u64> { order.iter().map(|&i| v[i]).collect() };
        let ts = pick(&self.timestamps);
        let mut fields = vec![
            Field::new("timestamp", DataType::UInt64, false),
            Field::new(WALL_OFFSET_COLUMN, DataType::Int64, true),
            Field::new(WINDOW_BEGIN_COLUMN, DataType::Int64, true),
            Field::new(WINDOW_WIDTH_COLUMN, DataType::UInt64, true),
            Field::new(OCCUPANT_COLUMN, DataType::UInt64, false),
        ];
        let mut arrays: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(ts.clone())),
            Arc::new(Int64Array::from_iter_values(
                order.iter().map(|&i| self.wall_offsets[i]),
            )),
            Arc::new(Int64Array::from_iter(order.iter().zip(&ts).map(
                |(&i, &t)| self.windows[i].map(|w| w.begin_ns as i64 - t as i64),
            ))),
            Arc::new(UInt64Array::from_iter(
                order.iter().map(|&i| self.windows[i].map(|w| w.width_ns())),
            )),
            Arc::new(UInt64Array::from(pick(&self.occupants))),
        ];
        for name in &self.order {
            let col = &self.columns[name];
            match &col.values {
                LongValues::Counter(v) => {
                    fields.push(
                        Field::new(&col.name, DataType::UInt64, true)
                            .with_metadata(col.metadata.clone()),
                    );
                    arrays.push(Arc::new(UInt64Array::from_iter(
                        order.iter().map(|&i| v[i]),
                    )));
                }
                LongValues::Gauge(v) => {
                    fields.push(
                        Field::new(&col.name, DataType::Int64, true)
                            .with_metadata(col.metadata.clone()),
                    );
                    arrays.push(Arc::new(Int64Array::from_iter(order.iter().map(|&i| v[i]))));
                }
                LongValues::Histogram(v) => {
                    let mut b = ListBuilder::new(UInt64Builder::new());
                    for &i in &order {
                        match &v[i] {
                            Some((_, _, buckets)) => {
                                b.values().append_slice(buckets);
                                b.append(true);
                            }
                            None => b.append(false),
                        }
                    }
                    let arr = b.finish();
                    fields.push(
                        Field::new(
                            format!("{}:buckets", col.name),
                            arr.data_type().clone(),
                            true,
                        )
                        .with_metadata(col.metadata.clone()),
                    );
                    arrays.push(Arc::new(arr));
                }
            }
        }
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(Arc::clone(&schema), arrays)?;
        let kv = vec![
            KeyValue::new(LAYOUT_KEY.to_string(), LAYOUT_LONG.to_string()),
            KeyValue::new(
                OCCUPANTS_KEY.to_string(),
                encode_occupant_ranges(self.occupants.iter().copied()),
            ),
        ];
        let props = props
            .into_builder()
            .set_key_value_metadata(Some(kv))
            .build();
        let mut buf = Vec::new();
        let mut w = ArrowWriter::try_new(&mut buf, schema, Some(props))?;
        w.write(&batch)?;
        w.close()?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::{
        decode_wal_long_row, encode_wal_long_row, materialize_long_wal_tail, WalLongRow,
        WalRowSource,
    };
    use arrow::array::AsArray;
    use arrow::datatypes::UInt64Type;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    fn schema() -> GroupSchema {
        let d = |n: &str, m: &str| MetricDesc {
            name: n.to_string(),
            metadata: [("metric".to_string(), m.to_string())]
                .into_iter()
                .collect(),
        };
        GroupSchema {
            counters: vec![d("7", "cpu")],
            gauges: vec![d("8", "depth")],
            histograms: vec![d("9", "lat")],
        }
    }

    fn occ(n: u64, v: u64) -> LongOccupant {
        LongOccupant {
            occupant: n,
            counters: vec![Some(v)],
            gauges: vec![n.is_multiple_of(2).then_some(v as i64)],
            histograms: vec![Some((2, 8, vec![v, 0, 1]))],
        }
    }

    struct Row(u64, Vec<u8>);
    impl WalRowSource for Row {
        fn ts(&self) -> u64 {
            self.0
        }
        fn wall_offset(&self) -> i64 {
            0
        }
        fn row(&self) -> &[u8] {
            &self.1
        }
    }

    fn read(bytes: &[u8]) -> (RecordBatch, HashMap<String, String>) {
        let b =
            ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(bytes)).unwrap();
        let kv: HashMap<String, String> = b
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .map(|e| (e.key.clone(), e.value.clone().unwrap_or_default()))
            .collect();
        let batches: Vec<RecordBatch> = b.build().unwrap().map(|r| r.unwrap()).collect();
        (
            arrow::compute::concat_batches(&batches[0].schema(), &batches).unwrap(),
            kv,
        )
    }

    #[test]
    fn a_long_segment_carries_its_markers_and_every_row() {
        let mut b = LongTableBuilder::new();
        b.push_tick(
            10,
            0,
            Some(Window::new(5, 10)),
            &schema(),
            &[occ(0, 1), occ(3, 2)],
        );
        b.push_tick(
            20,
            0,
            Some(Window::new(15, 20)),
            &schema(),
            &[occ(3, 4), occ(1, 5)],
        );
        let (batch, kv) = read(&b.finish(true).unwrap());
        assert_eq!(kv[LAYOUT_KEY], LAYOUT_LONG);
        assert_eq!(kv[OCCUPANTS_KEY], "0-1,3");
        // Sorted by (occupant, timestamp).
        let occs: Vec<u64> = batch
            .column_by_name(OCCUPANT_COLUMN)
            .unwrap()
            .as_primitive::<UInt64Type>()
            .values()
            .to_vec();
        assert_eq!(occs, vec![0, 1, 3, 3]);
        let ts: Vec<u64> = batch
            .column_by_name("timestamp")
            .unwrap()
            .as_primitive::<UInt64Type>()
            .values()
            .to_vec();
        assert_eq!(ts, vec![10, 20, 10, 20]);
        let cpu: Vec<Option<u64>> = batch
            .column_by_name("7")
            .unwrap()
            .as_primitive::<UInt64Type>()
            .iter()
            .collect();
        assert_eq!(cpu, vec![Some(1), Some(5), Some(2), Some(4)]);
        // A gauge absent for an occupant is null in its row.
        assert_eq!(batch.column_by_name("8").unwrap().null_count(), 3);
        let f = batch.schema();
        let lat = f.field_with_name("9:buckets").unwrap();
        assert_eq!(lat.metadata()["grouping_power"], "2");
        assert_eq!(lat.metadata()["metric_type"], "histogram");
    }

    #[test]
    fn arrival_order_is_kept_when_not_sorting() {
        let mut b = LongTableBuilder::new();
        b.push_tick(10, 0, None, &schema(), &[occ(3, 1), occ(0, 2)]);
        let (batch, _) = read(&b.finish(false).unwrap());
        let occs: Vec<u64> = batch
            .column_by_name(OCCUPANT_COLUMN)
            .unwrap()
            .as_primitive::<UInt64Type>()
            .values()
            .to_vec();
        assert_eq!(occs, vec![3, 0]);
    }

    /// A tail materialized from WAL rows is the segment a builder fed the
    /// same ticks produces, and rows before the first anchor are skipped.
    #[test]
    fn a_materialized_tail_matches_the_builder() {
        let s = schema();
        let hash = s.hash();
        let rows = vec![
            Row(
                5,
                encode_wal_long_row(&WalLongRow {
                    schema_hash: (9, 9),
                    schema: None,
                    window: None,
                    occupants: vec![occ(0, 9)],
                })
                .unwrap(),
            ),
            Row(
                10,
                encode_wal_long_row(&WalLongRow {
                    schema_hash: hash,
                    schema: Some(s.clone()),
                    window: Some((5, 10)),
                    occupants: vec![occ(0, 1), occ(3, 2)],
                })
                .unwrap(),
            ),
            Row(
                20,
                encode_wal_long_row(&WalLongRow {
                    schema_hash: hash,
                    schema: None,
                    window: Some((15, 20)),
                    occupants: vec![occ(3, 4)],
                })
                .unwrap(),
            ),
        ];
        assert_eq!(decode_wal_long_row(&rows[1].1).unwrap().occupants.len(), 2);
        let tail = materialize_long_wal_tail("t", &rows, true)
            .unwrap()
            .unwrap();
        assert_eq!((tail.rows, tail.first_ts), (3, 10));
        let mut b = LongTableBuilder::new();
        b.push_tick(10, 0, Some(Window::new(5, 10)), &s, &[occ(0, 1), occ(3, 2)]);
        b.push_tick(20, 0, Some(Window::new(15, 20)), &s, &[occ(3, 4)]);
        assert_eq!(tail.bytes, b.finish(true).unwrap());
    }
}
