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
/// counter rate queries); the `Rate` variant computes the windowed
/// average of per-pair increments (PromQL `rate(M[Wm])`).
#[derive(Clone, Copy)]
enum PerColExpr {
    Value,
    IrateRate,
    /// `range_secs` is the trailing window width in seconds, derived
    /// from the entry's `[Wm]` capture. Implementation: per-pair
    /// reset-aware increment in one CTE, then `(SUM - FIRST_VALUE) /
    /// (curr_ts - first_ts)` over `ROWS BETWEEN range_secs PRECEDING
    /// AND CURRENT ROW` in a second CTE — matches the long-form
    /// `counter_rate_bare_generic` formula and PromQL semantics in the
    /// 1Hz-uniform-sampling regime our fixtures use.
    Rate { range_secs: u64 },
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
    /// Divisor applied as `/ scale`. The catalogue's "scaled" entries
    /// pass `d` here (PromQL `/ d`). For "* d" entries (only
    /// `counter_irate_total_mul` today), pass `1.0 / d` — fine when
    /// `d` is a power of two (8, 1024, ...) since `1/d` is then exact;
    /// for non-power-of-two multipliers (e.g. * 1000) the unary path
    /// loses a ULP. See `Binary.multiplier` for the precise alternative.
    scale: f64,
    /// Divide the aggregated value by the gauge `cpu_cores` per-row.
    /// Used by the `_per_cpu_core_pct` shapes. The generator looks
    /// up `cpu_cores` in the catalog at SQL-gen time and projects it
    /// alongside the rate columns; if `cpu_cores` is missing from the
    /// fixture, the generator returns `None` (caller falls back to
    /// long-form, which short-circuits to empty).
    cpu_cores_div: bool,
}


/// If `entry`'s shape is one we know how to compile to wide-form SQL,
/// return that SQL. Otherwise `None`, and the caller should fall back
/// to interpolating the entry's long-form `sql` template.
pub fn try_generate(
    entry: &CatalogueEntry,
    captures: &Captures,
    catalog: &MetricCatalog,
) -> Option<String> {
    if let Some(sql) = try_chain(entry, captures, catalog) {
        return Some(sql);
    }
    if let Some(sql) = try_pair_match(entry, captures, catalog) {
        return Some(sql);
    }
    if let Some(sql) = try_histogram(entry, captures, catalog) {
        return Some(sql);
    }
    if let Some(sql) = try_avg_over_time(entry, catalog) {
        return Some(sql);
    }
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
    /// Multiplicative scale applied as `* multiplier` (not `/ d`).
    /// Reason: PromQL `expr * d` for `d = 1000` is bit-exact
    /// `(a/b) * 1000.0`, but `(a/b) / 0.001` rounds differently in the
    /// last mantissa bit (0.001 is not representable in IEEE 754) and
    /// produces divergent canonical-JSON comparisons.
    multiplier: f64,
    /// Divisor applied as `/ divisor`. Same precision rationale as
    /// `multiplier`: PromQL `expr / 1e9` differs in the last bit from
    /// `expr * (1.0/1e9)` because `1e-9` is not exactly representable.
    /// Applied after the binary `op` and before `multiplier`.
    divisor: f64,
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
        // sum(irate(cpu_usage{state="user"}[5m])) / sum(irate(cpu_usage[5m]))
        "counter_ratio" => Some(Binary {
            a: Shape {
                metric: "cpu_usage",
                filter: single_eq_filter("state", "user"),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            b: Shape {
                metric: "cpu_usage",
                filter: Vec::new(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            op: BinaryOp::Div,
            leading_constant: None,
            multiplier: 1.0,
            divisor: 1.0,
        }),
        // sum(irate(a[5m])) / sum(irate(b[5m]))
        "counter_ratio_generic" => Some(Binary {
            a: Shape {
                metric: ident_capture(captures, "a")?,
                filter: Vec::new(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: Vec::new(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            op: BinaryOp::Div,
            leading_constant: None,
            multiplier: 1.0,
            divisor: 1.0,
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
                cpu_cores_div: false,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: Vec::new(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            op: BinaryOp::Div,
            leading_constant: Some(1.0),
            multiplier: 1.0,
            divisor: 1.0,
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
                cpu_cores_div: false,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: Vec::new(),
                group_label: Some("id"),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            op: BinaryOp::Div,
            leading_constant: None,
            multiplier: 1.0,
            divisor: 1.0,
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
                cpu_cores_div: false,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: Vec::new(),
                group_label: Some("id"),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            op: BinaryOp::Div,
            leading_constant: Some(1.0),
            multiplier: 1.0,
            divisor: 1.0,
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
                cpu_cores_div: false,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: labels_capture(captures, "lb").unwrap_or(&[]).to_vec(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            op: BinaryOp::Sub,
            leading_constant: None,
            multiplier: 1.0,
            divisor: 1.0,
        }),
        // sum(irate(a[ra])) / sum(irate(b[rb])) * d
        "counter_ratio_scaled" => Some(Binary {
            a: Shape {
                metric: ident_capture(captures, "a")?,
                filter: Vec::new(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: Vec::new(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            op: BinaryOp::Div,
            leading_constant: None,
            multiplier: number_capture(captures, "d").unwrap_or(1.0),
            divisor: 1.0,
        }),
        // sum by (id) (irate(a[ra])) / sum by (id) (irate(b[rb])) * d
        "counter_ratio_by_id_scaled" => Some(Binary {
            a: Shape {
                metric: ident_capture(captures, "a")?,
                filter: Vec::new(),
                group_label: Some("id"),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: Vec::new(),
                group_label: Some("id"),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            op: BinaryOp::Div,
            leading_constant: None,
            multiplier: number_capture(captures, "d").unwrap_or(1.0),
            divisor: 1.0,
        }),
        // sum by (ga) (irate(a{la}[ra])) / sum by (gb) (irate(b{lb}[rb]))
        // — production queries always use ga == gb. When they differ
        // generate_binary returns None (caller falls back to long-form).
        "counter_ratio_by_g_with_labels" => Some(Binary {
            a: Shape {
                metric: ident_capture(captures, "a")?,
                filter: labels_capture(captures, "la").unwrap_or(&[]).to_vec(),
                group_label: Some(ident_capture(captures, "ga")?),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: labels_capture(captures, "lb").unwrap_or(&[]).to_vec(),
                group_label: Some(ident_capture(captures, "gb")?),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            op: BinaryOp::Div,
            leading_constant: None,
            multiplier: 1.0,
            divisor: 1.0,
        }),
        // (sum by (sampler) (irate(rezolus_bpf_run_time[5m]))
        //  / sum by (sampler) (irate(rezolus_bpf_run_count[5m]))) / 1e9
        "rezolus_bpf_avg_run_time_per_sampler" => Some(Binary {
            a: Shape {
                metric: "rezolus_bpf_run_time",
                filter: Vec::new(),
                group_label: Some("sampler"),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            b: Shape {
                metric: "rezolus_bpf_run_count",
                filter: Vec::new(),
                group_label: Some("sampler"),
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            op: BinaryOp::Div,
            leading_constant: None,
            multiplier: 1.0,
            divisor: 1_000_000_000.0,
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
                cpu_cores_div: false,
            },
            b: Shape {
                metric: ident_capture(captures, "b")?,
                filter: labels_capture(captures, "lb").unwrap_or(&[]).to_vec(),
                group_label: None,
                expr: IrateRate,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            },
            op: BinaryOp::Div,
            leading_constant: None,
            multiplier: 1.0,
            divisor: 1.0,
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
    let div_expr = if binary.divisor == 1.0 {
        String::new()
    } else {
        format!(" / {}", binary.divisor)
    };
    let mult_expr = if binary.multiplier == 1.0 {
        String::new()
    } else {
        format!(" * {}", binary.multiplier)
    };
    // Order: `(combine) / divisor * multiplier`. PromQL is left-to-right
    // associative; entries today use exactly one of the two, so the
    // ordering only matters when both happen to be set.
    let value_select = format!("({combine_with_constant}){div_expr}{mult_expr} AS v");

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

/// `avg_over_time(queue_depth[5m])` — single hardcoded entry. The
/// long-form uses an in-line `AVG OVER (ROWS BETWEEN 300 PRECEDING)`
/// against `read_parquet(...)`; wide-form does the same against
/// `_src` and respects the snapped timestamps.
fn try_avg_over_time(entry: &CatalogueEntry, catalog: &MetricCatalog) -> Option<String> {
    if entry.id != "gauge_avg_over_time" {
        return None;
    }
    let series = match catalog.series_by_metric.get("queue_depth") {
        Some(s) if s.len() == 1 => s,
        _ => return Some(empty_chain_sql(None)),
    };
    let phys = series[0].physical.replace('"', "\"\"");
    Some(format!(
        "SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, \
                AVG(CAST(\"{phys}\" AS DOUBLE)) OVER (\
                    ORDER BY timestamp ROWS BETWEEN 300 PRECEDING AND CURRENT ROW\
                ) AS v\n\
         FROM _src WHERE \"{phys}\" IS NOT NULL\n\
         ORDER BY t"
    ))
}

/// Histogram entries: `histogram_quantile`, `histogram_quantiles`,
/// `histogram_heatmap`. The bucket columns are projected directly off
/// `_src` (no metric VIEW required) and combined with `h2_combine` if
/// the metric has multiple series. The per-tick delta is taken with
/// `h2_delta + LAG`, then quantile/heatmap math is applied.
fn try_histogram(
    entry: &CatalogueEntry,
    captures: &Captures,
    catalog: &MetricCatalog,
) -> Option<String> {
    enum HistOp {
        Quantile { q: String },                 // single quantile, scalar output column
        Quantiles { qs: Vec<String> },           // multi-quantile, list-indexed
        Heatmap,                                 // UNNEST per bucket
    }
    let (metric, op) = match entry.id.as_str() {
        "histogram_quantile_generic" => {
            let m = ident_capture(captures, "m")?;
            let q = number_capture(captures, "q")?;
            (m, HistOp::Quantile { q: format_lit(q) })
        }
        "histogram_quantile_with_reset" | "histogram_quantile_empty_period" => {
            ("request_latency", HistOp::Quantile { q: "0.5".to_string() })
        }
        "histogram_quantiles_rezolus_ext" => (
            "request_latency",
            HistOp::Quantiles {
                qs: vec!["0.5".to_string(), "0.9".to_string(), "0.99".to_string()],
            },
        ),
        "histogram_heatmap_generic" => {
            let m = ident_capture(captures, "m")?;
            (m, HistOp::Heatmap)
        }
        _ => return None,
    };
    let series = match catalog.series_by_metric.get(metric) {
        Some(s) if !s.is_empty() => s,
        _ => return Some(empty_chain_sql(None)),
    };
    // Pull `p` (grouping_power) from the catalog for the metric. Today
    // we don't store it on `MetricSeries`; fall back to the macro's
    // default or read from the first row at runtime via h2_quantile's
    // implicit-p overload. Simplest: reuse the metric's first
    // physical column and let `h2_quantile(buckets, q)` (no `p` arg)
    // pick up the parquet's stored grouping_power via the macro chain.
    //
    // Actually the catalog's MetricShape::Histogram doesn't carry `p`
    // — it only knows it's a histogram. The long-form path threads `p`
    // through the metric VIEW (`{grouping_power}::INTEGER AS p`). We
    // need the same here. Pull from the catalog by recomputing classify
    // — or, simpler, read the parquet metadata directly. For now,
    // require the catalog to surface `p` via a side channel; see
    // `MetricSeries`.
    let p = catalog.histogram_p_by_metric.get(metric).copied()?;

    // Build `buckets_expr`: for one column, just project it directly;
    // for many, use `h2_combine([c0, c1, ...])`.
    let buckets_expr = if series.len() == 1 {
        let phys = series[0].physical.replace('"', "\"\"");
        format!("\"{phys}\"")
    } else {
        let cols: Vec<String> = series
            .iter()
            .map(|s| {
                let phys = s.physical.replace('"', "\"\"");
                format!("\"{phys}\"")
            })
            .collect();
        format!("h2_combine([{}])", cols.join(", "))
    };

    // Per-tick delta uses LAG over timestamp.
    let delta_expr = format!(
        "h2_delta({buckets_expr}, LAG({buckets_expr}) OVER w)"
    );

    match op {
        HistOp::Quantile { q } => Some(format!(
            "WITH per_tick AS (\n  \
                SELECT timestamp, {delta_expr} AS d FROM _src WINDOW w AS (ORDER BY timestamp)\n\
             )\n\
             SELECT t, v FROM (\n  \
                SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, \
                       CAST(h2_quantile(d, {q}, {p}) AS DOUBLE) AS v\n  \
                FROM per_tick\n)\n\
             WHERE v IS NOT NULL\n\
             ORDER BY t"
        )),
        HistOp::Quantiles { qs } => {
            let qs_lit = format!(
                "[{}]::DOUBLE[]",
                qs.iter().cloned().collect::<Vec<_>>().join(", ")
            );
            // Index 1..N into the list, label per quantile.
            let values: Vec<String> = qs
                .iter()
                .enumerate()
                .map(|(i, q)| format!("({}, '{q}')", i + 1))
                .collect();
            let values_lit = values.join(", ");
            Some(format!(
                "WITH walked AS (\n  \
                    SELECT timestamp, h2_quantiles({delta_expr}, {qs_lit}, {p}) AS qs \n  \
                    FROM _src WINDOW w AS (ORDER BY timestamp)\n\
                 )\n\
                 SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, \
                        CAST(qs[q.idx] AS DOUBLE) AS v, q.label AS quantile\n\
                 FROM walked CROSS JOIN (VALUES {values_lit}) AS q(idx, label)\n\
                 ORDER BY quantile, t"
            ))
        }
        HistOp::Heatmap => Some(format!(
            "WITH per_tick AS (\n  \
                SELECT timestamp, {delta_expr} AS d FROM _src WINDOW w AS (ORDER BY timestamp)\n\
             )\n\
             SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, \
                    (u.ordinal - 1)::INTEGER AS bucket_idx, \
                    u.value::DOUBLE AS count, \
                    {p}::INTEGER AS p\n\
             FROM per_tick CROSS JOIN UNNEST(d) WITH ORDINALITY AS u(value, ordinal)\n\
             WHERE u.value > 0\n\
             ORDER BY t, bucket_idx"
        )),
    }
}

fn format_lit(n: f64) -> String {
    // PromQL number captures are `f64`; render in a form DuckDB parses
    // as DOUBLE without loss. `{n}` uses `Display` which can produce
    // scientific notation; explicit format keeps it stable.
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{n}.0")
    } else {
        format!("{n}")
    }
}

/// Per-series label-matched binary ops: `a OP b`, `a OP ignoring(L) b`,
/// `(a - b) / a` (memory_util_pct). PromQL evaluates these by pairing
/// LHS columns with RHS columns whose labels match (modulo any
/// `ignoring` exemption), then emitting one output series per matched
/// pair. Wide-form does the pairing at SQL-gen time and emits a
/// per-pair UNION-ALL of arithmetic over the raw `_src` columns —
/// one column read per pair, no JOIN, no aggregation.
fn try_pair_match(
    entry: &CatalogueEntry,
    captures: &Captures,
    catalog: &MetricCatalog,
) -> Option<String> {
    enum Op {
        Sub,                       // a - b
        Div,                       // a / b
        OneMinusBOverA,            // (a - b) / a  (= 1 - b/a)
    }
    // Resolve the entry to (a_metric, b_metric, lhs_filter, ignoring_label, op).
    let (a_metric, b_metric, lhs_filter, ig, op) = match entry.id.as_str() {
        "gauge_subtract" => (
            ident_capture(captures, "a")?,
            ident_capture(captures, "b")?,
            Vec::<LabelMatcher>::new(),
            None,
            Op::Sub,
        ),
        "gauge_divide" => (
            ident_capture(captures, "a")?,
            ident_capture(captures, "b")?,
            Vec::<LabelMatcher>::new(),
            None,
            Op::Div,
        ),
        "gauge_ratio_with_labels_ignoring" => (
            ident_capture(captures, "a")?,
            ident_capture(captures, "b")?,
            labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            Some(ident_capture(captures, "ig")?),
            Op::Div,
        ),
        "memory_util_pct" => ("memory_total", "memory_available", Vec::new(), None, Op::OneMinusBOverA),
        _ => return None,
    };
    let lhs_filter = coerce_literal_regex(&lhs_filter)?;

    let (a_series, b_series) = match (
        catalog.series_by_metric.get(a_metric),
        catalog.series_by_metric.get(b_metric),
    ) {
        (Some(a), Some(b)) => (a, b),
        _ => return Some(empty_chain_sql(None)),
    };

    let a_matching: Vec<&MetricSeries> =
        a_series.iter().filter(|s| matches_all(s, &lhs_filter)).collect();

    // Pairing key: drop `ig` (if any) from the labels.
    let pair_key = |s: &MetricSeries| -> std::collections::BTreeMap<String, String> {
        let mut m = s.labels.clone();
        if let Some(name) = ig {
            m.remove(name);
        }
        m
    };
    let mut b_by_key: std::collections::BTreeMap<
        std::collections::BTreeMap<String, String>,
        &MetricSeries,
    > = Default::default();
    for s in b_series {
        b_by_key.insert(pair_key(s), s);
    }

    if a_matching.is_empty() {
        return Some(empty_chain_sql(None));
    }

    // Union of LHS label keys → constant projection per branch (PromQL
    // "left labels on output" rule).
    let mut all_lhs_labels: std::collections::BTreeSet<&str> = Default::default();
    for s in &a_matching {
        for k in s.labels.keys() {
            all_lhs_labels.insert(k.as_str());
        }
    }

    let mut branches: Vec<String> = Vec::new();
    for s_a in &a_matching {
        let key = pair_key(s_a);
        let Some(s_b) = b_by_key.get(&key) else {
            continue; // unpaired LHS: PromQL drops it on the floor.
        };
        let phys_a = s_a.physical.replace('"', "\"\"");
        let phys_b = s_b.physical.replace('"', "\"\"");
        let mut label_proj = String::new();
        for k in &all_lhs_labels {
            let v = s_a.labels.get(*k).cloned().unwrap_or_default().replace('\'', "''");
            label_proj.push_str(&format!(", '{v}' AS {}", quote_ident(k)));
        }
        // Reset-aware not relevant here (gauges); use a plain CAST and
        // operator. `NULLIF`s match PromQL's div-by-zero semantics
        // (returns NaN/dropped — we use NULL).
        let v_expr = match op {
            Op::Sub => format!(
                "CAST(\"{phys_a}\" AS DOUBLE) - CAST(\"{phys_b}\" AS DOUBLE)"
            ),
            Op::Div => format!(
                "CAST(\"{phys_a}\" AS DOUBLE) / NULLIF(CAST(\"{phys_b}\" AS DOUBLE), 0)"
            ),
            // `(a - b) / a` (= 1 - b/a) — PromQL's memory_util_pct is
            // `(memory_total - memory_available) / memory_total`. Note
            // `a_metric` is the `total` (denominator), `b_metric` is the
            // `available` (numerator term that gets subtracted). The
            // output: `(a - b) / a`.
            Op::OneMinusBOverA => format!(
                "(CAST(\"{phys_a}\" AS DOUBLE) - CAST(\"{phys_b}\" AS DOUBLE)) \
                 / NULLIF(CAST(\"{phys_a}\" AS DOUBLE), 0)"
            ),
        };
        branches.push(format!(
            "SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, {v_expr} AS v{label_proj} \
             FROM _src WHERE \"{phys_a}\" IS NOT NULL AND \"{phys_b}\" IS NOT NULL"
        ));
    }
    if branches.is_empty() {
        return Some(empty_chain_sql(None));
    }
    Some(branches.join("\nUNION ALL\n"))
}

/// N-ary chain dispatcher. Handles entries whose PromQL is a left-fold
/// of `op` applied to N `sum(irate(M_i))` lanes, optionally divided by
/// a tail constant or by `cpu_cores`. Each lane is built via
/// `build_lane` (same machinery as `generate_binary`'s lanes).
///
/// The chain entries today are all hardcoded-metric Rezolus shapes —
/// CPU effective frequency (aperf chain), instructions-per-ns, plus
/// the `gauge_a_over_a_plus_b` ternary. Adding a new chain is O(one
/// match arm).
fn try_chain(
    entry: &CatalogueEntry,
    captures: &Captures,
    catalog: &MetricCatalog,
) -> Option<String> {
    use Aggregation::Sum;
    use PerColExpr::{IrateRate, Value};
    let bare_irate_sum = |metric: &'static str, group: Option<&'static str>| Shape {
        metric,
        filter: Vec::new(),
        group_label: group,
        expr: IrateRate,
        aggregation: Sum,
        scale: 1.0,
        cpu_cores_div: false,
    };

    match entry.id.as_str() {
        // sum(tsc) * sum(aperf) / sum(mperf) / cpu_cores
        "rezolus_cpu_aperf_chain_total" => generate_chain_with_cpu_cores(
            catalog,
            &[
                ("a", bare_irate_sum("cpu_tsc", None)),
                ("b", bare_irate_sum("cpu_aperf", None)),
                ("c", bare_irate_sum("cpu_mperf", None)),
            ],
            "(a.v * b.v / NULLIF(c.v, 0)) / NULLIF(cores.v, 0)",
            None,
            true,
        ),
        // sum by (id) (tsc) * sum by (id) (aperf) / sum by (id) (mperf)  [no cpu_cores]
        "rezolus_cpu_aperf_chain_per_id" => generate_chain_with_cpu_cores(
            catalog,
            &[
                ("a", bare_irate_sum("cpu_tsc", Some("id"))),
                ("b", bare_irate_sum("cpu_aperf", Some("id"))),
                ("c", bare_irate_sum("cpu_mperf", Some("id"))),
            ],
            "a.v * b.v / NULLIF(c.v, 0)",
            Some("id"),
            false,
        ),
        // sum(inst)/sum(cyc) * sum(tsc) * sum(aperf) / sum(mperf) / 1e9 / cpu_cores
        "rezolus_cpu_ipns" => generate_chain_with_cpu_cores(
            catalog,
            &[
                ("a", bare_irate_sum("cpu_instructions", None)),
                ("b", bare_irate_sum("cpu_cycles", None)),
                ("c", bare_irate_sum("cpu_tsc", None)),
                ("d", bare_irate_sum("cpu_aperf", None)),
                ("e", bare_irate_sum("cpu_mperf", None)),
            ],
            // Left-to-right: (((a/b)*c)*d) / e / 1e9 / cores.
            "(a.v / NULLIF(b.v, 0) * c.v * d.v / NULLIF(e.v, 0)) / 1e9 / NULLIF(cores.v, 0)",
            None,
            true,
        ),
        // Same chain, per-id, no cpu_cores tail.
        "rezolus_cpu_ipns_per_id" => generate_chain_with_cpu_cores(
            catalog,
            &[
                ("a", bare_irate_sum("cpu_instructions", Some("id"))),
                ("b", bare_irate_sum("cpu_cycles", Some("id"))),
                ("c", bare_irate_sum("cpu_tsc", Some("id"))),
                ("d", bare_irate_sum("cpu_aperf", Some("id"))),
                ("e", bare_irate_sum("cpu_mperf", Some("id"))),
            ],
            "(a.v / NULLIF(b.v, 0) * c.v * d.v / NULLIF(e.v, 0)) / 1e9",
            Some("id"),
            false,
        ),
        // sum(a{la}) / (sum(b{lb}) + sum(c{lc})) — three gauge lanes.
        "gauge_a_over_a_plus_b" => {
            let a_metric = ident_capture(captures, "a")?;
            let b_metric = ident_capture(captures, "b")?;
            let c_metric = ident_capture(captures, "c")?;
            let la = labels_capture(captures, "la").unwrap_or(&[]).to_vec();
            let lb = labels_capture(captures, "lb").unwrap_or(&[]).to_vec();
            let lc = labels_capture(captures, "lc").unwrap_or(&[]).to_vec();
            let mk_lane = |metric, filter| Shape {
                metric,
                filter,
                group_label: None,
                expr: Value,
                aggregation: Sum,
                scale: 1.0,
                cpu_cores_div: false,
            };
            generate_chain_with_cpu_cores(
                catalog,
                &[
                    ("a", mk_lane(a_metric, la)),
                    ("b", mk_lane(b_metric, lb)),
                    ("c", mk_lane(c_metric, lc)),
                ],
                "a.v / NULLIF(b.v + c.v, 0)",
                None,
                false,
            )
        }
        _ => None,
    }
}

/// Build a chain SQL: N CTE-pairs (one per lane), inner-joined on
/// `timestamp` (and `group_label` when shared), then a final SELECT
/// applying `expr_template` (which references `a.v`, `b.v`, ...).
///
/// When `cpu_cores_div` is true, an extra `cores` lane is added that
/// projects the cpu_cores gauge as a single-row CTE per timestamp; the
/// expression template is expected to reference `NULLIF(cores.v, 0)`.
fn generate_chain_with_cpu_cores(
    catalog: &MetricCatalog,
    lanes_in: &[(&str, Shape)],
    expr_template: &str,
    group_label: Option<&str>,
    cpu_cores_div: bool,
) -> Option<String> {
    // Build all lanes. If any returns None (regex matchers, too many
    // cols, etc.) bail out; long-form fallback handles it.
    let mut lanes: Vec<(String, Lane)> = Vec::with_capacity(lanes_in.len());
    for (prefix, shape) in lanes_in {
        let lane = build_lane(catalog, shape, prefix)?;
        // Empty lane => empty result. PromQL semantics for any binary
        // op on empty is empty, so short-circuit.
        if lane.matching_count == 0 {
            return Some(empty_chain_sql(group_label));
        }
        lanes.push(((*prefix).to_string(), lane));
    }
    // cpu_cores lane: single-column gauge.
    let cores_cte = if cpu_cores_div {
        let series = match catalog.series_by_metric.get("cpu_cores") {
            Some(s) if s.len() == 1 => s,
            // Missing or multi-column cpu_cores: empty result.
            _ => return Some(empty_chain_sql(group_label)),
        };
        let phys = series[0].physical.replace('"', "\"\"");
        Some(format!(
            "cores AS (\n  SELECT timestamp, CAST(\"{phys}\" AS DOUBLE) AS v FROM _src\n)"
        ))
    } else {
        None
    };

    // Join clauses: first lane is `FROM a_summed a`, every subsequent
    // lane is `JOIN b_summed b ON a.timestamp = b.timestamp [AND a.g = b.g]`.
    let (first_prefix, _) = &lanes[0];
    let mut from_clause = format!("{first_prefix}_summed {first_prefix}", first_prefix = first_prefix);
    for (prefix, _) in lanes.iter().skip(1) {
        let on = match group_label {
            Some(g) => format!(
                "ON {first_prefix}.timestamp = {prefix}.timestamp AND {first_prefix}.{g} = {prefix}.{g}",
                g = quote_ident(g)
            ),
            None => format!("ON {first_prefix}.timestamp = {prefix}.timestamp"),
        };
        from_clause.push_str(&format!(" JOIN {prefix}_summed {prefix} {on}"));
    }
    if cores_cte.is_some() {
        // cpu_cores has no group_label and is constant per timestamp;
        // join on timestamp only.
        from_clause.push_str(&format!(
            " JOIN cores cores ON {first_prefix}.timestamp = cores.timestamp"
        ));
    }

    let group_select = match group_label {
        Some(g) => format!(", {first_prefix}.{g}", g = quote_ident(g)),
        None => String::new(),
    };

    // Concatenate all lane CTEs.
    let mut all_ctes = String::new();
    for (i, (_, lane)) in lanes.iter().enumerate() {
        if i > 0 {
            all_ctes.push_str(",\n");
        }
        all_ctes.push_str(&lane.ctes);
    }
    if let Some(c) = cores_cte {
        all_ctes.push_str(",\n");
        all_ctes.push_str(&c);
    }

    Some(format!(
        "WITH\n{all_ctes}\n\
         SELECT CAST({first_prefix}.timestamp AS DOUBLE) / 1e9 AS t, ({expr_template}) AS v{group_select}\n\
         FROM {from_clause}"
    ))
}

fn empty_chain_sql(group_label: Option<&str>) -> String {
    if let Some(g) = group_label {
        let g = quote_ident(g);
        format!(
            "SELECT CAST(NULL AS DOUBLE) AS t, CAST(NULL AS DOUBLE) AS v, CAST(NULL AS VARCHAR) AS {g} WHERE FALSE"
        )
    } else {
        "SELECT CAST(NULL AS DOUBLE) AS t, CAST(NULL AS DOUBLE) AS v WHERE FALSE".to_string()
    }
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
    let series = match catalog.series_by_metric.get(shape.metric) {
        Some(s) => s,
        None => {
            // Missing metric: empty lane. Caller short-circuits to
            // `empty_binary_sql` / `empty_chain_sql`.
            return Some(Lane {
                ctes: String::new(),
                matching_count: 0,
            });
        }
    };
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

    // build_lane is currently invoked only by binary entries; cpu_cores
    // broadcast doesn't compose with binary today (no entry uses both).
    // Refuse to support it here so a misuse is caught immediately.
    if shape.cpu_cores_div {
        return None;
    }
    let (rates_chain, rates_name) = build_rates_chain(&matching, shape.expr, prefix, "");
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
        // UNNEST fold for many-group binary lanes (mirrors the unary
        // generate_sum optimization). Above ~4 groups the single-pass
        // UNNEST plan beats the N-way UNION-ALL.
        if groups.len() > 4 {
            let mut struct_elems: Vec<String> = Vec::with_capacity(groups.len());
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
                let v_expr = format!(
                    "CASE WHEN {any_present} THEN ({sum_expr}){scale_expr} END"
                );
                let g_lit = g_value.replace('\'', "''");
                struct_elems.push(format!(
                    "{{'g': '{g_lit}', 'v': {v_expr}}}"
                ));
            }
            let group_col = match shape.group_label {
                Some(g) => format!(", u.g AS {}", quote_ident(g)),
                None => String::new(),
            };
            let elems_lit = struct_elems.join(",\n        ");
            format!(
                "{prefix}_summed AS (\n  \
                    SELECT {rates_name}.timestamp, u.v AS v{group_col}\n  \
                    FROM {rates_name},\n  \
                    UNNEST([\n        {elems_lit}\n     ]) AS u(g, v)\n  \
                    WHERE u.v IS NOT NULL\n)"
            )
        } else {
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
                    "SELECT timestamp, ({sum_expr}){scale_expr} AS v{g_select} FROM {rates_name} WHERE {any_present}"
                ));
            }
            let unions = union_branches.join("\n      UNION ALL\n      ");
            format!(
                "{prefix}_summed AS (\n      {unions}\n)"
            )
        }
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
                "{p}SELECT timestamp, val_{i} AS v{label_proj} FROM {rates_name} WHERE val_{i} IS NOT NULL"
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
        "{rates_chain},\n\
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
        // ---- bare irate shapes (one row per series-sample) ----
        // irate(requests[5m]) — synthetic counter_basic fixture.
        "counter_irate_basic" | "counter_irate_reset" => Some(Shape {
            metric: "requests",
            filter: Vec::new(),
            group_label: None,
            expr: IrateRate,
            aggregation: Aggregation::None,
            scale: 1.0,
            cpu_cores_div: false,
        }),

        // ---- counter / irate shapes ----
        // sum by (id) (irate(softirq{kind=K}[5m]))
        "softirq_irate_by_id_by_kind" => Some(Shape {
            metric: "softirq",
            filter: single_eq_filter("kind", string_capture(captures, "k")?),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum(irate(softirq_time{kind=K}[5m])) / cores / 1e9 — `cores`
        // here is a numeric capture, not the cpu_cores metric.
        "softirq_time_pct_by_kind" => Some(Shape {
            metric: "softirq_time",
            filter: single_eq_filter("kind", string_capture(captures, "k")?),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "cores").unwrap_or(1.0) * 1_000_000_000.0,
            cpu_cores_div: false,
        }),
        // sum by (id) (irate(softirq_time{kind=K}[5m])) / 1e9
        "softirq_time_pct_by_id_by_kind" => Some(Shape {
            metric: "softirq_time",
            filter: single_eq_filter("kind", string_capture(captures, "k")?),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1_000_000_000.0,
            cpu_cores_div: false,
        }),
        // sum (irate(softirq{kind=K}[5m]))
        "softirq_irate_total_by_kind" => Some(Shape {
            metric: "softirq",
            filter: single_eq_filter("kind", string_capture(captures, "k")?),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum (irate(M{labels}[R]))
        "counter_irate_sum_with_labels" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum by (id) (irate(M{labels}[R]))
        "counter_irate_by_id_with_labels" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum (irate(M[R])) / D
        "counter_irate_total_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: false,
        }),
        // sum by (id) (irate(M[R])) / D
        "counter_irate_by_id_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: false,
        }),
        // sum by (id) (irate(M{labels}[R])) / D
        "counter_irate_by_id_with_labels_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: false,
        }),
        // sum by (G) (irate(M{labels}[R])) — generic group label
        "counter_irate_by_g_with_labels" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: Some(ident_capture(captures, "g")?),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum by (G) (irate(M{labels}[R])) / D — generic group label, scaled
        "counter_irate_by_g_with_labels_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: Some(ident_capture(captures, "g")?),
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: false,
        }),
        // sum (irate(M{labels}[R])) / D — note the entry id is "counter_irate_with_labels_scaled"
        "counter_irate_with_labels_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: false,
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
            cpu_cores_div: false,
        }),
        // M (catalogue entry "gauge_bare")
        "gauge_bare" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: Value,
            aggregation: Aggregation::None,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum(M)
        "gauge_sum_bare" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: Value,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum(M{labels})
        "gauge_sum_with_labels" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: None,
            expr: Value,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum by (g) (M)
        "gauge_sum_by_g_bare" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: Some(ident_capture(captures, "g")?),
            expr: Value,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum by (g) (M{labels})
        "gauge_sum_by_g_with_labels" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: Some(ident_capture(captures, "g")?),
            expr: Value,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum by (g) (M) / D
        "gauge_sum_by_g_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: Some(ident_capture(captures, "g")?),
            expr: Value,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: false,
        }),
        // sum by (g) (M{labels}) / D
        "gauge_sum_by_g_with_labels_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: Some(ident_capture(captures, "g")?),
            expr: Value,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: false,
        }),
        // max(M)
        "gauge_max_bare" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: Value,
            aggregation: Max,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // avg(M)
        "gauge_avg_bare" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: Value,
            aggregation: Avg,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // avg(M) / D
        "gauge_avg_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: Value,
            aggregation: Avg,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: false,
        }),
        // sum(M) / D
        "gauge_sum_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: Value,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: false,
        }),
        // temperature_offset (gauge_negative — hardcoded fixture metric)
        "gauge_negative" => Some(Shape {
            metric: "temperature_offset",
            filter: Vec::new(),
            group_label: None,
            expr: Value,
            aggregation: Aggregation::None,
            scale: 1.0,
            cpu_cores_div: false,
        }),

        // ---- Hardcoded counter shapes (synthetic-fixture variants) ----
        // sum by (id) (irate(cpu_usage[5m])) — counter_multi_label fixture.
        "counter_sum_by_id" => Some(Shape {
            metric: "cpu_usage",
            filter: Vec::new(),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum by (id) (irate(cpu_usage{state="user"}[5m])).
        "counter_sum_by_id_filtered" => Some(Shape {
            metric: "cpu_usage",
            filter: single_eq_filter("state", "user"),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum(irate(cpu_usage[5m])).
        "counter_total_sum" => Some(Shape {
            metric: "cpu_usage",
            filter: Vec::new(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum(irate(${m}[5m])) — generic metric, hardcoded 5m range.
        "counter_total_sum_generic" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum by (id) (irate(${m}[${r}])) — generic metric, no filter.
        "counter_irate_by_id_generic" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum(irate(M{labels}[r])) * D — multiplicative scale (= scale by 1/d).
        "counter_irate_total_mul" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: 1.0 / number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: false,
        }),

        // ---- cpu_cores-broadcast (per-cpu-core %) shapes ----
        // sum(irate(M[r])) / cpu_cores / D
        "counter_irate_total_per_cpu_core_pct" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: true,
        }),
        // sum(irate(M{labels}[r])) / cpu_cores / D
        "counter_irate_with_labels_per_cpu_core_pct" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: labels_capture(captures, "labels").unwrap_or(&[]).to_vec(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: true,
        }),

        // ---- Rezolus hardcoded-metric variants ----
        // sum(irate(rezolus_bpf_run_time[5m])) / 1e9
        "rezolus_bpf_run_time_sec" => Some(Shape {
            metric: "rezolus_bpf_run_time",
            filter: Vec::new(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: 1_000_000_000.0,
            cpu_cores_div: false,
        }),
        // sum by (sampler) (irate(rezolus_bpf_run_time[5m])) / 1e9
        "rezolus_bpf_run_time_sec_per_sampler" => Some(Shape {
            metric: "rezolus_bpf_run_time",
            filter: Vec::new(),
            group_label: Some("sampler"),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1_000_000_000.0,
            cpu_cores_div: false,
        }),
        // sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1e9
        "rezolus_cpu_user_per_id" => Some(Shape {
            metric: "cpu_usage",
            filter: single_eq_filter("state", "user"),
            group_label: Some("id"),
            expr: IrateRate,
            aggregation: Sum,
            scale: 1_000_000_000.0,
            cpu_cores_div: false,
        }),
        // sum(irate(cpu_usage[5m])) / 2 / 1e9 — combined scale = 2e9.
        "rezolus_cpu_busy_pct" => Some(Shape {
            metric: "cpu_usage",
            filter: Vec::new(),
            group_label: None,
            expr: IrateRate,
            aggregation: Sum,
            scale: 2_000_000_000.0,
            cpu_cores_div: false,
        }),

        // ---- Rate (windowed-average) variants ----
        // rate(requests[5m]) — synthetic counter_basic / counter_reset.
        "counter_rate_basic" | "counter_rate_reset" => Some(Shape {
            metric: "requests",
            filter: Vec::new(),
            group_label: None,
            expr: PerColExpr::Rate { range_secs: 300 },
            aggregation: Aggregation::None,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // rate(${m}[${r}]) — bare rate, generic.
        "counter_rate_bare_generic" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: PerColExpr::Rate {
                range_secs: duration_capture(captures, "r").unwrap_or(300),
            },
            aggregation: Aggregation::None,
            scale: 1.0,
            cpu_cores_div: false,
        }),
        // sum(rate(${m}[5m])) / ${d}
        "counter_rate_sum_scaled" => Some(Shape {
            metric: ident_capture(captures, "m")?,
            filter: Vec::new(),
            group_label: None,
            expr: PerColExpr::Rate { range_secs: 300 },
            aggregation: Sum,
            scale: number_capture(captures, "d").unwrap_or(1.0),
            cpu_cores_div: false,
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

/// Build the wide-form SQL for `shape`. Returns `None` only when the
/// shape uses a feature the generator can't handle (real regex
/// matchers, too many columns); for missing metrics or filter
/// mismatches it returns `Some(empty_matrix_sql)` so the caller can
/// route every recognised-id query through the wide path uniformly.
fn generate(catalog: &MetricCatalog, shape: &Shape) -> Option<String> {
    let filter = coerce_literal_regex(&shape.filter)?;
    let series = match catalog.series_by_metric.get(shape.metric) {
        Some(s) => s,
        None => return Some(empty_matrix_sql(shape)),
    };
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

    // cpu_cores broadcast: project the gauge into the rates CTE so
    // the per-row `/ cores` slot has direct access. If the fixture
    // doesn't carry cpu_cores, treat the result as empty (PromQL
    // `expr / missing_metric` is empty).
    let extra_cols = match cpu_cores_extra(catalog, shape.cpu_cores_div) {
        Some(s) => s,
        None => return Some(empty_matrix_sql(shape)),
    };

    // Per-column expression. For irate/rate we need a WINDOW for LAG;
    // for rate we also get a second CTE wrapping the windowed average.
    let (rates_chain, rates_name) = build_rates_chain(&matching, shape.expr, "", &extra_cols);

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
        return Some(generate_sum(&matching, shape, &rates_chain, &rates_name, &scale_expr));
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
            "{prefix}SELECT timestamp, val_{i} AS v{label_proj} FROM {rates_name} WHERE val_{i} IS NOT NULL"
        ));
    }

    let value_expr = match shape.aggregation {
        Aggregation::None => format!("v{scale_expr}"),
        Aggregation::Max => format!("MAX(v){scale_expr}"),
        Aggregation::Avg => format!("AVG(v){scale_expr}"),
        Aggregation::Sum => unreachable!("handled above"),
    };
    let (group_clause, group_select) = match (shape.aggregation, shape.group_label) {
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
            (String::new(), labels_sql)
        }
        (_, Some(g)) => {
            let g = quote_ident(g);
            (format!("GROUP BY {g}, timestamp"), format!(", {g}"))
        }
        (_, None) => ("GROUP BY timestamp".to_string(), String::new()),
    };

    // ORDER BY only when the path uses HASH GROUP BY (Avg/Max — those
    // operators don't preserve scan order). The Aggregation::None
    // branch projects directly off `rates`, which has natural
    // timestamp order from the parquet scan, so no ORDER BY needed.
    let order_clause = if group_clause.is_empty() {
        ""
    } else {
        " ORDER BY t"
    };
    Some(format!(
        "WITH {rates_chain}\n\
         SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, {value_expr} AS v{group_select}\n\
         FROM (\n      {unions}\n) per_series\n\
         {group_clause}{order_clause}"
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
    rates_chain: &str,
    rates_name: &str,
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

    // cpu_cores broadcast slots in *between* the sum and the trailing
    // scale: `(sum_of_irate / NULLIF(cores, 0)) / scale`. Match the
    // long-form's evaluation order so rounding matches PromQL.
    let cores_div = if shape.cpu_cores_div {
        " / NULLIF(cores, 0)"
    } else {
        ""
    };

    // For shapes with many groups (e.g. sum-by-id over 32 CPUs), the
    // N-way UNION-ALL plan inflates DuckDB's planning + execution
    // overhead (each branch reads the rates relation independently).
    // Fold into a single SELECT × UNNEST: build one struct-list per
    // row whose elements are the per-group `{id, v}` tuples, then
    // unnest. Result: one rates pass instead of N.
    //
    // Threshold: above ~4 groups the UNNEST form wins. Below that the
    // UNION-ALL form has slightly less per-row overhead since each
    // branch can short-circuit on a `WHERE val_i IS NOT NULL`
    // predicate. Empirical knee from the bench.
    let unnest_threshold = 4;
    if groups.len() > unnest_threshold {
        let mut struct_elems: Vec<String> = Vec::with_capacity(groups.len());
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
            // Use NULL when no underlying col is non-null; the outer
            // WHERE filters those out. Same semantics as the per-branch
            // `WHERE any_present`.
            let v_expr = format!(
                "CASE WHEN {any_present} THEN (({sum_expr}){cores_div}){scale_expr} END"
            );
            let g_lit = g_value.replace('\'', "''");
            struct_elems.push(format!(
                "{{'g': '{g_lit}', 'v': {v_expr}}}"
            ));
        }
        let group_col = match shape.group_label {
            Some(g) => format!(", u.g AS {}", quote_ident(g)),
            // Even with no group_label the struct still carries 'g' as
            // empty string; project it as a no-op.
            None => String::new(),
        };
        let elems_lit = struct_elems.join(",\n        ");
        return format!(
            "WITH {rates_chain}\n\
             SELECT CAST({rates_name}.timestamp AS DOUBLE) / 1e9 AS t, u.v AS v{group_col}\n\
             FROM {rates_name},\n\
             UNNEST([\n        {elems_lit}\n     ]) AS u(g, v)\n\
             WHERE u.v IS NOT NULL"
        );
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
            "SELECT CAST(timestamp AS DOUBLE) / 1e9 AS t, (({sum_expr}){cores_div}){scale_expr} AS v{g_select} FROM {rates_name} WHERE {any_present}"
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
        "WITH {rates_chain}\n\
         {unions}"
    )
}

/// Emit the prefix CTE chain that lands every per-column rate (or
/// gauge value) in a `<rates_name>` relation with columns
/// `(timestamp, val_0, val_1, ...)`. Returns `(cte_chain, rates_name)`.
/// The CTE chain is intended for direct interpolation after `WITH `
/// (no leading `WITH`, no trailing comma).
///
/// Naming: `prefix=""` produces `rates`; `prefix="a"` produces
/// `a_rates`. Binary lanes use distinct prefixes so the two lanes
/// don't collide in the same `WITH`.
///
/// IrateRate uses the `irate_lag` UDF (in `metriken_query_sql::udf`):
/// SQL still does the LAG bookkeeping inside the WINDOW operator, but
/// the per-row CASE+arithmetic that PromQL irate semantics require
/// runs inside one tight Rust loop per chunk. Measured ~2.5x faster
/// than the equivalent inline `CASE WHEN LAG IS NULL ... WHEN curr >=
/// prev ... ELSE ... END` SQL on a 16-col workload — DuckDB's
/// vectorized expression evaluator's per-cell overhead dominated the
/// CASE form even after CSE.
///
/// Rate emits two CTEs: a per-pair reset-aware increment, then a
/// trailing `ROWS BETWEEN N PRECEDING AND CURRENT ROW` window doing
/// the `(SUM - FIRST_VALUE) / (curr_ts - first_ts)` formula.
fn build_rates_chain(
    matching: &[&MetricSeries],
    expr: PerColExpr,
    prefix: &str,
    extra_cols: &str,
) -> (String, String) {
    let rates_name = if prefix.is_empty() {
        "rates".to_string()
    } else {
        format!("{prefix}_rates")
    };
    // `extra_cols` (e.g. `, "cpu_cores_phys" AS cores`) is appended to
    // the rates CTE's projection so callers can carry per-row scalar
    // divisors (cpu_cores) or other ancillary columns into the final
    // SELECT without a separate join. The string must already include
    // a leading comma — empty when no extras.
    match expr {
        PerColExpr::Value => {
            let mut per_col = String::new();
            for (i, s) in matching.iter().enumerate() {
                let phys = s.physical.replace('"', "\"\"");
                per_col.push_str(&format!(",\n    CAST(\"{phys}\" AS DOUBLE) AS val_{i}"));
            }
            let cte = format!(
                "{rates_name} AS (\n  SELECT timestamp{per_col}{extra_cols}\n  FROM _src\n)"
            );
            (cte, rates_name)
        }
        PerColExpr::IrateRate => {
            let mut per_col = String::new();
            for (i, s) in matching.iter().enumerate() {
                let phys = s.physical.replace('"', "\"\"");
                per_col.push_str(&format!(
                    ",\n    irate_lag(\"{phys}\", LAG(\"{phys}\") OVER w, timestamp - LAG(timestamp) OVER w) AS val_{i}"
                ));
            }
            let cte = format!(
                "{rates_name} AS (\n  SELECT timestamp{per_col}{extra_cols}\n  FROM _src\n  WINDOW w AS (ORDER BY timestamp)\n)"
            );
            (cte, rates_name)
        }
        PerColExpr::Rate { range_secs } => {
            let per_pair_name = if prefix.is_empty() {
                "per_pair".to_string()
            } else {
                format!("{prefix}_per_pair")
            };
            let mut per_pair_cols = String::new();
            let mut rates_cols = String::new();
            for (i, s) in matching.iter().enumerate() {
                let phys = s.physical.replace('"', "\"\"");
                per_pair_cols.push_str(&format!(
                    ",\n    irate_lag(\"{phys}\", LAG(\"{phys}\") OVER w, timestamp - LAG(timestamp) OVER w) AS inc_{i}"
                ));
                // Match counter_rate_bare_generic's long-form formula:
                // `(SUM(inc) OVER w - COALESCE(FIRST_VALUE(inc) OVER w, 0))
                //  / NULLIF((curr_ts - first_ts_in_window)/1e9, 0)`.
                // Subtracting FIRST_VALUE drops the pre-window pair (whose
                // `prev` sample sits one step before the [t-W, t] window);
                // COALESCE handles the early-data case where inc[0] is NULL.
                rates_cols.push_str(&format!(
                    ",\n    (SUM(inc_{i}) OVER w - COALESCE(FIRST_VALUE(inc_{i}) OVER w, 0)) \
                     / NULLIF(CAST(timestamp - FIRST_VALUE(timestamp) OVER w AS DOUBLE) / 1e9, 0) AS val_{i}"
                ));
            }
            // For Rate, extras carry through both CTEs unchanged: the
            // per_pair CTE projects them off `_src`, the rates CTE
            // forwards them via column reference.
            let extra_forward: String = extra_cols
                .split(", ")
                .filter(|s| !s.is_empty())
                .map(|frag| {
                    // Each fragment is "<expr> AS <name>"; forward by name.
                    if let Some(idx) = frag.rfind(" AS ") {
                        format!(",\n    {}", &frag[idx + 4..])
                    } else {
                        format!(",\n    {frag}")
                    }
                })
                .collect();
            let cte = format!(
                "{per_pair_name} AS (\n  SELECT timestamp{per_pair_cols}{extra_cols}\n  FROM _src\n  WINDOW w AS (ORDER BY timestamp)\n),\n\
                 {rates_name} AS (\n  SELECT timestamp{rates_cols}{extra_forward}\n  FROM {per_pair_name}\n  WINDOW w AS (ORDER BY timestamp ROWS BETWEEN {range_secs} PRECEDING AND CURRENT ROW)\n)"
            );
            (cte, rates_name)
        }
    }
}

/// Build the `extra_cols` fragment for `cpu_cores_div`. Returns `None`
/// if the catalog doesn't carry `cpu_cores` (caller falls back), or
/// if the metric has more than one column (we'd need to pick one and
/// don't have a defined rule). Returns `Some("")` (empty) when
/// `cpu_cores_div` is false — caller passes through.
fn cpu_cores_extra(catalog: &MetricCatalog, cpu_cores_div: bool) -> Option<String> {
    if !cpu_cores_div {
        return Some(String::new());
    }
    let series = catalog.series_by_metric.get("cpu_cores")?;
    if series.len() != 1 {
        return None;
    }
    let phys = series[0].physical.replace('"', "\"\"");
    Some(format!(",\n    CAST(\"{phys}\" AS DOUBLE) AS cores"))
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

fn duration_capture(captures: &Captures, name: &str) -> Option<u64> {
    match captures.get(name)? {
        CaptureValue::Duration { seconds } => Some(*seconds),
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
