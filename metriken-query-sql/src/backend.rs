//! Embedded-DuckDB implementation of `metriken_query::SqlBackend`.
//!
//! `DuckDbBackend` keeps one in-memory connection per `data_source` (parquet
//! path or glob). The first request for a given source pays the cold-start
//! cost — open in-memory DB, register UDFs + macros, load the parquet into a
//! `_src` temp table via `views::ensure_views`. Subsequent requests reuse
//! that same connection and only pay the SQL `prepare` + `execute` cost.
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use duckdb::Connection;
use metriken_query::{
    CaptureValue, CatalogueEntry, Captures, HistogramHeatmapResult, MatrixSample, OutputShape,
    QueryResult, SqlBackend as TraitSqlBackend, SqlError,
};

use crate::views::{MetricCatalog, MetricShape};

/// Per-data-source state: the DuckDB connection plus the metadata
/// catalog produced by `ensure_views`. Pre-computing the catalog lets
/// every per-query pre-flight check (view-exists, view-shape,
/// available-label-set) be a hashmap read instead of a DESCRIBE
/// roundtrip — measured to save ~1-2ms per catalogue entry that uses
/// metric-ident captures, which is most of them.
struct ConnState {
    conn: Mutex<Connection>,
    catalog: MetricCatalog,
}

/// Default DuckDB-backed implementation of `SqlBackend`. Holds one in-memory
/// `Connection` per unique `data_source`, lazily initialised on first request.
/// `Connection` is `!Sync`, so each cached connection is wrapped in an inner
/// `Mutex` that serialises queries against the same fixture; the outer mutex
/// guards the lookup map and is rarely contended.
pub struct DuckDbBackend {
    connections: Mutex<HashMap<String, Arc<ConnState>>>,
}

impl DuckDbBackend {
    pub fn new() -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
        }
    }

    /// Look up (or lazily build) the cached connection state for
    /// `data_source`. Returns the `Arc` and a `cold` flag — `true`
    /// means this call paid the open + UDF-registration + view-building
    /// cost, `false` means the state was already warm.
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
        let conn = Connection::open_in_memory()
            .map_err(|e| SqlError::Backend(format!("open duckdb: {e}")))?;
        // Default prepared-statement cache is 16; bump it so the catalogue
        // (~70 distinct shapes) plus per-label-value variants all fit
        // and stay parsed/planned across repeat queries from the viewer.
        // Each cached Statement is small (planning artifacts only).
        conn.set_prepared_statement_cache_capacity(1024);
        let t_open = t0.elapsed();
        crate::register_all(&conn)
            .map_err(|e| SqlError::Backend(format!("register UDFs/macros: {e}")))?;
        let t_register = t0.elapsed() - t_open;
        let catalog = crate::views::ensure_views(&conn, data_source)
            .map_err(|e| SqlError::Backend(format!("create metric views: {e}")))?;
        let t_views = t0.elapsed() - t_open - t_register;
        if timing {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            eprintln!(
                "duckdb cold-start data_source={} open={:.1}ms reg={:.1}ms views={:.1}ms",
                data_source,
                ms(t_open),
                ms(t_register),
                ms(t_views)
            );
        }
        let state = Arc::new(ConnState {
            conn: Mutex::new(conn),
            catalog,
        });
        map.insert(data_source.to_string(), state.clone());
        Ok((state, true))
    }
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
        let template = entry
            .sql
            .as_ref()
            .ok_or_else(|| SqlError::Backend(format!("entry {} has no SQL twin", entry.id)))?;

        let (state, cold) = self.get_or_init(data_source)?;

        // Pre-flight checks against the cached MetricCatalog (no DESCRIBE
        // roundtrips). All checks return `Ok(empty matrix)` when they
        // detect a query that targets metrics the fixture doesn't carry,
        // matching PromQL semantics.
        let needs_buckets = entry
            .sql
            .as_deref()
            .map(|s| s.contains("buckets"))
            .unwrap_or(false);
        // 1) Metric-ident capture check: if any of m/a/b/c references a
        //    metric not in the catalog, or one whose shape doesn't match
        //    what the entry needs, short-circuit.
        const METRIC_IDENT_NAMES: &[&str] = &["m", "a", "b", "c"];
        for &name in METRIC_IDENT_NAMES {
            if let Some(CaptureValue::Ident(metric)) = captures.get(name) {
                match state.catalog.shapes.get(metric) {
                    None => return Ok(QueryResult::Matrix { result: Vec::new() }),
                    Some(MetricShape::Scalar) if needs_buckets => {
                        return Ok(QueryResult::Matrix { result: Vec::new() })
                    }
                    Some(MetricShape::Histogram) if !needs_buckets => {
                        return Ok(QueryResult::Matrix { result: Vec::new() })
                    }
                    _ => {}
                }
            }
        }
        // 2) Available-labels lookup for `m` (drives interp's missing-label
        //    fold to PromQL semantics).
        let available_labels = match captures.get("m") {
            Some(CaptureValue::Ident(metric)) => state.catalog.label_keys.get(metric),
            _ => None,
        };

        let sql = crate::interp::interpolate(template, captures, data_source, available_labels)
            .map_err(|e| SqlError::Backend(format!("interp {}: {e}", entry.id)))?;

        // 3) Hardcoded-metric scan: catalogue entries that bake metric
        //    names directly into SQL (e.g. `FROM rezolus_bpf_run_time`)
        //    short-circuit if any FROM target isn't in the catalog.
        if let Some(missing) = first_missing_hardcoded_view(&state.catalog, &sql) {
            let _ = missing;
            return Ok(QueryResult::Matrix { result: Vec::new() });
        }

        let conn = state.conn.lock().expect("poisoned");

        let timing = std::env::var("METRIKEN_SQL_TIMING").is_ok();
        let t0 = std::time::Instant::now();

        let result = match entry.output_shape {
            OutputShape::Matrix => run_matrix(&conn, entry, captures, &sql, timing),
            OutputShape::Heatmap => run_heatmap(&conn, entry, &sql, timing),
        };
        let t_exec = t0.elapsed();

        if timing {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            eprintln!(
                "duckdb {} {} exec={:.1}ms",
                entry.id,
                if cold { "cold" } else { "warm" },
                ms(t_exec)
            );
        }

        result
    }
}

/// Scan rendered SQL for `FROM <ident>` tokens and return the first one
/// that names a metric absent from the fixture's `MetricCatalog`. Used
/// to pre-flight catalogue entries that hardcode metric names directly
/// in their SQL templates (e.g. `rezolus_bpf_avg_run_time_per_sampler`
/// reads `FROM rezolus_bpf_run_time` instead of going through `{m}`).
/// Without this check, fixtures missing such a hardcoded metric raise
/// `Catalog Error: Table with name X does not exist`; with it, the
/// backend returns an empty matrix matching PromQL semantics.
///
/// Heuristic-only: skips quoted identifiers, function-call FROM
/// (read_parquet), parenthesised subqueries, CTE-introduced names, and
/// any non-metric identifier (e.g. `_src`, `_metadata`) by checking
/// against the catalog's `view_names` set — only metric views are in
/// that set, so unrecognised idents fall through and the SQL itself
/// gets to surface any problem.
fn first_missing_hardcoded_view(catalog: &MetricCatalog, sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut cte_names: std::collections::BTreeSet<String> = Default::default();

    // Pre-pass: collect WITH ... AS (...) names so a self-referential
    // FROM doesn't count as missing.
    let lower = sql.to_ascii_lowercase();
    let mut search = lower.as_str();
    let mut offset = 0;
    while let Some(pos) = search.find("with ") {
        let abs = offset + pos + "with ".len();
        // After "WITH ", expect one or more `<name> AS (...)` separated by commas.
        // Walk through until we hit a top-level keyword (SELECT/INSERT/etc.).
        let mut k = abs;
        loop {
            // Skip whitespace.
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            // Read an identifier.
            let id_start = k;
            while k < bytes.len()
                && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_')
            {
                k += 1;
            }
            if id_start == k {
                break;
            }
            let name = sql[id_start..k].to_string();
            // Skip whitespace.
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            // Expect "AS".
            if k + 2 > bytes.len()
                || !sql[k..(k + 2).min(sql.len())]
                    .eq_ignore_ascii_case("as")
            {
                break;
            }
            cte_names.insert(name);
            // Skip past the parenthesised body of the CTE.
            k += 2;
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b'(' {
                let mut depth = 1;
                k += 1;
                while k < bytes.len() && depth > 0 {
                    match bytes[k] {
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                    k += 1;
                }
            }
            // Skip whitespace then optional comma.
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b',' {
                k += 1;
                continue;
            }
            break;
        }
        offset = abs;
        search = &lower[abs..];
    }

    // Scan for FROM <ident>. Lowercase form to match case-insensitively.
    let mut search = lower.as_str();
    let mut offset = 0;
    while let Some(pos) = search.find("from") {
        let abs = offset + pos;
        // Word boundary checks: the char before must not be alphanumeric
        // (otherwise this is e.g. `inform`), and the char after `from`
        // must be whitespace.
        let before_ok = abs == 0
            || !(bytes[abs - 1].is_ascii_alphanumeric() || bytes[abs - 1] == b'_');
        let after_pos = abs + 4;
        let after_ok = after_pos < bytes.len() && bytes[after_pos].is_ascii_whitespace();
        if !(before_ok && after_ok) {
            offset = abs + 4;
            search = &lower[offset..];
            continue;
        }
        // Skip whitespace after FROM.
        i = after_pos;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        // If the next char isn't an ident start, skip (function call,
        // subquery, quoted name, etc.).
        if i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
            let name = &sql[start..i];
            // Function call? Skip.
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let is_function_call = j < bytes.len() && bytes[j] == b'(';
            // A FROM target outside the catalog is "missing" only if it
            // also isn't an internal table/CTE. Internal names known not
            // to be metrics: `_src`, `_metadata` (created in views.rs).
            const INTERNAL_TABLES: &[&str] = &["_src", "_metadata"];
            if !is_function_call
                && !cte_names.contains(name)
                && !INTERNAL_TABLES.contains(&name)
                && !catalog.view_names.contains(name)
            {
                return Some(name.to_string());
            }
        }
        offset = i;
        search = &lower[offset..];
    }
    None
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

    // Resolve label column names. Two strategies:
    // 1. Catalogue-declared (`entry.label_columns` non-empty) — used by entries
    //    that explicitly project per-label columns (e.g. `id` for
    //    `sum by (id) (...)`).
    // 2. Schema-inferred — when the catalogue leaves `label_columns` empty but
    //    the SQL projects extra columns past `(t, value(s))`, treat each
    //    trailing column as a label name. This lets passthrough entries like
    //    `gauge_bare` project a parquet's per-source labels via `* EXCLUDE
    //    (timestamp, value, col)` without hard-coding the label set in TOML.
    //    `column_names()` requires the statement to have been executed first
    //    (otherwise duckdb-rs panics — see backend.rs module docs), so we
    //    enter the rows path before reading the schema.
    let mut rows = stmt
        .query([])
        .map_err(|e| SqlError::Backend(format!("query {}: {e}", entry.id)))?;
    let label_names: Vec<String> = if !entry.label_columns.is_empty() {
        entry.label_columns.clone()
    } else {
        let schema = rows
            .as_ref()
            .map(|s| s.column_names())
            .unwrap_or_default();
        if schema.len() > label_offset {
            schema[label_offset..].to_vec()
        } else {
            Vec::new()
        }
    };

    let mut collected: Vec<(Vec<(String, String)>, Option<f64>, Option<f64>)> = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| SqlError::Backend(format!("collect rows for {}: {e}", entry.id)))?
    {
        let t: Option<f64> = row
            .get(0)
            .map_err(|e| SqlError::Backend(format!("read t for {}: {e}", entry.id)))?;
        let v: Option<f64> = row
            .get(1)
            .map_err(|e| SqlError::Backend(format!("read v for {}: {e}", entry.id)))?;
        let labels: Vec<(String, String)> = label_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let val: Option<String> = row.get(label_offset + i).ok().flatten();
                (name.clone(), val.unwrap_or_default())
            })
            .collect();
        collected.push((labels, t, v));
    }
    let rows = collected;

    let mut series: BTreeMap<Vec<(String, String)>, Vec<(f64, f64)>> = BTreeMap::new();
    for (labels, t, v) in rows {
        let (Some(t), Some(v)) = (t, v) else { continue };
        series.entry(labels).or_default().push((t, v));
    }

    // Interpolate any `{capture}` placeholders in output_metric values
    // (e.g. `quantile = "{q}"` for the templated histogram_quantile entry).
    // The data_source argument is irrelevant here — output_metric never
    // legitimately contains `{fixture_path}` — but we pass empty rather
    // than threading the real value to make that explicit.
    let mut interpolated_metric: HashMap<String, String> = HashMap::with_capacity(entry.output_metric.len());
    for (k, v) in &entry.output_metric {
        let resolved = crate::interp::interpolate(v, captures, "", None)
            .map_err(|e| SqlError::Backend(format!("interp output_metric[{k}] for {}: {e}", entry.id)))?;
        interpolated_metric.insert(k.clone(), resolved);
    }

    let result: Vec<MatrixSample> = series
        .into_iter()
        .map(|(labels, values)| {
            let mut metric: HashMap<String, String> = interpolated_metric.clone();
            for (k, v) in labels {
                metric.insert(k, v);
            }
            MatrixSample { metric, values }
        })
        .collect();

    Ok(QueryResult::Matrix { result })
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
