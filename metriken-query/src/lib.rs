pub(crate) mod labels;
#[cfg(test)]
pub(crate) mod memory;
pub mod parquet;
pub(crate) mod promql;
pub(crate) mod types;

pub use parquet::Parquet;
pub use promql::{QueryError, QueryResult};

use labels::Labels;
use types::{Counters, Gauges, Histograms};

pub(crate) trait DataSource: Send + Sync {
    fn counters(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Counters>;
    fn gauges(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Gauges>;
    fn histograms(&self, name: &str, filter: &Labels, start_ns: u64, end_ns: u64) -> Option<Histograms>;
    /// Sampling interval in seconds.
    fn interval(&self) -> f64;
    /// Full time extent of the stored data in nanoseconds, or `None` if empty.
    fn time_range(&self) -> Option<(u64, u64)>;
    /// Parquet column name for every `(metric_name, labels)` pair.
    #[cfg(test)]
    fn column_map(&self) -> std::collections::HashMap<String, std::collections::HashMap<Labels, String>>;
}
