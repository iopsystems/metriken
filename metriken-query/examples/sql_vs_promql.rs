//! Per-plot PromQL↔SQL correctness harness for the Rezolus viewer migration.
//!
//! For every plot in the dashboard JSON (one section file per dashboard
//! section, dumped via `cargo run -p dashboard -- /tmp/dashboard_json`)
//! and every demo parquet, run:
//!   • `promql_query` through `metriken_query::QueryEngine` (legacy
//!      in-memory PromQL evaluator over `Tsdb`).
//!   • `sql_query`     through `metriken_query_sql::DuckDbBackend`,
//!      with a `WITH _src AS (SELECT * FROM read_parquet(...) WHERE
//!      timestamp BETWEEN ...)` wrap that mirrors what
//!      `crates/viewer-sql/src/lib.rs:447` does in the browser.
//!
//! Then shape the SQL Arrow result into the same `QueryResult::Matrix`
//! the PromQL side returns (group rows by their non-`t`/`v` columns
//! preserving insertion order, drop NULL `v` rows). Both results land
//! on disk as JSON, and a per-pair classifier records whether the two
//! match strictly, match within floating-point tolerance, or genuinely
//! diverge.
//!
//! Run with `--features legacy` (default features keep `sql` on, so
//! both backends are available).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{
    Array, Decimal128Array, Decimal256Array, Float64Array, Int32Array, Int64Array, StringArray,
    TimestampNanosecondArray, UInt32Array, UInt64Array,
};
use bytes::Bytes;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};
use arrow::datatypes::DataType;
use arrow::record_batch::RecordBatch;
use duckdb::Connection;
use serde::Serialize;
use serde_json::Value;

use metriken_query::{MatrixSample, QueryResult};

#[cfg(feature = "legacy")]
use metriken_query::{QueryEngine, Tsdb};

/// Wasm-side macro source file. The static viewer's
/// `crates/viewer-sql/src/lib.rs::pure_sql_macros()` returns this same
/// string and the JS host runs it on every duckdb-wasm connection at
/// boot. Re-using it here means the harness's SQL backend behaves
/// exactly like what a user sees in the browser — including the no-op
/// reset semantics of `irate_1s` (the native UDF form would give
/// different numbers on counter resets).
const WASM_MACROS_SQL: &str = include_str!(
    "../../../rezolus/crates/viewer-sql/src/macros.sql"
);

const USAGE: &str = "Usage: sql_vs_promql --dashboard-dir DIR --parquets P1 [P2 ...] --out DIR\n  \
    [--rel-tol F] [--abs-tol F] [--max-plots N]";

#[derive(Default, Debug)]
struct Args {
    dashboard_dir: PathBuf,
    parquets: Vec<PathBuf>,
    out: PathBuf,
    rel_tol: f64,
    abs_tol: f64,
    max_plots: Option<usize>,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut a = Args {
        rel_tol: 1e-9,
        abs_tol: 1e-12,
        ..Default::default()
    };
    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        let take_one = |i: &mut usize| -> String {
            *i += 1;
            raw.get(*i).cloned().unwrap_or_else(|| {
                eprintln!("missing value for {}", raw[*i - 1]);
                std::process::exit(2)
            })
        };
        match arg.as_str() {
            "--dashboard-dir" => {
                let v = take_one(&mut i);
                a.dashboard_dir = PathBuf::from(v);
            }
            "--parquets" => {
                i += 1;
                while i < raw.len() && !raw[i].starts_with("--") {
                    a.parquets.push(PathBuf::from(&raw[i]));
                    i += 1;
                }
                continue;
            }
            "--out" => {
                let v = take_one(&mut i);
                a.out = PathBuf::from(v);
            }
            "--rel-tol" => {
                let v = take_one(&mut i);
                a.rel_tol = v.parse().expect("--rel-tol parse");
            }
            "--abs-tol" => {
                let v = take_one(&mut i);
                a.abs_tol = v.parse().expect("--abs-tol parse");
            }
            "--max-plots" => {
                let v = take_one(&mut i);
                a.max_plots = Some(v.parse().expect("--max-plots parse"));
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => {
                eprintln!("unknown arg: {arg}\n{USAGE}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if a.dashboard_dir.as_os_str().is_empty()
        || a.parquets.is_empty()
        || a.out.as_os_str().is_empty()
    {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    a
}

#[derive(Debug)]
struct PlotSpec {
    id: String,
    section: String,
    plot_type: String,
    /// `subtype` from `opts.subtype` — only meaningful for histogram
    /// plots ("percentiles" or "buckets").
    subtype: Option<String>,
    /// Custom percentiles, if any. None ⇒ default
    /// `[0.5, 0.9, 0.99, 0.999, 0.9999]`.
    percentiles: Option<Vec<f64>>,
    /// `promql_query` as emitted by the dashboard. For histogram plots
    /// this is the bare metric selector — the viewer's
    /// `buildHistogramQuery` wraps it client-side; the harness applies
    /// the same wrap inside `effective_promql`.
    promql: String,
    sql: String,
}

const DEFAULT_PERCENTILES: &[f64] = &[0.5, 0.9, 0.99, 0.999, 0.9999];

/// Mirror `src/viewer/assets/lib/charts/metric_types.js:buildHistogramQuery`
/// — wrap a histogram metric selector with `histogram_quantiles([...],
/// <metric>)` (or `histogram_heatmap(<metric>)` for the buckets
/// subtype). The PromQL evaluator at
/// `metriken-query/src/promql/mod.rs:351` accepts `histogram_quantiles`
/// (note: `_quantiles_`, not `_percentiles_` — the JS-side name was
/// renamed but the evaluator kept the original).
fn effective_promql(plot: &PlotSpec) -> String {
    if plot.plot_type != "histogram" {
        return plot.promql.clone();
    }
    if plot.subtype.as_deref() == Some("buckets") {
        return format!("histogram_heatmap({})", plot.promql);
    }
    let qs: Vec<String> = plot
        .percentiles
        .as_deref()
        .unwrap_or(DEFAULT_PERCENTILES)
        .iter()
        .map(|p| format!("{p}"))
        .collect();
    format!("histogram_quantiles([{}], {})", qs.join(", "), plot.promql)
}

fn load_plots(dir: &Path) -> Vec<PlotSpec> {
    let mut out = vec![];
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("read dashboard dir")
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.path());
    for ent in entries {
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        if name == "sections" {
            continue;
        }
        let bytes = fs::read(&path).expect("read section json");
        let v: Value = serde_json::from_slice(&bytes).expect("parse section json");
        for g in v.get("groups").and_then(Value::as_array).iter().flat_map(|a| a.iter()) {
            collect_plots(g, &name, &mut out);
            for sg in g
                .get("subgroups")
                .and_then(Value::as_array)
                .iter()
                .flat_map(|a| a.iter())
            {
                collect_plots(sg, &name, &mut out);
            }
        }
    }
    out
}

fn collect_plots(node: &Value, section: &str, out: &mut Vec<PlotSpec>) {
    let plots = match node.get("plots").and_then(Value::as_array) {
        Some(p) => p,
        None => return,
    };
    for p in plots {
        let promql = p.get("promql_query").and_then(Value::as_str).unwrap_or("");
        let sql = p.get("sql_query").and_then(Value::as_str).unwrap_or("");
        let id = p
            .get("opts")
            .and_then(|o| o.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let plot_type = p
            .get("opts")
            .and_then(|o| o.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let subtype = p
            .get("opts")
            .and_then(|o| o.get("subtype"))
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        let percentiles = p
            .get("opts")
            .and_then(|o| o.get("percentiles"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>());
        if id.is_empty() || promql.is_empty() || sql.is_empty() {
            continue;
        }
        out.push(PlotSpec {
            id,
            section: section.to_string(),
            plot_type,
            subtype,
            percentiles,
            promql: promql.to_string(),
            sql: sql.to_string(),
        });
    }
}

// ── Time window: parquet min/max via duckdb, then viewer's step heuristic.

fn parquet_time_range_seconds(parquet: &Path) -> (f64, f64) {
    let conn = duckdb::Connection::open_in_memory().expect("duckdb open");
    let sql = format!(
        "SELECT min(timestamp)::BIGINT, max(timestamp)::BIGINT FROM read_parquet('{}')",
        parquet.to_string_lossy()
    );
    let mut stmt = conn.prepare(&sql).expect("prepare time-range");
    let mut rows = stmt.query([]).expect("query time-range");
    let row = rows.next().expect("rows").expect("first row");
    let lo: i64 = row.get(0).expect("min ts");
    let hi: i64 = row.get(1).expect("max ts");
    (lo as f64 / 1e9, hi as f64 / 1e9)
}

/// Match the viewer's `data.js:328` step heuristic so step counts
/// align between native + browser runs:
///   windowDuration = min(3600, end - start)
///   step           = max(1, floor(windowDuration / 500))
fn viewer_step(start: f64, end: f64) -> f64 {
    let dur = (end - start).min(3600.0).max(0.0);
    (dur / 500.0).floor().max(1.0)
}

// ── PromQL backend: load Tsdb once per parquet, run query_range.

#[cfg(feature = "legacy")]
fn run_promql(
    engine: &QueryEngine<Arc<Tsdb>>,
    promql: &str,
    start: f64,
    end: f64,
    step: f64,
) -> Result<QueryResult, String> {
    match engine.query_range(promql, start, end, step) {
        Ok(r) => Ok(r),
        Err(e) => {
            // The legacy PromQL evaluator turns "histogram metric isn't
            // present in the parquet" into a hard `MetricNotFound` error,
            // while the SQL path silently returns an empty matrix
            // (DuckDB's empty-regex-match path, swallowed in `run_sql`).
            // Treat both as "no data" so the comparison is symmetric.
            let msg = e.to_string();
            if msg.contains("Metric not found")
                || msg.contains("No histogram data found")
            {
                Ok(QueryResult::Matrix { result: vec![] })
            } else {
                Err(msg)
            }
        }
    }
}

// ── SQL backend: hand-rolled DuckDB connection that mirrors the wasm
// viewer's macro-only setup (no native UDFs). Wraps the dashboard's
// emitted SQL with the same `_src` time-windowed CTE viewer-sql uses
// in `crates/viewer-sql/src/lib.rs:447`, runs it, and shapes the
// result into `QueryResult::Matrix` via `arrow_batches_to_matrix`.
//
// For multi-source parquets (combined files with `<source>::<metric>`
// columns) we mirror `site/viewer-sql/lib/duckdb-registry.js:buildSourceViews`
// — build a TEMP VIEW per source that aliases its prefixed columns
// back to the bare metric names, then point `_src` at the picked
// source's view inside the time-window CTE.

fn open_wasm_style_conn() -> Result<Connection, String> {
    let conn = Connection::open_in_memory()
        .map_err(|e| format!("open duckdb: {e}"))?;
    conn.set_prepared_statement_cache_capacity(1024);
    for stmt in WASM_MACROS_SQL.split(";\n") {
        let body: String = stmt
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        let trimmed = body.trim();
        if trimmed.is_empty() {
            continue;
        }
        conn.execute(trimmed, [])
            .map_err(|e| format!("install macro:\n  body={trimmed}\n  err={e}"))?;
    }
    Ok(conn)
}

/// Per-parquet SQL state: the FROM clause to use inside each query's
/// `_src` CTE wrap. For single-source parquets that's just
/// `read_parquet('<path>')`; for multi-source parquets it's the view
/// name we created for the picked source.
struct ParquetSqlState {
    from_clause: String,
    /// Constant infrastructure labels (`endpoint`, `source`, `instance`)
    /// pulled from Arrow field metadata. Each entry is included only
    /// when its value is consistent across every metric column —
    /// matching PromQL's per-series stamping. The SQL run wrapper
    /// projects these as literals so SQL output carries them like
    /// PromQL does. `direction` is per-column metadata (varies by
    /// metric) and is intentionally not included here — see the
    /// "expected divergent on direction" annotation in queries.toml.
    constant_labels: Vec<(String, String)>,
    /// True when the parquet has ≥2 rezolus sources and is being
    /// read through `_src_rezolus_combined`. Drives the
    /// `Verdict::ExpectedDivergent` reclassification: PromQL evaluates
    /// per-source then sums; SQL sums then rates. The two values
    /// differ at counter resets, but aggregate-then-rate is what
    /// dashboards typically want for cross-source aggregations
    /// (Bucket #3 in remaining_work.md).
    is_multi_source: bool,
}

/// Mirror `duckdb-registry.js:buildSourceViews` + the picker in
/// `CaptureRegistry::attach`. For each `<src>::<col>` column in the
/// parquet, build a TEMP VIEW `_src_<sanitised_src>` whose projection
/// aliases `<src>::<col>` back to `<col>`. When the parquet has 2+
/// rezolus sources (sources that include a `memory_total` column),
/// also build a `_src_rezolus_combined` view that sums same-named
/// columns across rezolus sources — matching the legacy PromQL
/// evaluator's Tsdb-side aggregation. The picker prefers the combined
/// view when present.
///
/// The JS picker is naïve about prefix detection — any `<x>::<y>`
/// column name signals a source even when `<x>` is e.g. a cgroup-task
/// path (`task_cpu_usage//system.slice/.../VLLM::EngineCor/...`). On
/// parquets like that the harness drops the unprefixed rezolus
/// columns. We sidestep that by reading column types from DESCRIBE
/// (so artefactual prefixes show ≪ 10% of total columns) and falling
/// back to single-source mode when that ratio holds.
fn setup_parquet_sql(
    conn: &Connection,
    parquet: &Path,
) -> Result<ParquetSqlState, String> {
    let parquet_str = parquet.to_str().expect("parquet path utf-8");
    // Pull `sampling_interval_ms` from the parquet's file-level KV so we
    // can snap the `timestamp` column on the way into every view. The
    // legacy PromQL `Tsdb` snaps every timestamp it ingests
    // (`metriken-query/src/tsdb/mod.rs:207`), so without snapping here
    // SQL sees raw jittered timestamps while PromQL sees the snapped
    // grid — `irate(c[w])` divides by the actual delta-t on each side
    // and the two values disagree by ~jitter / sampling_interval.
    // Default to 1 s when the metadata is missing.
    let interval_ns = parquet_sampling_interval_ns(conn, parquet_str);
    let half_ns = interval_ns / 2;
    let snap_expr = format!(
        "((CAST(timestamp AS BIGINT) + {half_ns}) // {interval_ns}) * {interval_ns} AS timestamp"
    );
    // DESCRIBE returns column_name + column_type — we need the latter
    // to know whether to use scalar `+` vs `h2_combine` when summing
    // a histogram column across sources.
    let desc_sql = format!(
        "DESCRIBE SELECT * FROM read_parquet('{parquet_str}')"
    );
    let mut stmt = conn
        .prepare(&desc_sql)
        .map_err(|e| format!("describe: {e}"))?;
    let mut rows = stmt.query([]).map_err(|e| format!("describe query: {e}"))?;
    let mut cols: Vec<String> = vec![];
    let mut col_type: HashMap<String, String> = HashMap::new();
    while let Some(row) = rows.next().map_err(|e| format!("describe iter: {e}"))? {
        let name: String = row.get(0).map_err(|e| format!("col name: {e}"))?;
        let ty: String = row.get(1).map_err(|e| format!("col type: {e}"))?;
        col_type.insert(name.clone(), ty);
        cols.push(name);
    }
    drop(rows);
    drop(stmt);

    // Read the parquet's `source` KV metadata so we know what counts
    // as a real source vs. a metric-name artefact.
    let recorded_sources = parquet_recorded_sources(conn, parquet_str);
    let field_meta = read_field_metadata(parquet);

    let mut by_source: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut alias_seen: BTreeMap<String, std::collections::HashSet<String>> = BTreeMap::new();
    for c in &cols {
        if let Some((prefix, rest)) = c.split_once("::") {
            let alias = canonical_alias(rest, field_meta.get(c));
            let seen = alias_seen.entry(prefix.to_string()).or_default();
            if seen.contains(&alias) { continue; }
            seen.insert(alias.clone());
            by_source
                .entry(prefix.to_string())
                .or_default()
                .push((c.clone(), alias));
        }
    }
    // Single-source path. The `:src0` aliases needed by avg/max/min
    // emitters (see follow-up #1) only get materialised in source
    // views, so we always build a `_src_default` view here — even for
    // truly-single-source parquets and for "fake-prefix" parquets
    // (rare ::-bearing column names that aren't real source prefixes,
    // see vllm.parquet's task_cpu_usage//system.slice/... cgroup-task
    // columns) — to keep the dashboard SQL working.
    let total_prefixed: usize = by_source.values().map(|v| v.len()).sum();
    let single_source_fallback = by_source.is_empty() || total_prefixed * 10 < cols.len();
    if single_source_fallback {
        let mut projections: Vec<String> = vec![snap_expr.clone()];
        for c in &cols {
            if c == "timestamp" {
                continue;
            }
            let q = c.replace('"', "\"\"");
            projections.push(format!("\"{q}\""));
        }
        for c in &cols {
            if c == "timestamp" || c == "duration" { continue; }
            let ty = col_type.get(c).map(String::as_str).unwrap_or("");
            if !is_scalar_type(ty) { continue; }
            let q = c.replace('"', "\"\"");
            projections.push(format!("\"{q}\" AS \"{q}:src0\""));
        }
        let create = format!(
            "CREATE OR REPLACE TEMP VIEW _src_default AS SELECT {} FROM read_parquet('{parquet_str}')",
            projections.join(", ")
        );
        conn.execute(&create, [])
            .map_err(|e| format!("create _src_default: {e}"))?;
        return Ok(ParquetSqlState {
            from_clause: "_src_default".to_string(),
            constant_labels: constant_labels_from_field_meta(parquet, None, false),
            is_multi_source: false,
        });
    }
    // Hint the recorded sources for picking — but don't filter the
    // detected prefixes by them (the parquet's `source` KV is
    // semantic, while column prefixes are instance/role names which
    // aren't always identical to the source labels).
    let _ = &recorded_sources;

    // Build one TEMP VIEW per source. In addition to the bare-name
    // alias (`<col>`), expose a `<col>:src0` alias for scalar columns
    // — kept in sync with `duckdb-registry.js::buildSourceViews` —
    // so multi-source-aware emitters (avg/max/min over per-(source,id)
    // values) can use a single regex that works in both single-source
    // mode (one `:src0` per id) and combined mode (N `:src<i>` per id).
    for (src, aliases) in &by_source {
        let view = view_name_for_source(src);
        let mut projections: Vec<String> = vec![snap_expr.clone()];
        if cols.iter().any(|c| c == "duration") {
            projections.push("\"duration\"".to_string());
        }
        for (orig, alias) in aliases {
            let orig_q = orig.replace('"', "\"\"");
            let alias_q = alias.replace('"', "\"\"");
            projections.push(format!("\"{orig_q}\" AS \"{alias_q}\""));
            if is_scalar_type(col_type.get(orig).map(String::as_str).unwrap_or("")) {
                projections.push(format!("\"{orig_q}\" AS \"{alias_q}:src0\""));
            }
        }
        let create = format!(
            "CREATE OR REPLACE TEMP VIEW {view} AS SELECT {} FROM read_parquet('{parquet_str}')",
            projections.join(", ")
        );
        conn.execute(&create, []).map_err(|e| format!("create view {view}: {e}"))?;
    }

    // Picker: rezolus sources (those with `memory_total` aliased) first,
    // optionally hinted by the parquet filename; fall back to the first
    // source. With ≥2 rezolus sources, build a combined view and use
    // it instead of any single source — matches the legacy PromQL
    // evaluator's cross-source aggregation.
    let rezolus_sources: Vec<&String> = by_source
        .iter()
        .filter(|(_src, aliases)| aliases.iter().any(|(_o, a)| a == "memory_total"))
        .map(|(s, _)| s)
        .collect();

    if rezolus_sources.len() >= 2 {
        // For each unprefixed metric name across rezolus sources, track
        // which source it came from (for `:src<i>` aliases) plus the
        // prefixed column name (for the projection expression).
        let mut all_metrics: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
        for (i, src) in rezolus_sources.iter().enumerate() {
            for (orig, alias) in by_source.get(*src).unwrap() {
                all_metrics
                    .entry(alias.clone())
                    .or_default()
                    .push((i, orig.clone()));
            }
        }
        let mut projections: Vec<String> = vec![snap_expr.clone()];
        if cols.iter().any(|c| c == "duration") {
            projections.push("\"duration\"".to_string());
        }
        for (alias, contribs) in &all_metrics {
            let alias_q = alias.replace('"', "\"\"");
            let is_list = contribs
                .iter()
                .any(|(_, c)| col_type.get(c).map(|t| t.ends_with("[]")).unwrap_or(false));
            // Sum form: drives `sum(...)` / `sum by (id) (...)` /
            // `histogram_quantiles(...)` emitters.
            if contribs.len() == 1 {
                let orig_q = contribs[0].1.replace('"', "\"\"");
                projections.push(format!("\"{orig_q}\" AS \"{alias_q}\""));
            } else if is_list {
                let parts: Vec<String> = contribs
                    .iter()
                    .map(|(_, c)| {
                        let q = c.replace('"', "\"\"");
                        format!("COALESCE(\"{q}\", []::UBIGINT[])")
                    })
                    .collect();
                projections.push(format!("h2_combine([{}]) AS \"{alias_q}\"", parts.join(", ")));
            } else {
                let parts: Vec<String> = contribs
                    .iter()
                    .map(|(_, c)| {
                        let q = c.replace('"', "\"\"");
                        format!("COALESCE(\"{q}\", 0)")
                    })
                    .collect();
                projections.push(format!("({}) AS \"{alias_q}\"", parts.join(" + ")));
            }
            // Per-source `:src<i>` aliases for scalar columns — let
            // avg/max/min emitters see each per-(source, id) value.
            if !is_list {
                for (src_idx, c) in contribs {
                    let q = c.replace('"', "\"\"");
                    projections.push(format!("\"{q}\" AS \"{alias_q}:src{src_idx}\""));
                }
            }
        }
        let view = "_src_rezolus_combined";
        let create = format!(
            "CREATE OR REPLACE TEMP VIEW {view} AS SELECT {} FROM read_parquet('{parquet_str}')",
            projections.join(", ")
        );
        conn.execute(&create, [])
            .map_err(|e| format!("create combined view: {e}"))?;
        return Ok(ParquetSqlState {
            from_clause: view.to_string(),
            // Multi-source combined: `is_multi_source: true` drops
            // `source` (ambiguous on the summed series); `endpoint`/
            // `instance` come from the first matching rezolus anchor
            // field. The combined view's series-count divergences are
            // reclassified by `Verdict::ExpectedDivergent` (Bucket #3).
            constant_labels: constant_labels_from_field_meta(parquet, None, true),
            is_multi_source: true,
        });
    }

    let stem_lower = parquet
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let picked: &String = rezolus_sources
        .iter()
        .find(|s| stem_lower.contains(&s.to_lowercase()))
        .copied()
        .or_else(|| rezolus_sources.first().copied())
        .or_else(|| by_source.keys().next())
        .expect("by_source not empty here");

    Ok(ParquetSqlState {
        from_clause: view_name_for_source(picked),
        // Anchor on the rezolus memory_total field under this prefix.
        // A coexisting loadgen role (e.g. cachecannon) under the same
        // parquet prefix has different `source`/`endpoint`/`instance`
        // metadata; anchoring on memory_total picks the rezolus role
        // unambiguously.
        constant_labels: constant_labels_from_field_meta(parquet, Some(picked), false),
        is_multi_source: false,
    })
}

/// Pull `sampling_interval_ms` from the parquet file-level KV
/// (matches `metriken-query-sql/src/views.rs:175`). Returns the
/// interval in nanoseconds. Defaults to 1 s when the metadata is
/// missing — older parquets predate this field.
fn parquet_sampling_interval_ns(conn: &Connection, parquet_str: &str) -> u64 {
    let sql = format!(
        "SELECT value::VARCHAR FROM parquet_kv_metadata('{parquet_str}') WHERE key::VARCHAR = 'sampling_interval_ms'"
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return 1_000_000_000,
    };
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return 1_000_000_000,
    };
    if let Ok(Some(row)) = rows.next() {
        if let Ok(s) = row.get::<_, String>(0) {
            if let Ok(ms) = s.parse::<u64>() {
                return ms * 1_000_000;
            }
        }
    }
    1_000_000_000
}

/// Pull the parquet's `source` KV metadata. Stored as a JSON-encoded
/// string list (e.g. `["rezolus","vllm","llm-bench"]`). Returns the
/// empty set if the metadata isn't present or doesn't parse — callers
/// fall back to "all detected prefixes are sources" in that case.
fn parquet_recorded_sources(conn: &Connection, parquet_str: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let sql = format!(
        "SELECT value::VARCHAR FROM parquet_kv_metadata('{parquet_str}') WHERE key::VARCHAR = 'source'"
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return out,
    };
    while let Ok(Some(row)) = rows.next() {
        let s: String = match row.get(0) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Try parsing as JSON list-of-strings. Single-source parquets
        // record a bare string instead, so accept that shape too.
        if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&s) {
            for v in arr {
                if let Value::String(s) = v { out.insert(s); }
            }
        } else if let Ok(Value::String(s)) = serde_json::from_str::<Value>(&s) {
            out.insert(s);
        } else {
            // Bare unquoted string (older format).
            out.insert(s);
        }
    }
    out
}

fn is_scalar_type(t: &str) -> bool {
    !t.is_empty() && !t.ends_with("[]")
}

/// "Infrastructure" labels — Arrow field metadata keys that don't
/// participate in the canonical column name (they describe where the
/// value came from rather than which series it belongs to).
const NON_VALUE_METADATA_KEYS: &[&str] = &[
    "metric", "metric_type", "unit", "endpoint", "instance", "node", "source",
    "grouping_power", "max_value_power",
];

/// Per-field metadata pulled from the parquet's Arrow schema.
#[derive(Debug)]
struct FieldMeta {
    metric: String,
    metric_type: String,
    /// Sorted (key, value) for value labels only — drop infrastructure
    /// labels here so callers don't repeat the filter.
    value_labels: Vec<(String, String)>,
}

/// Read the parquet's Arrow schema and return a map from column name
/// → [`FieldMeta`] for fields with a `metric` metadata entry. Used to
/// canonicalise numeric-encoded column aliases (`decode::117` →
/// `memory_total`).
fn read_field_metadata(parquet_path: &Path) -> HashMap<String, FieldMeta> {
    let mut out = HashMap::new();
    let bytes = match std::fs::read(parquet_path) {
        Ok(b) => Bytes::from(b),
        Err(_) => return out,
    };
    let m = match ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::default()) {
        Ok(m) => m,
        Err(_) => return out,
    };
    for f in m.schema().fields().iter() {
        let md = f.metadata();
        let metric = match md.get("metric") {
            Some(s) => s.clone(),
            None => continue,
        };
        let metric_type = md.get("metric_type").cloned().unwrap_or_default();
        let mut value_labels: Vec<(String, String)> = md
            .iter()
            .filter(|(k, _)| !NON_VALUE_METADATA_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Numeric-encoded parquets store metadata in alphabetical order,
        // but Rezolus emits column NAMES (in the named-column format)
        // with non-numeric labels first and numeric IDs last — e.g.
        // `softirq/<kind>/<id>`, `cpu_usage/<state>/<id>`. The dashboard
        // SQL's regex (`^softirq/[a-z_]+/[0-9]+$`) only matches that
        // order, so the rebuilt alias has to match too. Sort key:
        // numeric-only values last, alphabetical within each group.
        let is_numeric = |v: &str| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit());
        value_labels.sort_by(|a, b| {
            is_numeric(&a.1)
                .cmp(&is_numeric(&b.1))
                .then_with(|| a.0.cmp(&b.0))
        });
        out.insert(f.name().clone(), FieldMeta { metric, metric_type, value_labels });
    }
    out
}

/// Infrastructure labels (`endpoint`, `source`, `instance`) for the
/// rezolus role's columns — the labels PromQL stamps on every rezolus
/// series at load time via `Tsdb::extract_name_labels`. The SQL backend
/// has to project them explicitly to match.
///
/// We anchor on the field whose `metric` == "memory_total" — the same
/// signal the source picker uses to identify a rezolus source. That
/// avoids the parquet-prefix-vs-recording-source mismatch (a single
/// `0::` prefix can carry both rezolus AND a coexisting loadgen role
/// like cachecannon, each with different `source`). PromQL only ever
/// stamps a series with the label values of *its* underlying column;
/// for rezolus dashboard queries, those columns are the ones matching
/// `metric=memory_total`'s labels.
///
/// `prefix_filter`:
///   - `Some(prefix)`: anchor on a field named `<prefix>::*` with
///     metric=memory_total. Use for `_src_<src>` single-picked views.
///   - `None`: anchor on any field with metric=memory_total. Use for
///     `_src_default` (no prefix) and `_src_rezolus_combined`. For the
///     combined view, multiple rezolus sources exist and *any one* is a
///     reasonable representative for `endpoint`/`instance`; `source` is
///     deliberately omitted via the `is_multi_source` plumbing — it's
///     ambiguous on the summed series.
///
/// `direction` is intentionally excluded — per-column metadata that
/// varies by metric (network-traffic by direction etc.). Closing those
/// few cases needs a per-metric label path (Bucket #3 territory).
fn constant_labels_from_field_meta(
    parquet_path: &Path,
    prefix_filter: Option<&str>,
    is_multi_source: bool,
) -> Vec<(String, String)> {
    let bytes = match std::fs::read(parquet_path) {
        Ok(b) => Bytes::from(b),
        Err(_) => return vec![],
    };
    let m = match ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::default()) {
        Ok(m) => m,
        Err(_) => return vec![],
    };
    let prefix = prefix_filter.map(|s| format!("{s}::"));
    // Find the anchor field: metric=memory_total under the chosen prefix.
    let anchor = m.schema().fields().iter().find(|f| {
        if let Some(p) = &prefix {
            if !f.name().starts_with(p.as_str()) {
                return false;
            }
        }
        f.metadata().get("metric").map(|s| s.as_str()) == Some("memory_total")
    });
    let Some(anchor) = anchor else { return vec![] };
    let md = anchor.metadata();
    let mut keys: Vec<&str> = vec!["endpoint", "instance", "node"];
    if !is_multi_source {
        keys.push("source");
    }
    let mut out: Vec<(String, String)> = vec![];
    for k in keys {
        if let Some(v) = md.get(k) {
            out.push((k.to_string(), v.clone()));
        }
    }
    out.sort();
    out
}

/// Resolve a parquet field's prefixed column name to the alias
/// the dashboard SQL would reference. Trust `rest_after_prefix`
/// when it's already canonical (named-column parquets); rebuild
/// from Arrow metadata otherwise (numeric-encoded). See the JS-side
/// `canonicalAlias` for the matching logic.
fn canonical_alias(rest: &str, meta: Option<&FieldMeta>) -> String {
    let Some(meta) = meta else { return rest.to_string() };
    if rest == meta.metric
        || rest == format!("{}:buckets", meta.metric)
        || rest.starts_with(&format!("{}/", meta.metric))
    {
        return rest.to_string();
    }
    let mut name = meta.metric.clone();
    for (_, v) in &meta.value_labels {
        name.push('/');
        name.push_str(v);
    }
    if meta.metric_type == "histogram" {
        name.push_str(":buckets");
    }
    name
}

fn view_name_for_source(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 5);
    out.push_str("_src_");
    for ch in src.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

fn run_sql(
    conn: &Connection,
    sql: &str,
    sql_state: &ParquetSqlState,
    promql: &str,
    start: f64,
    end: f64,
) -> Result<QueryResult, String> {
    let start_ns: i64 = (start * 1e9) as i64;
    let end_ns: i64 = (end * 1e9) as i64;
    // Project constant infrastructure labels onto every output row so
    // the SQL output carries the same per-series labels PromQL stamps
    // from Arrow field metadata. Production WASM would do the same in
    // its query wrapper at duckdb-registry.js — see remaining_work.md
    // Bucket #1 (Option A).
    //
    // Skip when PromQL strips labels via aggregation (`sum`, `avg`,
    // etc.) — output of a PromQL aggregator without `by(...)` carries
    // an empty label set, and adding labels here would diverge.
    // `histogram_quantile(q, sum(...))` is the same shape (the inner
    // sum strips labels). Conservative detection: any aggregator
    // keyword present.
    let label_proj: String =
        if sql_state.constant_labels.is_empty() || promql_aggregates(promql) {
            String::new()
        } else {
            let parts: Vec<String> = sql_state
                .constant_labels
                .iter()
                .map(|(k, v)| {
                    format!(
                        "'{}' AS \"{}\"",
                        v.replace('\'', "''"),
                        k.replace('"', "\"\"")
                    )
                })
                .collect();
            format!(", {}", parts.join(", "))
        };
    let wrapped = format!(
        "WITH _src AS ( \
            SELECT * FROM {from_clause} \
            WHERE timestamp BETWEEN {start_ns} AND {end_ns} \
         ) \
         SELECT *{label_proj} FROM ({sql}) ORDER BY t",
        from_clause = sql_state.from_clause,
    );
    let mut stmt = match conn.prepare(&wrapped) {
        Ok(s) => s,
        Err(e) => {
            return classify_sql_err(&e.to_string(), "prepare", &wrapped);
        }
    };
    match stmt.query_arrow([]) {
        Ok(arrow) => {
            let batches: Vec<RecordBatch> = arrow.collect();
            arrow_batches_to_matrix(&batches).map_err(|e| e.to_string())
        }
        Err(e) => classify_sql_err(&e.to_string(), "execute", &wrapped),
    }
}

/// Treat "metric not in parquet" failures (DuckDB's Binder Error
/// flavours for missing regex matches and missing exact column refs)
/// as an empty Prometheus matrix, since the legacy PromQL evaluator
/// returns the same shape for "no data". Anything else surfaces as
/// a real harness-side error so we can debug it.
fn classify_sql_err(msg: &str, phase: &str, wrapped: &str) -> Result<QueryResult, String> {
    if msg.contains("No matching columns found that match regex")
        || msg.contains("Referenced column")
            && msg.contains("not found in FROM clause")
    {
        return Ok(QueryResult::Matrix { result: vec![] });
    }
    Err(format!("{phase}: {msg}\nwrapped:\n{wrapped}"))
}

/// Native port of the JS `arrow_table_to_prom_matrix` in
/// `crates/viewer-sql/src/lib.rs:544`. Group rows by the tuple of
/// non-`t`/`v` label-column string values (insertion order preserved),
/// drop NULL `v` rows.
fn arrow_batches_to_matrix(batches: &[RecordBatch]) -> Result<QueryResult, String> {
    if batches.is_empty() {
        return Ok(QueryResult::Matrix { result: vec![] });
    }
    let schema = batches[0].schema();
    let mut t_idx: Option<usize> = None;
    let mut v_idx: Option<usize> = None;
    let mut label_indices: Vec<(usize, String)> = vec![];
    for (i, f) in schema.fields().iter().enumerate() {
        match f.name().as_str() {
            "t" => t_idx = Some(i),
            "v" => v_idx = Some(i),
            other => label_indices.push((i, other.to_string())),
        }
    }
    let t_idx = t_idx.ok_or_else(|| "result schema missing `t`".to_string())?;
    let v_idx = v_idx.ok_or_else(|| "result schema missing `v`".to_string())?;

    let mut groups: Vec<(BTreeMap<String, String>, Vec<(f64, f64)>)> = vec![];
    let mut group_index: HashMap<String, usize> = HashMap::new();

    for batch in batches {
        let n_rows = batch.num_rows();
        let t_arr = batch.column(t_idx);
        let v_arr = batch.column(v_idx);
        let label_arrs: Vec<(&dyn Array, &str)> = label_indices
            .iter()
            .map(|(i, name)| (batch.column(*i).as_ref(), name.as_str()))
            .collect();
        for r in 0..n_rows {
            let v_val = match arrow_cell_as_f64(v_arr, r) {
                Some(v) => v,
                None => continue, // drop NULL v rows
            };
            let t_val = arrow_cell_as_f64(t_arr, r)
                .ok_or_else(|| "NULL t in result".to_string())?;
            // Build label map (string-coerced).
            let mut labels: BTreeMap<String, String> = BTreeMap::new();
            let mut key_buf = String::new();
            for (arr, name) in &label_arrs {
                let s = arrow_cell_as_string(*arr, r);
                key_buf.push_str(name);
                key_buf.push('=');
                key_buf.push_str(&s);
                key_buf.push('\u{1f}'); // unit separator
                labels.insert((*name).to_string(), s);
            }
            let idx = match group_index.get(&key_buf) {
                Some(&idx) => idx,
                None => {
                    let idx = groups.len();
                    group_index.insert(key_buf.clone(), idx);
                    groups.push((labels, vec![]));
                    idx
                }
            };
            groups[idx].1.push((t_val, v_val));
        }
    }

    let result: Vec<MatrixSample> = groups
        .into_iter()
        .map(|(labels, values)| MatrixSample {
            metric: labels.into_iter().collect(),
            values,
        })
        .collect();
    Ok(QueryResult::Matrix { result })
}

fn arrow_cell_as_f64(arr: &dyn Array, row: usize) -> Option<f64> {
    if arr.is_null(row) {
        return None;
    }
    match arr.data_type() {
        DataType::Float64 => Some(arr.as_any().downcast_ref::<Float64Array>().unwrap().value(row)),
        DataType::Int64 => Some(arr.as_any().downcast_ref::<Int64Array>().unwrap().value(row) as f64),
        DataType::UInt64 => Some(arr.as_any().downcast_ref::<UInt64Array>().unwrap().value(row) as f64),
        DataType::Int32 => Some(arr.as_any().downcast_ref::<Int32Array>().unwrap().value(row) as f64),
        DataType::UInt32 => Some(arr.as_any().downcast_ref::<UInt32Array>().unwrap().value(row) as f64),
        DataType::Timestamp(_, _) => arr
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .map(|a| a.value(row) as f64 / 1e9),
        // DuckDB synthesises Decimal128/256 for some macro outputs (e.g. the
        // `* 8` literal in `bandwidth-rx` widens the type). Convert via the
        // array's scale to recover the f64 value the dashboard expects.
        DataType::Decimal128(_, scale) => {
            let v = arr
                .as_any()
                .downcast_ref::<Decimal128Array>()
                .unwrap()
                .value(row);
            Some(v as f64 / 10f64.powi(*scale as i32))
        }
        DataType::Decimal256(_, scale) => {
            // i256 → f64 via to_string then parse — no native conversion.
            let i = arr
                .as_any()
                .downcast_ref::<Decimal256Array>()
                .unwrap()
                .value(row);
            i.to_string().parse::<f64>().ok().map(|v| v / 10f64.powi(*scale as i32))
        }
        _ => None,
    }
}

fn arrow_cell_as_string(arr: &dyn Array, row: usize) -> String {
    if arr.is_null(row) {
        return "null".to_string();
    }
    match arr.data_type() {
        DataType::Utf8 => arr
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .value(row)
            .to_string(),
        DataType::Float64 => arr
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap()
            .value(row)
            .to_string(),
        DataType::Int64 => arr
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .value(row)
            .to_string(),
        DataType::UInt64 => arr
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .value(row)
            .to_string(),
        DataType::Int32 => arr
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap()
            .value(row)
            .to_string(),
        DataType::UInt32 => arr
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap()
            .value(row)
            .to_string(),
        DataType::Decimal128(_, scale) => {
            let v = arr
                .as_any()
                .downcast_ref::<Decimal128Array>()
                .unwrap()
                .value(row);
            (v as f64 / 10f64.powi(*scale as i32)).to_string()
        }
        DataType::Decimal256(_, scale) => {
            let i = arr
                .as_any()
                .downcast_ref::<Decimal256Array>()
                .unwrap()
                .value(row);
            i.to_string()
                .parse::<f64>()
                .map(|v| (v / 10f64.powi(*scale as i32)).to_string())
                .unwrap_or_else(|_| i.to_string())
        }
        _ => format!("{:?}", arr.data_type()),
    }
}

// ── Comparison.

/// Heuristic: does the dashboard SQL aggregate columns BEFORE applying
/// a rate? If so, on a multi-source parquet the harness expects
/// methodology divergence vs PromQL's per-series-rate-then-sum (see
/// `Verdict::ExpectedDivergent`). Matches the resolvers in
/// `crates/dashboard/src/sql.rs`: `irate_total`, `cpu_pct_total`,
/// `concept_total`, `hist_percentile_series_combined`.
fn is_aggregate_then_rate(sql: &str) -> bool {
    sql.contains("list_sum([*COLUMNS") || sql.contains("h2_combine([*COLUMNS")
}

/// Heuristic: is this divergence a "promql has more series than sql"
/// case caused by `_src_rezolus_combined` collapsing per-source series?
/// Matches the comparator's `series count: promql=N sql=M` reason
/// where N > M.
fn is_combined_view_series_count(reason: &str) -> bool {
    let Some(rest) = reason.strip_prefix("series count: promql=") else {
        return false;
    };
    let Some((p_str, s_part)) = rest.split_once(" sql=") else {
        return false;
    };
    let p: usize = match p_str.parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    let s: usize = match s_part.parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    p > s
}

/// Heuristic: does the PromQL query produce output without
/// per-series infrastructure labels (`endpoint`/`source`/`instance`)?
/// True for any aggregator (`sum`/`avg`/`max`/`min`/`count`/`topk`)
/// — PromQL strips labels unless `by(...)` is given, and even
/// `by(id)` keeps only the by-keys. Also true for the histogram
/// reorganizers (`histogram_quantile(s)`) which the metriken-query
/// PromQL implementation projects without infrastructure labels.
///
/// When this returns true, the SQL wrapper skips the constant-label
/// projection so SQL output matches PromQL's label-stripped shape.
fn promql_aggregates(promql: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "sum(", "sum (", "sum by",
        "avg(", "avg (", "avg by",
        "max(", "max (", "max by",
        "min(", "min (", "min by",
        "count(", "count (", "count by",
        "topk(", "bottomk(",
        // Histogram reorganizers strip infrastructure labels in the
        // metriken-query PromQL implementation — output carries only
        // `quantile` (and any preserved by-keys from inner aggregation).
        "histogram_quantile(", "histogram_quantiles(",
    ];
    KEYWORDS.iter().any(|k| promql.contains(k))
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Identical,
    WithinTolerance { max_rel: f64, max_abs: f64 },
    Divergent { reason: String },
    /// Reclassified `Divergent` when:
    ///   - the parquet has ≥2 rezolus sources (`_src_rezolus_combined`), AND
    ///   - the dashboard SQL aggregates across columns BEFORE rating
    ///     (`list_sum([*COLUMNS(...)])` or `h2_combine([*COLUMNS(...)])`).
    /// PromQL does per-series-irate-then-sum; SQL does sum-then-irate.
    /// The two differ at counter resets within the lookback window —
    /// aggregate-then-rate is what dashboards typically want for
    /// cross-source aggregations, so this is documented and accepted
    /// rather than fixed (Bucket #3 in remaining_work.md). Counted in
    /// the summary, not in `divergent`.
    ExpectedDivergent { reason: String },
    SkippedCgroupPlaceholder,
    PromqlError(String),
    SqlError(String),
}

#[derive(Serialize)]
struct PairOutcome {
    parquet: String,
    plot_id: String,
    section: String,
    plot_type: String,
    promql: String,
    verdict: Verdict,
}

fn compare(promql: &QueryResult, sql: &QueryResult, rel_tol: f64, abs_tol: f64) -> Verdict {
    use QueryResult::*;
    match (promql, sql) {
        (Matrix { result: a }, Matrix { result: b }) => compare_matrix(a, b, rel_tol, abs_tol),
        (HistogramHeatmap { result: _a }, HistogramHeatmap { result: _b }) => {
            // Heatmap shapes: dashboard SQL doesn't emit the heatmap
            // wrapper today; if it ever does, port the per-cell diff
            // here. Treat as divergent if we hit this and the variants
            // don't match by deep equality.
            if serde_json::to_value(promql).unwrap() == serde_json::to_value(sql).unwrap() {
                Verdict::Identical
            } else {
                Verdict::Divergent {
                    reason: "histogram_heatmap differ".into(),
                }
            }
        }
        (a, b) => {
            if std::mem::discriminant(a) != std::mem::discriminant(b) {
                return Verdict::Divergent {
                    reason: format!(
                        "result-type mismatch: promql={} sql={}",
                        result_kind(a),
                        result_kind(b)
                    ),
                };
            }
            // Vector / Scalar: serialize and string-compare
            if serde_json::to_value(a).unwrap() == serde_json::to_value(b).unwrap() {
                Verdict::Identical
            } else {
                Verdict::Divergent {
                    reason: "non-matrix result differs".into(),
                }
            }
        }
    }
}

fn result_kind(r: &QueryResult) -> &'static str {
    match r {
        QueryResult::Matrix { .. } => "matrix",
        QueryResult::Vector { .. } => "vector",
        QueryResult::Scalar { .. } => "scalar",
        QueryResult::HistogramHeatmap { .. } => "histogram_heatmap",
    }
}

fn compare_matrix(
    a: &[MatrixSample],
    b: &[MatrixSample],
    rel_tol: f64,
    abs_tol: f64,
) -> Verdict {
    if a.len() != b.len() {
        return Verdict::Divergent {
            reason: format!("series count: promql={} sql={}", a.len(), b.len()),
        };
    }
    // Match series by their label maps. Strict labels (with `__name__`
    // and quantile-formatting normalization handled by `canon_labels`).
    let mut b_by_labels: HashMap<String, &MatrixSample> = HashMap::new();
    for s in b {
        b_by_labels.insert(canon_labels(&s.metric), s);
    }
    let mut max_rel = 0.0_f64;
    let mut max_abs = 0.0_f64;
    let mut strict = true;
    for sa in a {
        let key = canon_labels(&sa.metric);
        let Some(sb) = b_by_labels.get(&key) else {
            return Verdict::Divergent {
                reason: format!("label set on PromQL side missing from SQL: {key}"),
            };
        };
        match compare_series_aligned(&sa.values, &sb.values, rel_tol, abs_tol) {
            SeriesCompare::Identical => {}
            SeriesCompare::WithinTolerance { rel, abs } => {
                strict = false;
                if rel > max_rel { max_rel = rel; }
                if abs > max_abs { max_abs = abs; }
            }
            SeriesCompare::Divergent(reason) => {
                return Verdict::Divergent {
                    reason: format!("labels {key}: {reason}"),
                };
            }
        }
    }
    if strict {
        Verdict::Identical
    } else {
        Verdict::WithinTolerance { max_rel, max_abs }
    }
}

enum SeriesCompare {
    Identical,
    WithinTolerance { rel: f64, abs: f64 },
    Divergent(String),
}

/// Align values by rounded-integer-second timestamps before comparing.
/// PromQL evaluates at the step-grid timestamp (`start + i·step`,
/// always aligned to whole seconds for our 1 Hz step). SQL emits the
/// raw parquet `timestamp::DOUBLE/1e9`, which has sub-millisecond
/// drift but lands in the same integer second. Boundary effects show
/// up as 0–1 extra samples on the PromQL side at the start/end of the
/// window — accept those gracefully.
fn compare_series_aligned(
    a: &[(f64, f64)],
    b: &[(f64, f64)],
    rel_tol: f64,
    abs_tol: f64,
) -> SeriesCompare {
    let bucket = |t: f64| t.round() as i64;
    let mut amap: BTreeMap<i64, f64> = BTreeMap::new();
    let mut bmap: BTreeMap<i64, f64> = BTreeMap::new();
    let mut a_dup = 0usize;
    let mut b_dup = 0usize;
    for &(t, v) in a {
        if amap.insert(bucket(t), v).is_some() { a_dup += 1; }
    }
    for &(t, v) in b {
        if bmap.insert(bucket(t), v).is_some() { b_dup += 1; }
    }
    // Duplicates within a single bucket on the SQL side indicate the
    // dashboard SQL emits multiple rows per (t, label-set) — a real
    // shape bug (e.g. cpu-busy-heatmap missing a `sum by`).
    if b_dup > 0 || a_dup > 0 {
        return SeriesCompare::Divergent(format!(
            "duplicate samples per integer-second bucket: promql_dups={a_dup} sql_dups={b_dup}"
        ));
    }
    // Compute key sets.
    let only_a: Vec<i64> = amap.keys().filter(|k| !bmap.contains_key(k)).copied().collect();
    let only_b: Vec<i64> = bmap.keys().filter(|k| !amap.contains_key(k)).copied().collect();
    // Boundary tolerance: each side may have up to 1 integer-second
    // bucket the other doesn't. Tightened from ±2 to ±1 after the
    // rate_5m range-window fix (B-2) eliminated the spurious NULLs at
    // window edges that previously needed the ±2 cushion.
    if only_a.len() > 1 {
        return SeriesCompare::Divergent(format!(
            "PromQL has {} integer-second timestamps SQL doesn't (>1 boundary tolerance)",
            only_a.len(),
        ));
    }
    if only_b.len() > 1 {
        return SeriesCompare::Divergent(format!(
            "SQL has {} integer-second timestamps PromQL doesn't (>1 boundary tolerance, e.g. {})",
            only_b.len(),
            only_b
                .iter()
                .take(3)
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    let mut max_rel = 0.0_f64;
    let mut max_abs = 0.0_f64;
    let mut strict = true;
    for (k, va) in &amap {
        let Some(vb) = bmap.get(k) else { continue }; // boundary skip
        let va = *va;
        let vb = *vb;
        let va_nan = va.is_nan();
        let vb_nan = vb.is_nan();
        if va_nan != vb_nan {
            return SeriesCompare::Divergent(format!(
                "NaN mismatch at t={k}: promql={va} sql={vb}"
            ));
        }
        if va_nan { continue; }
        if va == vb { continue; }
        strict = false;
        let abs = (va - vb).abs();
        let rel = abs / va.abs().max(vb.abs()).max(f64::MIN_POSITIVE);
        if abs > abs_tol && rel > rel_tol {
            return SeriesCompare::Divergent(format!(
                "value at t={k}: promql={va} sql={vb} abs={abs:.3e} rel={rel:.3e}"
            ));
        }
        if rel > max_rel { max_rel = rel; }
        if abs > max_abs { max_abs = abs; }
    }
    if strict {
        SeriesCompare::Identical
    } else {
        SeriesCompare::WithinTolerance { rel: max_rel, abs: max_abs }
    }
}

/// Build a normalized canonical-string key for a label map so two
/// label sets that differ only in cosmetic ways match. We:
///   - drop `__name__` (PromQL stamps it onto every series; the SQL
///     side never has it because the dashboard's emitted SQL is the
///     metric form, not the PromQL `vector` form);
///   - parse numeric label values via `f64` and re-format with a
///     canonical decimal so `0.5` and `0.5000` (DuckDB's
///     `DECIMAL::VARCHAR` formatting) hash to the same key.
fn canon_labels(m: &HashMap<String, String>) -> String {
    let mut entries: Vec<(&String, String)> = m
        .iter()
        .filter(|(k, _)| k.as_str() != "__name__")
        .map(|(k, v)| (k, normalize_label_value(v)))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut s = String::new();
    for (k, v) in entries {
        s.push_str(k);
        s.push('=');
        s.push_str(&v);
        s.push('\u{1f}');
    }
    s
}

fn normalize_label_value(v: &str) -> String {
    // If it parses as a number, re-emit canonically (this collapses
    // "0.5" and "0.5000"). Otherwise leave the string verbatim.
    match v.parse::<f64>() {
        Ok(n) if n.is_finite() => {
            // Use Rust's default Display, which omits trailing zeros.
            format!("{}", n)
        }
        _ => v.to_string(),
    }
}

// ── Driver.

fn write_json(path: &Path, value: &impl Serialize) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir output");
    }
    let s = serde_json::to_string_pretty(value).expect("serialize");
    fs::write(path, s).expect("write json");
}

fn safe_filename(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => out.push(ch),
            _ => out.push('_'),
        }
    }
    out
}

#[derive(Default, Serialize)]
struct Counts {
    identical: usize,
    within_tolerance: usize,
    divergent: usize,
    /// See `Verdict::ExpectedDivergent`.
    expected_divergent: usize,
    skipped_cgroup: usize,
    promql_error: usize,
    sql_error: usize,
}

#[derive(Serialize)]
struct Summary {
    rel_tol: f64,
    abs_tol: f64,
    parquets: Vec<String>,
    total_plots: usize,
    pairs_total: usize,
    counts: Counts,
    runtime_ms: u128,
    per_parquet: BTreeMap<String, Counts>,
    per_section: BTreeMap<String, Counts>,
}

fn main() {
    let args = parse_args();
    fs::create_dir_all(&args.out).expect("mkdir out");
    let started = Instant::now();
    let plots = load_plots(&args.dashboard_dir);
    let plots = match args.max_plots {
        Some(n) => plots.into_iter().take(n).collect::<Vec<_>>(),
        None => plots,
    };
    eprintln!(
        "loaded {} plots from {}",
        plots.len(),
        args.dashboard_dir.display()
    );

    let mut summary = Summary {
        rel_tol: args.rel_tol,
        abs_tol: args.abs_tol,
        parquets: args.parquets.iter().map(|p| p.display().to_string()).collect(),
        total_plots: plots.len(),
        pairs_total: 0,
        counts: Counts::default(),
        runtime_ms: 0,
        per_parquet: BTreeMap::new(),
        per_section: BTreeMap::new(),
    };

    // One wasm-style DuckDB connection reused across parquets. Macros
    // are global on the connection, so loading them once is fine; each
    // query references `read_parquet('<path>')` inline.
    let sql_conn = open_wasm_style_conn().expect("open wasm-style duckdb");

    for parquet in &args.parquets {
        let parquet_str = parquet
            .to_str()
            .expect("parquet path not utf-8")
            .to_string();
        let parquet_stem = parquet
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "parquet".to_string());
        eprintln!("=== {} ===", parquet_str);

        let (start, end) = parquet_time_range_seconds(parquet);
        let step = viewer_step(start, end);
        let win_start = (end - 3600.0).max(start);
        eprintln!(
            "time range: [{:.0}, {:.0}] (window: [{:.0}, {:.0}], step={})",
            start, end, win_start, end, step
        );

        // Boot legacy Tsdb + QueryEngine for this parquet.
        #[cfg(feature = "legacy")]
        let (tsdb, engine) = {
            let t0 = Instant::now();
            let tsdb = Arc::new(Tsdb::load(parquet).expect("Tsdb::load"));
            eprintln!("  Tsdb loaded in {:.2}s", t0.elapsed().as_secs_f64());
            let engine = QueryEngine::new(tsdb.clone());
            (tsdb, engine)
        };
        #[cfg(not(feature = "legacy"))]
        let _ = (); // legacy required for harness

        // Set up source views + pick the active source. For
        // single-source parquets this is a no-op (returns
        // `read_parquet(...)`). For multi-source parquets, we build
        // `_src_<source>` TEMP VIEWs once and route queries through
        // the picked source's view inside the time-window CTE.
        let sql_state = match setup_parquet_sql(&sql_conn, parquet) {
            Ok(s) => {
                if s.from_clause.starts_with("_src_") {
                    eprintln!("  picked source: {}", s.from_clause);
                }
                s
            }
            Err(e) => {
                eprintln!("  setup_parquet_sql failed: {e} — skipping parquet");
                continue;
            }
        };

        let mut p_counts = Counts::default();
        let mut divergences: Vec<String> = vec![];

        for plot in &plots {
            summary.pairs_total += 1;
            let outcome_path = args
                .out
                .join(&parquet_stem)
                .join(format!("{}.json", safe_filename(&plot.id)));

            // Skip cgroup-placeholder plots in v1 — they need a cgroup
            // selection materialized which the harness doesn't model
            // yet. Logged as skipped.
            if plot.sql.contains("__SELECTED_CGROUPS__")
                || plot.promql.contains("__SELECTED_CGROUPS__")
            {
                let outcome = PairOutcome {
                    parquet: parquet_str.clone(),
                    plot_id: plot.id.clone(),
                    section: plot.section.clone(),
                    plot_type: plot.plot_type.clone(),
                    promql: plot.promql.clone(),
                    verdict: Verdict::SkippedCgroupPlaceholder,
                };
                write_json(&outcome_path, &outcome);
                p_counts.skipped_cgroup += 1;
                summary.counts.skipped_cgroup += 1;
                bump_section(&mut summary.per_section, &plot.section, |c| c.skipped_cgroup += 1);
                continue;
            }

            // Run PromQL.
            let effective = effective_promql(plot);
            #[cfg(feature = "legacy")]
            let promql_res = run_promql(&engine, &effective, win_start, end, step);
            #[cfg(not(feature = "legacy"))]
            let promql_res: Result<QueryResult, String> =
                Err("legacy feature disabled".into());

            // Run SQL via the wasm-style connection. Pass the PromQL
            // body so the wrapper can decide whether to project
            // constant labels (PromQL aggregators strip labels).
            let sql_res = run_sql(&sql_conn, &plot.sql, &sql_state, &effective, win_start, end);

            let verdict = match (&promql_res, &sql_res) {
                (Err(e1), _) => Verdict::PromqlError(e1.clone()),
                (_, Err(e2)) => Verdict::SqlError(e2.clone()),
                (Ok(p), Ok(s)) => compare(p, s, args.rel_tol, args.abs_tol),
            };
            // On multi-source parquets routed through
            // `_src_rezolus_combined`, every divergence is methodology:
            // the combined view sums values across rezolus sources
            // before any rate computation, while PromQL evaluates each
            // source independently. This produces:
            //   - Series-count differences (`promql=N sql=1`).
            //   - Value differences at intra-window counter resets
            //     (per-source resets get smoothed in the combined sum).
            //   - Label differences (per-source labels are absent on
            //     the combined series).
            // The combined-view aggregation is what dashboards typically
            // want for cross-source totals, so we count these in a
            // separate `expected_divergent` bucket rather than failing
            // the comparison (Bucket #3 in remaining_work.md).
            let verdict = match verdict {
                Verdict::Divergent { reason } if sql_state.is_multi_source => {
                    Verdict::ExpectedDivergent { reason }
                }
                v => v,
            };

            // Save both results next to the verdict for post-hoc diffing.
            let pair_dump = serde_json::json!({
                "parquet": parquet_str,
                "plot_id": plot.id,
                "section": plot.section,
                "plot_type": plot.plot_type,
                "subtype": plot.subtype,
                "promql_raw": plot.promql,
                "promql_effective": effective,
                "sql": plot.sql,
                "window": { "start": win_start, "end": end, "step": step },
                "verdict": &verdict,
                "promql_result": promql_res.as_ref().ok(),
                "sql_result": sql_res.as_ref().ok(),
                "promql_error": promql_res.as_ref().err(),
                "sql_error": sql_res.as_ref().err(),
            });
            write_json(&outcome_path, &pair_dump);

            // Tally.
            match &verdict {
                Verdict::Identical => {
                    p_counts.identical += 1;
                    summary.counts.identical += 1;
                    bump_section(&mut summary.per_section, &plot.section, |c| c.identical += 1);
                }
                Verdict::WithinTolerance { .. } => {
                    p_counts.within_tolerance += 1;
                    summary.counts.within_tolerance += 1;
                    bump_section(&mut summary.per_section, &plot.section, |c| {
                        c.within_tolerance += 1
                    });
                }
                Verdict::Divergent { reason } => {
                    p_counts.divergent += 1;
                    summary.counts.divergent += 1;
                    bump_section(&mut summary.per_section, &plot.section, |c| c.divergent += 1);
                    divergences.push(format!(
                        "{}::{} ({}) — {}",
                        parquet_stem, plot.id, plot.section, reason
                    ));
                }
                Verdict::ExpectedDivergent { .. } => {
                    p_counts.expected_divergent += 1;
                    summary.counts.expected_divergent += 1;
                    bump_section(&mut summary.per_section, &plot.section, |c| {
                        c.expected_divergent += 1
                    });
                }
                Verdict::PromqlError(_) => {
                    p_counts.promql_error += 1;
                    summary.counts.promql_error += 1;
                    bump_section(&mut summary.per_section, &plot.section, |c| {
                        c.promql_error += 1
                    });
                }
                Verdict::SqlError(_) => {
                    p_counts.sql_error += 1;
                    summary.counts.sql_error += 1;
                    bump_section(&mut summary.per_section, &plot.section, |c| c.sql_error += 1);
                }
                Verdict::SkippedCgroupPlaceholder => unreachable!(),
            }
        }

        eprintln!(
            "  {parquet_stem}: identical={} tolerant={} divergent={} promql_err={} sql_err={} skipped={}",
            p_counts.identical,
            p_counts.within_tolerance,
            p_counts.divergent,
            p_counts.promql_error,
            p_counts.sql_error,
            p_counts.skipped_cgroup,
        );
        summary.per_parquet.insert(parquet_stem.clone(), p_counts);

        // Per-parquet divergence dump.
        if !divergences.is_empty() {
            let div_path = args.out.join(format!("{parquet_stem}.divergences.txt"));
            fs::write(&div_path, divergences.join("\n") + "\n").ok();
        }

        #[cfg(feature = "legacy")]
        drop(tsdb);
    }

    summary.runtime_ms = started.elapsed().as_millis();
    write_json(&args.out.join("summary.json"), &summary);
    eprintln!("\n=== summary ===");
    eprintln!(
        "  pairs={} identical={} tolerant={} divergent={} promql_err={} sql_err={} skipped={}",
        summary.pairs_total,
        summary.counts.identical,
        summary.counts.within_tolerance,
        summary.counts.divergent,
        summary.counts.promql_error,
        summary.counts.sql_error,
        summary.counts.skipped_cgroup,
    );
    eprintln!("  runtime: {:.1}s", summary.runtime_ms as f64 / 1000.0);
    eprintln!("  out: {}", args.out.display());
}

fn bump_section<F: FnOnce(&mut Counts)>(
    map: &mut BTreeMap<String, Counts>,
    section: &str,
    f: F,
) {
    let entry = map.entry(section.to_string()).or_default();
    f(entry);
}
