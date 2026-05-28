use std::collections::HashMap;
use std::sync::Arc;

use promql_parser::label::Matcher;
use promql_parser::parser::{self, Expr};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::labels::Labels;
use crate::DataSource;

#[cfg(test)]
mod columns;
pub(crate) mod streaming;

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

/// The PromQL query engine, backed by any `DataSource`.
pub(crate) struct QueryEngine {
    source: Arc<dyn DataSource>,
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

/// Rewrite `sum [by/without (...)] (<histogram_func>(...))` to the
/// native `<histogram_func> [by/without (...)] (...)` form.
fn unwrap_sum_around_histogram(query: &str) -> Option<String> {
    let query = query.trim();
    let after_sum = query.strip_prefix("sum")?;
    if !matches!(after_sum.chars().next(), Some('(' | ' ' | '\t')) {
        return None;
    }
    let rest = after_sum.trim_start();

    let (grouping_clause, after_modifier) = if let Some(r) = rest.strip_prefix("by") {
        let (after, clause) = take_grouping_clause(r.trim_start(), "by")?;
        (Some(clause), after)
    } else if let Some(r) = rest.strip_prefix("without") {
        let (after, clause) = take_grouping_clause(r.trim_start(), "without")?;
        (Some(clause), after)
    } else {
        (None, rest)
    };

    let after_open = after_modifier.strip_prefix('(')?;
    let close = find_close_paren(after_open)?;
    if after_open[close + 1..]
        .trim_start()
        .chars()
        .next()
        .is_some()
    {
        return None;
    }
    let inner = after_open[..close].trim();

    let inner_func = [
        "histogram_irate",
        "histogram_mean",
        "histogram_count",
        "histogram_sum",
    ]
    .iter()
    .find(|f| inner.starts_with(*f))?;
    let after_name = inner.strip_prefix(*inner_func)?;
    if !matches!(after_name.chars().next(), Some('(' | ' ' | '\t')) {
        return None;
    }

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

fn take_grouping_clause<'a>(s: &'a str, keyword: &str) -> Option<(&'a str, String)> {
    let inside = s.strip_prefix('(')?;
    let close = find_close_paren(inside)?;
    let labels = &inside[..close];
    let after = inside[close + 1..].trim_start();
    Some((after, format!("{keyword} ({labels})")))
}

/// Parse `<func> [by (labels) | without (labels)] (body)`.
pub(crate) fn parse_histogram_call<'a>(
    func: &str,
    query: &'a str,
) -> Result<Option<(&'a str, HistogramGroupBy)>, QueryError> {
    let Some(after_name) = query.strip_prefix(func) else {
        return Ok(None);
    };
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

/// Temporary: convert a `HistogramStream` back to the fully-materialised
/// `Histograms` type so that the existing operator methods remain unchanged
/// until Task 3 rewrites the operators to consume the stream directly.
pub(crate) fn stream_to_histograms(
    stream: crate::histogram_stream::HistogramStream,
) -> crate::types::Histograms {
    use crate::types::{Histogram, Histograms};
    let n = stream.meta.series.len();
    let config = stream.meta.config;
    let mut ts_per_series: Vec<Vec<u64>> = vec![Vec::new(); n];
    let mut snap_per_series: Vec<Vec<crate::types::HistogramSnapshot>> = vec![Vec::new(); n];
    for row in stream.rows {
        debug_assert!(row.series_idx < n, "series_idx {} out of bounds (n={})", row.series_idx, n);
        ts_per_series[row.series_idx].push(row.timestamp);
        snap_per_series[row.series_idx].push(row.snapshot);
    }
    let series = stream
        .meta
        .series
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !ts_per_series[*i].is_empty())
        .map(|(i, labels)| Histogram {
            labels,
            config,
            timestamps: ts_per_series[i].clone(),
            snapshots: snap_per_series[i].clone(),
        })
        .collect();
    Histograms { series }
}

impl QueryEngine {
    pub(crate) fn new(source: Arc<dyn DataSource>) -> Self {
        Self { source }
    }

    /// Time range of the underlying source in nanoseconds, or `None` if empty.
    pub(crate) fn time_range(&self) -> Option<(u64, u64)> {
        self.source.time_range()
    }

    /// Execute an instant query at a single timestamp (test helper).
    #[cfg(test)]
    pub(crate) fn query(&self, query_str: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        let target = time.unwrap_or_else(|| {
            self.source
                .time_range()
                .map(|(_, max_ns)| max_ns as f64 / 1e9)
                .unwrap_or(0.0)
        });
        let step = self.source.interval().max(1.0);
        let result = self.query_range(query_str, target, target, step)?;
        let QueryResult::Matrix { result: samples } = result else {
            return Ok(result);
        };
        let vector: Vec<Sample> = samples
            .into_iter()
            .filter_map(|s| s.values.last().copied().map(|value| Sample { metric: s.metric, value }))
            .collect();
        Ok(QueryResult::Vector { result: vector })
    }

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

                let (key, value, negate) = if let Some(pos) = part.find("!=") {
                    (&part[..pos], part[pos + 2..].trim().trim_matches('"'), true)
                } else if let Some(pos) = part.find("!~") {
                    (&part[..pos], part[pos + 2..].trim().trim_matches('"'), true)
                } else if let Some(pos) = part.find("=~") {
                    (&part[..pos], part[pos + 2..].trim().trim_matches('"'), false)
                } else if let Some(pos) = part.find('=') {
                    (&part[..pos], part[pos + 1..].trim().trim_matches('"'), false)
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

    fn evaluate_expr(
        &self,
        expr: &Expr,
        start: f64,
        end: f64,
        step: f64,
    ) -> Result<QueryResult, QueryError> {
        streaming::dispatch::try_streaming(&*self.source, expr, start, end, step)
    }

    fn handle_histogram_quantiles(
        &self,
        query_str: &str,
        start: f64,
        end: f64,
    ) -> Result<QueryResult, QueryError> {
        let inner = &query_str["histogram_quantiles(".len()..query_str.len() - 1];

        let array_start = inner.find('[').ok_or_else(|| {
            QueryError::ParseError(
                "histogram_quantiles first argument must be an array of quantiles".to_string(),
            )
        })?;
        let array_end = inner.find(']').ok_or_else(|| {
            QueryError::ParseError("Missing closing bracket in quantiles array".to_string())
        })?;

        let array_str = &inner[array_start + 1..array_end];
        let quantiles: Vec<f64> = array_str
            .split(',')
            .map(|s| s.trim().parse::<f64>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| QueryError::ParseError(format!("Failed to parse quantile value: {}", e)))?;

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

        let after_array = &inner[array_end + 1..].trim();
        let remaining = after_array
            .strip_prefix(',')
            .map(|s| s.trim())
            .ok_or_else(|| {
                QueryError::ParseError(
                    "histogram_quantiles requires a metric name as second argument".to_string(),
                )
            })?;

        let (metric_selector, stride_ns) = parse_optional_stride(remaining)?;
        let (metric_name, labels) = self.parse_metric_selector(metric_selector)?;

        let start_ns = (start * 1e9) as u64;
        let end_ns = (end * 1e9) as u64;
        let stream = self
            .source
            .histogram_stream(&metric_name, &labels, start_ns, end_ns)
            .ok_or_else(|| QueryError::MetricNotFound(metric_name.clone()))?;
        let histograms = stream_to_histograms(stream);
        let result = histograms.quantiles(&quantiles, start_ns, end_ns, stride_ns, &metric_name);
        if result.is_empty() {
            return Err(QueryError::MetricNotFound(format!(
                "No histogram data found for {}",
                metric_name
            )));
        }
        Ok(QueryResult::Matrix { result })
    }

    fn handle_histogram_heatmap(
        &self,
        query_str: &str,
        start: f64,
        end: f64,
    ) -> Result<QueryResult, QueryError> {
        let inner = &query_str["histogram_heatmap(".len()..query_str.len() - 1];
        let (metric_selector, stride_ns) = parse_optional_stride(inner.trim())?;
        let (metric_name, labels) = self.parse_metric_selector(metric_selector)?;

        let start_ns = (start * 1e9) as u64;
        let end_ns = (end * 1e9) as u64;
        let stream = self
            .source
            .histogram_stream(&metric_name, &labels, start_ns, end_ns)
            .ok_or_else(|| QueryError::MetricNotFound(metric_name.clone()))?;
        let histograms = stream_to_histograms(stream);
        let result = histograms.heatmap(start_ns, end_ns, stride_ns);
        match result {
            Some(result) => Ok(QueryResult::HistogramHeatmap { result }),
            None => Err(QueryError::MetricNotFound(format!(
                "No histogram data found for {metric_name}"
            ))),
        }
    }

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

        let start_ns = (start * 1e9) as u64;
        let end_ns = (end * 1e9) as u64;
        let stream = self
            .source
            .histogram_stream(&metric_name, &labels, start_ns, end_ns)
            .ok_or_else(|| QueryError::MetricNotFound(metric_name.clone()))?;
        let histograms = stream_to_histograms(stream);
        let result = histograms.irate(group_by.as_ref(), start_ns, end_ns, &metric_name);
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

        let start_ns = (start * 1e9) as u64;
        let end_ns = (end * 1e9) as u64;
        let stream = self
            .source
            .histogram_stream(&metric_name, &labels, start_ns, end_ns)
            .ok_or_else(|| QueryError::MetricNotFound(metric_name.clone()))?;
        let histograms = stream_to_histograms(stream);
        let group = group_by.as_ref();
        let result = match func {
            "histogram_mean" => {
                histograms.mean(group, start_ns, end_ns, stride_ns, &metric_name)
            }
            "histogram_sum" => {
                histograms.sum(group, start_ns, end_ns, stride_ns, &metric_name)
            }
            _ => histograms.count(group, start_ns, end_ns, stride_ns, &metric_name),
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
        if query_str.starts_with("histogram_quantiles(") && query_str.ends_with(")") {
            return self.handle_histogram_quantiles(query_str, start, end);
        }

        if query_str.starts_with("histogram_heatmap(") && query_str.ends_with(")") {
            return self.handle_histogram_heatmap(query_str, start, end);
        }

        let rewritten = unwrap_sum_around_histogram(query_str);
        let query_str: &str = rewritten.as_deref().unwrap_or(query_str);

        for func in ["histogram_mean", "histogram_count", "histogram_sum"] {
            if let Some((inner, group_by)) = parse_histogram_call(func, query_str)? {
                return self.handle_histogram_scalar(func, inner, group_by, start, end);
            }
        }
        if let Some((inner, group_by)) = parse_histogram_call("histogram_irate", query_str)? {
            return self.handle_histogram_irate(inner, group_by, start, end);
        }

        match parser::parse(query_str) {
            Ok(expr) => self.evaluate_expr(&expr, start, end, step),
            Err(err) => {
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
