//! Wide-form SQL generator.
//!
//! For catalogue entries whose shape we recognise, emit SQL that
//! references the parquet's physical columns directly via `_src`, with
//! a single un-partitioned `WINDOW w AS (ORDER BY timestamp)` doing
//! N parallel `LAG(...)` expressions — one per matching physical
//! column. Compare with the long-form approach, where the metric VIEW
//! UNION-ALLs every column into a long stream and the catalogue's
//! query then `WINDOW (PARTITION BY col ORDER BY timestamp)` re-derives
//! a partitioning the wide layout already has.
//!
//! The wide-form output rows are byte-identical to the long-form
//! output rows for the shapes we handle (verified by the
//! `frontend_coverage` integration test). Wins are concentrated on the
//! shapes whose long-form WINDOW is the dominant cost — measured 7x
//! faster on `WINDOW` and ~2x faster end-to-end on the worst-case
//! `sum by (id) (irate(softirq{kind="rcu"}[5m]))` against
//! `vllm.parquet`.
//!
//! When no recognised shape matches the entry, `try_generate` returns
//! `None` and the caller falls back to the long-form VIEW path. This
//! keeps the catalogue's "long tail" of entries working unchanged.
//!
//! The recognised-shape registry is intentionally small to start with;
//! we add shapes as their long-form counterparts show up at the top of
//! the bench's worst-ratio list.

use metriken_query::{CaptureValue, CatalogueEntry, Captures};

use crate::views::{MetricCatalog, MetricSeries};

/// If `entry`'s shape is one we know how to compile to wide-form SQL,
/// return that SQL. Otherwise `None`, and the caller should fall back
/// to interpolating the entry's long-form `sql` template.
pub fn try_generate(
    entry: &CatalogueEntry,
    captures: &Captures,
    catalog: &MetricCatalog,
) -> Option<String> {
    // Prototype: only the canonical `sum by (id) (irate(softirq{kind=K}[5m]))`
    // shape. The catalogue id is the dispatch key; we'll generalise once we
    // see the shape of one win in the bench.
    match entry.id.as_str() {
        "softirq_irate_by_id_by_kind" => sum_by_g_irate(
            entry,
            captures,
            catalog,
            /* metric */ "softirq",
            /* group_label */ "id",
            /* filter_label */ "kind",
            /* filter_capture */ "k",
            /* scale */ 1.0,
        ),
        _ => None,
    }
}

/// Generate SQL for `sum by (G) (irate(M{Lf=Vf}[R])) [/ scale]`.
///
/// Reads M's series list from the catalog, filters to ones where
/// `labels[Lf] = Vf` (Vf comes from `captures[filter_capture]`),
/// emits one `LAG(...)`-based rate expression per matching column,
/// and UNPIVOTs the per-column rates into the catalogue's expected
/// output schema `(t, v, G)`.
///
/// Returns `None` if the catalog doesn't carry the metric (caller
/// falls through to the long-form path, which also short-circuits to
/// empty for missing metrics).
fn sum_by_g_irate(
    _entry: &CatalogueEntry,
    captures: &Captures,
    catalog: &MetricCatalog,
    metric: &str,
    group_label: &str,
    filter_label: &str,
    filter_capture: &str,
    scale: f64,
) -> Option<String> {
    let series = catalog.series_by_metric.get(metric)?;
    let filter_value = match captures.get(filter_capture)? {
        CaptureValue::String(s) => s.as_str(),
        CaptureValue::Ident(s) => s.as_str(),
        _ => return None,
    };
    let matching: Vec<&MetricSeries> = series
        .iter()
        .filter(|s| s.labels.get(filter_label).map(|v| v.as_str()) == Some(filter_value))
        .collect();
    if matching.is_empty() {
        // No series match — emit an empty-result query that still has
        // the right output schema. Trivial cost.
        return Some(empty_matrix_sql(group_label));
    }

    let scale_expr = if scale == 1.0 {
        String::new()
    } else {
        format!(" / {scale}")
    };

    let mut rate_exprs = String::new();
    let mut union_branches = String::new();
    for (i, s) in matching.iter().enumerate() {
        let phys = s.physical.replace('"', "\"\"");
        rate_exprs.push_str(&format!(
            ",\n    CASE\n        WHEN LAG(\"{phys}\") OVER w IS NULL THEN NULL\n        \
             WHEN \"{phys}\" >= LAG(\"{phys}\") OVER w\n            \
             THEN CAST(\"{phys}\" - LAG(\"{phys}\") OVER w AS DOUBLE) / NULLIF(CAST(timestamp - LAG(timestamp) OVER w AS DOUBLE) / 1e9, 0)\n        \
             ELSE CAST(\"{phys}\" AS DOUBLE) / NULLIF(CAST(timestamp - LAG(timestamp) OVER w AS DOUBLE) / 1e9, 0)\n    \
             END AS rate_{i}"
        ));
        let g_value = s
            .labels
            .get(group_label)
            .cloned()
            .unwrap_or_default()
            .replace('\'', "''");
        let prefix = if i == 0 { "" } else { "\n      UNION ALL\n      " };
        union_branches.push_str(&format!(
            "{prefix}SELECT timestamp, '{g_value}' AS {group_label}, rate_{i} AS rate FROM rates WHERE rate_{i} IS NOT NULL"
        ));
    }

    let group_label_quoted = quote_ident(group_label);

    Some(format!(
        "WITH rates AS (\n  SELECT timestamp{rate_exprs}\n  FROM _src\n  WINDOW w AS (ORDER BY timestamp)\n)\n\
         SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, SUM(rate){scale_expr} AS v, {group_label_quoted}\n\
         FROM (\n      {union_branches}\n) per_series\n\
         GROUP BY {group_label_quoted}, timestamp\n\
         ORDER BY {group_label_quoted}, timestamp"
    ))
}

/// SQL that returns zero rows in the schema `(t, v, G)`. Emitted when
/// no physical column matches the filter — the long-form path would
/// also produce zero rows for that query.
fn empty_matrix_sql(group_label: &str) -> String {
    let g = quote_ident(group_label);
    format!(
        "SELECT CAST(NULL AS DOUBLE) AS t, CAST(NULL AS DOUBLE) AS v, CAST(NULL AS VARCHAR) AS {g} WHERE FALSE"
    )
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
