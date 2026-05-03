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
    if let Some(shape) = resolve_shape(entry, captures) {
        return generate(catalog, &shape);
    }
    if let Some(binary) = resolve_binary(entry, captures) {
        return generate_binary(catalog, &binary);
    }
    None
}

/// Binary "lane combiner" shape for `f(sum(a), sum(b))` style entries.
/// Each lane is a unary `Shape`; the lanes are joined on
/// `timestamp` (and `group_label`, if both lanes share one) and
/// combined via `op`.
struct Binary<'a> {
    a: Shape<'a>,
    b: Shape<'a>,
    op: BinaryOp,
    /// Constant added to the result *before* dividing/multiplying by
    /// the other lane. Currently only `Some(1.0)` for the "1 - a/b"
    /// shape.
    leading_constant: Option<f64>,
    /// Multiplicative scale applied to the result.
    scale: f64,
}

#[derive(Clone, Copy)]
enum BinaryOp {
    /// `a / NULLIF(b, 0)`.
    Div,
    /// `a - COALESCE(b, 0)` (LEFT JOIN so a-rows survive missing b).
    Sub,
    /// `a + COALESCE(b, 0)` (rare).
    Add,
    /// `a * COALESCE(b, 0)`.
    Mul,
}

fn resolve_binary<'a>(entry: &'a CatalogueEntry, captures: &'a Captures) -> Option<Binary<'a>> {
    use Aggregation::Sum;
    use PerColExpr::IrateRate;
    match entry.id.as_str() {
        // sum(irate(a[5m])) / sum(irate(b[5m]))
        "counter_ratio_generic" => Some(Binary {
            a: Shape {
                metric: ident_capture(captures, "a")?,
                filter: Vec::new(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: Vec::new(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            op: BinaryOp::Div,
            leading_constant: None,
            scale: 1.0,
        }),
        // 1 - sum(irate(a)) / sum(irate(b))
        "counter_ratio_complement" => Some(Binary {
            a: Shape {
                metric: ident_capture(captures, "a")?,
                filter: Vec::new(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: Vec::new(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            op: BinaryOp::Div,
            leading_constant: Some(1.0),
            scale: 1.0,
        }),
        // sum by (id) (irate(a)) / sum by (id) (irate(b))
        "counter_ratio_by_id_generic" => Some(Binary {
            a: Shape {
                metric: ident_capture(captures, "a")?,
                filter: Vec::new(),
                group_label: Some("id"),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: Vec::new(),
                group_label: Some("id"),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            op: BinaryOp::Div,
            leading_constant: None,
            scale: 1.0,
        }),
        // 1 - sum by (id) (irate(a)) / sum by (id) (irate(b))
        "counter_ratio_by_id_complement" => Some(Binary {
            a: Shape {
                metric: ident_capture(captures, "a")?,
                filter: Vec::new(),
                group_label: Some("id"),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: Vec::new(),
                group_label: Some("id"),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            op: BinaryOp::Div,
            leading_constant: Some(1.0),
            scale: 1.0,
        }),
        // sum(irate(a{la}[ra])) - sum(irate(b{lb}[rb]))
        "counter_irate_subtract_with_labels" => Some(Binary {
            a: Shape {
                metric: ident_capture(captures, "a")?,
                filter: labels_capture(captures, "la").unwrap_or(&[]).to_vec(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: labels_capture(captures, "lb").unwrap_or(&[]).to_vec(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            op: BinaryOp::Sub,
            leading_constant: None,
            scale: 1.0,
        }),
        // sum(irate(a{la}[ra])) / sum(irate(b{lb}[rb]))
        "counter_irate_ratio_with_labels" => Some(Binary {
            a: Shape {
                metric: ident_capture(captures, "a")?,
                filter: labels_capture(captures, "la").unwrap_or(&[]).to_vec(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: labels_capture(captures, "lb").unwrap_or(&[]).to_vec(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
            },
            op: BinaryOp::Div,
            leading_constant: None,
            scale: 1.0,
        }),
        _ => None,
    }
}

/// Generate SQL for a binary shape: two unary lanes joined on
/// `timestamp` (and on `group_label` when both lanes share one) and
/// combined via the binary `op`. Both lanes use the same wide-form
/// rates+sum CTE pattern as `generate`.
fn generate_binary(catalog: &MetricCatalog, binary: &Binary) -> Option<String> {
    let a_lane = build_lane(catalog, &binary.a, "a")?;
    let b_lane = build_lane(catalog, &binary.b, "b")?;

    // Both lanes must agree on group_label for the join schema.
    let group_label = match (binary.a.group_label, binary.b.group_label) {
        (Some(ga), Some(gb)) if ga == gb => Some(ga),
        (None, None) => None,
        _ => return None, // mismatched grouping — fall back to long-form
    };

    // Empty-result short-circuit. If either lane returns no rows, the
    // combined query is empty regardless of op (subtract, divide, etc.
    // all collapse to empty under join).
    if a_lane.matching_count == 0 || b_lane.matching_count == 0 {
        return Some(empty_binary_sql(group_label));
    }

    let join_clause = match group_label {
        Some(g) => format!("JOIN b_summed b ON a.timestamp = b.timestamp AND a.{g} = b.{g}", g = quote_ident(g)),
        None => "JOIN b_summed b ON a.timestamp = b.timestamp".to_string(),
    };
    let (a_sub, _b_only_join) = match binary.op {
        BinaryOp::Sub => {
            // Subtract semantically should still emit if a present and b missing.
            // Use LEFT JOIN with COALESCE(b, 0).
            let lj = match group_label {
                Some(g) => format!("LEFT JOIN b_summed b ON a.timestamp = b.timestamp AND a.{g} = b.{g}", g = quote_ident(g)),
                None => "LEFT JOIN b_summed b ON a.timestamp = b.timestamp".to_string(),
            };
            (lj, true)
        }
        _ => (join_clause, false),
    };

    let combine_expr = match binary.op {
        BinaryOp::Div => "a.v / NULLIF(b.v, 0)".to_string(),
        BinaryOp::Sub => "a.v - COALESCE(b.v, 0)".to_string(),
        BinaryOp::Add => "a.v + COALESCE(b.v, 0)".to_string(),
        BinaryOp::Mul => "a.v * COALESCE(b.v, 0)".to_string(),
    };
    let combine_with_constant = match binary.leading_constant {
        Some(c) => format!("({c} - ({combine_expr}))"),
        None => combine_expr,
    };
    let scale_expr = if binary.scale == 1.0 {
        String::new()
    } else {
        format!(" / {}", binary.scale)
    };
    let value_select = format!("({combine_with_constant}){scale_expr} AS v");

    let group_select = match group_label {
        Some(g) => format!(", a.{g}", g = quote_ident(g)),
        None => String::new(),
    };

    // No ORDER BY here either — same reasoning as the unary
    // generate_sum: the Rust-side `run_matrix` collects rows into a
    // BTreeMap, and the canonical-JSON comparison sorts samples by
    // timestamp before diffing. Profiled measurement: dropping ORDER
    // BY shaves ~0.3-0.4ms (>30%) on the worst-case shape.
    Some(format!(
        "WITH\n{a_ctes},\n{b_ctes}\n\
         SELECT CAST(a.timestamp AS DOUBLE) / 1e9 AS t, {value_select}{group_select}\n\
         FROM a_summed a {a_sub}",
        a_ctes = a_lane.ctes,
        b_ctes = b_lane.ctes,
    ))
}

struct Lane {
    /// SQL fragments forming one or more CTEs whose final CTE has the
    /// alias `<prefix>_summed` and the schema `(timestamp, v[, g])`.
    ctes: String,
    matching_count: usize,
}

/// Build a Lane for one side of a binary expression. Reuses the
/// per-column WINDOW + UNPIVOT-ish-then-aggregate pattern from
/// `generate` but emits CTEs the binary combiner can join, rather
/// than a final SELECT.
fn build_lane(catalog: &MetricCatalog, shape: &Shape, prefix: &str) -> Option<Lane> {
    let filter = coerce_literal_regex(&shape.filter)?;
    let series = catalog.series_by_metric.get(shape.metric)?;
    let matching: Vec<&MetricSeries> = series
        .iter()
        .filter(|s| matches_all(s, &filter))
        .collect();
    if matching.is_empty() {
        return Some(Lane {
            ctes: String::new(),
            matching_count: 0,
        });
    }
    if matching.len() > WIDE_FORM_MAX_COLS {
        return None;
    }
    if shape.aggregation == Aggregation::None {
        return None; // binary lanes always aggregate (sum_by or sum)
    }

    let (per_col_select, window_clause) = build_per_col(&matching, shape.expr);
    let scale_expr = if shape.scale == 1.0 {
        String::new()
    } else {
        format!(" / {}", shape.scale)
    };

    // For Aggregation::Sum: same pre-aggregate trick as the unary
    // path — bucket cols by group_label value, emit one row per
    // (timestamp, g_value) via per-row arithmetic, no GROUP BY.
    let summed_cte = if shape.aggregation == Aggregation::Sum {
        let mut groups: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
        for (i, s) in matching.iter().enumerate() {
            let key = match shape.group_label {
                Some(g) => s.labels.get(g).cloned().unwrap_or_default(),
                None => String::new(),
            };
            groups.entry(key).or_default().push(i);
        }
        let mut union_branches: Vec<String> = Vec::with_capacity(groups.len());
        for (g_value, col_idxs) in &groups {
            let coalesce_terms: Vec<String> = col_idxs
                .iter()
                .map(|i| format!("COALESCE(val_{i}, 0)"))
                .collect();
            let null_check_terms: Vec<String> = col_idxs
                .iter()
                .map(|i| format!("val_{i} IS NOT NULL"))
                .collect();
            let sum_expr = coalesce_terms.join(" + ");
            let any_present = null_check_terms.join(" OR ");
            let g_select = match shape.group_label {
                Some(g) => {
                    let g_quoted = quote_ident(g);
                    let g_lit = g_value.replace('\'', "''");
                    format!(", '{g_lit}' AS {g_quoted}")
                }
                None => String::new(),
            };
            union_branches.push(format!(
                "SELECT timestamp, ({sum_expr}){scale_expr} AS v{g_select} FROM {prefix}_rates WHERE {any_present}"
            ));
        }
        let unions = union_branches.join("\n      UNION ALL\n      ");
        format!(
            "{prefix}_summed AS (\n      {unions}\n)"
        )
    } else {
        // Max / Avg keep the GROUP BY path.
        let mut unions = String::new();
        for (i, s) in matching.iter().enumerate() {
            let label_proj = if let Some(g) = shape.group_label {
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
            let p = if i == 0 { "" } else { "\n      UNION ALL\n      " };
            unions.push_str(&format!(
                "{p}SELECT timestamp, val_{i} AS v{label_proj} FROM {prefix}_rates WHERE val_{i} IS NOT NULL"
            ));
        }
        let agg_op = match shape.aggregation {
            Aggregation::Max => "MAX",
            Aggregation::Avg => "AVG",
            _ => unreachable!(),
        };
        let (group_clause, group_select) = match shape.group_label {
            Some(g) => (
                format!("GROUP BY {g}, timestamp", g = quote_ident(g)),
                format!(", {g}", g = quote_ident(g)),
            ),
            None => ("GROUP BY timestamp".to_string(), String::new()),
        };
        format!(
            "{prefix}_summed AS (\n  SELECT timestamp, {agg_op}(v){scale_expr} AS v{group_select}\n  FROM (\n      {unions}\n  ) per_series\n  {group_clause}\n)"
        )
    };

    let ctes = format!(
        "{prefix}_rates AS (\n  SELECT timestamp{per_col_select}\n  FROM _src\n  {window_clause}\n),\n\
         {summed_cte}"
    );
    Some(Lane {
        ctes,
        matching_count: matching.len(),
    })
}

fn empty_binary_sql(group_label: Option<&str>) -> String {
    if let Some(g) = group_label {
        let g = quote_ident(g);
        format!(
            "SELECT CAST(NULL AS DOUBLE) AS t, CAST(NULL AS DOUBLE) AS v, CAST(NULL AS VARCHAR) AS {g} WHERE FALSE"
        )
    } else {
        "SELECT CAST(NULL AS DOUBLE) AS t, CAST(NULL AS DOUBLE) AS v WHERE FALSE".to_string()
    }
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

/// Beyond this many matching physical columns the wide-form SQL grows
/// too large (one CASE-LAG expression per column) and the long-form
/// VIEW path's predicate pushdown is competitive or faster. Empirical
/// crossover from per-query timings: at ~60 matching cols the wide
/// SQL's parse/plan overhead starts dominating its WINDOW savings.
const WIDE_FORM_MAX_COLS: usize = 60;

/// Build the wide-form SQL for `shape`. Returns `None` if the catalog
/// doesn't carry the metric (caller falls through to long-form, which
/// short-circuits to empty for missing metrics), if the shape uses a
/// feature the generator can't handle (e.g. real regex matchers), or
/// if the matching column count exceeds the wide-form's working range.
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
    if matching.len() > WIDE_FORM_MAX_COLS {
        return None;
    }

    let scale_expr = if shape.scale == 1.0 {
        String::new()
    } else {
        format!(" / {}", shape.scale)
    };

    // Per-column expression. For irate we need a WINDOW for LAG.
    let (per_col_select, window_clause) = build_per_col(&matching, shape.expr);

    // For Aggregation::Sum we pre-aggregate at the rate-expression
    // level: group matching cols by their `group_label` value (or all
    // into one group when there's no by-clause), emit one row per
    // group via `(COALESCE(val_i, 0) + COALESCE(val_j, 0) + ...) AS v`,
    // and skip the GROUP BY operator entirely. Profiled measurement:
    // for vllm softirq{kind=rcu} the HASH_GROUP_BY operator was 1.14ms
    // out of 1.94ms total (59%) — its work was identity-grouping rows
    // that already had unique (id, timestamp) tuples. Eliminating it
    // is a clean win.
    if shape.aggregation == Aggregation::Sum {
        return Some(generate_sum(&matching, shape, &per_col_select, &window_clause, &scale_expr));
    }

    // Other aggregations (None / Max / Avg) keep the UNPIVOT + GROUP BY
    // pattern. None is also the schema-projecting passthrough for bare
    // gauge selectors. Max/Avg can't be pushed through arithmetic the
    // way Sum can.
    let mut unions = String::new();
    for (i, s) in matching.iter().enumerate() {
        let prefix = if i == 0 { "" } else { "\n      UNION ALL\n      " };
        let label_proj = if shape.aggregation == Aggregation::None {
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

    let value_expr = match shape.aggregation {
        Aggregation::None => format!("v{scale_expr}"),
        Aggregation::Max => format!("MAX(v){scale_expr}"),
        Aggregation::Avg => format!("AVG(v){scale_expr}"),
        Aggregation::Sum => unreachable!("handled above"),
    };
    let (group_clause, order_clause, group_select) = match (shape.aggregation, shape.group_label) {
        (Aggregation::None, _) => {
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

/// Sum aggregation, pre-aggregated at the rate-expression level so
/// the final query has no GROUP BY operator. Group matching cols by
/// their `group_label` value (or all into one group when there's no
/// by-clause), then emit one row per (timestamp, group_value) via
/// arithmetic on the rate columns.
fn generate_sum(
    matching: &[&MetricSeries],
    shape: &Shape,
    per_col_select: &str,
    window_clause: &str,
    scale_expr: &str,
) -> String {
    // Bucket matching cols by their group-label value. With no by-clause
    // there's just one bucket containing all cols.
    let mut groups: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    for (i, s) in matching.iter().enumerate() {
        let key = match shape.group_label {
            Some(g) => s.labels.get(g).cloned().unwrap_or_default(),
            None => String::new(),
        };
        groups.entry(key).or_default().push(i);
    }

    let mut union_branches: Vec<String> = Vec::with_capacity(groups.len());
    for (g_value, col_idxs) in &groups {
        let coalesce_terms: Vec<String> = col_idxs
            .iter()
            .map(|i| format!("COALESCE(val_{i}, 0)"))
            .collect();
        let null_check_terms: Vec<String> = col_idxs
            .iter()
            .map(|i| format!("val_{i} IS NOT NULL"))
            .collect();
        let sum_expr = coalesce_terms.join(" + ");
        let any_present = null_check_terms.join(" OR ");
        let g_select = match shape.group_label {
            Some(g) => {
                let g_quoted = quote_ident(g);
                let g_lit = g_value.replace('\'', "''");
                format!(", '{g_lit}' AS {g_quoted}")
            }
            None => String::new(),
        };
        union_branches.push(format!(
            "SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, ({sum_expr}){scale_expr} AS v{g_select} FROM rates WHERE {any_present}"
        ));
    }
    let unions = union_branches.join("\n      UNION ALL\n      ");

    // No ORDER BY: the Rust-side `run_matrix` collects rows into a
    // BTreeMap keyed by labels (which sorts the groups), and the
    // canonical-JSON comparison in `divergence_inspector` sorts each
    // series's samples by timestamp before diffing. The bench likewise
    // doesn't depend on a particular row order. Profiled measurement:
    // dropping ORDER BY shaves another ~0.4ms (37% of remaining warm
    // exec) on the worst-case sum_by shape.
    format!(
        "WITH rates AS (\n  SELECT timestamp{per_col_select}\n  FROM _src\n  {window_clause}\n)\n\
         {unions}"
    )
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
