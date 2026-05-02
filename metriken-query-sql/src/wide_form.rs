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
//! faster on `WINDOW` and ~3x faster end-to-end on
//! `softirq_irate_by_id_by_kind`.
//!
//! When no recognised shape matches the entry, `try_generate` returns
//! `None` and the caller falls back to the long-form VIEW path. This
//! keeps the catalogue's "long tail" of entries working unchanged.

use metriken_query::{CaptureValue, Captures, CatalogueEntry, LabelMatcher, LabelOp};

use crate::views::{MetricCatalog, MetricSeries};

/// If `entry`'s shape is one we know how to compile to wide-form SQL,
/// return that SQL. Otherwise `None`, and the caller should fall back
/// to interpolating the entry's long-form `sql` template.
pub fn try_generate(
    entry: &CatalogueEntry,
    captures: &Captures,
    catalog: &MetricCatalog,
) -> Option<String> {
    // Each handler resolves the (metric, filter, group, scale) tuple for
    // its shape and delegates to `irate_aggregate`. New shapes should
    // be added here, not by extending the per-shape parsing — the
    // generator function itself is shape-agnostic.
    match entry.id.as_str() {
        // sum by (id) (irate(softirq{kind=K}[5m]))
        "softirq_irate_by_id_by_kind" => irate_aggregate(
            catalog,
            "softirq",
            &single_eq_filter("kind", string_capture(captures, "k")?),
            Some("id"),
            1.0,
        ),
        // sum by (id) (irate(softirq_time{kind=K}[5m])) / 1e9
        "softirq_time_pct_by_id_by_kind" => irate_aggregate(
            catalog,
            "softirq_time",
            &single_eq_filter("kind", string_capture(captures, "k")?),
            Some("id"),
            1_000_000_000.0,
        ),
        // sum (irate(softirq{kind=K}[5m]))
        "softirq_irate_total_by_kind" => irate_aggregate(
            catalog,
            "softirq",
            &single_eq_filter("kind", string_capture(captures, "k")?),
            None,
            1.0,
        ),
        // sum (irate(M{labels}[R]))
        "counter_irate_sum_with_labels" => irate_aggregate(
            catalog,
            ident_capture(captures, "m")?,
            labels_capture(captures, "labels").unwrap_or(&[]),
            None,
            1.0,
        ),
        // sum by (id) (irate(M{labels}[R]))
        "counter_irate_by_id_with_labels" => irate_aggregate(
            catalog,
            ident_capture(captures, "m")?,
            labels_capture(captures, "labels").unwrap_or(&[]),
            Some("id"),
            1.0,
        ),
        _ => None,
    }
}

/// Generate SQL for `sum [by (G)] (irate(metric{filter}[R])) [/ scale]`.
///
/// Reads the metric's series list from the catalog, filters to the
/// matching physical columns, emits one `LAG(...)`-based rate
/// expression per matching column, and combines them either via
/// UNPIVOT-style UNION ALL + GROUP BY (when `group_label` is set) or
/// via a per-timestamp arithmetic sum (when there's no by-clause).
///
/// Returns `None` if the catalog doesn't carry the metric — caller
/// falls through to the long-form path, which short-circuits to
/// empty for missing metrics.
fn irate_aggregate(
    catalog: &MetricCatalog,
    metric: &str,
    filter: &[LabelMatcher],
    group_label: Option<&str>,
    scale: f64,
) -> Option<String> {
    // Decline if any matcher uses a feature the wide-form generator
    // doesn't support yet — caller falls back to long-form, which
    // handles them. Important: this is NOT the same as "no rows match"
    // (which still wants a wide-form empty-matrix SQL).
    if !filter.iter().all(|m| matches!(m.op, LabelOp::Eq | LabelOp::Ne)) {
        return None;
    }
    let series = catalog.series_by_metric.get(metric)?;
    let matching: Vec<&MetricSeries> = series
        .iter()
        .filter(|s| matches_all(s, filter))
        .collect();
    if matching.is_empty() {
        return Some(empty_matrix_sql(group_label));
    }

    let scale_expr = if scale == 1.0 {
        String::new()
    } else {
        format!(" / {scale}")
    };

    // One rate expression per matching column.
    let mut rate_exprs = String::new();
    for (i, s) in matching.iter().enumerate() {
        let phys = s.physical.replace('"', "\"\"");
        rate_exprs.push_str(&format!(
            ",\n    CASE\n        WHEN LAG(\"{phys}\") OVER w IS NULL THEN NULL\n        \
             WHEN \"{phys}\" >= LAG(\"{phys}\") OVER w\n            \
             THEN CAST(\"{phys}\" - LAG(\"{phys}\") OVER w AS DOUBLE) / NULLIF(CAST(timestamp - LAG(timestamp) OVER w AS DOUBLE) / 1e9, 0)\n        \
             ELSE CAST(\"{phys}\" AS DOUBLE) / NULLIF(CAST(timestamp - LAG(timestamp) OVER w AS DOUBLE) / 1e9, 0)\n    \
             END AS rate_{i}"
        ));
    }

    if let Some(g) = group_label {
        // sum by (G): UNPIVOT each rate into (timestamp, G_value, rate) and group.
        let mut unions = String::new();
        for (i, s) in matching.iter().enumerate() {
            let g_value = s
                .labels
                .get(g)
                .cloned()
                .unwrap_or_default()
                .replace('\'', "''");
            let prefix = if i == 0 { "" } else { "\n      UNION ALL\n      " };
            unions.push_str(&format!(
                "{prefix}SELECT timestamp, '{g_value}' AS {g}, rate_{i} AS rate FROM rates WHERE rate_{i} IS NOT NULL"
            ));
        }
        let g_quoted = quote_ident(g);
        Some(format!(
            "WITH rates AS (\n  SELECT timestamp{rate_exprs}\n  FROM _src\n  WINDOW w AS (ORDER BY timestamp)\n)\n\
             SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, SUM(rate){scale_expr} AS v, {g_quoted}\n\
             FROM (\n      {unions}\n) per_series\n\
             GROUP BY {g_quoted}, timestamp\n\
             ORDER BY {g_quoted}, timestamp"
        ))
    } else {
        // sum (no by-clause): collapse all per-column rates via COALESCE+arithmetic
        // per timestamp. Skip rows where ALL rates are NULL (PromQL semantics).
        let n = matching.len();
        let coalesce_terms: Vec<String> = (0..n)
            .map(|i| format!("COALESCE(rate_{i}, 0)"))
            .collect();
        let null_check_terms: Vec<String> = (0..n)
            .map(|i| format!("rate_{i} IS NOT NULL"))
            .collect();
        let sum_expr = coalesce_terms.join(" + ");
        let any_present = null_check_terms.join(" OR ");
        Some(format!(
            "WITH rates AS (\n  SELECT timestamp{rate_exprs}\n  FROM _src\n  WINDOW w AS (ORDER BY timestamp)\n)\n\
             SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, ({sum_expr}){scale_expr} AS v\n\
             FROM rates\n\
             WHERE {any_present}\n\
             ORDER BY timestamp"
        ))
    }
}

/// True iff `s.labels` satisfies every Eq/Ne matcher. Missing labels
/// are treated as the empty string per PromQL semantics. Caller is
/// responsible for screening out regex matchers (`irate_aggregate`
/// returns `None` if any are present so the long-form path takes over).
fn matches_all(s: &MetricSeries, filter: &[LabelMatcher]) -> bool {
    for m in filter {
        let actual = s.labels.get(&m.name).map(|v| v.as_str()).unwrap_or("");
        let ok = match m.op {
            LabelOp::Eq => actual == m.value,
            LabelOp::Ne => actual != m.value,
            LabelOp::ReEq | LabelOp::ReNe => unreachable!("screened by irate_aggregate"),
        };
        if !ok {
            return false;
        }
    }
    true
}

fn single_eq_filter(name: &str, value: &str) -> Vec<LabelMatcher> {
    vec![LabelMatcher {
        name: name.to_string(),
        op: LabelOp::Eq,
        value: value.to_string(),
    }]
}

fn string_capture<'a>(captures: &'a Captures, name: &str) -> Option<&'a str> {
    match captures.get(name)? {
        CaptureValue::String(s) => Some(s.as_str()),
        _ => None,
    }
}

fn ident_capture<'a>(captures: &'a Captures, name: &str) -> Option<&'a str> {
    match captures.get(name)? {
        CaptureValue::Ident(s) => Some(s.as_str()),
        _ => None,
    }
}

fn labels_capture<'a>(captures: &'a Captures, name: &str) -> Option<&'a [LabelMatcher]> {
    match captures.get(name)? {
        CaptureValue::Labels(v) => Some(v.as_slice()),
        _ => None,
    }
}

/// SQL that returns zero rows in the right schema. Emitted when no
/// physical column matches the filter.
fn empty_matrix_sql(group_label: Option<&str>) -> String {
    if let Some(g) = group_label {
        let g = quote_ident(g);
        format!("SELECT CAST(NULL AS DOUBLE) AS t, CAST(NULL AS DOUBLE) AS v, CAST(NULL AS VARCHAR) AS {g} WHERE FALSE")
    } else {
        "SELECT CAST(NULL AS DOUBLE) AS t, CAST(NULL AS DOUBLE) AS v WHERE FALSE".to_string()
    }
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
