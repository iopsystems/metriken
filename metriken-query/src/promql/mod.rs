use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use promql_parser::label::Matcher;
use promql_parser::parser::{self, Expr};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tsdb::{Labels, Tsdb};

mod columns;
pub mod streaming;

#[cfg(test)]
mod tests;

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

/// Owned counterpart to `streaming::GroupBy<'_>` — held on the
/// handler stack between parse and dispatch, then borrowed.
#[derive(Debug, Default, Clone)]
pub(crate) enum HistogramGroupBy {
    #[default]
    None,
    By(Vec<String>),
    Without(Vec<String>),
}

impl HistogramGroupBy {
    pub(crate) fn as_ref(&self) -> streaming::GroupBy<'_> {
        match self {
            HistogramGroupBy::None => streaming::GroupBy::Include(&[]),
            HistogramGroupBy::By(ls) => streaming::GroupBy::Include(ls),
            HistogramGroupBy::Without(ls) => streaming::GroupBy::Exclude(ls),
        }
    }
}

/// Detect `sum [by/without (...)] (<histogram_func>(...))` and rewrite
/// to the native `<histogram_func> [by/without (...)] (...)` form.
///
/// `histogram_irate`/`histogram_mean`/`histogram_count` aren't standard
/// PromQL function names, so the promql-parser crate rejects them
/// outright — even when wrapped in a standard aggregator.  Rather than
/// teach the parser these names, the wrapping `sum` is unwrapped at
/// the string level here and passed through the existing top-level
/// dispatcher.  Returns `None` if the input doesn't match the pattern.
///
/// Restricted to `sum` because the internal histogram grouping already
/// reduces across label sets via `total_count` / mean / per-step rate;
/// `sum` is therefore a pass-through.  Other aggregators (`avg`, `min`,
/// `max`, `count`) would require post-pipeline reduction over the
/// per-group output and are intentionally left untouched.
fn unwrap_sum_around_histogram(query: &str) -> Option<String> {
    let query = query.trim();
    let after_sum = query.strip_prefix("sum")?;
    // Guard against names with `sum` as a prefix (e.g. `summary_foo`).
    if !matches!(after_sum.chars().next(), Some('(' | ' ' | '\t')) {
        return None;
    }
    let rest = after_sum.trim_start();

    // Outer aggregation modifier on the wrapping `sum`.
    let (grouping_clause, after_modifier) = if let Some(r) = rest.strip_prefix("by") {
        let (after, clause) = take_grouping_clause(r.trim_start(), "by")?;
        (Some(clause), after)
    } else if let Some(r) = rest.strip_prefix("without") {
        let (after, clause) = take_grouping_clause(r.trim_start(), "without")?;
        (Some(clause), after)
    } else {
        (None, rest)
    };

    // Body of the outer `sum`: must be `(<inner>)` with balanced parens.
    let after_open = after_modifier.strip_prefix('(')?;
    let close = find_close_paren(after_open)?;
    if after_open[close + 1..]
        .trim_start()
        .chars()
        .next()
        .is_some()
    {
        return None; // trailing tokens — not the shape we rewrite.
    }
    let inner = after_open[..close].trim();

    // Inner must be exactly one of the histogram_* function calls.
    let inner_func = ["histogram_irate", "histogram_mean", "histogram_count"]
        .iter()
        .find(|f| inner.starts_with(*f))?;
    let after_name = inner.strip_prefix(*inner_func)?;
    if !matches!(after_name.chars().next(), Some('(' | ' ' | '\t')) {
        return None;
    }

    // Refuse to rewrite if the inner call already carries its own
    // grouping modifier — combining two would be ambiguous and the
    // existing dispatcher rejects it for the same reason.
    let inner_after_name = after_name.trim_start();
    if inner_after_name.starts_with("by") || inner_after_name.starts_with("without") {
        let next = inner_after_name
            .chars()
            .nth("by".len().max("without".len()))
            .unwrap_or(' ');
        if matches!(next, '(' | ' ' | '\t') {
            return None;
        }
    }

    match grouping_clause {
        Some(clause) => Some(format!("{inner_func} {clause} {inner_after_name}")),
        None => Some(inner.to_string()),
    }
}

/// Consume a `by (..)` / `without (..)` clause from the start of `s`,
/// returning `(rest_after_clause, "by (..)" | "without (..)")`.  The
/// returned clause string keeps the keyword so callers can splice it
/// directly into a rewritten query.
fn take_grouping_clause<'a>(s: &'a str, keyword: &str) -> Option<(&'a str, String)> {
    let inside = s.strip_prefix('(')?;
    let close = find_close_paren(inside)?;
    let labels = &inside[..close];
    let after = inside[close + 1..].trim_start();
    Some((after, format!("{keyword} ({labels})")))
}

/// Parse `<func> [by (labels) | without (labels)] (body)`. `Ok(None)`
/// if the prefix doesn't match.
pub(crate) fn parse_histogram_call<'a>(
    func: &str,
    query: &'a str,
) -> Result<Option<(&'a str, HistogramGroupBy)>, QueryError> {
    let Some(after_name) = query.strip_prefix(func) else {
        return Ok(None);
    };
    // Guard against prefix collisions with longer function names.
    if !matches!(after_name.chars().next(), Some('(' | ' ' | '\t')) {
        return Ok(None);
    }
    let rest = after_name.trim_start();

    let (group_by, after_modifier) = if let Some(r) = rest.strip_prefix("by") {
        let (labels, after) = parse_grouping_clause(func, r.trim_start(), "by")?;
        (HistogramGroupBy::By(labels), after)
    } else if let Some(r) = rest.strip_prefix("without") {
        let (labels, after) = parse_grouping_clause(func, r.trim_start(), "without")?;
        (HistogramGroupBy::Without(labels), after)
    } else {
        (HistogramGroupBy::None, rest)
    };

    let body = after_modifier.strip_prefix('(').ok_or_else(|| {
        QueryError::ParseError(format!("{func}: expected '(' after function name"))
    })?;
    if !body.ends_with(')') {
        return Err(QueryError::ParseError(format!(
            "{func}: missing closing ')'"
        )));
    }
    Ok(Some((&body[..body.len() - 1], group_by)))
}

fn parse_grouping_clause<'a>(
    func: &str,
    s: &'a str,
    keyword: &str,
) -> Result<(Vec<String>, &'a str), QueryError> {
    let inside = s
        .strip_prefix('(')
        .ok_or_else(|| QueryError::ParseError(format!("{func}: expected '(' after '{keyword}'")))?;
    let close = find_close_paren(inside).ok_or_else(|| {
        QueryError::ParseError(format!("{func}: unbalanced parens in '{keyword}' clause"))
    })?;
    let labels = parse_label_list(&inside[..close])?;
    Ok((labels, inside[close + 1..].trim_start()))
}

fn parse_label_list(s: &str) -> Result<Vec<String>, QueryError> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(QueryError::ParseError(format!(
                "invalid label name in grouping clause: {p}"
            )));
        }
        out.push(p.to_string());
    }
    Ok(out)
}

fn find_close_paren(s: &str) -> Option<usize> {
    let mut depth: usize = 1;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
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
    /// Internally a degenerate range query (`start = end = time`,
    /// `step = sampling_interval`) collapsed into a vector by taking
    /// the latest point of each result series. Inherits the full
    /// supported PromQL surface from `query_range`.
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

    /// Evaluate a parsed PromQL expression. Thin wrapper around the
    /// streaming dispatcher; any shape the dispatcher doesn't
    /// recognise becomes `QueryError::Unsupported`.
    fn evaluate_expr(
        &self,
        expr: &Expr,
        start: f64,
        end: f64,
        step: f64,
    ) -> Result<QueryResult, QueryError> {
        streaming::dispatch::try_streaming(&self.tsdb, expr, start, end, step)
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
    /// `histogram_heatmap(metric{matchers}[, stride])` — output is
    /// inherently 2-D so a `Vec` of `(time_idx, bucket_idx, count)`
    /// triples gets materialised either way; the streaming-side win
    /// is avoiding the `tsdb.histograms()` clone and the
    /// `collection.sum()` merged-series allocation.
    fn handle_histogram_heatmap(
        &self,
        query_str: &str,
        start: f64,
        end: f64,
    ) -> Result<QueryResult, QueryError> {
        let inner = &query_str["histogram_heatmap(".len()..query_str.len() - 1];
        let (metric_selector, stride_ns) = parse_optional_stride(inner.trim())?;
        let (metric_name, labels) = self.parse_metric_selector(metric_selector)?;

        let Some(collection) = self.tsdb.histograms_ref(&metric_name) else {
            return Err(QueryError::MetricNotFound(metric_name.to_string()));
        };

        let start_ns = (start * 1e9) as u64;
        let end_ns = (end * 1e9) as u64;
        let result =
            streaming::histogram::heatmap(collection, &labels, start_ns, end_ns, stride_ns);
        match result {
            Some(result) => Ok(QueryResult::HistogramHeatmap { result }),
            None => Err(QueryError::MetricNotFound(format!(
                "No histogram data found for {metric_name}"
            ))),
        }
    }

    /// Rejects a trailing stride — see [`streaming::histogram::irate`].
    fn handle_histogram_irate(
        &self,
        inner: &str,
        group_by: HistogramGroupBy,
        start: f64,
        end: f64,
    ) -> Result<QueryResult, QueryError> {
        let (metric_selector, stride_ns) = parse_optional_stride(inner.trim())?;
        if stride_ns.is_some() {
            return Err(QueryError::ParseError(
                "histogram_irate does not accept a window/stride argument".to_string(),
            ));
        }
        let (metric_name, labels) = self.parse_metric_selector(metric_selector)?;

        let Some(collection) = self.tsdb.histograms_ref(&metric_name) else {
            return Err(QueryError::MetricNotFound(metric_name.to_string()));
        };

        let start_ns = (start * 1e9) as u64;
        let end_ns = (end * 1e9) as u64;
        let result = streaming::histogram::irate(
            collection,
            &labels,
            group_by.as_ref(),
            start_ns,
            end_ns,
            &metric_name,
        );
        if result.is_empty() {
            return Err(QueryError::MetricNotFound(format!(
                "No histogram data found for {metric_name}"
            )));
        }
        Ok(QueryResult::Matrix { result })
    }

    fn handle_histogram_scalar(
        &self,
        func: &str,
        inner: &str,
        group_by: HistogramGroupBy,
        start: f64,
        end: f64,
    ) -> Result<QueryResult, QueryError> {
        let (metric_selector, stride_ns) = parse_optional_stride(inner.trim())?;
        let (metric_name, labels) = self.parse_metric_selector(metric_selector)?;

        let Some(collection) = self.tsdb.histograms_ref(&metric_name) else {
            return Err(QueryError::MetricNotFound(metric_name.to_string()));
        };

        let start_ns = (start * 1e9) as u64;
        let end_ns = (end * 1e9) as u64;
        let group = group_by.as_ref();
        let result = match func {
            "histogram_mean" => streaming::histogram::mean(
                collection,
                &labels,
                group,
                start_ns,
                end_ns,
                stride_ns,
                &metric_name,
            ),
            _ => streaming::histogram::count(
                collection,
                &labels,
                group,
                start_ns,
                end_ns,
                stride_ns,
                &metric_name,
            ),
        };
        if result.is_empty() {
            return Err(QueryError::MetricNotFound(format!(
                "No histogram data found for {metric_name}"
            )));
        }
        Ok(QueryResult::Matrix { result })
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

        // Recognise `sum [by/without (...)] (<histogram_func>(...))` and
        // rewrite to the native grouped form before the histogram
        // dispatchers below match.  `sum` is the only aggregator with a
        // direct native equivalent — `histogram_irate`/`mean`/`count`
        // already collapse across label sets in their internal grouping
        // pass, so `sum [grouping] (...)` is semantically identical to
        // `<func> [grouping] (...)`.  Other aggregators (`avg`, `min`,
        // `max`, `count`) would require post-pipeline reduction and are
        // not rewritten here.
        let rewritten = unwrap_sum_around_histogram(query_str);
        let query_str: &str = rewritten.as_deref().unwrap_or(query_str);

        for func in ["histogram_mean", "histogram_count"] {
            if let Some((inner, group_by)) = parse_histogram_call(func, query_str)? {
                return self.handle_histogram_scalar(func, inner, group_by, start, end);
            }
        }
        if let Some((inner, group_by)) = parse_histogram_call("histogram_irate", query_str)? {
            return self.handle_histogram_irate(inner, group_by, start, end);
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
