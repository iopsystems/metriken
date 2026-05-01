use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::sync::Arc;

use promql_parser::label::Matcher;
use promql_parser::parser::token::TokenType;
use promql_parser::parser::{self, Expr};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tsdb::{Labels, Tsdb};

#[cfg(feature = "http")]
mod api;

pub mod streaming;

#[cfg(test)]
mod tests;

#[cfg(feature = "http")]
pub use api::routes;

#[derive(Error, Debug)]
pub enum QueryError {
    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Evaluation error: {0}")]
    EvaluationError(String),

    #[error("Unsupported operation: {0}")]
    Unsupported(String),

    #[error("Metric not found: {0}")]
    MetricNotFound(String),
}

/// A single sample in the result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub metric: HashMap<String, String>,
    pub value: (f64, f64), // (timestamp_seconds, value)
}

/// A matrix sample with multiple values over time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixSample {
    pub metric: HashMap<String, String>,
    pub values: Vec<(f64, f64)>, // Vec of (timestamp_seconds, value)
}

/// Histogram heatmap data for visualization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistogramHeatmapResult {
    /// Timestamps in seconds
    pub timestamps: Vec<f64>,
    /// Bucket boundaries (latency values in the histogram's unit, e.g.,
    /// nanoseconds)
    pub bucket_bounds: Vec<u64>,
    /// Heatmap data as [time_index, bucket_index, count]
    pub data: Vec<(usize, usize, f64)>,
    /// Minimum count value (for color scaling)
    pub min_value: f64,
    /// Maximum count value (for color scaling)
    pub max_value: f64,
}

/// Result of a PromQL query
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resultType", rename_all = "camelCase")]
pub enum QueryResult {
    #[serde(rename = "vector")]
    Vector { result: Vec<Sample> },

    #[serde(rename = "matrix")]
    Matrix { result: Vec<MatrixSample> },

    #[serde(rename = "scalar")]
    Scalar { result: (f64, f64) }, // (timestamp, value)

    #[serde(rename = "histogram_heatmap")]
    HistogramHeatmap { result: HistogramHeatmapResult },
}

/// The PromQL query engine
///
/// Generic over the TSDB handle type. The default (`Arc<Tsdb>`) covers the
/// common owned case (MCP, tests, file-backed viewer). Pass `&Tsdb` (or any
/// other `Deref<Target = Tsdb>`) for zero-copy borrowed access.
pub struct QueryEngine<T: Deref<Target = Tsdb> = Arc<Tsdb>> {
    tsdb: T,
}

/// Try to parse an optional stride (in seconds) from the trailing argument.
/// Input like `"metric, 15"` returns `("metric", Some(15_000_000_000))`.
/// Input like `"metric"` returns `("metric", None)`.
fn parse_optional_stride(s: &str) -> Result<(&str, Option<u64>), QueryError> {
    let (before, after) = split_last_top_level_comma(s);
    if let Some(tail) = after {
        if let Ok(secs) = tail.parse::<f64>() {
            if secs <= 0.0 {
                return Err(QueryError::ParseError(
                    "stride must be a positive number of seconds".to_string(),
                ));
            }
            return Ok((before.trim(), Some((secs * 1_000_000_000.0) as u64)));
        }
    }
    Ok((s, None))
}

/// Split a string at the last comma that is not inside `{}` braces.
/// Returns `(before, Some(after))` if found, or `(whole_string, None)`.
fn split_last_top_level_comma(s: &str) -> (&str, Option<&str>) {
    let mut brace_depth = 0i32;
    let mut last_comma = None;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' => brace_depth += 1,
            '}' => brace_depth -= 1,
            ',' if brace_depth == 0 => last_comma = Some(i),
            _ => {}
        }
    }
    match last_comma {
        Some(i) => (&s[..i], Some(s[i + 1..].trim())),
        None => (s, None),
    }
}

/// Collapse a matrix result into a vector by taking the latest
/// point of each series. Used by `query()` to convert a degenerate
/// range query (`start = end = time`) into instant-query shape.
/// Non-matrix results pass through unchanged.
fn matrix_to_vector(result: QueryResult) -> QueryResult {
    let QueryResult::Matrix { result: samples } = result else {
        return result;
    };
    let vector: Vec<Sample> = samples
        .into_iter()
        .filter_map(|s| {
            s.values.last().copied().map(|value| Sample {
                metric: s.metric,
                value,
            })
        })
        .collect();
    QueryResult::Vector { result: vector }
}

/// Extract label filter from parsed PromQL matchers, skipping `__name__`.
pub(crate) fn extract_filter_labels(matchers: &[Matcher]) -> Labels {
    let mut filter_labels = Labels::default();
    for matcher in matchers {
        if matcher.name == "__name__" {
            continue;
        }
        let op = matcher.op.to_string();
        if op == "=" {
            filter_labels
                .inner
                .insert(matcher.name.clone(), matcher.value.clone());
        } else if op == "=~" {
            filter_labels
                .inner
                .insert(matcher.name.clone(), format!("~{}", matcher.value));
        } else if op == "!=" || op == "!~" {
            filter_labels
                .inner
                .insert(matcher.name.clone(), format!("!{}", matcher.value));
        }
    }
    filter_labels
}

/// Build the label set used to match two series in a binary operation, honoring
/// any `on(...)` / `ignoring(...)` modifier attached to the binary expression.
///
/// When no modifier is supplied the match key is the full label set with the
/// reserved `__name__` label dropped (this is the historical behavior). With
/// `on(labels)` only the listed labels participate in matching; with
/// `ignoring(labels)` every label except the listed ones (and `__name__`)
/// participates. This lets operators combine series with otherwise mismatched
/// label sets — e.g. `tx_bytes / ignoring(direction) link_bandwidth` where the
/// tx/rx series carries a `direction` label that the bandwidth series does not.
fn match_key(
    metric: &HashMap<String, String>,
    matching: Option<&parser::LabelModifier>,
) -> BTreeMap<String, String> {
    let mut key = BTreeMap::new();
    match matching {
        Some(parser::LabelModifier::Include(labels)) => {
            for label_name in &labels.labels {
                if let Some(value) = metric.get(label_name) {
                    key.insert(label_name.clone(), value.clone());
                }
            }
        }
        Some(parser::LabelModifier::Exclude(labels)) => {
            for (k, v) in metric {
                if k != "__name__" && !labels.labels.contains(k) {
                    key.insert(k.clone(), v.clone());
                }
            }
        }
        None => {
            for (k, v) in metric {
                if k != "__name__" {
                    key.insert(k.clone(), v.clone());
                }
            }
        }
    }
    key
}

impl<T: Deref<Target = Tsdb>> QueryEngine<T> {
    pub fn new(tsdb: T) -> Self {
        Self { tsdb }
    }

    /// Get a reference to the underlying TSDB
    pub fn tsdb(&self) -> &Tsdb {
        &self.tsdb
    }

    /// Get the time range (min, max) of all data in seconds
    pub fn get_time_range(&self) -> (f64, f64) {
        self.tsdb
            .time_range()
            .map(|(min_ns, max_ns)| (min_ns as f64 / 1e9, max_ns as f64 / 1e9))
            .unwrap_or_else(|| {
                // No data found, return a reasonable default
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64();
                (now - 3600.0, now) // 1 hour ago to now
            })
    }

    /// Execute an instant query — evaluate `query_str` at a single
    /// timestamp. Mirrors Prometheus' `/api/v1/query` semantics.
    ///
    /// Internally this is a degenerate range query (`start = end =
    /// time`, `step = sampling_interval`) collapsed into a vector by
    /// taking the latest point of each result series. That gives us
    /// the full PromQL surface — every operator the streaming
    /// pipeline supports plus the eager backstops (`scalar`, `vector`,
    /// `deriv` on counters, `histogram_heatmap`) — for free.
    ///
    /// `time` defaults to the latest timestamp in the TSDB.
    pub fn query(&self, query_str: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        let target = time.unwrap_or_else(|| self.get_time_range().1);
        let step = self.tsdb.interval().max(1.0);
        let result = self.query_range(query_str, target, target, step)?;
        Ok(matrix_to_vector(result))
    }

    /// Parse a metric selector like
    /// `metric_name{label1="value1",label2="value2"}`. Used by the
    /// `histogram_quantiles` / `histogram_heatmap` pre-parsers in
    /// `query_range`, which take their argument as a query string
    /// rather than going through the standard PromQL AST.
    fn parse_metric_selector(&self, selector: &str) -> Result<(String, Labels), QueryError> {
        if let Some(brace_pos) = selector.find('{') {
            let metric_name = selector[..brace_pos].trim().to_string();
            let labels_part = &selector[brace_pos + 1..selector.len() - 1];

            let mut labels = Labels::default();
            for part in labels_part.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }

                // Parse operator: check for !=, !~, =~, then = (order matters)
                let (key, value, negate) = if let Some(pos) = part.find("!=") {
                    (&part[..pos], part[pos + 2..].trim().trim_matches('"'), true)
                } else if let Some(pos) = part.find("!~") {
                    (&part[..pos], part[pos + 2..].trim().trim_matches('"'), true)
                } else if let Some(pos) = part.find("=~") {
                    (
                        &part[..pos],
                        part[pos + 2..].trim().trim_matches('"'),
                        false,
                    )
                } else if let Some(pos) = part.find('=') {
                    (
                        &part[..pos],
                        part[pos + 1..].trim().trim_matches('"'),
                        false,
                    )
                } else {
                    continue;
                };

                let key = key.trim().to_string();
                if negate {
                    labels.inner.insert(key, format!("!{value}"));
                } else {
                    labels.inner.insert(key, value.to_string());
                }
            }
            Ok((metric_name, labels))
        } else {
            Ok((selector.to_string(), Labels::default()))
        }
    }

    /// Handle function calls from the parsed AST. Most functions are
    /// handled by the streaming dispatcher (called first in
    /// `evaluate_expr`); the branches here cover the residual cases:
    /// `scalar`, `vector`, `deriv` on counter rates, `histogram_quantile`
    /// (which routes through `streaming::histogram::quantiles`), plus
    /// the `histogram_quantiles` placeholder error.
    fn handle_function_call(
        &self,
        call: &parser::Call,
        start: f64,
        end: f64,
        step: f64,
    ) -> Result<QueryResult, QueryError> {
        match call.func.name {
            "histogram_quantile" => {
                // histogram_quantile(quantile, histogram) — standard PromQL,
                // single quantile.  Internally the same operation as
                // `histogram_quantiles([q], m)`; both routes share the
                // `streaming::histogram::quantiles` pipeline below.
                if call.args.args.len() < 2 {
                    return Err(QueryError::ParseError(
                        "histogram_quantile requires 2 arguments".to_string(),
                    ));
                }

                let quantile = match &*call.args.args[0] {
                    Expr::NumberLiteral(num) => num.val,
                    _ => {
                        return Err(QueryError::ParseError(
                            "histogram_quantile first argument must be a number".to_string(),
                        ))
                    }
                };
                if !(0.0..=1.0).contains(&quantile) {
                    return Err(QueryError::ParseError(format!(
                        "histogram_quantile quantile must be between 0.0 and 1.0, got {}",
                        quantile
                    )));
                }

                let metric_name = match &*call.args.args[1] {
                    Expr::VectorSelector(selector) => {
                        selector.name.as_deref().ok_or_else(|| {
                            QueryError::ParseError("Vector selector missing name".to_string())
                        })?
                    }
                    _ => {
                        return Err(QueryError::ParseError(
                            "histogram_quantile second argument must be a metric name".to_string(),
                        ))
                    }
                };

                let start_ns = (start * 1e9) as u64;
                let end_ns = (end * 1e9) as u64;

                let Some(collection) = self.tsdb.histograms_ref(metric_name) else {
                    return Err(QueryError::MetricNotFound(metric_name.to_string()));
                };
                let result = streaming::histogram::quantiles(
                    collection,
                    &Labels::default(),
                    &[quantile],
                    start_ns,
                    end_ns,
                    None,
                    metric_name,
                );
                if result.is_empty() {
                    return Err(QueryError::MetricNotFound(format!(
                        "No histogram data found for {}",
                        metric_name
                    )));
                }
                Ok(QueryResult::Matrix { result })
            }
            "histogram_quantiles" => {
                // histogram_quantiles is handled via pre-parsing in query_range()
                // due to array literal syntax not being standard PromQL.
                // This branch handles any case where the parser does parse it.
                Err(QueryError::Unsupported(
                    "histogram_quantiles should be handled via query_range pre-parser".to_string(),
                ))
            }
            "scalar" => {
                // scalar() converts a single-element vector to a scalar.
                // If the vector has 0 or more than 1 element, returns NaN.
                if let Some(first_arg) = call.args.args.first() {
                    let inner_result = self.evaluate_expr(first_arg, start, end, step)?;

                    match inner_result {
                        QueryResult::Vector { result: samples } => {
                            if samples.len() == 1 {
                                Ok(QueryResult::Scalar {
                                    result: samples[0].value,
                                })
                            } else {
                                // More than one element or empty - return NaN
                                Ok(QueryResult::Scalar {
                                    result: (start, f64::NAN),
                                })
                            }
                        }
                        QueryResult::Matrix { result: samples } => {
                            // For matrix results, we need to return the scalar at each timestamp
                            // If there's exactly one series, convert it
                            if samples.len() == 1 && !samples[0].values.is_empty() {
                                // Return the last value as a scalar
                                let last_value = samples[0].values.last().unwrap();
                                Ok(QueryResult::Scalar {
                                    result: *last_value,
                                })
                            } else {
                                Ok(QueryResult::Scalar {
                                    result: (start, f64::NAN),
                                })
                            }
                        }
                        QueryResult::Scalar { result } => {
                            // Already a scalar, return as-is
                            Ok(QueryResult::Scalar { result })
                        }
                        QueryResult::HistogramHeatmap { .. } => Ok(QueryResult::Scalar {
                            result: (start, f64::NAN),
                        }),
                    }
                } else {
                    Err(QueryError::ParseError(
                        "scalar requires 1 argument".to_string(),
                    ))
                }
            }
            "vector" => {
                // vector(s) converts a scalar s to a vector with a single element
                if let Some(first_arg) = call.args.args.first() {
                    let inner_result = self.evaluate_expr(first_arg, start, end, step)?;

                    match inner_result {
                        QueryResult::Scalar { result } => {
                            let mut metric = HashMap::new();
                            metric.insert("__name__".to_string(), "".to_string());
                            Ok(QueryResult::Vector {
                                result: vec![Sample {
                                    metric,
                                    value: result,
                                }],
                            })
                        }
                        other => Ok(other), // Already a vector/matrix, return as-is
                    }
                } else {
                    Err(QueryError::ParseError(
                        "vector requires 1 argument".to_string(),
                    ))
                }
            }
            _ => Err(QueryError::Unsupported(format!(
                "Function {} not yet supported",
                call.func.name
            ))),
        }
    }

    /// Aggregate-expression backstop. The streaming dispatcher
    /// (called first by `evaluate_expr`) handles every supported
    /// `sum`/`avg`/`min`/`max`/`count` over a streamable inner; this
    /// path runs only when the inner couldn't stream — e.g.
    /// `sum(scalar(...))`, where the inner evaluates to a `Scalar`.
    /// Per PromQL semantics the aggregate of a scalar passes the
    /// scalar through unchanged.
    fn handle_aggregate(
        &self,
        agg: &parser::AggregateExpr,
        start: f64,
        end: f64,
        step: f64,
    ) -> Result<QueryResult, QueryError> {
        let op_str = agg.op.to_string();
        if !matches!(op_str.as_str(), "sum" | "avg" | "min" | "max" | "count") {
            return Err(QueryError::Unsupported(format!(
                "Unknown aggregation: {op_str}"
            )));
        }
        // Streaming would have handled a Series-typed inner; only
        // Scalar (or other non-Matrix) results reach here.
        self.evaluate_expr(&agg.expr, start, end, step)
    }

    /// Apply a binary operation to two query results
    fn apply_binary_op(
        &self,
        op: &TokenType,
        left: QueryResult,
        right: QueryResult,
        modifier: Option<&parser::BinModifier>,
    ) -> Result<QueryResult, QueryError> {
        match (left, right) {
            // Both sides are matrices (time series)
            (
                QueryResult::Matrix {
                    result: left_samples,
                },
                QueryResult::Matrix {
                    result: right_samples,
                },
            ) => {
                let mut result_samples = Vec::new();
                let matching = modifier.and_then(|m| m.matching.as_ref());

                // Build a map of right samples by their matching label set
                let mut right_by_labels: HashMap<BTreeMap<String, String>, &MatrixSample> =
                    HashMap::new();
                for right_sample in &right_samples {
                    let labels = match_key(&right_sample.metric, matching);
                    right_by_labels.insert(labels, right_sample);
                }

                for left_sample in &left_samples {
                    let left_labels = match_key(&left_sample.metric, matching);

                    // Find matching right sample by labels
                    let right_sample = if let Some(matched) = right_by_labels.get(&left_labels) {
                        Some(*matched)
                    } else if matching.is_none() && right_samples.len() == 1 {
                        // No explicit matcher and right has a single series: fall back to it.
                        // This preserves the common case of combining a vector with a
                        // single-series metric or aggregate without specifying matchers.
                        right_samples.first()
                    } else {
                        None
                    };

                    if let Some(right_sample) = right_sample {
                        let mut result_values = Vec::new();

                        // Create a map of right values by timestamp (rounded to milliseconds
                        // to handle f64 precision loss when converting ns -> sec -> ns)
                        let right_map: HashMap<u64, f64> = right_sample
                            .values
                            .iter()
                            .map(|(ts, val)| {
                                // Round to milliseconds to avoid precision issues
                                let ts_ms = (*ts * 1e3).round() as u64;
                                (ts_ms, *val)
                            })
                            .collect();

                        for (left_ts, left_val) in &left_sample.values {
                            // Round to milliseconds to match
                            let ts_ms = (*left_ts * 1e3).round() as u64;

                            if let Some(&right_val) = right_map.get(&ts_ms) {
                                let op_str = op.to_string();
                                let result_val = match op_str.as_str() {
                                    "+" => left_val + right_val,
                                    "-" => left_val - right_val,
                                    "*" => left_val * right_val,
                                    "/" => {
                                        if right_val != 0.0 {
                                            left_val / right_val
                                        } else {
                                            continue; // Skip division by zero
                                        }
                                    }
                                    _ => {
                                        return Err(QueryError::Unsupported(format!(
                                            "Unsupported operator: {}",
                                            op_str
                                        )))
                                    }
                                };
                                result_values.push((*left_ts, result_val));
                            }
                        }

                        if !result_values.is_empty() {
                            result_samples.push(MatrixSample {
                                metric: left_sample.metric.clone(),
                                values: result_values,
                            });
                        }
                    }
                }

                Ok(QueryResult::Matrix {
                    result: result_samples,
                })
            }
            // Left is matrix, right is scalar (constant or single-value metric)
            (
                QueryResult::Matrix {
                    result: mut samples,
                },
                QueryResult::Scalar { result: scalar },
            ) => {
                let scalar_val = scalar.1;
                let op_str = op.to_string();

                for sample in &mut samples {
                    for value in &mut sample.values {
                        value.1 = match op_str.as_str() {
                            "+" => value.1 + scalar_val,
                            "-" => value.1 - scalar_val,
                            "*" => value.1 * scalar_val,
                            "/" => {
                                if scalar_val != 0.0 {
                                    value.1 / scalar_val
                                } else {
                                    continue; // Skip division by zero
                                }
                            }
                            _ => {
                                return Err(QueryError::Unsupported(format!(
                                    "Unsupported operator: {}",
                                    op_str
                                )))
                            }
                        };
                    }
                }
                Ok(QueryResult::Matrix { result: samples })
            }
            // Right is matrix, left is scalar (for commutative operations)
            (
                QueryResult::Scalar { result: scalar },
                QueryResult::Matrix {
                    result: mut samples,
                },
            ) => {
                let scalar_val = scalar.1;
                let op_str = op.to_string();

                for sample in &mut samples {
                    for value in &mut sample.values {
                        value.1 = match op_str.as_str() {
                            "+" => scalar_val + value.1,
                            "-" => scalar_val - value.1,
                            "*" => scalar_val * value.1,
                            "/" => {
                                if value.1 != 0.0 {
                                    scalar_val / value.1
                                } else {
                                    continue; // Skip division by zero
                                }
                            }
                            _ => {
                                return Err(QueryError::Unsupported(format!(
                                    "Unsupported operator: {}",
                                    op_str
                                )))
                            }
                        };
                    }
                }
                Ok(QueryResult::Matrix { result: samples })
            }
            // Handle other cases as needed
            _ => Err(QueryError::EvaluationError(
                "Incompatible operands for binary operation".to_string(),
            )),
        }
    }

    /// Evaluate an AST expression
    fn evaluate_expr(
        &self,
        expr: &Expr,
        start: f64,
        end: f64,
        step: f64,
    ) -> Result<QueryResult, QueryError> {
        // Streaming dispatcher first — handles every shape the
        // pipeline currently covers (rate/irate/gauge selector/
        // avg_over_time/idelta/deriv/sum-avg-min-max-count by/
        // without/+ - * / matrix x scalar/matrix x matrix). Returns
        // `None` for shapes streaming doesn't yet handle so we fall
        // through to the eager match below. Firing here (rather than
        // only in `query_range`) means the inner sub-tree of an
        // eager-only top-level shape — e.g. `scalar(sum(irate(...)))`
        // — also goes through streaming.
        if let Some(result) =
            streaming::dispatch::try_streaming(&self.tsdb, expr, start, end, step)?
        {
            return Ok(result);
        }
        match expr {
            Expr::Binary(binary) => {
                // Evaluate left and right sides
                let left = self.evaluate_expr(&binary.lhs, start, end, step)?;
                let right = self.evaluate_expr(&binary.rhs, start, end, step)?;

                // Apply the binary operation, honoring any on()/ignoring() modifier
                self.apply_binary_op(&binary.op, left, right, binary.modifier.as_ref())
            }
            Expr::VectorSelector(selector) => {
                // VectorSelector backstop: streaming covers gauge
                // selectors (the 99% case). Reaching this arm means
                // either the metric doesn't exist or it's a counter
                // (PromQL requires rate/irate to access counters);
                // both produce MetricNotFound.
                let metric_name = selector.name.as_deref().ok_or_else(|| {
                    QueryError::ParseError("Vector selector missing name".to_string())
                })?;
                Err(QueryError::MetricNotFound(metric_name.to_string()))
            }
            Expr::NumberLiteral(num) => {
                // Return a scalar value
                Ok(QueryResult::Scalar {
                    result: (start, num.val),
                })
            }
            Expr::Call(call) => {
                // Handle function calls
                self.handle_function_call(call, start, end, step)
            }
            Expr::Aggregate(agg) => {
                // Handle aggregation operations like sum()
                self.handle_aggregate(agg, start, end, step)
            }
            Expr::MatrixSelector(_selector) => {
                // This shouldn't appear at top level, but handle it anyway
                Err(QueryError::Unsupported(
                    "Direct matrix selector not supported".to_string(),
                ))
            }
            Expr::Paren(paren) => {
                // Parenthesized expression - just evaluate the inner expression
                self.evaluate_expr(&paren.expr, start, end, step)
            }
            _ => Err(QueryError::Unsupported(format!(
                "Unsupported expression type: {:?}",
                expr
            ))),
        }
    }

    /// Handle histogram_quantiles(quantiles_array, histogram_metric)
    /// queries.  Example: `histogram_quantiles([0.5, 0.9, 0.99, 0.999],
    /// tcp_packet_latency)`.
    ///
    /// This is a rezolus extension on top of standard PromQL — the
    /// scalar `histogram_quantile(q, m)` only supports a single
    /// quantile per call, so a dashboard wanting p50/p90/p99/p999
    /// from one metric would otherwise issue four separate queries
    /// and walk the histogram series four times.  The plural form
    /// fuses them into a single walk.
    ///
    /// Output series are labeled `{__name__: name, quantile: "0.99"}`
    /// to match the standard PromQL `histogram_quantile` convention,
    /// so a dashboard can consume either entry point uniformly.
    fn handle_histogram_quantiles(
        &self,
        query_str: &str,
        start: f64,
        end: f64,
    ) -> Result<QueryResult, QueryError> {
        // Extract the inner part: [0.5, 0.9, 0.99, 0.999], tcp_packet_latency
        let inner = &query_str["histogram_quantiles(".len()..query_str.len() - 1];

        // Find the array portion [...]
        let array_start = inner.find('[').ok_or_else(|| {
            QueryError::ParseError(
                "histogram_quantiles first argument must be an array of quantiles".to_string(),
            )
        })?;
        let array_end = inner.find(']').ok_or_else(|| {
            QueryError::ParseError("Missing closing bracket in quantiles array".to_string())
        })?;

        // Parse the quantiles array
        let array_str = &inner[array_start + 1..array_end];
        let quantiles: Vec<f64> = array_str
            .split(',')
            .map(|s| s.trim().parse::<f64>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                QueryError::ParseError(format!("Failed to parse quantile value: {}", e))
            })?;

        if quantiles.is_empty() {
            return Err(QueryError::ParseError(
                "Quantiles array cannot be empty".to_string(),
            ));
        }

        for &q in &quantiles {
            if !(0.0..=1.0).contains(&q) {
                return Err(QueryError::ParseError(format!(
                    "histogram_quantiles values must be between 0.0 and 1.0, got {}",
                    q
                )));
            }
        }

        // Extract the metric selector (everything after the array and comma)
        let after_array = &inner[array_end + 1..].trim();
        let remaining = after_array
            .strip_prefix(',')
            .map(|s| s.trim())
            .ok_or_else(|| {
                QueryError::ParseError(
                    "histogram_quantiles requires a metric name as second argument".to_string(),
                )
            })?;

        // Split off an optional trailing stride parameter (e.g. ", 15")
        let (metric_selector, stride_ns) = parse_optional_stride(remaining)?;

        // Parse the metric selector to extract name and labels
        let (metric_name, labels) = self.parse_metric_selector(metric_selector)?;

        let Some(collection) = self.tsdb.histograms_ref(&metric_name) else {
            return Err(QueryError::MetricNotFound(metric_name.to_string()));
        };
        let start_ns = (start * 1e9) as u64;
        let end_ns = (end * 1e9) as u64;
        let result = streaming::histogram::quantiles(
            collection,
            &labels,
            &quantiles,
            start_ns,
            end_ns,
            stride_ns,
            &metric_name,
        );
        if result.is_empty() {
            return Err(QueryError::MetricNotFound(format!(
                "No histogram data found for {}",
                metric_name
            )));
        }
        Ok(QueryResult::Matrix { result })
    }

    /// Handle histogram_heatmap(histogram_metric) queries
    /// Returns bucket data suitable for rendering as a latency heatmap
    fn handle_histogram_heatmap(
        &self,
        query_str: &str,
        start: f64,
        end: f64,
    ) -> Result<QueryResult, QueryError> {
        // Extract the metric selector from histogram_heatmap(metric_selector[, stride])
        let inner = &query_str["histogram_heatmap(".len()..query_str.len() - 1];
        let (metric_selector, stride_ns) = parse_optional_stride(inner.trim())?;

        // Parse the metric selector to extract name and labels
        let (metric_name, labels) = self.parse_metric_selector(metric_selector)?;

        // Get the histogram data with label filtering
        if let Some(collection) = self.tsdb.histograms(&metric_name, labels) {
            // Sum all histogram series together
            let summed_series = collection.sum();

            // Get heatmap data
            if let Some(heatmap_data) = summed_series.heatmap(stride_ns) {
                let start_sec = start;
                let end_sec = end;

                // Filter data to the requested time range
                let mut filtered_timestamps = Vec::new();
                let mut filtered_data = Vec::new();
                let mut time_index_map = std::collections::HashMap::new();

                for (old_idx, ts) in heatmap_data.timestamps.iter().enumerate() {
                    if *ts >= start_sec && *ts <= end_sec {
                        let new_idx = filtered_timestamps.len();
                        time_index_map.insert(old_idx, new_idx);
                        filtered_timestamps.push(*ts);
                    }
                }

                let mut min_value = f64::MAX;
                let mut max_value = f64::MIN;
                let mut min_bucket_idx = usize::MAX;
                let mut max_bucket_idx = 0usize;

                for (time_idx, bucket_idx, count) in heatmap_data.data.iter() {
                    if let Some(&new_time_idx) = time_index_map.get(time_idx) {
                        filtered_data.push((new_time_idx, *bucket_idx, *count));
                        min_value = min_value.min(*count);
                        max_value = max_value.max(*count);
                        min_bucket_idx = min_bucket_idx.min(*bucket_idx);
                        max_bucket_idx = max_bucket_idx.max(*bucket_idx);
                    }
                }

                if min_value == f64::MAX {
                    min_value = 0.0;
                }
                if max_value == f64::MIN {
                    max_value = 0.0;
                }

                // Trim bucket_bounds and remap bucket indices to the active range
                let (trimmed_bounds, trimmed_data) = if !filtered_data.is_empty()
                    && max_bucket_idx < heatmap_data.bucket_bounds.len()
                {
                    let bounds =
                        heatmap_data.bucket_bounds[min_bucket_idx..=max_bucket_idx].to_vec();
                    let data = filtered_data
                        .into_iter()
                        .map(|(t, b, c)| (t, b - min_bucket_idx, c))
                        .collect();
                    (bounds, data)
                } else {
                    (heatmap_data.bucket_bounds, filtered_data)
                };

                return Ok(QueryResult::HistogramHeatmap {
                    result: HistogramHeatmapResult {
                        timestamps: filtered_timestamps,
                        bucket_bounds: trimmed_bounds,
                        data: trimmed_data,
                        min_value,
                        max_value,
                    },
                });
            }

            Err(QueryError::MetricNotFound(format!(
                "No histogram data found for {}",
                metric_name
            )))
        } else {
            Err(QueryError::MetricNotFound(metric_name.to_string()))
        }
    }

    pub fn query_range(
        &self,
        query_str: &str,
        start: f64,
        end: f64,
        step: f64,
    ) -> Result<QueryResult, QueryError> {
        // Handle histogram_quantiles specially since it uses array literal syntax
        // that may not be parsed correctly by the standard PromQL parser
        if query_str.starts_with("histogram_quantiles(") && query_str.ends_with(")") {
            return self.handle_histogram_quantiles(query_str, start, end);
        }

        // Handle histogram_heatmap specially
        if query_str.starts_with("histogram_heatmap(") && query_str.ends_with(")") {
            return self.handle_histogram_heatmap(query_str, start, end);
        }

        // Parse the query into an AST and evaluate. The streaming
        // dispatcher fires inside `evaluate_expr`, so it covers
        // every recursion level — not just the top — letting eager
        // wrappers like `scalar(sum(irate(...)))` benefit from
        // streaming on their inner sub-trees too.
        match parser::parse(query_str) {
            Ok(expr) => self.evaluate_expr(&expr, start, end, step),
            Err(err) => {
                // Provide more helpful error messages for common mistakes
                let error_msg = format!("{:?}", err);
                if error_msg.contains("invalid promql query") && query_str.contains(" by ") {
                    Err(QueryError::ParseError(
                        "Invalid query syntax. Aggregation operators require parentheses around the expression, e.g., 'sum by (id) (irate(metric[5m]))' not 'sum by (id) irate(metric[5m])'".to_string()
                    ))
                } else {
                    Err(QueryError::ParseError(format!(
                        "Failed to parse query: {}",
                        error_msg
                    )))
                }
            }
        }
    }
}
