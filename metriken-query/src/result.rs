//! Result types shared by both query backends.
//!
//! The legacy in-memory PromQL evaluator (gated under feature `legacy`)
//! and the DuckDB-backed SQL pipeline (gated under feature `sql`) both
//! produce `QueryResult`s. Keeping the type definitions here, outside
//! either feature gate, lets callers depend on the shape without
//! committing to a backend.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

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

/// A single sample in the result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub metric: HashMap<String, String>,
    /// `(timestamp_seconds, value)`.
    pub value: (f64, f64),
}

/// A matrix sample with multiple values over time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixSample {
    pub metric: HashMap<String, String>,
    /// `Vec<(timestamp_seconds, value)>`.
    pub values: Vec<(f64, f64)>,
}

/// Histogram heatmap data for visualization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistogramHeatmapResult {
    /// Timestamps in seconds.
    pub timestamps: Vec<f64>,
    /// Bucket boundaries (latency values in the histogram's unit, e.g.
    /// nanoseconds).
    pub bucket_bounds: Vec<u64>,
    /// Heatmap data as `[(time_index, bucket_index, count)]`.
    pub data: Vec<(usize, usize, f64)>,
    /// Minimum count value (for color scaling).
    pub min_value: f64,
    /// Maximum count value (for color scaling).
    pub max_value: f64,
}

/// Top-level result envelope returned by both query backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resultType", rename_all = "camelCase")]
pub enum QueryResult {
    #[serde(rename = "vector")]
    Vector { result: Vec<Sample> },

    #[serde(rename = "matrix")]
    Matrix { result: Vec<MatrixSample> },

    #[serde(rename = "scalar")]
    /// `(timestamp, value)`.
    Scalar { result: (f64, f64) },

    #[serde(rename = "histogram_heatmap")]
    HistogramHeatmap { result: HistogramHeatmapResult },
}
