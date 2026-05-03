use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use promql_parser::label::Matcher;
use promql_parser::parser::{self, Expr};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tsdb::{Labels, Tsdb};

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
///
/// Optionally hosts a [`DispatchConfig`] to route incoming queries through
/// the SQL twin per the catalogue's `mode`. Without one, behavior is
/// unchanged from the pre-dispatcher engine.
pub struct QueryEngine<T: Deref<Target = Tsdb> = Arc<Tsdb>> {
    tsdb: T,
    dispatch: Option<DispatchConfig>,
}

/// Routing configuration for a `QueryEngine`. Built once and shared across
/// every query the engine serves. See `metriken_query::dispatch` for the
/// `Catalogue` / `SqlBackend` / `DispatchObserver` types.
pub struct DispatchConfig {
    pub catalogue: crate::dispatch::Catalogue,
    pub backend: Arc<dyn crate::dispatch::SqlBackend>,
    pub observer: Arc<dyn crate::dispatch::DispatchObserver>,
    /// The parquet path (or glob) the SQL backend reads from. Substituted
    /// into the `{fixture_path}` placeholder in catalogue SQL templates.
    pub data_source: String,
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

impl<T: Deref<Target = Tsdb>> QueryEngine<T> {
    pub fn new(tsdb: T) -> Self {
        Self {
            tsdb,
            dispatch: None,
        }
    }

    /// Attach (or replace) a dispatcher that routes queries through the
    /// catalogue. Without one, every query goes through the PromQL path.
    pub fn with_dispatch(mut self, config: DispatchConfig) -> Self {
        self.dispatch = Some(config);
        self
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

    pub fn query_range(
        &self,
        query_str: &str,
        start: f64,
        end: f64,
        step: f64,
    ) -> Result<QueryResult, QueryError> {
        // Catalogue-driven dispatch — only when a `DispatchConfig` was
        // attached via `with_dispatch`. The pre-dispatcher path is just
        // `query_range_promql` directly.
        if let Some(dispatch) = &self.dispatch {
            if let Some((entry, captures)) = dispatch.catalogue.lookup(query_str) {
                use crate::dispatch::{canonicalise, Diff, Mode};
                let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
                // METRIKEN_FORCE_PRIMARY=1 promotes every catalogue entry to
                // Mode::Primary at runtime — useful for "what does the viewer
                // look like backed purely by SQL/DuckDB" experiments without
                // editing queries.toml. The catalogue's per-entry `mode` is
                // ignored when this is set.
                let effective_mode = if std::env::var("METRIKEN_FORCE_PRIMARY").is_ok() {
                    Mode::Primary
                } else {
                    entry.mode
                };
                match effective_mode {
                    Mode::Off => self.query_range_promql(query_str, start, end, step),
                    Mode::Shadow => {
                        let p_t0 = std::time::Instant::now();
                        let promql = self.query_range_promql(query_str, start, end, step)?;
                        let promql_ms = ms(p_t0.elapsed());

                        // Best-effort SQL: a backend error in shadow mode is
                        // logged via the observer but does not fail the request.
                        let s_t0 = std::time::Instant::now();
                        let sql_outcome = dispatch.backend.run(
                            entry,
                            &captures,
                            &dispatch.data_source,
                            start,
                            end,
                            step,
                        );
                        let sql_ms = ms(s_t0.elapsed());

                        match sql_outcome {
                            Ok(sql) => {
                                let p = canonicalise(&promql);
                                let s = canonicalise(&sql);
                                if p != s {
                                    dispatch.observer.on_diff(
                                        entry,
                                        &Diff { promql: p, sql: s },
                                    );
                                }
                                dispatch.observer.on_dispatch(
                                    entry,
                                    Mode::Shadow,
                                    Some(promql_ms),
                                    Some(sql_ms),
                                );
                            }
                            Err(err) => {
                                let p = canonicalise(&promql);
                                let s = serde_json::json!({ "sql_error": err.to_string() });
                                dispatch.observer.on_diff(
                                    entry,
                                    &Diff { promql: p, sql: s },
                                );
                                dispatch.observer.on_dispatch(
                                    entry,
                                    Mode::Shadow,
                                    Some(promql_ms),
                                    None,
                                );
                            }
                        }
                        Ok(promql)
                    }
                    Mode::Strict => {
                        let p_t0 = std::time::Instant::now();
                        let promql = self.query_range_promql(query_str, start, end, step)?;
                        let promql_ms = ms(p_t0.elapsed());

                        let s_t0 = std::time::Instant::now();
                        let sql = dispatch
                            .backend
                            .run(entry, &captures, &dispatch.data_source, start, end, step)
                            .map_err(|e| QueryError::EvaluationError(format!("strict-mode SQL: {e}")))?;
                        let sql_ms = ms(s_t0.elapsed());

                        let p = canonicalise(&promql);
                        let s = canonicalise(&sql);
                        let diverged = p != s;
                        if diverged {
                            dispatch.observer.on_diff(entry, &Diff { promql: p, sql: s });
                        }
                        dispatch.observer.on_dispatch(
                            entry,
                            Mode::Strict,
                            Some(promql_ms),
                            Some(sql_ms),
                        );
                        if diverged {
                            return Err(QueryError::EvaluationError(format!(
                                "strict-mode divergence on {}: PromQL and SQL outputs differ",
                                entry.id
                            )));
                        }
                        Ok(promql)
                    }
                    Mode::Primary => {
                        let s_t0 = std::time::Instant::now();
                        let result = dispatch
                            .backend
                            .run(entry, &captures, &dispatch.data_source, start, end, step)
                            .map_err(|e| QueryError::EvaluationError(e.to_string()));
                        let sql_ms = ms(s_t0.elapsed());
                        dispatch.observer.on_dispatch(
                            entry,
                            Mode::Primary,
                            None,
                            if result.is_ok() { Some(sql_ms) } else { None },
                        );
                        result
                    }
                }
            } else {
                self.query_range_promql(query_str, start, end, step)
            }
        } else {
            self.query_range_promql(query_str, start, end, step)
        }
    }

    /// Direct PromQL evaluation, bypassing the dispatcher. Useful inside
    /// the dispatcher itself (for shadow-mode comparison) and as the
    /// fallback path when no dispatcher is attached.
    pub fn query_range_promql(
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
