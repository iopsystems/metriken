//! Embedded-DuckDB implementation of `metriken_query::SqlBackend`.
//!
//! `DuckDbBackend` keeps a small **connection pool** per `data_source`
//! (parquet path or glob). The first request for a given source pays the
//! cold-start cost: for each pool slot, open an in-memory DB, register
//! UDFs + macros, and load the parquet into a `_src` TEMP TABLE via
//! `views::ensure_views`. Subsequent requests check out a slot via
//! atomic round-robin and only pay the SQL `prepare_cached` + `execute`
//! cost.
//!
//! `Connection` is `!Sync` (DuckDB connections are single-threaded), so
//! each pool slot is a `Mutex<Connection>`. With pool size N, up to N
//! requests against the same fixture proceed concurrently before any
//! lock contention. Default N is `available_parallelism().min(8)`,
//! overridable via `METRIKEN_SQL_POOL`.
//!
//! Two output shapes, dispatched on `entry.output_shape`:
//!
//! **Matrix** (default) — positional columns:
//! - Column 0 — `t`, DOUBLE seconds.
//! - Columns 1..1+`value_columns.len()` — DOUBLE values (column 1 is used
//!   today; multi-value support arrives with multi-quantile queries).
//! - Remaining columns — series-defining label values, one per
//!   `label_columns` entry, in the order declared.
//!
//! **Heatmap** — positional columns:
//! - Column 0 — `t`, DOUBLE seconds.
//! - Column 1 — `bucket_idx`, INTEGER (H2 bucket index, NOT remapped).
//! - Column 2 — `count`, DOUBLE (non-zero count for this `(t, bucket)`).
//! - Column 3 — `p`, INTEGER (grouping_power; expected constant within a query).
//!
//! Positional rather than name-based access avoids duckdb-rs's habit of
//! panicking if column metadata is read before the statement executes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arrow::array::{Array, Float64Array, StringArray};
use duckdb::Connection;
use metriken_query::{
    CatalogueEntry, Captures, HistogramHeatmapResult, MatrixSample, OutputShape,
    QueryResult, SqlBackend as TraitSqlBackend, SqlError,
};

use crate::views::MetricCatalog;

/// Per-data-source state: a pool of independent in-memory DuckDB
/// connections plus the shared (Rust-side) metadata catalog produced
/// by `ensure_views`. Each connection has its own UDF registrations,
/// `_src` TEMP TABLE, and prepared-statement cache; checkout is
/// lockless via a round-robin atomic counter, so contention only
/// arises when concurrent requests > pool size.
///
/// Each slot is `Mutex<Option<Connection>>` rather than
/// `Mutex<Connection>`. A slot can be **empty** in two cases:
/// 1. After a panic inside DuckDB query execution — we
///    `catch_unwind` at the `run` boundary and clear the slot so
///    its (possibly inconsistent) connection state isn't reused.
/// 2. Lazy rebuild path — a fresh connection is built on the next
///    checkout when the slot is `None`.
///
/// Crucially, because we catch the panic **before** the lock guard
/// is dropped, the `Mutex` itself is never poisoned: peer slots in
/// the pool keep serving without any cross-slot fallout. With the
/// pool sized to expected concurrency, one bad query loses its own
/// response and (briefly) one slot, not the whole process.
struct ConnState {
    pool: Vec<Mutex<Option<Connection>>>,
    next: AtomicUsize,
    catalog: MetricCatalog,
    /// Captured at backend-construction time; needed when a slot
    /// post-panic rebuilds, since `pool_size` lives on the backend.
    pool_size: usize,
}

/// Default DuckDB-backed implementation of `SqlBackend`. Lazily builds
/// a connection pool per unique `data_source` on first request. Pool
/// size is fixed at backend construction; tune via
/// `DuckDbBackend::with_pool_size` or the `METRIKEN_SQL_POOL` env var.
pub struct DuckDbBackend {
    connections: Mutex<HashMap<String, Arc<ConnState>>>,
    pool_size: usize,
}

/// Default pool size: `available_parallelism().min(8)`, with
/// `METRIKEN_SQL_POOL` as an explicit override. Cap at 8 because
/// cold-start scales linearly with pool size (each slot pays the
/// open + register + parquet-read cost) and 8 is well past the
/// point of diminishing returns on a busy single-fixture viewer.
fn default_pool_size() -> usize {
    if let Ok(v) = std::env::var("METRIKEN_SQL_POOL") {
        if let Ok(n) = v.parse::<usize>() {
            return n.max(1);
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4)
}

impl DuckDbBackend {
    pub fn new() -> Self {
        Self::with_pool_size(default_pool_size())
    }

    /// Construct a backend with an explicit pool size. `n` is clamped to
    /// at least 1; passing 1 reproduces the pre-pool single-connection
    /// behavior. Useful in tests where deterministic single-threaded
    /// execution is wanted.
    pub fn with_pool_size(n: usize) -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
            pool_size: n.max(1),
        }
    }

    /// Look up (or lazily build) the cached connection pool for
    /// `data_source`. Returns the `Arc` and a `cold` flag — `true`
    /// means this call paid the per-slot open + UDF-registration +
    /// view-building cost, `false` means the pool was already warm.
    fn get_or_init(&self, data_source: &str) -> Result<(Arc<ConnState>, bool), SqlError> {
        // Fast path: already cached.
        {
            let map = self.connections.lock().expect("poisoned");
            if let Some(arc) = map.get(data_source) {
                return Ok((arc.clone(), false));
            }
        }
        // Slow path: build under the outer lock.
        let mut map = self.connections.lock().expect("poisoned");
        if let Some(arc) = map.get(data_source) {
            return Ok((arc.clone(), false));
        }
        let timing = std::env::var("METRIKEN_SQL_TIMING").is_ok();
        let t0 = std::time::Instant::now();

        let mut pool: Vec<Mutex<Option<Connection>>> = Vec::with_capacity(self.pool_size);
        let mut catalog: Option<MetricCatalog> = None;
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;

        for _ in 0..self.pool_size {
            let (conn, cat) = build_slot_connection(self.pool_size, data_source)?;
            if catalog.is_none() {
                catalog = Some(cat);
            }
            pool.push(Mutex::new(Some(conn)));
        }

        if timing {
            eprintln!(
                "duckdb cold-start data_source={} pool_size={} wall={:.1}ms",
                data_source,
                self.pool_size,
                ms(t0.elapsed()),
            );
        }

        let state = Arc::new(ConnState {
            pool,
            next: AtomicUsize::new(0),
            catalog: catalog.expect("pool_size >= 1 → catalog set"),
            pool_size: self.pool_size,
        });
        map.insert(data_source.to_string(), state.clone());
        Ok((state, true))
    }
}

/// Build one pool slot's connection: open the in-memory DB, set the
/// statement-cache capacity, register UDFs/macros, build the
/// `_src` TEMP TABLE. Returns the `Connection` and the
/// `MetricCatalog` it yielded (only the first slot's catalog is
/// kept on `ConnState`; the rest are discarded since they're
/// identical Rust-side metadata).
fn build_slot_connection(
    pool_size: usize,
    data_source: &str,
) -> Result<(Connection, MetricCatalog), SqlError> {
    let conn = Connection::open_in_memory()
        .map_err(|e| SqlError::Backend(format!("open duckdb: {e}")))?;
    // Default prepared-statement cache is 16; bump it so the catalogue
    // (~70 distinct shapes) plus per-label-value variants all fit
    // and stay parsed/planned across repeat queries from the viewer.
    // Each cached Statement is small (planning artifacts only).
    conn.set_prepared_statement_cache_capacity(1024);
    // When pooling, cap DuckDB's intra-query worker threads to 1: we
    // are already achieving parallelism at the pool level. pool_size
    // connections × DuckDB's default of N internal workers per query
    // oversubscribes the box (e.g. 4 slots × 4 internal workers = 16
    // threads on 4 cores), causing context-switch storms that show up
    // as a *regression* at N ≈ pool_size in the concurrent bench.
    // With pool_size=1 (single-connection mode) we leave DuckDB's
    // intra-query parallelism alone — that's the only way a single
    // request gets multi-core.
    if pool_size > 1 {
        conn.execute("PRAGMA threads=1", [])
            .map_err(|e| SqlError::Backend(format!("pragma threads: {e}")))?;
    }
    crate::register_all(&conn)
        .map_err(|e| SqlError::Backend(format!("register UDFs/macros: {e}")))?;
    let cat = crate::views::ensure_views(&conn, data_source)
        .map_err(|e| SqlError::Backend(format!("create metric views: {e}")))?;
    Ok((conn, cat))
}

impl Default for DuckDbBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TraitSqlBackend for DuckDbBackend {
    fn run(
        &self,
        entry: &CatalogueEntry,
        captures: &Captures,
        data_source: &str,
        _start: f64,
        _end: f64,
        _step: f64,
    ) -> Result<QueryResult, SqlError> {
        let (state, cold) = self.get_or_init(data_source)?;

        // Wide-form is the only SQL path. Returns `None` only when the
        // entry id is unknown to the generator — in which case the
        // catalog has an entry we forgot to wire up. Surface that as a
        // hard error, since the long-form fallback is gone.
        let sql = crate::wide_form::try_generate(entry, captures, &state.catalog).ok_or_else(
            || SqlError::Backend(format!(
                "entry {} has no wide-form generator (long-form fallback was removed)",
                entry.id
            )),
        )?;

        // Atomic round-robin pool checkout. With pool size N, up to N
        // concurrent requests proceed in parallel; the `% pool.len()`
        // is the only ordering needed (Relaxed is fine — we don't care
        // which slot a given request lands on, just that distribution
        // is roughly uniform). NOTE for future telemetry: this is the
        // natural place to record lock-acquire wait time per slot if
        // we want to surface contention as the catalogue grows.
        let idx = state.next.fetch_add(1, Ordering::Relaxed) % state.pool.len();
        // The slot's mutex is never poisoned: we catch_unwind below
        // *inside* the lock guard, so a UDF/SQL panic doesn't
        // unwind through the lock release. `expect` here is therefore
        // a real bug-or-OOM signal, not a normal-failure path.
        let mut slot = state.pool[idx].lock().expect("slot mutex poisoned");

        // Lazy rebuild: a previous request on this slot panicked
        // (catch_unwind below cleared the slot) — build a fresh
        // connection now. Cold-start cost is paid by the unlucky
        // caller that lands on the empty slot, which is the desired
        // behavior: the rest of the pool kept serving uninterrupted.
        if slot.is_none() {
            let (conn, _cat) = build_slot_connection(state.pool_size, data_source)?;
            *slot = Some(conn);
        }

        let timing = std::env::var("METRIKEN_SQL_TIMING").is_ok();
        let t0 = std::time::Instant::now();

        // catch_unwind boundary. DuckDB's intra-query execution can
        // panic at chunk boundaries (e.g. the documented LAG-on-list
        // class of bugs in `udf.rs`). Pre-pool, such a panic aborted
        // the process — bad but only one in-flight request died. With
        // the pool, that blast radius would extend to *every*
        // concurrent request. Catching here surfaces the panic as
        // `SqlError::Backend` for the offending request only, drops
        // the slot's connection (its internal state may be
        // inconsistent), and lets the next checkout rebuild it.
        // Other slots in the pool are unaffected.
        let conn_ref = slot.as_ref().expect("just-initialised slot");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match entry.output_shape {
                OutputShape::Matrix => run_matrix(conn_ref, entry, captures, &sql, timing),
                OutputShape::Heatmap => run_heatmap(conn_ref, entry, &sql, timing),
            }
        }));
        let t_exec = t0.elapsed();

        let result = match result {
            Ok(r) => r,
            Err(payload) => {
                // Drop the slot's connection — its prepared-statement
                // cache and internal state may be inconsistent.
                *slot = None;
                let msg = panic_message(&payload);
                Err(SqlError::Backend(format!(
                    "internal SQL panic on entry {} (slot {}): {msg}",
                    entry.id, idx
                )))
            }
        };

        if timing {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            eprintln!(
                "duckdb {} {} slot={} exec={:.1}ms",
                entry.id,
                if cold { "cold" } else { "warm" },
                idx,
                ms(t_exec)
            );
        }

        result
    }
}

/// Best-effort extraction of a printable message from a panic payload.
/// `Box<dyn Any + Send>` carries the `panic!()` argument, which is
/// usually a `&'static str` or a `String`; a few panics carry other
/// types and we fall back to a generic label rather than guessing.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

fn run_matrix(
    conn: &Connection,
    entry: &CatalogueEntry,
    captures: &Captures,
    sql: &str,
    _timing: bool,
) -> Result<QueryResult, SqlError> {
    let mut stmt = conn
        .prepare_cached(sql)
        .map_err(|e| SqlError::Backend(format!("prepare {}: {e}", entry.id)))?;

    let n_values = entry.value_columns.len().max(1);
    let label_offset = 1 + n_values;

    // Bulk Arrow extraction: for queries that emit hundreds of rows
    // (common for per-id or per-cpu shapes), per-row `Row::get()` calls
    // through duckdb-rs are a measurable chunk of the warm-exec cost.
    // `query_arrow` returns RecordBatches we can iterate with typed
    // direct-array access (no virtual dispatch per cell).
    let arrow = stmt
        .query_arrow([])
        .map_err(|e| SqlError::Backend(format!("query_arrow {}: {e}", entry.id)))?;
    let schema = arrow.get_schema();

    // Resolve label column names — same two strategies as before:
    //   1. Catalogue-declared (`entry.label_columns` non-empty).
    //   2. Schema-inferred (any columns past `(t, value(s))`). The latter
    //      lets passthrough entries like `gauge_bare` project per-source
    //      label columns via `* EXCLUDE (timestamp, value, col)` without
    //      hard-coding them in TOML.
    let label_names: Vec<String> = if !entry.label_columns.is_empty() {
        entry.label_columns.clone()
    } else if schema.fields().len() > label_offset {
        schema.fields()[label_offset..]
            .iter()
            .map(|f| f.name().clone())
            .collect()
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
        let resolved = crate::interp::interpolate(v, captures, "", None)
            .map_err(|e| SqlError::Backend(format!("interp output_metric[{k}] for {}: {e}", entry.id)))?;
        interpolated_metric.insert(k.clone(), resolved);
    }

    // Fast path: no label columns in the result schema. Most catalogue
    // entries (every gauge_bare / counter_*_sum / softirq_*_total
    // shape) project no labels — there's exactly one series and no
    // grouping is needed. Skip the BTreeMap entirely and append (t, v)
    // pairs directly to a Vec; no per-row hash, alloc, or clone.
    if label_names.is_empty() {
        let mut values: Vec<(f64, f64)> = Vec::new();
        for batch in arrow {
            let n_rows = batch.num_rows();
            if n_rows == 0 {
                continue;
            }
            let t_col = downcast_f64(&batch, 0, &entry.id)?;
            let v_col = downcast_f64(&batch, 1, &entry.id)?;
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
    let mut series: HashMap<Vec<String>, Vec<(f64, f64)>> =
        HashMap::with_capacity(8);
    let mut row_buf: Vec<String> = vec![String::new(); label_names.len()];
    for batch in arrow {
        let n_rows = batch.num_rows();
        if n_rows == 0 {
            continue;
        }
        let t_col = downcast_f64(&batch, 0, &entry.id)?;
        let v_col = downcast_f64(&batch, 1, &entry.id)?;
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

fn downcast_f64<'a>(
    batch: &'a arrow::record_batch::RecordBatch,
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

/// Project rows shaped `(t DOUBLE, bucket_idx INTEGER, count DOUBLE, p INTEGER)`
/// into a `HistogramHeatmapResult` matching `streaming/histogram.rs:357-560`:
/// timestamps + axis-trimmed bucket_bounds + remapped non-zero data triples.
fn run_heatmap(
    conn: &Connection,
    entry: &CatalogueEntry,
    sql: &str,
    _timing: bool,
) -> Result<QueryResult, SqlError> {
    let mut stmt = conn
        .prepare_cached(sql)
        .map_err(|e| SqlError::Backend(format!("prepare {}: {e}", entry.id)))?;

    let rows = stmt
        .query_map([], |row| {
            let t: f64 = row.get(0)?;
            let bucket_idx: i32 = row.get(1)?;
            let count: f64 = row.get(2)?;
            let p: i32 = row.get(3)?;
            Ok((t, bucket_idx, count, p))
        })
        .map_err(|e| SqlError::Backend(format!("query {}: {e}", entry.id)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| SqlError::Backend(format!("collect rows for {}: {e}", entry.id)))?;

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
    let bucket_bounds: Vec<u64> = (min_bucket_idx as u32..=max_bucket_idx as u32)
        .map(|i| crate::udf::h2_upper(i, p))
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
