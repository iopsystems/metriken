use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use crate::promql::{QueryEngine, QueryError, QueryResult};
use crate::tsdb::ParquetFile;

/// A query engine that reads directly from a parquet file, materializing only
/// the time window needed for each query. Holds the file open and reuses parquet
/// metadata across queries; actual column data is decoded per-query.
pub struct ParquetQueryEngine {
    pf: Arc<ParquetFile>,
}

impl ParquetQueryEngine {
    pub fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        let pf = Arc::new(ParquetFile::open(path)?);
        Ok(Self { pf })
    }

    /// Execute a PromQL range query over `[start_s, end_s]` with `step_s` resolution.
    /// Times are in seconds (f64), matching the `QueryEngine::query_range` interface.
    pub fn query_range(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
    ) -> Result<QueryResult, QueryError> {
        // Add one sampling interval on each side so histogram `prev` state and
        // range-selector lookback windows are primed before the query window.
        let interval_s = self.pf.sampling_interval_ms as f64 / 1000.0;
        let start_ns = ((start_s - interval_s).max(0.0) * 1e9) as u64;
        let end_ns = ((end_s + interval_s) * 1e9) as u64;

        let tsdb = self
            .pf
            .load_range(start_ns, end_ns)
            .map_err(|e| QueryError::EvaluationError(e.to_string()))?;

        QueryEngine::new(Arc::new(tsdb)).query_range(expr, start_s, end_s, step_s)
    }

    /// Time range of data in the file, derived from row-group statistics without
    /// decoding column data. Returns `(start_s, end_s)` in seconds, or `None`
    /// if the file is empty or has no statistics.
    pub fn get_time_range(&self) -> Option<(f64, f64)> {
        self.pf
            .time_range_from_stats()
            .map(|(min_ns, max_ns)| (min_ns as f64 / 1e9, max_ns as f64 / 1e9))
    }
}
