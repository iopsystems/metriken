use std::collections::HashMap;
use std::fmt::Write;

use metriken::{MetricEntry, Value};

/// Options for Prometheus text format rendering.
pub struct PrometheusOptions {
    /// Include `# HELP` lines when metric descriptions are available.
    pub help_text: bool,
    /// Percentiles to compute for histogram summaries (0.0-1.0 scale).
    /// If empty, full cumulative bucket exposition is used instead.
    pub percentiles: Vec<f64>,
}

impl Default for PrometheusOptions {
    fn default() -> Self {
        Self {
            help_text: true,
            percentiles: Vec::new(),
        }
    }
}

impl PrometheusOptions {
    /// Use summary-style histogram exposition with these percentiles.
    pub fn with_percentiles(mut self, percentiles: Vec<f64>) -> Self {
        self.percentiles = percentiles;
        self
    }

    /// Disable `# HELP` lines.
    pub fn without_help(mut self) -> Self {
        self.help_text = false;
        self
    }
}

/// Render all registered metriken metrics in Prometheus text exposition format.
///
/// This walks the global metric registry and produces a complete Prometheus
/// response body. Group metrics are exploded into individual labeled series.
///
/// # Example
/// ```no_run
/// use metriken_exposition::prometheus_text;
/// use metriken_exposition::PrometheusOptions;
///
/// let body = prometheus_text(&PrometheusOptions::default());
/// ```
pub fn prometheus_text(options: &PrometheusOptions) -> String {
    let mut output = String::new();

    for metric in &metriken::metrics() {
        let name = sanitize_name(metric.name());

        match metric.value() {
            Some(Value::Counter(value)) => {
                write_type_help(&mut output, &name, "counter", metric, options);
                write_metric_line(&mut output, &name, None, &value.to_string());
            }
            Some(Value::Gauge(value)) => {
                write_type_help(&mut output, &name, "gauge", metric, options);
                write_metric_line(&mut output, &name, None, &value.to_string());
            }
            Some(Value::Histogram(h)) => {
                if let Some(snapshot) = h.load() {
                    write_histogram(&mut output, &name, None, &snapshot, metric, options);
                }
            }
            Some(Value::CounterGroup(g)) => {
                let base_metadata = entry_metadata(metric);
                if let Some(values) = g.load_counters() {
                    let mut first = true;
                    for (idx, &value) in values.iter().enumerate() {
                        let labels = merge_labels(&base_metadata, g.load_metadata(idx));
                        if first {
                            write_type_help(&mut output, &name, "counter", metric, options);
                            first = false;
                        }
                        write_metric_line(&mut output, &name, Some(&labels), &value.to_string());
                    }
                }
            }
            Some(Value::GaugeGroup(g)) => {
                let base_metadata = entry_metadata(metric);
                if let Some(values) = g.load_gauges() {
                    let mut first = true;
                    for (idx, &value) in values.iter().enumerate() {
                        let labels = merge_labels(&base_metadata, g.load_metadata(idx));
                        if first {
                            write_type_help(&mut output, &name, "gauge", metric, options);
                            first = false;
                        }
                        write_metric_line(&mut output, &name, Some(&labels), &value.to_string());
                    }
                }
            }
            Some(Value::HistogramGroup(g)) => {
                let base_metadata = entry_metadata(metric);
                if let Some(hists) = g.load_all_histograms() {
                    for (idx, snapshot) in hists.iter().enumerate() {
                        let labels = merge_labels(&base_metadata, g.load_metadata(idx));
                        write_histogram(
                            &mut output,
                            &name,
                            Some(&labels),
                            snapshot,
                            metric,
                            options,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    output
}

fn write_type_help(
    output: &mut String,
    name: &str,
    kind: &str,
    metric: &MetricEntry,
    options: &PrometheusOptions,
) {
    if options.help_text {
        if let Some(description) = metric.description() {
            let _ = writeln!(output, "# HELP {name} {description}");
        }
    }
    let _ = writeln!(output, "# TYPE {name} {kind}");
}

fn write_metric_line(output: &mut String, name: &str, labels: Option<&str>, value: &str) {
    match labels {
        Some(l) if !l.is_empty() => {
            let _ = writeln!(output, "{name}{{{l}}} {value}");
        }
        _ => {
            let _ = writeln!(output, "{name} {value}");
        }
    }
}

fn write_histogram(
    output: &mut String,
    name: &str,
    labels: Option<&str>,
    snapshot: &histogram::Histogram,
    metric: &MetricEntry,
    options: &PrometheusOptions,
) {
    if !options.percentiles.is_empty() {
        // Summary-style: emit percentile gauges
        write_type_help(output, name, "summary", metric, options);

        if let Ok(Some(results)) = snapshot.percentiles(&options.percentiles) {
            for (percentile, bucket) in results {
                let value = bucket.end();
                let quantile_label = format!("quantile=\"{percentile}\"");
                let combined = match labels {
                    Some(l) if !l.is_empty() => format!("{l}, {quantile_label}"),
                    _ => quantile_label,
                };
                write_metric_line(output, name, Some(&combined), &value.to_string());
            }
        }

        // count and sum
        let mut count: u64 = 0;
        let mut sum: u128 = 0;
        for bucket in snapshot {
            let c = bucket.count();
            count += c;
            sum += c as u128 * ((bucket.start() as u128 + bucket.end() as u128) / 2);
        }
        write_metric_line(output, &format!("{name}_count"), labels, &count.to_string());
        write_metric_line(output, &format!("{name}_sum"), labels, &sum.to_string());
    } else {
        // Full cumulative bucket exposition
        write_type_help(output, name, "histogram", metric, options);

        let mut count: u64 = 0;
        let mut sum: u128 = 0;
        for bucket in snapshot {
            let c = bucket.count();
            sum += c as u128 * bucket.end() as u128;
            count += c;
            let le_label = format!("le=\"{}\"", bucket.end());
            let combined = match labels {
                Some(l) if !l.is_empty() => format!("{l}, {le_label}"),
                _ => le_label,
            };
            write_metric_line(
                output,
                &format!("{name}_bucket"),
                Some(&combined),
                &count.to_string(),
            );
        }
        let inf_label = "le=\"+Inf\"".to_string();
        let combined = match labels {
            Some(l) if !l.is_empty() => format!("{l}, {inf_label}"),
            _ => inf_label,
        };
        write_metric_line(
            output,
            &format!("{name}_bucket"),
            Some(&combined),
            &count.to_string(),
        );
        write_metric_line(output, &format!("{name}_count"), labels, &count.to_string());
        write_metric_line(output, &format!("{name}_sum"), labels, &sum.to_string());
    }
}

/// Extract metadata key-value pairs from a metric entry.
fn entry_metadata(metric: &MetricEntry) -> HashMap<String, String> {
    metric
        .metadata()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Merge base metadata from the metric entry with per-index group metadata,
/// and format as a Prometheus label string.
fn merge_labels(
    base: &HashMap<String, String>,
    index_meta: Option<HashMap<String, String>>,
) -> String {
    let mut all = base.clone();
    if let Some(meta) = index_meta {
        all.extend(meta);
    }
    format_labels(&all)
}

/// Format a metadata map as a sorted Prometheus label string.
fn format_labels(metadata: &HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = metadata
        .iter()
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect();
    pairs.sort();
    pairs.join(", ")
}

/// Sanitize a metric name for Prometheus compatibility.
///
/// Prometheus metric names must match `[a-zA-Z_:][a-zA-Z0-9_:]*`.
fn sanitize_name(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == ':' {
            result.push(c);
        } else {
            result.push('_');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_name() {
        assert_eq!(sanitize_name("simple"), "simple");
        assert_eq!(sanitize_name("with/slash"), "with_slash");
        assert_eq!(sanitize_name("with.dots"), "with_dots");
        assert_eq!(sanitize_name("ok_under_score"), "ok_under_score");
        assert_eq!(sanitize_name("has:colon"), "has:colon");
    }

    #[test]
    fn test_format_labels() {
        let mut meta = HashMap::new();
        meta.insert("b".into(), "2".into());
        meta.insert("a".into(), "1".into());
        assert_eq!(format_labels(&meta), "a=\"1\", b=\"2\"");
    }

    #[test]
    fn test_format_labels_empty() {
        let meta = HashMap::new();
        assert_eq!(format_labels(&meta), "");
    }

    #[test]
    fn test_write_metric_line_no_labels() {
        let mut out = String::new();
        write_metric_line(&mut out, "my_counter", None, "42");
        assert_eq!(out, "my_counter 42\n");
    }

    #[test]
    fn test_write_metric_line_with_labels() {
        let mut out = String::new();
        write_metric_line(&mut out, "my_counter", Some("cpu=\"0\""), "42");
        assert_eq!(out, "my_counter{cpu=\"0\"} 42\n");
    }

    #[test]
    fn test_write_metric_line_empty_labels() {
        let mut out = String::new();
        write_metric_line(&mut out, "my_counter", Some(""), "42");
        assert_eq!(out, "my_counter 42\n");
    }
}
