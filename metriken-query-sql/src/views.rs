//! Parquet load + per-metric catalog.
//!
//! Metriken parquet files store metrics in *wide* form: one column per
//! labeled series, with the canonical metric name and label values
//! living in Arrow field metadata. The wide-form SQL generator
//! projects directly off these columns, so this module's job is only:
//!
//! 1. Load the parquet into an in-memory `_src` table (one read,
//!    snapped timestamps).
//! 2. Build a `MetricCatalog` indexing each canonical metric name to
//!    its physical columns + label maps + (for histograms)
//!    `grouping_power`. The wide-form generator consumes this index
//!    to know which columns to project for any `M{labels}` selector.
//!
//! No per-metric VIEWs are created — the long-form
//! `metric(timestamp, value, ...labels)` layer was removed once
//! every catalogue entry had a wide-form path.

use std::collections::{BTreeMap, BTreeSet};

use arrow::datatypes::DataType;
use duckdb::Connection;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};

/// Internal: classify a column from its data type + metadata.
#[derive(Debug)]
pub(crate) enum ColumnKind {
    Counter,
    Gauge,
    Histogram { grouping_power: u8 },
    Other,
}

#[derive(Debug)]
pub(crate) struct ColumnInfo {
    /// The actual column name in the parquet file.
    pub(crate) physical: String,
    /// Canonical metric name (from `metric` metadata, or the column name
    /// minus a `:buckets` suffix).
    pub(crate) metric: String,
    pub(crate) kind: ColumnKind,
    /// All label key-value pairs (excluding the internal keys `metric`,
    /// `metric_type`, `unit`, `grouping_power`, `max_value_power`).
    pub(crate) labels: BTreeMap<String, String>,
}

fn classify(field: &arrow::datatypes::Field) -> ColumnInfo {
    let mut meta = field.metadata().clone();
    let metric = meta.remove("metric").unwrap_or_else(|| {
        field
            .name()
            .strip_suffix(":buckets")
            .unwrap_or(field.name())
            .to_string()
    });
    let _metric_type = meta.remove("metric_type");
    let _unit = meta.remove("unit");
    let grouping_power: Option<u8> = meta.remove("grouping_power").and_then(|v| v.parse().ok());
    let _max_value_power = meta.remove("max_value_power");
    let labels: BTreeMap<String, String> = meta.into_iter().collect();

    let kind = match field.data_type() {
        DataType::UInt64 => ColumnKind::Counter,
        DataType::Int64 => ColumnKind::Gauge,
        DataType::List(inner) if inner.data_type() == &DataType::UInt64 => {
            if let Some(gp) = grouping_power {
                ColumnKind::Histogram { grouping_power: gp }
            } else {
                ColumnKind::Other
            }
        }
        _ => ColumnKind::Other,
    };

    ColumnInfo {
        physical: field.name().clone(),
        metric,
        kind,
        labels,
    }
}

/// Read the parquet schema, load the file into a single in-memory
/// `_src` TEMP TABLE, and build the per-metric catalog the wide-form
/// SQL generator consumes.
///
/// **Performance:** the parquet is loaded **once** into the `_src`
/// table. Wide-form SQL queries reference `_src` instead of
/// `read_parquet(...)` directly, so each query is an in-memory scan
/// (cheap) rather than a fresh parquet read (~10-100s on Rezolus
/// production data). The single parquet read amortises across every
/// query for the lifetime of the connection.
/// Per-metric metadata built once at `ensure_views` time. The backend
/// caches this alongside the connection so per-query pre-flight
/// checks (which physical columns belong to a metric, what the
/// histogram's grouping_power is) are hashmap reads.
///
/// `series_by_metric` is the per-(metric, physical-column) label map.
/// Used by the wide-form SQL generator to project values/rates
/// directly off `_src` columns; the long-form metric VIEW is gone.
#[derive(Debug, Default, Clone)]
pub struct MetricCatalog {
    /// Per-metric ordered list of physical columns + their label maps.
    /// Drives the wide-form SQL generator. Order is parquet-schema order.
    pub series_by_metric: std::collections::HashMap<String, Vec<MetricSeries>>,
    /// Per-histogram-metric `grouping_power` (`p`). The wide-form
    /// histogram path needs `p` to call `h2_quantile` / `h2_heatmap`.
    /// Counters/gauges are absent from this map.
    pub histogram_p_by_metric: std::collections::HashMap<String, u8>,
}

#[derive(Debug, Clone)]
pub struct MetricSeries {
    pub physical: String,
    pub labels: BTreeMap<String, String>,
}

/// Read parquet metadata: classify columns, dedupe duplicates, extract
/// the sampling interval. Pure introspection — no DuckDB side effects.
pub(crate) fn read_introspection(parquet_path: &str) -> duckdb::Result<(Vec<ColumnInfo>, u64)> {
    let bytes = std::fs::read(parquet_path).map_err(|e| {
        duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error {
                code: duckdb::ErrorCode::InternalMalfunction,
                extended_code: 0,
            },
            Some(format!("read parquet {parquet_path}: {e}")),
        )
    })?;
    let bytes = bytes::Bytes::from(bytes);
    let meta = ArrowReaderMetadata::load(&bytes, ArrowReaderOptions::default()).map_err(|e| {
        duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error {
                code: duckdb::ErrorCode::InternalMalfunction,
                extended_code: 0,
            },
            Some(format!("read parquet metadata: {e}")),
        )
    })?;
    let schema = meta.schema().clone();

    // Some Rezolus parquet writers (e.g. the disaggregated-serving sglang
    // recordings) emit duplicate top-level column names with the same
    // type/metadata. DuckDB's `read_parquet` silently keeps only the first
    // occurrence in `_src`, but `arrow::Schema::fields()` returns all of
    // them — without dedup the catalog would index physical column names
    // that don't exist in `_src`. Dedupe by physical name, first
    // occurrence wins, matching DuckDB's behavior.
    let mut seen_physical: BTreeSet<String> = BTreeSet::new();
    let columns: Vec<ColumnInfo> = schema
        .fields()
        .iter()
        .filter_map(|f| {
            if f.name() == "timestamp" || f.name() == "duration" {
                return None;
            }
            let info = classify(f);
            if matches!(info.kind, ColumnKind::Other) {
                return None;
            }
            if !seen_physical.insert(info.physical.clone()) {
                return None;
            }
            Some(info)
        })
        .collect();

    // Sampling interval (ns), pulled from the parquet file-level kv. The
    // metriken-query loader snaps every parquet timestamp to the nearest
    // multiple of this interval (`tsdb/mod.rs:50-58`), so the SQL side has
    // to do the same — otherwise shadow-mode comparison against PromQL diffs
    // on the timestamp axis even when values agree exactly.
    let interval_ns: u64 = meta
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|kvs| {
            kvs.iter().find_map(|kv| {
                if kv.key == "sampling_interval_ms" {
                    kv.value.as_ref().and_then(|v| v.parse::<u64>().ok())
                } else {
                    None
                }
            })
        })
        .map(|ms| ms * 1_000_000)
        .unwrap_or(1_000_000_000);

    Ok((columns, interval_ns))
}

/// Group classified columns by canonical metric name and record
/// histogram grouping_power. Pure data transformation; no I/O.
pub(crate) fn build_catalog(columns: &[ColumnInfo]) -> MetricCatalog {
    let mut by_metric: BTreeMap<String, Vec<&ColumnInfo>> = BTreeMap::new();
    for c in columns {
        by_metric.entry(c.metric.clone()).or_default().push(c);
    }

    let mut catalog = MetricCatalog::default();
    for (metric, cols) in by_metric {
        // Record `grouping_power` for histogram metrics so wide-form
        // can pass it to h2_quantile / h2_heatmap without recomputing.
        if let Some(ColumnKind::Histogram { grouping_power }) = cols.first().map(|c| &c.kind) {
            catalog
                .histogram_p_by_metric
                .insert(metric.clone(), *grouping_power);
        }
        let series: Vec<MetricSeries> = cols
            .iter()
            .map(|c| MetricSeries {
                physical: c.physical.clone(),
                labels: c.labels.clone(),
            })
            .collect();
        catalog.series_by_metric.insert(metric, series);
    }
    catalog
}

/// Read the parquet schema and return the per-metric catalog without
/// any DuckDB side effects. Used by callers that need parquet metadata
/// (e.g. to generate SQL for a wide-form selector) but don't yet have
/// — or don't want to commit to — a `_src` table. Cheap to call from
/// any thread; reads the file once.
pub fn describe_parquet(parquet_path: &str) -> duckdb::Result<MetricCatalog> {
    let (columns, _interval_ns) = read_introspection(parquet_path)?;
    Ok(build_catalog(&columns))
}

/// Create the `_src` TEMP TABLE on `conn` from `parquet_path`,
/// snapping timestamps to the nearest multiple of `interval_ns`.
///
/// The snap matches PromQL's `Tsdb` loader so SQL queries see the
/// same canonical timestamps PromQL does. `//` is integer division —
/// DuckDB's `/` promotes to DOUBLE which loses precision at parquet-
/// timestamp scale (~1.8e18 ns; DOUBLE has only ~15.95 significant
/// decimal digits, so the snap would silently become a no-op).
///
/// **Multi-source aliasing.** Combined parquets (e.g. cachecannon
/// emits `cachecannon` + `rezolus` sources) have columns prefixed
/// `<source>::<column>`; the dashboard SQL expects unprefixed
/// canonical names like `cpu_usage/<state>/<id>`. When prefixes are
/// detected we pick the single rezolus source's columns (those with
/// `source=rezolus` in their Arrow field metadata) and project them
/// as canonical aliases — same shape the wasm viewer's
/// `buildSourceViews` produces. Non-rezolus prefixed columns
/// (application-level cachecannon metrics, sglang router metrics,
/// etc.) are dropped from `_src`; service-extension KPIs targeting
/// them remain a deferred carve-out (the service templates are
/// PromQL-only today, per REVIEWING.md item 1). Multi-rezolus
/// aggregation across more than one rezolus source is not yet
/// implemented — captures with 2+ rezolus sources will still error.
pub(crate) fn create_src_table(
    conn: &Connection,
    parquet_path: &str,
    interval_ns: u64,
    columns: &[ColumnInfo],
) -> duckdb::Result<()> {
    conn.execute(&render_src_sql(parquet_path, interval_ns, columns), [])?;
    Ok(())
}

/// Build the `CREATE OR REPLACE TEMP TABLE _src AS SELECT ...` SQL
/// for a parquet, with the appropriate column projection given the
/// parquet's source-prefix shape. Pre-rendered at pool init so lazy
/// slot rebuilds re-execute the same string without re-walking the
/// parquet schema.
pub(crate) fn render_src_sql(
    parquet_path: &str,
    interval_ns: u64,
    columns: &[ColumnInfo],
) -> String {
    let half = interval_ns / 2;
    let parquet_lit = parquet_path.replace('\'', "''");

    // Single-source path: column names are already canonical. Pull
    // everything via the existing `* EXCLUDE (timestamp)` shortcut.
    if !columns.iter().any(|c| c.physical.contains("::")) {
        return format!(
            "CREATE OR REPLACE TEMP TABLE _src AS \
             SELECT \
                ((CAST(timestamp AS BIGINT) + {half}) // {interval_ns}) * {interval_ns} AS timestamp, \
                * EXCLUDE (timestamp) \
             FROM read_parquet('{parquet_lit}')"
        );
    }

    // Multi-source path: pick rezolus-tagged columns and project
    // them under canonical names. De-dupe on canonical alias so
    // two columns resolving to the same name don't error the CREATE
    // (matches the wasm viewer's `aliasSeen` dedup).
    let rezolus_columns: Vec<&ColumnInfo> = columns
        .iter()
        .filter(|c| c.labels.get("source").map_or(false, |s| s == "rezolus"))
        .collect();
    let mut seen_alias: BTreeSet<String> = BTreeSet::new();
    let mut projections: Vec<String> = Vec::with_capacity(rezolus_columns.len());
    for c in &rezolus_columns {
        let alias = canonical_alias(c);
        if !seen_alias.insert(alias.clone()) {
            continue;
        }
        projections.push(format!(
            "{} AS {}",
            quote_ident(&c.physical),
            quote_ident(&alias),
        ));
    }
    if projections.is_empty() {
        // No rezolus columns found — fall back to the raw
        // projection so dashboard SQL still gets *some* `_src`
        // table (it'll bind-error on missing metrics, but the
        // table itself exists). Service-only multi-source parquets
        // hit this path today.
        return format!(
            "CREATE OR REPLACE TEMP TABLE _src AS \
             SELECT \
                ((CAST(timestamp AS BIGINT) + {half}) // {interval_ns}) * {interval_ns} AS timestamp, \
                * EXCLUDE (timestamp) \
             FROM read_parquet('{parquet_lit}')"
        );
    }
    format!(
        "CREATE OR REPLACE TEMP TABLE _src AS \
         SELECT \
            ((CAST(timestamp AS BIGINT) + {half}) // {interval_ns}) * {interval_ns} AS timestamp, \
            {projs} \
         FROM read_parquet('{parquet_lit}')",
        projs = projections.join(", "),
    )
}

/// Render `CREATE OR REPLACE TEMP VIEW _src_<source> AS SELECT ...
/// FROM read_parquet(...)` statements, one per distinct `source` label
/// value found across the parquet's column field metadata. Returned as
/// a single `;`-separated batch suitable for `execute_batch`. Lets
/// service-extension KPI SQL target `_src_<service_name>` (e.g.
/// `_src_cachecannon`, `_src_vllm_prefill`) and bind regardless of
/// whether the parquet ships a single-instance source or multiple
/// instances of the same source.
///
/// Each view projects:
///   - `timestamp` (snapped to interval, same as `_src`)
///   - `duration` when present in the parquet
///   - one column per canonical alias, aliased via [`canonical_alias`].
///     When multiple physical columns share an alias (multi-instance:
///     two cachecannon instances both expose `target_rate`), values
///     are aggregated: `COALESCE(c1, 0) + COALESCE(c2, 0)` for
///     scalars, `h2_combine_lol([COALESCE(c1, [...]), …])` for
///     histograms — same shape `_src_rezolus_combined` uses across
///     multi-rezolus parquets.
///
/// View name applies wasm-compatible sanitisation:
/// non-`[a-zA-Z0-9_]` chars in `<source>` become `_`, so
/// `vllm-prefill` resolves to `_src_vllm_prefill`.
///
/// Returns an empty string when no column carries a `source` label —
/// pure single-source rezolus parquets already expose everything in
/// canonical form via `_src` and need no per-source aliasing.
pub(crate) fn render_per_source_views_sql(
    parquet_path: &str,
    interval_ns: u64,
    columns: &[ColumnInfo],
) -> String {
    let half = interval_ns / 2;
    let parquet_lit = parquet_path.replace('\'', "''");
    let has_duration = columns.iter().any(|c| c.physical == "duration");

    // Group columns by `source` label value (from field metadata).
    // Each entry: source → alias → list of (info, alias) sharing that alias.
    let mut by_source: BTreeMap<&str, BTreeMap<String, Vec<&ColumnInfo>>> = BTreeMap::new();
    for c in columns {
        let Some(source) = c.labels.get("source") else {
            continue;
        };
        let alias = canonical_alias(c);
        by_source
            .entry(source.as_str())
            .or_default()
            .entry(alias)
            .or_default()
            .push(c);
    }
    if by_source.is_empty() {
        return String::new();
    }

    let mut statements: Vec<String> = Vec::with_capacity(by_source.len());
    for (source, aliases) in &by_source {
        let view_name = view_name_for_source(source);
        let mut projections: Vec<String> = Vec::with_capacity(2 + aliases.len());
        projections.push(format!(
            "((CAST(timestamp AS BIGINT) + {half}) // {interval_ns}) * {interval_ns} AS timestamp"
        ));
        if has_duration {
            projections.push("duration".to_string());
        }
        for (alias, contribs) in aliases {
            let alias_q = quote_ident(alias);
            if contribs.len() == 1 {
                projections.push(format!(
                    "{} AS {}",
                    quote_ident(&contribs[0].physical),
                    alias_q,
                ));
            } else {
                // Multi-instance contributions to the same canonical
                // alias: aggregate. Histograms combine via `h2_combine_lol`;
                // scalars sum with COALESCE so NULL-only rows don't poison
                // the result.
                let is_histogram = matches!(contribs[0].kind, ColumnKind::Histogram { .. });
                if is_histogram {
                    let parts: Vec<String> = contribs
                        .iter()
                        .map(|c| format!("COALESCE({}, []::UBIGINT[])", quote_ident(&c.physical)))
                        .collect();
                    projections.push(format!(
                        "h2_combine_lol([{}]) AS {}",
                        parts.join(", "),
                        alias_q,
                    ));
                } else {
                    let parts: Vec<String> = contribs
                        .iter()
                        .map(|c| format!("COALESCE({}, 0)", quote_ident(&c.physical)))
                        .collect();
                    projections.push(format!("({}) AS {}", parts.join(" + "), alias_q));
                }
            }
        }
        statements.push(format!(
            "CREATE OR REPLACE TEMP VIEW {view} AS SELECT {projs} FROM read_parquet('{parquet_lit}')",
            view = view_name,
            projs = projections.join(", "),
        ));
    }
    statements.join("; ")
}

/// Wasm-compatible view name for a source. Non-`[a-zA-Z0-9_]` chars
/// become `_` so `vllm-prefill` resolves to `_src_vllm_prefill` on
/// both backends. Mirrors `viewNameForSource` in
/// `site/viewer-sql/lib/duckdb-registry.js`.
pub fn view_name_for_source(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + 5);
    out.push_str("_src_");
    for ch in source.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

/// Field metadata keys that describe *where* a value came from
/// rather than *which* series it belongs to. These never appear in
/// the canonical column name. Mirrors `NON_VALUE_METADATA_KEYS` in
/// the wasm viewer's `duckdb-registry.js` so canonical aliasing is
/// identical on both backends.
const NON_VALUE_METADATA_KEYS: &[&str] = &[
    "metric",
    "metric_type",
    "unit",
    "endpoint",
    "instance",
    "node",
    "source",
    "grouping_power",
    "max_value_power",
];

/// Resolve a parquet column to the canonical name the dashboard SQL
/// expects. Mirrors `canonicalAlias` in
/// `site/viewer-sql/lib/duckdb-registry.js`. For columns whose
/// physical name (after the `<source>::` prefix is stripped) is
/// already canonical, pass through. For numeric-encoded columns
/// (e.g. `rezolus-client::10x0`), rebuild the name from the
/// `metric` metadata + value-label values, sorted with non-numeric
/// values first and numeric IDs last to match the named-column
/// convention.
fn canonical_alias(info: &ColumnInfo) -> String {
    // Strip the `<prefix>::` prefix from physical name.
    let rest = match info.physical.split_once("::") {
        Some((_, r)) => r,
        None => info.physical.as_str(),
    };
    let metric = &info.metric;

    // Already-canonical names short-circuit.
    if rest == metric
        || rest == format!("{metric}:buckets")
        || rest.starts_with(&format!("{metric}/"))
    {
        return rest.to_string();
    }

    // Rebuild from value labels. `info.labels` was already stripped
    // of {metric, metric_type, unit, grouping_power, max_value_power}
    // by `classify`; we additionally exclude the wasm side's
    // {endpoint, instance, node, source} infrastructure keys.
    let mut value_labels: Vec<(&str, &str)> = info
        .labels
        .iter()
        .filter(|(k, _)| !NON_VALUE_METADATA_KEYS.iter().any(|nv| *nv == k.as_str()))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    value_labels.sort_by(|a, b| {
        let na = a.1.chars().all(|c| c.is_ascii_digit()) && !a.1.is_empty();
        let nb = b.1.chars().all(|c| c.is_ascii_digit()) && !b.1.is_empty();
        (na as u8).cmp(&(nb as u8)).then_with(|| a.0.cmp(b.0))
    });

    let mut name = metric.clone();
    for (_, v) in &value_labels {
        name.push('/');
        name.push_str(v);
    }
    if matches!(info.kind, ColumnKind::Histogram { .. }) {
        name.push_str(":buckets");
    }
    name
}

/// Quote an SQL identifier — wrap in `"` and double any embedded
/// `"`. Column names from Rezolus parquets can contain `/`, `:`,
/// and other characters DuckDB's bare-word identifier parser rejects.
fn quote_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Render the SQL needed to (re)create the `_cgroup_index` TEMP
/// TABLE on a fresh connection. Combines a `CREATE OR REPLACE
/// TEMP TABLE` definition with a single bulk `INSERT INTO ...
/// VALUES (...)` of one row per cgroup_* metric column. Returns
/// `None` when the parquet carries no cgroup columns (the table
/// itself is still created so dashboard SQL that JOINs against
/// it gets an empty result rather than a binder error).
///
/// The dashboard's cgroup SQL (`crates/dashboard/src/sql.rs`
/// `cgroup_irate_total` / `cgroup_irate_by_name` /
/// `cgroup_ratio_*`) JOINs against `_cgroup_index` on
/// `(column_name, metric)` and filters on `name` / `labels[k]`,
/// so we mirror the wasm viewer's index shape exactly — see
/// `site/viewer-sql/lib/duckdb-registry.js::buildCgroupIndex`
/// for the source-of-truth schema and dedup rules.
///
/// Histogram `:buckets` cgroup columns are intentionally included
/// (matching wasm); the dashboard's UNPIVOT regex
/// `^<metric>(/[^:]+)?$` skips them at query time so they sit
/// in the index unused. Cheaper than maintaining a kind-aware
/// filter that has to track every histogram metric.
pub(crate) fn render_cgroup_index_sql(columns: &[ColumnInfo]) -> String {
    // The CREATE always runs — dashboard SQL needs the table to
    // exist even on parquets without cgroups (the JOIN returns
    // zero rows rather than erroring).
    let create = "CREATE OR REPLACE TEMP TABLE _cgroup_index(\
            metric VARCHAR, \
            column_name VARCHAR, \
            name VARCHAR, \
            id VARCHAR, \
            labels MAP(VARCHAR, VARCHAR)\
         )";

    // `column_name` must match the projection used in `_src` so the
    // dashboard's `idx.column_name = u.col` JOIN actually binds. With
    // the single-source path that's `c.physical` (passthrough); with
    // the multi-source path it's `canonical_alias(c)`. Dedupe on
    // (metric, canonical column name) to mirror `_src`'s
    // `seen_alias` dedup — otherwise a metric that appears under
    // multiple source prefixes lands in the index twice but only
    // once in `_src`, producing duplicate JOIN rows that double-count
    // the aggregate.
    let multi_source = columns.iter().any(|c| c.physical.contains("::"));
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut rows: Vec<String> = Vec::new();
    for c in columns {
        if !c.metric.starts_with("cgroup_") {
            continue;
        }
        // For multi-source, skip non-rezolus rows: `_src` doesn't
        // project them, so a JOIN row pointing at a non-existent
        // `_src` column would be unreachable noise.
        if multi_source && c.labels.get("source").map_or(true, |s| s != "rezolus") {
            continue;
        }
        let col = if multi_source {
            canonical_alias(c)
        } else {
            c.physical.clone()
        };
        if !seen.insert((c.metric.clone(), col.clone())) {
            continue;
        }
        let name = c.labels.get("name").map(String::as_str);
        let id = c.labels.get("id").map(String::as_str);
        let extra_pairs: Vec<String> = c
            .labels
            .iter()
            .filter(|(k, _)| {
                let k = k.as_str();
                // Exclude both the lifted-to-top-level labels and the
                // infrastructure keys that `canonical_alias` filters
                // out — otherwise multi-source rows expose `node` /
                // `source` keys that the single-source path doesn't.
                !NON_VALUE_METADATA_KEYS.iter().any(|nv| *nv == k) && k != "name" && k != "id"
            })
            .map(|(k, v)| format!("{}:{}", sql_string_lit(k), sql_string_lit(v)))
            .collect();
        let labels_lit = if extra_pairs.is_empty() {
            // Empty MAP literal in DuckDB: MAP() — `MAP{}` parses as
            // an empty struct and binder-errors on the column type.
            "MAP([]::VARCHAR[], []::VARCHAR[])".to_string()
        } else {
            format!("MAP{{{}}}", extra_pairs.join(","))
        };
        rows.push(format!(
            "({}, {}, {}, {}, {})",
            sql_string_lit(&c.metric),
            sql_string_lit(&col),
            name.map(sql_string_lit)
                .unwrap_or_else(|| "NULL".to_string()),
            id.map(sql_string_lit).unwrap_or_else(|| "NULL".to_string()),
            labels_lit,
        ));
    }

    if rows.is_empty() {
        create.to_string()
    } else {
        format!(
            "{create}; INSERT INTO _cgroup_index VALUES {}",
            rows.join(","),
        )
    }
}

/// SQL single-quoted string literal with embedded `'` doubled
/// per the SQL standard. Cgroup names and label values come from
/// parquet metadata and can carry arbitrary characters.
fn sql_string_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push('\'');
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

/// Build the `_cgroup_index` TEMP TABLE on `conn`. Executes the
/// SQL rendered by `render_cgroup_index_sql` — see that function
/// for the schema and semantics.
pub(crate) fn create_cgroup_index(conn: &Connection, columns: &[ColumnInfo]) -> duckdb::Result<()> {
    let sql = render_cgroup_index_sql(columns);
    // `INSERT` may be empty (no cgroup columns); when it is, the
    // string is just the `CREATE` and `execute` runs one statement.
    // When both are present, DuckDB's `execute` accepts the `;`-
    // separated pair as a single batch.
    conn.execute_batch(&sql)?;
    Ok(())
}

/// Read the parquet schema, load the file into a single in-memory
/// `_src` TEMP TABLE on `conn`, and return the per-metric catalog the
/// wide-form SQL generator consumes.
///
/// Convenience: combines `read_introspection` + `create_src_table` +
/// `build_catalog` for callers that don't want to pool slot setup.
/// Used by tests and one-off perf scripts.
pub fn ensure_views(conn: &Connection, parquet_path: &str) -> duckdb::Result<MetricCatalog> {
    let (columns, interval_ns) = read_introspection(parquet_path)?;
    create_src_table(conn, parquet_path, interval_ns, &columns)?;
    Ok(build_catalog(&columns))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("metriken-query-fixtures")
            .join("fixtures")
            .join(format!("{name}.parquet"))
    }

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        crate::register_all(&conn).expect("register");
        conn
    }

    #[test]
    fn timestamps_are_snapped_to_sampling_interval() {
        // The synthetic fixtures already have clean integer-second timestamps,
        // so the snap is a no-op there. This test verifies the snap math and
        // its DuckDB types directly: a sub-second timestamp must round to the
        // nearest second in BIGINT (not DOUBLE — DuckDB's `/` would otherwise
        // promote to DOUBLE and lose precision at 1.8e18 scale).
        let conn = fresh_conn();
        let ts: i64 = conn
            .query_row(
                // Mirror the views.rs snap formula. 1768956638999716606 is
                // a real Rezolus timestamp (~1ms shy of the second mark);
                // it should snap UP to 1768956639_000000000.
                "SELECT ((1768956638999716606::BIGINT + 500000000) // 1000000000) * 1000000000",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ts, 1_768_956_639_000_000_000);
    }

    #[test]
    fn src_table_carries_snapped_timestamp() {
        // End-to-end: load a fixture, query MIN(timestamp) % 1e9 from `_src`.
        // Must be 0 for every row — proof that the snap was applied.
        let conn = fresh_conn();
        ensure_views(&conn, fixture_path("counter_basic").to_str().unwrap()).unwrap();
        let nonzero: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _src WHERE (CAST(timestamp AS BIGINT) % 1000000000) != 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            nonzero, 0,
            "every _src.timestamp must be a clean multiple of 1e9 ns"
        );
    }

    #[test]
    fn counter_basic_catalog_indexes_the_one_metric() {
        let conn = fresh_conn();
        let catalog = ensure_views(&conn, fixture_path("counter_basic").to_str().unwrap()).unwrap();
        let series = catalog
            .series_by_metric
            .get("requests")
            .expect("requests metric");
        assert_eq!(series.len(), 1);
    }

    #[test]
    fn counter_multi_label_catalog_indexes_four_series() {
        let conn = fresh_conn();
        let catalog =
            ensure_views(&conn, fixture_path("counter_multi_label").to_str().unwrap()).unwrap();
        let series = catalog
            .series_by_metric
            .get("cpu_usage")
            .expect("cpu_usage metric");
        // 4 labeled series (id × state).
        assert_eq!(series.len(), 4);
        // All four label permutations are present.
        let mut combos: std::collections::BTreeSet<(String, String)> = Default::default();
        for s in series {
            combos.insert((
                s.labels.get("id").cloned().unwrap_or_default(),
                s.labels.get("state").cloned().unwrap_or_default(),
            ));
        }
        assert_eq!(combos.len(), 4);
    }

    #[test]
    fn histogram_grouping_power_is_recorded() {
        let conn = fresh_conn();
        let catalog =
            ensure_views(&conn, fixture_path("histogram_basic").to_str().unwrap()).unwrap();
        let p = catalog
            .histogram_p_by_metric
            .get("request_latency")
            .copied()
            .expect("request_latency p");
        // metriken-query-fixtures uses gp=4 for the rezolus histogram config.
        assert_eq!(p, 4);
    }

    // ---------- malformed-metadata behaviour ----------
    //
    // These tests construct minimal parquets in a tempdir with the
    // metadata shapes that real-world writers can emit. They pin the
    // `read_introspection` contract on inputs the fixture suite doesn't
    // already exercise.

    fn write_test_parquet(
        path: &std::path::Path,
        fields: Vec<arrow::datatypes::Field>,
        columns: Vec<arrow::array::ArrayRef>,
        file_kv: &[(&str, &str)],
    ) {
        use arrow::datatypes::Schema;
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        use parquet::file::metadata::KeyValue;
        use parquet::file::properties::WriterProperties;

        let schema = std::sync::Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), columns).expect("RecordBatch");
        let kvs: Vec<KeyValue> = file_kv
            .iter()
            .map(|(k, v)| KeyValue {
                key: (*k).to_string(),
                value: Some((*v).to_string()),
            })
            .collect();
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(kvs))
            .build();
        let file = std::fs::File::create(path).expect("create");
        let mut writer = ArrowWriter::try_new(file, schema, Some(props)).expect("ArrowWriter");
        writer.write(&batch).expect("write");
        writer.close().expect("close");
    }

    fn ts_field() -> arrow::datatypes::Field {
        arrow::datatypes::Field::new("timestamp", arrow::datatypes::DataType::UInt64, false)
    }

    fn counter_field(name: &str, metric: &str) -> arrow::datatypes::Field {
        let mut md = std::collections::HashMap::new();
        md.insert("metric".into(), metric.into());
        md.insert("metric_type".into(), "counter".into());
        arrow::datatypes::Field::new(name, arrow::datatypes::DataType::UInt64, false)
            .with_metadata(md)
    }

    #[test]
    fn missing_sampling_interval_ms_falls_back_to_one_second() {
        // The recorder always emits `sampling_interval_ms`, but third-
        // party producers may not. The loader's fallback is 1s — pin
        // the value so a future refactor doesn't silently pick another
        // default (which would shift every snapped timestamp by the
        // gap between the new and old defaults).
        use arrow::array::{ArrayRef, UInt64Array};
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("no_interval.parquet");
        let ts: ArrayRef =
            std::sync::Arc::new(UInt64Array::from(vec![1_000_000_000u64, 2_000_000_000]));
        let v: ArrayRef = std::sync::Arc::new(UInt64Array::from(vec![10u64, 20]));
        write_test_parquet(
            &path,
            vec![ts_field(), counter_field("requests", "requests")],
            vec![ts, v],
            &[], // no file metadata at all
        );

        let (_columns, interval_ns) =
            read_introspection(path.to_str().unwrap()).expect("introspect");
        assert_eq!(interval_ns, 1_000_000_000);
    }

    #[test]
    fn list_uint64_without_grouping_power_is_classified_as_other_and_dropped() {
        // A list-of-UInt64 column with no `grouping_power` metadata is
        // shape-compatible with a histogram but missing the bucket-layout
        // hint — we can't safely call h2_* UDFs on it. The classifier
        // marks it `Other`, which `read_introspection` then drops from
        // the column list.
        use arrow::array::{ArrayRef, ListBuilder, UInt64Array, UInt64Builder};
        use arrow::datatypes::{DataType, Field};
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("no_grouping_power.parquet");

        let ts: ArrayRef =
            std::sync::Arc::new(UInt64Array::from(vec![1_000_000_000u64, 2_000_000_000]));
        let mut lb = ListBuilder::new(UInt64Builder::new()).with_field(Field::new(
            "item",
            DataType::UInt64,
            true,
        ));
        lb.values().append_value(1);
        lb.values().append_value(2);
        lb.append(true);
        lb.values().append_value(3);
        lb.values().append_value(4);
        lb.append(true);
        let list_array: ArrayRef = std::sync::Arc::new(lb.finish());

        let mut md = std::collections::HashMap::new();
        md.insert("metric".into(), "latency".into());
        md.insert("metric_type".into(), "histogram".into()); // claims to be a histogram…
        // …but no grouping_power, so we can't actually compute on it.
        let list_field =
            Field::new("latency:buckets", list_array.data_type().clone(), true).with_metadata(md);

        write_test_parquet(
            &path,
            vec![ts_field(), list_field],
            vec![ts, list_array],
            &[("sampling_interval_ms", "1000")],
        );

        let (columns, _) = read_introspection(path.to_str().unwrap()).expect("introspect");
        // The list column was dropped — only ts is gone, and the metric
        // column never made it through (timestamp is also filtered out
        // upstream of classify).
        assert!(
            !columns.iter().any(|c| c.metric == "latency"),
            "latency must be dropped without grouping_power; got: {:?}",
            columns.iter().map(|c| &c.metric).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn duplicate_physical_column_names_dedupe_first_occurrence_wins() {
        // Some Rezolus writers emit duplicate top-level column names.
        // DuckDB's `read_parquet` silently keeps only the first; the
        // loader's `seen_physical` set mirrors that so the catalog
        // doesn't index columns DuckDB can't see. Pin this against a
        // synthetic parquet that has two columns named `requests`.
        use arrow::array::{ArrayRef, UInt64Array};
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("dupes.parquet");

        let ts: ArrayRef = std::sync::Arc::new(UInt64Array::from(vec![1_000_000_000u64]));
        let v1: ArrayRef = std::sync::Arc::new(UInt64Array::from(vec![10u64]));
        let v2: ArrayRef = std::sync::Arc::new(UInt64Array::from(vec![20u64]));
        write_test_parquet(
            &path,
            vec![
                ts_field(),
                counter_field("requests", "requests"),
                counter_field("requests", "requests"),
            ],
            vec![ts, v1, v2],
            &[("sampling_interval_ms", "1000")],
        );

        let (columns, _) = read_introspection(path.to_str().unwrap()).expect("introspect");
        let requests: Vec<_> = columns
            .iter()
            .filter(|c| c.physical == "requests")
            .collect();
        assert_eq!(requests.len(), 1, "dedupe must keep exactly one `requests`");
    }

    // ---------- _cgroup_index ----------

    #[test]
    fn cgroup_index_is_empty_when_parquet_has_no_cgroup_columns() {
        // counter_basic has only a `requests` counter — no cgroup_*
        // metric. The table must still exist (dashboard SQL JOINs
        // against it unconditionally) but contain zero rows.
        let conn = fresh_conn();
        let (columns, _) =
            read_introspection(fixture_path("counter_basic").to_str().unwrap()).unwrap();
        create_cgroup_index(&conn, &columns).expect("create cgroup index");
        let n: i64 = conn
            .query_row("SELECT count(*) FROM _cgroup_index", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn cgroup_index_inserts_one_row_per_cgroup_column_with_split_labels() {
        // Synthetic parquet: one `cgroup_cpu_usage` counter column
        // labelled with `name`, `id`, and a free-form `state` label.
        // Expect a single row in `_cgroup_index` whose `name` + `id`
        // are top-level VARCHARs and whose `state` lands in `labels`.
        use arrow::array::{ArrayRef, UInt64Array};
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("cgroup.parquet");

        let ts: ArrayRef = std::sync::Arc::new(UInt64Array::from(vec![1_000_000_000u64]));
        let v: ArrayRef = std::sync::Arc::new(UInt64Array::from(vec![42u64]));
        let mut md = std::collections::HashMap::new();
        md.insert("metric".into(), "cgroup_cpu_usage".into());
        md.insert("metric_type".into(), "counter".into());
        md.insert("name".into(), "system.slice/foo".into());
        md.insert("id".into(), "1234".into());
        md.insert("state".into(), "user".into());
        let metric_field = arrow::datatypes::Field::new(
            "cgroup_cpu_usage/foo",
            arrow::datatypes::DataType::UInt64,
            false,
        )
        .with_metadata(md);
        write_test_parquet(
            &path,
            vec![ts_field(), metric_field],
            vec![ts, v],
            &[("sampling_interval_ms", "1000")],
        );

        let conn = fresh_conn();
        let (columns, _) = read_introspection(path.to_str().unwrap()).unwrap();
        create_cgroup_index(&conn, &columns).expect("create cgroup index");

        let (metric, column_name, name, id): (String, String, String, String) = conn
            .query_row(
                "SELECT metric, column_name, name, id FROM _cgroup_index",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(metric, "cgroup_cpu_usage");
        assert_eq!(column_name, "cgroup_cpu_usage/foo");
        assert_eq!(name, "system.slice/foo");
        assert_eq!(id, "1234");

        // `state` should be in the MAP but `name`/`id` should NOT (they
        // were lifted to top-level columns).
        let state: String = conn
            .query_row("SELECT labels['state'] FROM _cgroup_index", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "user");
        let n_keys: i64 = conn
            .query_row("SELECT len(map_keys(labels)) FROM _cgroup_index", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            n_keys, 1,
            "labels MAP should hold only `state`, not name/id"
        );
    }

    // ---------- canonical aliasing for multi-source parquets ----------

    fn make_info(
        physical: &str,
        metric: &str,
        kind: ColumnKind,
        labels: &[(&str, &str)],
    ) -> ColumnInfo {
        ColumnInfo {
            physical: physical.into(),
            metric: metric.into(),
            kind,
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn canonical_alias_passes_named_column_through() {
        // Already-canonical name: pass through after stripping the
        // source prefix.
        let info = make_info(
            "rezolus-client::cpu_usage/user/0",
            "cpu_usage",
            ColumnKind::Counter,
            &[("state", "user"), ("id", "0"), ("source", "rezolus")],
        );
        assert_eq!(super::canonical_alias(&info), "cpu_usage/user/0");
    }

    #[test]
    fn canonical_alias_rebuilds_numeric_encoded_columns() {
        // Numeric-encoded parquets (rezolus-client::10x0) carry the
        // canonical name in metadata. Rebuild: metric + sorted
        // value labels (non-numeric first, numeric last).
        let info = make_info(
            "rezolus-client::10x0",
            "cpu_cycles",
            ColumnKind::Counter,
            &[
                ("id", "0"),
                ("node", "rezolus-client"),
                ("source", "rezolus"),
                ("unit", "cycles"),
            ],
        );
        // {node, source, unit} are infrastructure labels — excluded.
        // Only {id} survives; numeric values land last.
        assert_eq!(super::canonical_alias(&info), "cpu_cycles/0");
    }

    #[test]
    fn canonical_alias_orders_non_numeric_before_numeric_labels() {
        // For a metric with both `state` (non-numeric) and `id`
        // (numeric), the canonical form is `cpu_usage/<state>/<id>` —
        // numeric values trail.
        let info = make_info(
            "src::ord1",
            "cpu_usage",
            ColumnKind::Counter,
            &[("id", "3"), ("state", "user")],
        );
        assert_eq!(super::canonical_alias(&info), "cpu_usage/user/3");
    }

    #[test]
    fn canonical_alias_appends_buckets_suffix_for_histograms() {
        let info = make_info(
            "src::hist1",
            "syscall_latency",
            ColumnKind::Histogram { grouping_power: 3 },
            &[("op", "read")],
        );
        assert_eq!(
            super::canonical_alias(&info),
            "syscall_latency/read:buckets"
        );
    }

    #[test]
    fn render_src_sql_single_source_uses_exclude_shortcut() {
        // No `::` in any column name → existing `* EXCLUDE (timestamp)`
        // shortcut. Cheaper than enumerating projections, and the
        // names are already canonical.
        let cols = vec![make_info(
            "cpu_usage/user/0",
            "cpu_usage",
            ColumnKind::Counter,
            &[("state", "user"), ("id", "0")],
        )];
        let sql = super::render_src_sql("/tmp/foo.parquet", 1_000_000_000, &cols);
        assert!(
            sql.contains("* EXCLUDE (timestamp)"),
            "single-source: {sql}"
        );
    }

    #[test]
    fn render_src_sql_multi_source_projects_rezolus_canonically() {
        // Cachecannon-shaped: one rezolus source + one application
        // source. Only the rezolus columns project into `_src`, with
        // canonical aliases.
        let cols = vec![
            make_info(
                "0::cache_hits",
                "cache_hits",
                ColumnKind::Counter,
                &[("source", "cachecannon"), ("instance", "0")],
            ),
            make_info(
                "rezolus-client::10x0",
                "cpu_cycles",
                ColumnKind::Counter,
                &[
                    ("id", "0"),
                    ("node", "rezolus-client"),
                    ("source", "rezolus"),
                    ("unit", "cycles"),
                ],
            ),
        ];
        let sql = super::render_src_sql("/tmp/foo.parquet", 1_000_000_000, &cols);
        // Has the canonical alias projection.
        assert!(
            sql.contains(r#""rezolus-client::10x0" AS "cpu_cycles/0""#),
            "expected canonical alias in: {sql}",
        );
        // The cachecannon-source column is NOT projected.
        assert!(
            !sql.contains("cache_hits"),
            "cachecannon-source columns should be excluded: {sql}",
        );
        // No `* EXCLUDE` shortcut on the multi-source path.
        assert!(!sql.contains("EXCLUDE"));
    }

    #[test]
    fn view_name_for_source_strips_non_alnum() {
        // Matches wasm's `viewNameForSource`: hyphens, dots, slashes
        // map to underscores.
        assert_eq!(
            super::view_name_for_source("cachecannon"),
            "_src_cachecannon"
        );
        assert_eq!(
            super::view_name_for_source("vllm-prefill"),
            "_src_vllm_prefill"
        );
        assert_eq!(
            super::view_name_for_source("llm.perf.v2"),
            "_src_llm_perf_v2"
        );
        // Already-alphanumeric underscores survive.
        assert_eq!(super::view_name_for_source("a_b_c"), "_src_a_b_c");
    }

    #[test]
    fn render_per_source_views_groups_by_source_label() {
        let cols = vec![
            make_info(
                "0::target_rate",
                "target_rate",
                ColumnKind::Gauge,
                &[("source", "cachecannon"), ("instance", "0")],
            ),
            make_info(
                "0::requests_sent",
                "requests_sent",
                ColumnKind::Counter,
                &[("source", "cachecannon"), ("instance", "0")],
            ),
            make_info(
                "rezolus-client::10x0",
                "cpu_cycles",
                ColumnKind::Counter,
                &[
                    ("id", "0"),
                    ("node", "rezolus-client"),
                    ("source", "rezolus"),
                    ("unit", "cycles"),
                ],
            ),
        ];
        let sql = super::render_per_source_views_sql("/tmp/foo.parquet", 1_000_000_000, &cols);
        // View names follow the source label, not the column prefix.
        assert!(sql.contains(r#"CREATE OR REPLACE TEMP VIEW _src_cachecannon AS"#));
        assert!(sql.contains(r#"CREATE OR REPLACE TEMP VIEW _src_rezolus AS"#));
        // Bare canonical aliases project from the prefixed physical names.
        assert!(sql.contains(r#""0::target_rate" AS "target_rate""#));
        assert!(sql.contains(r#""0::requests_sent" AS "requests_sent""#));
        assert!(sql.contains(r#""rezolus-client::10x0" AS "cpu_cycles/0""#));
        // Timestamp snap projection is present.
        assert!(sql.contains("AS timestamp"));
        // Statements are `;`-separated.
        assert_eq!(sql.matches("CREATE OR REPLACE TEMP VIEW").count(), 2);
    }

    #[test]
    fn render_per_source_views_empty_when_no_source_label() {
        // Columns carry no `source` label — single-source rezolus parquets
        // historically didn't tag this field, and `_src` already exposes
        // them in canonical form.
        let cols = vec![
            make_info(
                "cpu_cycles/0",
                "cpu_cycles",
                ColumnKind::Counter,
                &[("id", "0")],
            ),
            make_info("memory_total", "memory_total", ColumnKind::Gauge, &[]),
        ];
        let sql = super::render_per_source_views_sql("/tmp/foo.parquet", 1_000_000_000, &cols);
        assert!(sql.is_empty(), "expected empty SQL: {sql}");
    }

    #[test]
    fn render_per_source_views_aggregates_multi_instance_scalars() {
        // Two cachecannon instances expose `target_rate` under different
        // prefixes (0:: and 1::). The per-source view sums them with
        // COALESCE so a missing reading on one instance doesn't poison
        // the row.
        let cols = vec![
            make_info(
                "0::target_rate",
                "target_rate",
                ColumnKind::Gauge,
                &[("source", "cachecannon"), ("instance", "0")],
            ),
            make_info(
                "1::target_rate",
                "target_rate",
                ColumnKind::Gauge,
                &[("source", "cachecannon"), ("instance", "1")],
            ),
        ];
        let sql = super::render_per_source_views_sql("/tmp/foo.parquet", 1_000_000_000, &cols);
        assert!(
            sql.contains(r#"(COALESCE("0::target_rate", 0) + COALESCE("1::target_rate", 0)) AS "target_rate""#),
            "expected COALESCE sum aggregation: {sql}",
        );
    }

    #[test]
    fn render_per_source_views_aggregates_multi_instance_histograms() {
        // Two cachecannon instances expose `response_latency:buckets`
        // (histograms). Combined via `h2_combine_lol`, matching the
        // `_src_rezolus_combined` shape used for multi-rezolus parquets.
        let cols = vec![
            make_info(
                "0::response_latency:buckets",
                "response_latency",
                ColumnKind::Histogram { grouping_power: 4 },
                &[("source", "cachecannon"), ("instance", "0")],
            ),
            make_info(
                "1::response_latency:buckets",
                "response_latency",
                ColumnKind::Histogram { grouping_power: 4 },
                &[("source", "cachecannon"), ("instance", "1")],
            ),
        ];
        let sql = super::render_per_source_views_sql("/tmp/foo.parquet", 1_000_000_000, &cols);
        assert!(
            sql.contains(r#"h2_combine_lol([COALESCE("0::response_latency:buckets", []::UBIGINT[]), COALESCE("1::response_latency:buckets", []::UBIGINT[])])"#),
            "expected h2_combine_lol for histogram aggregation: {sql}",
        );
    }

    #[test]
    fn render_per_source_views_includes_duration_when_present() {
        let cols = vec![
            make_info("duration", "duration", ColumnKind::Other, &[]),
            make_info("0::x", "x", ColumnKind::Gauge, &[("source", "cachecannon")]),
        ];
        let sql = super::render_per_source_views_sql("/tmp/foo.parquet", 1_000_000_000, &cols);
        // duration projects under its bare name (not aliased).
        assert!(
            sql.contains("duration, "),
            "expected duration projection: {sql}"
        );
    }

    #[test]
    fn cgroup_index_handles_apostrophes_in_label_values() {
        // Cgroup names from user-space can contain apostrophes (e.g.
        // `john's-shell.scope`). The literal renderer must double them
        // to avoid binder errors mid-INSERT.
        use arrow::array::{ArrayRef, UInt64Array};
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("cgroup_apos.parquet");

        let ts: ArrayRef = std::sync::Arc::new(UInt64Array::from(vec![1_000_000_000u64]));
        let v: ArrayRef = std::sync::Arc::new(UInt64Array::from(vec![1u64]));
        let mut md = std::collections::HashMap::new();
        md.insert("metric".into(), "cgroup_cpu_usage".into());
        md.insert("metric_type".into(), "counter".into());
        md.insert("name".into(), "john's-shell.scope".into());
        let f = arrow::datatypes::Field::new(
            "cgroup_cpu_usage/abc",
            arrow::datatypes::DataType::UInt64,
            false,
        )
        .with_metadata(md);
        write_test_parquet(
            &path,
            vec![ts_field(), f],
            vec![ts, v],
            &[("sampling_interval_ms", "1000")],
        );

        let conn = fresh_conn();
        let (columns, _) = read_introspection(path.to_str().unwrap()).unwrap();
        create_cgroup_index(&conn, &columns).expect("create cgroup index");
        let name: String = conn
            .query_row("SELECT name FROM _cgroup_index", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "john's-shell.scope");
    }

    #[test]
    fn describe_parquet_returns_same_catalog_without_creating_src_table() {
        // describe_parquet does pure introspection — no DuckDB side
        // effects. Two assertions: (1) the catalog matches what
        // ensure_views builds; (2) `_src` does not exist on a fresh
        // connection that only saw describe_parquet.
        let path = fixture_path("counter_multi_label");
        let path = path.to_str().unwrap();

        let described = describe_parquet(path).unwrap();
        assert_eq!(
            described.series_by_metric.get("cpu_usage").unwrap().len(),
            4
        );

        let conn = fresh_conn();
        // No call to ensure_views — only describe_parquet was used.
        let src_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM duckdb_tables() WHERE table_name = '_src')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!src_exists, "describe_parquet must not create _src");

        // ensure_views still produces an equivalent catalog.
        let ensured = ensure_views(&conn, path).unwrap();
        assert_eq!(
            described.series_by_metric.keys().collect::<Vec<_>>(),
            ensured.series_by_metric.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            described.histogram_p_by_metric,
            ensured.histogram_p_by_metric
        );
    }
}
