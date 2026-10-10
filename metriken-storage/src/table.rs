//! A wide table: a timestamp column and one column per metric (or per
//! metric and slot), with acquisition windows per metric or per table. The
//! shape rezolus's `.rez` segments and dendro group tables have used so far;
//! encoded to and decoded from parquet here.

use std::collections::HashMap;
use std::sync::Arc;

use crate::window::Window;
use arrow::array::{Array, ArrayRef, Int64Array, ListBuilder, UInt64Array, UInt64Builder};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
// The eager reader (`read_table_parquet`) needs these.
use arrow::array::ListArray;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

/// Table-level wall-clock sidecar column. Reserved: the query engine skips a
/// column with exactly this name rather than surfacing it as a metric.
pub const WALL_OFFSET_COLUMN: &str = ":wall_offset";
/// Table-level acquisition-window sidecar columns: a BARE `:window_begin`/
/// `:window_width` pair (no metric-id prefix), applying to every metric in
/// the table. metriken-query's `parse_schema` resolves a metric's window
/// from its own `<m>:window_begin`/`<m>:window_width` sidecar first, falling
/// back to this table-level pair when present, and to no window otherwise.
/// Only a group table (`Table::table_window` is `Some`) emits these; a
/// table of individually windowed metrics keeps the per-metric sidecar
/// shape instead.
pub const WINDOW_BEGIN_COLUMN: &str = ":window_begin";
pub const WINDOW_WIDTH_COLUMN: &str = ":window_width";

/// Per-metric column values for a table (row-aligned with the table's timestamps).
#[derive(Debug, Clone, PartialEq)]
pub enum Values {
    Counter(Vec<Option<u64>>),
    Gauge(Vec<Option<i64>>),
    Histogram(Vec<Option<histogram::Histogram>>),
}

/// One metric column plus its per-row acquisition windows.
#[derive(Debug, Clone)]
pub struct Column {
    /// Column key (the snapshot entry's numeric-id name, e.g. `"5"` / `"5x3"`).
    pub name: String,
    /// Metric identity + annotations (`metric`, `sampler`, labels, `metric_type`).
    pub metadata: HashMap<String, String>,
    pub values: Values,
    pub windows: Vec<Option<Window>>,
}

/// One sampler's table: a timestamp column plus its metric/window columns.
#[derive(Debug, Clone)]
pub struct Table {
    /// Read by the atomic writer only — the streaming writer names segments
    /// from `SealJob::sampler`. See the note on `RezRecorder`.
    #[allow(dead_code)]
    pub sampler: String,
    pub timestamps: Vec<u64>,
    /// Per-row wall-clock observation: the raw `SystemTime` reading minus the
    /// row's (monotonically anchored) timestamp, in nanoseconds. Row-aligned
    /// with `timestamps`, or empty when the table carries no observations (a
    /// table decoded from an archive written before the sidecar existed).
    /// Serialized as the table-level `:wall_offset` column, which the query
    /// engine skips the same way it skips the `:window_*` sidecars.
    pub wall_offsets: Vec<i64>,
    pub columns: Vec<Column>,
    /// Table-level acquisition window, one per row — set for a V3
    /// acquisition-group table, whose members all share ONE window per tick
    /// (`docs/principles.md` principle 18). `None` for a V2-sourced (plain
    /// per-sampler) table, which instead carries a window per (metric, row)
    /// in each column's own `Column::windows`.
    ///
    /// This is what makes `table_to_batch`'s group-table layout a property
    /// of the table/data rather than a global flag: `Some` selects the
    /// single bare `:window_begin`/`:window_width` pair (no per-metric
    /// sidecars); `None` selects the legacy per-metric sidecar layout.
    /// `write_table_parquet` errors if this is `Some` and any column still
    /// carries its own non-empty `windows` — the two shapes are mutually
    /// exclusive by construction, and mixing them would silently drop one.
    pub table_window: Option<Vec<Option<Window>>>,
}

/// What encoding or decoding a table can fail with.
pub type Error = Box<dyn std::error::Error>;

/// Mean row interval hint; `None` when fewer than 2 rows.
///
/// Atomic-writer only: the streaming writer keeps the equivalent running totals
/// per table because its segments are long gone by manifest time. See the note
/// on `RezRecorder`.
#[allow(dead_code)]
pub fn cadence_hint(timestamps: &[u64]) -> Option<u64> {
    if timestamps.len() < 2 {
        return None;
    }
    let span = timestamps.last().unwrap().saturating_sub(timestamps[0]);
    Some(span / (timestamps.len() as u64 - 1))
}

fn window_offset_columns(
    windows: &[Option<Window>],
    ts: &[u64],
) -> (Vec<Option<i64>>, Vec<Option<u64>>) {
    let mut begin = Vec::with_capacity(windows.len());
    let mut width = Vec::with_capacity(windows.len());
    for (w, &t) in windows.iter().zip(ts.iter()) {
        match w {
            Some(win) => {
                begin.push(Some(win.begin_ns as i64 - t as i64));
                width.push(Some(win.width_ns()));
            }
            None => {
                begin.push(None);
                width.push(None);
            }
        }
    }
    (begin, width)
}

fn build_histogram_list(values: &[Option<histogram::Histogram>]) -> ArrayRef {
    let mut b = ListBuilder::new(UInt64Builder::new());
    for v in values {
        match v {
            Some(h) => {
                for &c in h.as_slice() {
                    b.values().append_value(c);
                }
                b.append(true);
            }
            None => b.append(false),
        }
    }
    Arc::new(b.finish())
}

/// Push one metric's value column (counter/gauge/histogram) onto `fields`/
/// `arrays`. Shared by both `table_to_batch` branches — the window sidecar
/// placement differs between them (table-level vs per-metric), but the value
/// column itself never does.
fn push_value_column(fields: &mut Vec<Field>, arrays: &mut Vec<ArrayRef>, col: &Column) {
    match &col.values {
        Values::Counter(v) => {
            fields.push(
                Field::new(&col.name, DataType::UInt64, true).with_metadata(col.metadata.clone()),
            );
            arrays.push(Arc::new(UInt64Array::from(v.clone())));
        }
        Values::Gauge(v) => {
            fields.push(
                Field::new(&col.name, DataType::Int64, true).with_metadata(col.metadata.clone()),
            );
            arrays.push(Arc::new(Int64Array::from(v.clone())));
        }
        Values::Histogram(v) => {
            let arr = build_histogram_list(v);
            fields.push(
                Field::new(
                    format!("{}:buckets", col.name),
                    arr.data_type().clone(),
                    true,
                )
                .with_metadata(col.metadata.clone()),
            );
            arrays.push(arr);
        }
    }
}

/// A table as an Arrow schema and batch, before parquet encoding.
pub fn table_to_batch(table: &Table) -> Result<(Arc<Schema>, RecordBatch), Error> {
    let mut fields: Vec<Field> = Vec::new();
    let mut arrays: Vec<ArrayRef> = Vec::new();

    fields.push(
        Field::new("timestamp", DataType::UInt64, false).with_metadata(HashMap::from([
            ("metric_type".to_string(), "timestamp".to_string()),
            ("unit".to_string(), "nanoseconds".to_string()),
        ])),
    );
    arrays.push(Arc::new(UInt64Array::from(table.timestamps.clone())));

    // Table-level (not per-metric) sidecar: one wall-clock observation per row.
    // Null where the table carries no observation for that row; a length
    // mismatch against `timestamps` surfaces as a `RecordBatch` error.
    fields.push(Field::new(WALL_OFFSET_COLUMN, DataType::Int64, true));
    arrays.push(Arc::new(if table.wall_offsets.is_empty() {
        Int64Array::from(vec![None; table.timestamps.len()])
    } else {
        Int64Array::from(table.wall_offsets.clone())
    }));

    // Group-table mode (`table.table_window` is `Some`) emits ONE bare
    // `:window_begin`/`:window_width` pair for the whole table, right after
    // `:wall_offset` and before any member column — the shape
    // metriken-query's `parse_schema` treats as a table-level window applying
    // to every metric in the table (Part A). V2-sourced tables (`None`) keep
    // today's exact per-metric sidecar layout, read-old/write-new.
    if let Some(windows) = &table.table_window {
        if table.columns.iter().any(|c| !c.windows.is_empty()) {
            return Err("a group table's columns must not carry their own \
                         per-metric windows; the table-level window and a \
                         column's windows are mutually exclusive"
                .into());
        }
        let (begin, width) = window_offset_columns(windows, &table.timestamps);
        fields.push(Field::new(WINDOW_BEGIN_COLUMN, DataType::Int64, true));
        arrays.push(Arc::new(Int64Array::from(begin)));
        fields.push(Field::new(WINDOW_WIDTH_COLUMN, DataType::UInt64, true));
        arrays.push(Arc::new(UInt64Array::from(width)));

        for col in &table.columns {
            push_value_column(&mut fields, &mut arrays, col);
        }
    } else {
        for col in &table.columns {
            push_value_column(&mut fields, &mut arrays, col);

            let (begin, width) = window_offset_columns(&col.windows, &table.timestamps);
            fields.push(Field::new(
                format!("{}:window_begin", col.name),
                DataType::Int64,
                true,
            ));
            arrays.push(Arc::new(Int64Array::from(begin)));
            fields.push(Field::new(
                format!("{}:window_width", col.name),
                DataType::UInt64,
                true,
            ));
            arrays.push(Arc::new(UInt64Array::from(width)));
        }
    }

    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), arrays)?;
    Ok((schema, batch))
}

/// Parquet writer settings for a `.rez` segment.
///
/// **Passing `None` here would not select parquet-rs's defaults — it selects
/// `Compression::UNCOMPRESSED`.** That is the trap this function exists to
/// close, so it must always be `Some(..)`.
///
/// **Compression: LZ4.** These columns are already RLE- and bit-packed by the
/// parquet encoders, so an entropy coder has little left to find; LZ4 is where
/// the ratio curve flattens, and it pays for its own encode by shrinking the
/// BLOB the segment insert then writes. Stronger codecs are rejected on
/// *memory*, not ratio or CPU: zstd's compression contexts are per column
/// writer, and this writer instantiates thousands of those at once (below).
/// `LZ4_RAW` rather than legacy `LZ4` because the legacy variant is a
/// Hadoop-framed encoding parquet-rs writes only for pre-2.9.0 readers.
///
/// Note that the codec has no bearing on read speed even though it halves the
/// archive; query time tracks segment *count*, which is `SealPolicy`'s
/// business, not this function's.
///
/// **Dictionary encoding: off, and this is the recorder's largest memory
/// decision.** `ArrowWriter` instantiates a column writer for every column of
/// a row group simultaneously, each carrying its own `DictEncoder` buffer and
/// interner. Rezolus tables are wide in a way that makes this dominant — a
/// per-CPU table runs to thousands of columns, since every metric also carries
/// `:window_begin`/`:window_width` sidecars — so dictionary state, not row
/// data, sets peak RSS during a seal.
///
/// It costs nothing to disable because the data is the worst possible
/// dictionary input: u64 counters and gauges, where a monotonic counter makes
/// every value distinct and the dictionary as large as the column it encodes.
/// There are no string columns; metric names and labels live in the parquet
/// schema, not the data.
///
/// **Deliberately left at parquet-rs defaults:** `write_batch_size`,
/// statistics granularity, and the page-size limits. Each looks like it should
/// bound per-column-writer memory and none of them measurably does, while
/// chunk-level statistics costs finalize latency and read pruning. The
/// dictionary is the whole effect.
pub fn segment_writer_props() -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::LZ4_RAW)
        .set_dictionary_enabled(false)
        .build()
}

/// Serialize one table to parquet bytes.
pub fn write_table_parquet(table: &Table) -> Result<Vec<u8>, Error> {
    write_table_parquet_with(table, segment_writer_props())
}

/// [`write_table_parquet`] with the caller's writer properties, for a
/// writer that seals with another codec than [`segment_writer_props`]'s.
pub fn write_table_parquet_with(table: &Table, props: WriterProperties) -> Result<Vec<u8>, Error> {
    let (schema, batch) = table_to_batch(table)?;
    let mut buf: Vec<u8> = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut buf, schema, Some(crate::format::stamped(props)))?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(buf)
}

fn u64_col(a: &ArrayRef) -> &UInt64Array {
    a.as_any()
        .downcast_ref::<UInt64Array>()
        .expect("UInt64 column")
}

/// Deserialize one table from parquet bytes.
///
/// The production read path decodes most tables lazily via metriken-query's
/// `ParquetReader` (`read_archive_bytes` → `RezReader`). This eager decoder
/// was written to verify the write path independently, and is now also the
/// decoder behind [`crate::indexed`]: a table whose slots are described by
/// the identity index is split by occupant before the query engine sees it,
/// and that split needs every row in hand rather than a footer.
pub fn read_table_parquet(sampler: String, bytes: Vec<u8>) -> Result<Table, Error> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes))?;
    crate::format::check(builder.metadata().file_metadata().key_value_metadata())?;
    let reader = builder.build()?;

    let mut timestamps: Vec<u64> = Vec::new();
    let mut wall_offsets: Vec<i64> = Vec::new();
    let mut order: Vec<String> = Vec::new();
    let mut values: HashMap<String, Values> = HashMap::new();
    let mut metas: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut begins: HashMap<String, Vec<Option<i64>>> = HashMap::new();
    let mut widths: HashMap<String, Vec<Option<u64>>> = HashMap::new();
    // Table-level (bare, no metric-id prefix) window sidecar — a group
    // table only. Checked by exact name BEFORE the per-metric
    // `strip_suffix` branches below, which would otherwise treat the bare
    // name as a per-metric column with an empty-string base.
    let mut table_begin: Vec<Option<i64>> = Vec::new();
    let mut table_width: Vec<Option<u64>> = Vec::new();
    let mut is_group_table = false;

    for batch in reader {
        let batch = batch?;
        let schema = batch.schema();
        for i in 0..batch.num_columns() {
            let field = schema.field(i);
            let name = field.name();
            let col = batch.column(i);
            if name == "timestamp" {
                let a = u64_col(col);
                timestamps.extend((0..a.len()).map(|r| a.value(r)));
            } else if name == WALL_OFFSET_COLUMN {
                let a = col
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .expect("i64 wall_offset");
                // An all-null column means the table carried no observations;
                // leave `wall_offsets` empty so a write→read→write round trip
                // does not fabricate zeros.
                if a.null_count() < a.len() {
                    wall_offsets.extend((0..a.len()).map(|r| a.value(r)));
                }
            } else if name == WINDOW_BEGIN_COLUMN {
                is_group_table = true;
                let a = col
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .expect("i64 table-level window_begin");
                table_begin.extend((0..a.len()).map(|r| (!a.is_null(r)).then(|| a.value(r))));
            } else if name == WINDOW_WIDTH_COLUMN {
                is_group_table = true;
                let a = u64_col(col);
                table_width.extend((0..a.len()).map(|r| (!a.is_null(r)).then(|| a.value(r))));
            } else if let Some(base) = name.strip_suffix(":window_begin") {
                let a = col
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .expect("i64 window_begin");
                begins
                    .entry(base.to_string())
                    .or_default()
                    .extend((0..a.len()).map(|r| (!a.is_null(r)).then(|| a.value(r))));
            } else if let Some(base) = name.strip_suffix(":window_width") {
                let a = u64_col(col);
                widths
                    .entry(base.to_string())
                    .or_default()
                    .extend((0..a.len()).map(|r| (!a.is_null(r)).then(|| a.value(r))));
            } else if let Some(base) = name.strip_suffix(":buckets") {
                let meta = field.metadata().clone();
                let gp: u8 = meta
                    .get("grouping_power")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let mvp: u8 = meta
                    .get("max_value_power")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let list = col
                    .as_any()
                    .downcast_ref::<ListArray>()
                    .expect("list histogram");
                let entry = match values.entry(base.to_string()) {
                    std::collections::hash_map::Entry::Vacant(v) => {
                        order.push(base.to_string());
                        metas.insert(base.to_string(), meta);
                        v.insert(Values::Histogram(Vec::new()))
                    }
                    std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                };
                if let Values::Histogram(hs) = entry {
                    for r in 0..list.len() {
                        if list.is_null(r) {
                            hs.push(None);
                        } else {
                            let vals = list.value(r);
                            let a = u64_col(&vals);
                            let buckets: Vec<u64> = (0..a.len()).map(|k| a.value(k)).collect();
                            hs.push(Some(histogram::Histogram::from_buckets(gp, mvp, buckets)?));
                        }
                    }
                }
            } else {
                // A metric value column: counter (UInt64) or gauge (Int64).
                let meta = field.metadata().clone();
                let is_gauge = meta.get("metric_type").map(String::as_str) == Some("gauge");
                let entry = match values.entry(name.to_string()) {
                    std::collections::hash_map::Entry::Vacant(v) => {
                        order.push(name.to_string());
                        metas.insert(name.to_string(), meta);
                        v.insert(if is_gauge {
                            Values::Gauge(Vec::new())
                        } else {
                            Values::Counter(Vec::new())
                        })
                    }
                    std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                };
                match entry {
                    Values::Counter(vs) => {
                        let a = u64_col(col);
                        vs.extend((0..a.len()).map(|r| (!a.is_null(r)).then(|| a.value(r))));
                    }
                    Values::Gauge(vs) => {
                        let a = col
                            .as_any()
                            .downcast_ref::<Int64Array>()
                            .expect("i64 gauge");
                        vs.extend((0..a.len()).map(|r| (!a.is_null(r)).then(|| a.value(r))));
                    }
                    Values::Histogram(_) => {}
                }
            }
        }
    }

    let columns = order
        .into_iter()
        .map(|base| {
            let begin = begins.remove(&base).unwrap_or_default();
            let width = widths.remove(&base).unwrap_or_default();
            let windows = (0..timestamps.len())
                .map(|r| {
                    match (
                        begin.get(r).copied().flatten(),
                        width.get(r).copied().flatten(),
                    ) {
                        (Some(b), Some(w)) => {
                            let begin_ns = (timestamps[r] as i64 + b) as u64;
                            Some(Window::new(begin_ns, begin_ns + w))
                        }
                        _ => None,
                    }
                })
                .collect();
            Column {
                metadata: metas.remove(&base).unwrap_or_default(),
                values: values.remove(&base).unwrap(),
                windows,
                name: base,
            }
        })
        .collect();

    let table_window = is_group_table.then(|| {
        (0..timestamps.len())
            .map(|r| {
                match (
                    table_begin.get(r).copied().flatten(),
                    table_width.get(r).copied().flatten(),
                ) {
                    (Some(b), Some(w)) => {
                        let begin_ns = (timestamps[r] as i64 + b) as u64;
                        Some(Window::new(begin_ns, begin_ns + w))
                    }
                    _ => None,
                }
            })
            .collect()
    });

    Ok(Table {
        sampler,
        timestamps,
        wall_offsets,
        columns,
        table_window,
    })
}
