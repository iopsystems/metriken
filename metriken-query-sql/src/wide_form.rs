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
//! `None` and the caller falls back to the long-form VIEW path.

use metriken_query::{CaptureValue, CatalogueEntry, Captures, LabelMatcher, LabelOp};

use crate::views::{MetricCatalog, MetricSeries};

/// What value to compute per matching physical column. The `Value`
/// variant emits `CAST("col" AS DOUBLE)` (gauges/counters as gauges);
/// the `IrateRate` variant emits the PromQL irate CASE over LAG (for
/// counter rate queries).
#[derive(Clone, Copy)]
enum PerColExpr {
    Value,
    IrateRate,
}

/// What aggregation to apply across matching columns at each
/// timestamp. `None` means each column emits its own per-series row;
/// the others reduce per (timestamp[, group_label]) tuple.
#[derive(Clone, Copy, PartialEq)]
enum Aggregation {
    None,
    Sum,
    Max,
    Avg,
}

/// Spec for one shape's wide-form SQL. Built by per-shape resolvers
/// from the catalogue entry + captures, then handed to `generate`.
struct Shape<'a> {
    metric: &'a str,
    filter: Vec<LabelMatcher>,
    group_label: Option<&'a str>,
    expr: PerColExpr,
    aggregation: Aggregation,
    /// Multiplicative divisor applied to the final value.
    scale: f64,
}

/// If `entry`'s shape is one we know how to compile to wide-form SQL,
/// return that SQL. Otherwise `None`, and the caller should fall back
/// to interpolating the entry's long-form `sql` template.
pub fn try_generate(
    entry: &CatalogueEntry,
    captures: &Captures,
    catalog: &MetricCatalog,
) -> Option<String> {
    let shape = resolve_shape(entry, captures)?;
    generate(catalog, &shape)
}

/// Map a recognised entry id + its captures onto a `Shape`. New entry
/// shapes get added here. The actual SQL emission is shape-agnostic.
fn resolve_shape<'a>(entry: &'a CatalogueEntry, captures: &'a Captures) -> Option<Shape<'a>> {
    use Aggregation::{Avg, Max, Sum};
    use PerColExpr::{IrateRate, Value};
    match entry.id.as_str() {
        // ---- counter / irate shapes ----
        // sum by (id) (irate(softirq{kind=K}[5m]))
        "softirq_irate_by_id_by_kind" => Some(Shape {
            metric: "softirq",
            filter: single_eq_filter("kind", string_capture(captures, "k")?),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
        }),
        // sum by (id) (irate(softirq_time{kind=K}[5m])) / 1e9
        "softirq_time_pct_by_id_by_kind" => Some(Shape {
            metric: "softirq_time",
            filter: single_eq_filter("kind", string_capture(captures, "k")?),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1_000_000_000.0,
        }),
        // sum (irate(softirq{kind=K}[5m]))
        "softirq_irate_total_by_kind" => Some(Shape {
            metric: "softirq",
            filter: single_eq_filter("kind", string_capture(captures, "k")?),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
        }),
        // sum (irate(M{labels}[R]))
        "counter_irate_sum_with_labels" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
        }),
        // sum by (id) (irate(M{labels}[R]))
        "counter_irate_by_id_with_labels" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
        }),
        // sum (irate(M[R])) / D
        "counter_irate_total_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
        }),
        // sum by (id) (irate(M[R])) / D
        "counter_irate_by_id_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
        }),
        // sum by (id) (irate(M{labels}[R])) / D
        "counter_irate_by_id_with_labels_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
        }),
        // sum (irate(M{labels}[R])) / D — note the entry id is "counter_irate_with_labels_scaled"
        "counter_irate_with_labels_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
        }),

        // ---- gauge / value shapes ----
        // M{labels}
        "gauge_bare_with_labels" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: None,
            expr: Value,
            aggregation: Aggregation::None,
            scale: 1.0,
        }),
        // M (catalogue entry "gauge_bare")
        "gauge_bare" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: Value,
            aggregation: Aggregation::None,
            scale: 1.0,
        }),
        // sum(M)
        "gauge_sum_bare" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: Value,
            aggregation: Sum,
            scale: 1.0,
        }),
        // sum(M{labels})
        "gauge_sum_with_labels" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: None,
            expr: Value,
            aggregation: Sum,
            scale: 1.0,
        }),
        // sum by (g) (M)
        "gauge_sum_by_g_bare" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: Some(ident_capture(captures, "g")?),
            expr: Value,
            aggregation: Sum,
            scale: 1.0,
        }),
        // sum by (g) (M{labels})
        "gauge_sum_by_g_with_labels" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: Some(ident_capture(captures, "g")?),
            expr: Value,
            aggregation: Sum,
            scale: 1.0,
        }),
        // sum by (g) (M) / D
        "gauge_sum_by_g_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: Some(ident_capture(captures, "g")?),
            expr: Value,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
        }),
        // sum by (g) (M{labels}) / D
        "gauge_sum_by_g_with_labels_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: Some(ident_capture(captures, "g")?),
            expr: Value,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
        }),
        // max(M)
        "gauge_max" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: Value,
            aggregation: Max,
            scale: 1.0,
        }),
        // avg(M)
        "gauge_avg" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: Value,
            aggregation: Avg,
            scale: 1.0,
        }),
        _ => None,
    }
}

/// Build the wide-form SQL for `shape`. Returns `None` if the catalog
/// doesn't carry the metric (caller falls through to long-form, which
/// short-circuits to empty for missing metrics) or if the shape uses
/// a feature the generator can't handle (e.g. real regex matchers).
fn generate(catalog: &MetricCatalog, shape: &Shape) -> Option<String> {
    let filter = coerce_literal_regex(&shape.filter)?;
    let series = catalog.series_by_metric.get(shape.metric)?;
    let matching: Vec<&MetricSeries> = series
        .iter()
        .filter(|s| matches_all(s, &filter))
        .collect();
    if matching.is_empty() {
        return Some(empty_matrix_sql(shape));
    }

    let scale_expr = if shape.scale == 1.0 {
        String::new()
    } else {
        format!(" / {}", shape.scale)
    };

    // 1) Per-column expression. For irate we need a WINDOW for LAG.
    let (per_col_select, window_clause) = build_per_col(&matching, shape.expr);

    // 2) UNPIVOT-ish: select rows of (timestamp[, group_label_value],
    //    per_col_value) for each column. This is the same shape for
    //    Aggregation::None (passthrough) and Aggregation::* (input to GROUP BY).
    let mut unions = String::new();
    for (i, s) in matching.iter().enumerate() {
        let prefix = if i == 0 { "" } else { "\n      UNION ALL\n      " };
        // Project all the labels of this series so passthrough output
        // (Aggregation::None) carries them. For aggregated output we
        // emit just the group label (or nothing).
        let label_proj = if shape.aggregation == Aggregation::None {
            // Project every label key the metric has across all series.
            let mut all_keys: std::collections::BTreeSet<&str> = Default::default();
            for s in &matching {
                for k in s.labels.keys() {
                    all_keys.insert(k.as_str());
                }
            }
            let mut parts = String::new();
            for k in &all_keys {
                let v = s
                    .labels
                    .get(*k)
                    .cloned()
                    .unwrap_or_default()
                    .replace('\'', "''");
                parts.push_str(&format!(", '{v}' AS {}", quote_ident(k)));
            }
            parts
        } else if let Some(g) = shape.group_label {
            let v = s
                .labels
                .get(g)
                .cloned()
                .unwrap_or_default()
                .replace('\'', "''");
            format!(", '{v}' AS {}", quote_ident(g))
        } else {
            String::new()
        };
        unions.push_str(&format!(
            "{prefix}SELECT timestamp, val_{i} AS v{label_proj} FROM rates WHERE val_{i} IS NOT NULL"
        ));
    }

    // 3) Final aggregation step. Aggregation::None passes through.
    let value_expr = match shape.aggregation {
        Aggregation::None => format!("v{scale_expr}"),
        Aggregation::Sum => format!("SUM(v){scale_expr}"),
        Aggregation::Max => format!("MAX(v){scale_expr}"),
        Aggregation::Avg => format!("AVG(v){scale_expr}"),
    };
    let (group_clause, order_clause, group_select) = match (shape.aggregation, shape.group_label) {
        (Aggregation::None, _) => {
            // Project labels we already SELECT in the UNPIVOT branches.
            let mut all_keys: std::collections::BTreeSet<&str> = Default::default();
            for s in &matching {
                for k in s.labels.keys() {
                    all_keys.insert(k.as_str());
                }
            }
            let parts: Vec<String> = all_keys.iter().map(|k| quote_ident(k)).collect();
            let labels_sql = if parts.is_empty() {
                String::new()
            } else {
                format!(", {}", parts.join(", "))
            };
            let order = if parts.is_empty() {
                "timestamp".to_string()
            } else {
                format!("{}, timestamp", parts.join(", "))
            };
            (String::new(), order, labels_sql)
        }
        (_, Some(g)) => {
            let g = quote_ident(g);
            (
                format!("GROUP BY {g}, timestamp"),
                format!("{g}, timestamp"),
                format!(", {g}"),
            )
        }
        (_, None) => (
            "GROUP BY timestamp".to_string(),
            "timestamp".to_string(),
            String::new(),
        ),
    };

    Some(format!(
        "WITH rates AS (\n  SELECT timestamp{per_col_select}\n  FROM _src\n  {window_clause}\n)\n\
         SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, {value_expr} AS v{group_select}\n\
         FROM (\n      {unions}\n) per_series\n\
         {group_clause}\n\
         ORDER BY {order_clause}"
    ))
}

/// Emit one per-column expression aliased `val_<i>`. Returns
/// `(per_col_select_text, window_clause_text)`. Only IrateRate needs
/// the WINDOW; Value just `CAST`s the column.
fn build_per_col(matching: &[&MetricSeries], expr: PerColExpr) -> (String, String) {
    let mut per_col = String::new();
    let mut needs_window = false;
    for (i, s) in matching.iter().enumerate() {
        let phys = s.physical.replace('"', "\"\"");
        match expr {
            PerColExpr::IrateRate => {
                needs_window = true;
                per_col.push_str(&format!(
                    ",\n    CASE\n        WHEN LAG(\"{phys}\") OVER w IS NULL THEN NULL\n        \
                     WHEN \"{phys}\" >= LAG(\"{phys}\") OVER w\n            \
                     THEN CAST(\"{phys}\" - LAG(\"{phys}\") OVER w AS DOUBLE) / NULLIF(CAST(timestamp - LAG(timestamp) OVER w AS DOUBLE) / 1e9, 0)\n        \
                     ELSE CAST(\"{phys}\" AS DOUBLE) / NULLIF(CAST(timestamp - LAG(timestamp) OVER w AS DOUBLE) / 1e9, 0)\n    \
                     END AS val_{i}"
                ));
            }
            PerColExpr::Value => {
                per_col.push_str(&format!(",\n    CAST(\"{phys}\" AS DOUBLE) AS val_{i}"));
            }
        }
    }
    let window_clause = if needs_window {
        "WINDOW w AS (ORDER BY timestamp)".to_string()
    } else {
        String::new()
    };
    (per_col, window_clause)
}

/// Coerce literal-pattern regex matchers to Eq/Ne. Returns `None` if
/// any matcher uses a true regex (with metacharacters) — the caller
/// should fall back to the long-form path.
fn coerce_literal_regex(filter: &[LabelMatcher]) -> Option<Vec<LabelMatcher>> {
    let mut out = Vec::with_capacity(filter.len());
    for m in filter {
        let coerced = match m.op {
            LabelOp::Eq | LabelOp::Ne => m.clone(),
            LabelOp::ReEq if is_regex_literal(&m.value) => LabelMatcher {
                name: m.name.clone(),
                op: LabelOp::Eq,
                value: m.value.clone(),
            },
            LabelOp::ReNe if is_regex_literal(&m.value) => LabelMatcher {
                name: m.name.clone(),
                op: LabelOp::Ne,
                value: m.value.clone(),
            },
            LabelOp::ReEq | LabelOp::ReNe => return None,
        };
        out.push(coerced);
    }
    Some(out)
}

/// True iff `s.labels` satisfies every Eq/Ne matcher. Missing labels
/// are treated as the empty string per PromQL semantics.
fn matches_all(s: &MetricSeries, filter: &[LabelMatcher]) -> bool {
    for m in filter {
        let actual = s.labels.get(&m.name).map(|v| v.as_str()).unwrap_or("");
        let ok = match m.op {
            LabelOp::Eq => actual == m.value,
            LabelOp::Ne => actual != m.value,
            LabelOp::ReEq | LabelOp::ReNe => unreachable!("screened by coerce_literal_regex"),
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

fn number_capture(captures: &Captures, name: &str) -> Option<f64> {
    match captures.get(name)? {
        CaptureValue::Number(n) => Some(*n),
        _ => None,
    }
}

/// SQL that returns zero rows in the right schema. Emitted when no
/// physical column matches the filter (the long-form path would
/// also produce zero rows for that query).
fn empty_matrix_sql(shape: &Shape) -> String {
    let extra = if shape.aggregation == Aggregation::None {
        // No way to know label columns without a matching col. Return
        // bare (t, v).
        String::new()
    } else if let Some(g) = shape.group_label {
        format!(", CAST(NULL AS VARCHAR) AS {}", quote_ident(g))
    } else {
        String::new()
    };
    format!("SELECT CAST(NULL AS DOUBLE) AS t, CAST(NULL AS DOUBLE) AS v{extra} WHERE FALSE")
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// True iff `s` contains no RE2 regex metacharacters. PromQL regex is
/// fully anchored, so a literal-pattern regex matcher is equivalent
/// to the corresponding `=`/`!=` matcher.
fn is_regex_literal(s: &str) -> bool {
    !s.chars().any(|c| matches!(c,
        '.' | '[' | ']' | '\\' | '^' | '$' | '(' | ')' | '|' |
        '*' | '+' | '?' | '{' | '}'
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_literal_detection() {
        assert!(is_regex_literal("__SELECTED_CGROUPS__"));
        assert!(is_regex_literal("net_rx"));
        assert!(is_regex_literal(""));
        assert!(is_regex_literal("a-b_c"));
        assert!(!is_regex_literal("net.*"));
        assert!(!is_regex_literal("a|b"));
        assert!(!is_regex_literal("a+"));
        assert!(!is_regex_literal("a{1,2}"));
        assert!(!is_regex_literal("[abc]"));
    }
}
