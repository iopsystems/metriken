//! Summary-style exposition of a histogram through `prometheus_text`.

use metriken::{metric, AtomicHistogram};
use metriken_exposition::{prometheus_text, PrometheusOptions};

#[metric(name = "prometheus_summary_probe")]
static PROBE: AtomicHistogram = AtomicHistogram::new(7, 64);

#[test]
fn percentiles_are_written_as_quantile_lines() {
    for v in 1..=100 {
        PROBE.increment(v).unwrap();
    }
    let body = prometheus_text(&PrometheusOptions::default().with_percentiles(vec![0.5, 0.99]));
    let lines: Vec<&str> = body
        .lines()
        .filter(|l| l.starts_with("prometheus_summary_probe"))
        .collect();
    assert_eq!(
        lines,
        vec![
            "prometheus_summary_probe{quantile=\"0.5\"} 50",
            "prometheus_summary_probe{quantile=\"0.99\"} 99",
            "prometheus_summary_probe_count 100",
            "prometheus_summary_probe_sum 5050",
        ],
        "{body}"
    );
}
