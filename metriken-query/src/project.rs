//! Arrow → `QueryResult` projection.
//!
//! The DuckDB engine in `metriken-query-sql` returns raw Arrow
//! `RecordBatch`es from `run_sql`. This module turns them into the
//! `QueryResult::{Matrix, HistogramHeatmap}` shapes the rest of
//! `metriken-query` (and Rezolus, today) consumes.
//!
//! Two projection paths, dispatched on the catalogue entry's
//! `OutputShape`:
//!
//! **Matrix** — positional columns:
//! - Column 0 — `t`, DOUBLE seconds.
//! - Columns 1..1+`value_columns.len()` — DOUBLE values (column 1 is
//!   used today; multi-value support arrives with multi-quantile queries).
//! - Remaining columns — series-defining label values, one per
//!   `label_columns` entry, in the order declared.
//!
//! **Heatmap** — positional columns:
//! - Column 0 — `t`, DOUBLE seconds.
//! - Column 1 — `bucket_idx`, INTEGER (H2 bucket index, NOT remapped).
//! - Column 2 — `count`, DOUBLE (non-zero count for this `(t, bucket)`).
//! - Column 3 — `p`, INTEGER (grouping_power; expected constant within a query).
//!
//! Positional rather than name-based access is a hidden contract with
//! the translator (`crate::translate`): SQL output column order must
//! match these expectations. Together they form a small private
//! protocol between two modules in this crate; documenting both halves
//! keeps the contract auditable.

use std::collections::HashMap;

use arrow::array::{Array, Float64Array, Int32Array, StringArray};
use arrow::record_batch::RecordBatch;
use metriken_query_sql::SqlError;

use crate::catalogue::OutputShape;
use crate::{CatalogueEntry, Captures, HistogramHeatmapResult, MatrixSample, QueryResult};

/// Project Arrow batches into a `QueryResult` shape per the catalogue
/// entry's `output_shape`. Single seam between the SQL pipeline and
/// the projector — every caller (Engine, tests, examples) goes through
/// here instead of duplicating the match.
pub fn run(
    batches: &[RecordBatch],
    entry: &CatalogueEntry,
    captures: &Captures,
) -> Result<QueryResult, SqlError> {
    match entry.output_shape {
        OutputShape::Matrix => matrix(batches, entry, captures),
        OutputShape::Heatmap => heatmap(batches, entry),
    }
}

/// Project Arrow `RecordBatch`es into `QueryResult::Matrix`. The
/// translator's wide-form SQL emits one row per `(timestamp, value,
/// labels...)` tuple; this function groups by label tuple to build
/// per-series sample arrays.
pub fn matrix(
    batches: &[RecordBatch],
    entry: &CatalogueEntry,
    captures: &Captures,
) -> Result<QueryResult, SqlError> {
    let n_values = entry.value_columns.len().max(1);
    let label_offset = 1 + n_values;

    // Resolve label column names — same two strategies as before:
    //   1. Catalogue-declared (`entry.label_columns` non-empty).
    //   2. Schema-inferred (any columns past `(t, value(s))`). The latter
    //      lets passthrough entries like `gauge_bare` project per-source
    //      label columns via `* EXCLUDE (timestamp, value, col)` without
    //      hard-coding them in TOML.
    let label_names: Vec<String> = if !entry.label_columns.is_empty() {
        entry.label_columns.clone()
    } else if let Some(first) = batches.first() {
        if first.num_columns() > label_offset {
            first
                .schema()
                .fields()
                .iter()
                .skip(label_offset)
                .map(|f| f.name().clone())
                .collect()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Interpolate `output_metric` placeholders once, before the per-row
    // unpack loop — saves a per-series clone of `interpolated_metric`
    // when the result has zero or one series (extremely common: every
    // unary aggregation entry without label columns hits this path).
    let mut interpolated_metric: HashMap<String, String> =
        HashMap::with_capacity(entry.output_metric.len());
    for (k, v) in &entry.output_metric {
        let resolved = crate::interp::interpolate(v, captures).map_err(|e| {
            SqlError::Backend(format!(
                "interp output_metric[{k}] for {}: {e}",
                entry.id
            ))
        })?;
        interpolated_metric.insert(k.clone(), resolved);
    }

    // Fast path: no label columns in the result schema. Most catalogue
    // entries (every gauge_bare / counter_*_sum / softirq_*_total
    // shape) project no labels — there's exactly one series and no
    // grouping is needed. Skip the BTreeMap entirely and append (t, v)
    // pairs directly to a Vec; no per-row hash, alloc, or clone.
    if label_names.is_empty() {
        let mut values: Vec<(f64, f64)> = Vec::new();
        for batch in batches {
            let n_rows = batch.num_rows();
            if n_rows == 0 {
                continue;
            }
            let t_col = downcast_f64(batch, 0, &entry.id)?;
            let v_col = downcast_f64(batch, 1, &entry.id)?;
            values.reserve(n_rows);
            for r in 0..n_rows {
                if t_col.is_null(r) || v_col.is_null(r) {
                    continue;
                }
                values.push((t_col.value(r), v_col.value(r)));
            }
        }
        let result = if values.is_empty() {
            Vec::new()
        } else {
            vec![MatrixSample {
                metric: interpolated_metric,
                values,
            }]
        };
        return Ok(QueryResult::Matrix { result });
    }

    // Multi-series path. HashMap (not BTreeMap) for O(1) inserts —
    // canonicalisation downstream sorts before diffing, so we don't
    // need the BTreeMap's ordered iteration.
    let mut series: HashMap<Vec<String>, Vec<(f64, f64)>> = HashMap::with_capacity(8);
    let mut row_buf: Vec<String> = vec![String::new(); label_names.len()];
    for batch in batches {
        let n_rows = batch.num_rows();
        if n_rows == 0 {
            continue;
        }
        let t_col = downcast_f64(batch, 0, &entry.id)?;
        let v_col = downcast_f64(batch, 1, &entry.id)?;
        // Label columns: downcast to StringArray once per batch. Some
        // columns may be null-filled (StringArray::value panics on null);
        // we check is_null per row in the inner loop.
        let label_cols: Vec<&StringArray> = (0..label_names.len())
            .map(|i| {
                batch
                    .column(label_offset + i)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| {
                        SqlError::Backend(format!(
                            "{}: label column {} is not Utf8 (got {:?})",
                            entry.id,
                            label_offset + i,
                            batch.column(label_offset + i).data_type()
                        ))
                    })
            })
            .collect::<Result<_, _>>()?;

        for r in 0..n_rows {
            if t_col.is_null(r) || v_col.is_null(r) {
                continue;
            }
            let t = t_col.value(r);
            let v = v_col.value(r);
            // Refill row_buf in place: `String::clear` + `push_str`
            // reuses the existing heap allocation rather than freeing
            // and re-allocating each cell. Only on a series-key
            // mismatch (HashMap miss) do we pay the `Vec<String>`
            // clone — and that clones strings that already have the
            // right backing storage.
            for (i, col) in label_cols.iter().enumerate() {
                let buf = &mut row_buf[i];
                buf.clear();
                if !col.is_null(r) {
                    buf.push_str(col.value(r));
                }
            }
            // Lookup-then-insert avoids the unconditional `entry().clone()`
            // — on hit (the common case after the first row of each
            // series) we don't allocate at all.
            if let Some(samples) = series.get_mut(&row_buf) {
                samples.push((t, v));
            } else {
                series.insert(row_buf.clone(), vec![(t, v)]);
            }
        }
    }

    let n_series = series.len();
    let mut result: Vec<MatrixSample> = Vec::with_capacity(n_series);
    let mut iter = series.into_iter();
    if let Some((label_values, values)) = iter.next() {
        // First series: take ownership of `interpolated_metric` (move,
        // no clone). Subsequent series clone from a frozen template.
        let template = if n_series > 1 {
            Some(interpolated_metric.clone())
        } else {
            None
        };
        let mut metric = interpolated_metric;
        for (i, val) in label_values.into_iter().enumerate() {
            metric.insert(label_names[i].clone(), val);
        }
        result.push(MatrixSample { metric, values });
        if let Some(template) = template {
            for (label_values, values) in iter {
                let mut metric = template.clone();
                for (i, val) in label_values.into_iter().enumerate() {
                    metric.insert(label_names[i].clone(), val);
                }
                result.push(MatrixSample { metric, values });
            }
        }
    }

    Ok(QueryResult::Matrix { result })
}

/// Project Arrow `RecordBatch`es shaped `(t DOUBLE, bucket_idx INTEGER,
/// count DOUBLE, p INTEGER)` into a `HistogramHeatmapResult` matching
/// `streaming/histogram.rs:357-560`: timestamps + axis-trimmed
/// bucket_bounds + remapped non-zero data triples.
pub fn heatmap(batches: &[RecordBatch], entry: &CatalogueEntry) -> Result<QueryResult, SqlError> {
    // Collect rows from every batch into a single Vec for the existing
    // index-build / range-scan logic. Rows are small (32B); the alloc
    // is cheap relative to DuckDB execution.
    let mut rows: Vec<(f64, i32, f64, i32)> = Vec::new();
    for batch in batches {
        let n_rows = batch.num_rows();
        if n_rows == 0 {
            continue;
        }
        let t_col = downcast_f64(batch, 0, &entry.id)?;
        let bucket_col = downcast_i32(batch, 1, &entry.id)?;
        let count_col = downcast_f64(batch, 2, &entry.id)?;
        let p_col = downcast_i32(batch, 3, &entry.id)?;
        rows.reserve(n_rows);
        for r in 0..n_rows {
            if t_col.is_null(r)
                || bucket_col.is_null(r)
                || count_col.is_null(r)
                || p_col.is_null(r)
            {
                continue;
            }
            rows.push((
                t_col.value(r),
                bucket_col.value(r),
                count_col.value(r),
                p_col.value(r),
            ));
        }
    }

    if rows.is_empty() {
        // Match PromQL's `streaming::histogram::heatmap` shape on the
        // "no events" case: return an empty HistogramHeatmap rather than
        // an error so the dispatcher doesn't surface a synthetic
        // "MetricNotFound"-shaped failure to callers when the metric exists
        // but the requested range happens to be free of bucket events.
        return Ok(QueryResult::HistogramHeatmap {
            result: HistogramHeatmapResult {
                timestamps: Vec::new(),
                bucket_bounds: Vec::new(),
                data: Vec::new(),
                min_value: 0.0,
                max_value: 0.0,
            },
        });
    }

    // Timestamps: sorted unique values, preserving the order in which rows
    // arrive (the SQL ORDER BY t guarantees ascending).
    let mut timestamps: Vec<f64> = Vec::new();
    let mut t_to_idx: HashMap<u64, usize> = HashMap::new();
    for (t, _, _, _) in &rows {
        let key = t.to_bits();
        if !t_to_idx.contains_key(&key) {
            t_to_idx.insert(key, timestamps.len());
            timestamps.push(*t);
        }
    }

    // H2 bucket index range observed in the data.
    let mut min_bucket_idx: i32 = i32::MAX;
    let mut max_bucket_idx: i32 = i32::MIN;
    for (_, b, _, _) in &rows {
        if *b < min_bucket_idx {
            min_bucket_idx = *b;
        }
        if *b > max_bucket_idx {
            max_bucket_idx = *b;
        }
    }

    // grouping_power should be constant across rows; take it from the first.
    let p = rows[0].3 as u32;

    // Trimmed bucket bounds: contiguous H2 upper bounds for buckets in
    // [min_bucket_idx, max_bucket_idx]. Includes zero-count buckets in the
    // interior so the visualisation has a continuous Y axis (matches
    // `streaming/histogram.rs:554-560`).
    //
    // Reaches across the crate boundary into `metriken-query-sql::udf` —
    // pure function over `(idx, p)`, no leak.
    let bucket_bounds: Vec<u64> = (min_bucket_idx as u32..=max_bucket_idx as u32)
        .map(|i| metriken_query_sql::udf::h2_upper(i, p))
        .collect();

    // Data triples: time index, *remapped* bucket index (relative to
    // min_bucket_idx), count.
    let mut data: Vec<(usize, usize, f64)> = Vec::with_capacity(rows.len());
    let mut min_value = f64::MAX;
    let mut max_value = f64::MIN;
    for (t, b, c, _) in rows {
        let time_idx = *t_to_idx
            .get(&t.to_bits())
            .expect("every row's t was inserted above");
        let bucket_idx = (b - min_bucket_idx) as usize;
        data.push((time_idx, bucket_idx, c));
        if c < min_value {
            min_value = c;
        }
        if c > max_value {
            max_value = c;
        }
    }

    // Same fallback semantics as `streaming/histogram.rs:547-552`.
    if min_value == f64::MAX {
        min_value = 0.0;
    }
    if max_value == f64::MIN {
        max_value = 0.0;
    }

    Ok(QueryResult::HistogramHeatmap {
        result: HistogramHeatmapResult {
            timestamps,
            bucket_bounds,
            data,
            min_value,
            max_value,
        },
    })
}

fn downcast_f64<'a>(
    batch: &'a RecordBatch,
    col: usize,
    entry_id: &str,
) -> Result<&'a Float64Array, SqlError> {
    batch
        .column(col)
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| {
            SqlError::Backend(format!(
                "{}: column {col} is not Float64 (got {:?})",
                entry_id,
                batch.column(col).data_type()
            ))
        })
}

fn downcast_i32<'a>(
    batch: &'a RecordBatch,
    col: usize,
    entry_id: &str,
) -> Result<&'a Int32Array, SqlError> {
    batch
        .column(col)
        .as_any()
        .downcast_ref::<Int32Array>()
        .ok_or_else(|| {
            SqlError::Backend(format!(
                "{}: column {col} is not Int32 (got {:?})",
                entry_id,
                batch.column(col).data_type()
            ))
        })
}
