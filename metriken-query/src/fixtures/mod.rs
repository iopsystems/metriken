//! Test fixture builders for parquet files.
//!
//! Available with the `fixtures` feature flag (or in test builds). Use
//! [`FixtureBuilder`] for synthetic deterministic fixtures, and
//! [`ParquetAugmentor`] (added separately) to scale up real captures.

mod augment;
mod synthetic;

pub use augment::ParquetAugmentor;
pub use synthetic::{Fixture, FixtureBuilder};

/// The most points the union stages of the batched rate reads on this
/// thread held at once (summed over each read's partitions, and over the
/// reads since the last call), reset to zero. A memory probe for tests of
/// process-group queries.
#[doc(hidden)]
pub fn take_union_held_peak() -> usize {
    crate::batch_rate::UNION_HELD_PEAK.with(|p| p.replace(0))
}

/// The most rows the union stages of the batched rate reads on this thread
/// held at once to compare a process group's sources (summed over each
/// read's partitions, and over the reads since the last call), reset to
/// zero.
#[doc(hidden)]
pub fn take_union_rows_held_peak() -> usize {
    crate::batch_rate::UNION_ROWS_HELD_PEAK.with(|p| p.replace(0))
}

/// Batched rate reads on this thread that gave up and left the query to
/// the per-series path because a series ended as silent came back (since
/// the last call), reset to zero.
#[doc(hidden)]
pub fn take_batch_abandoned() -> usize {
    crate::batch_rate::BATCH_ABANDONED.with(|p| p.replace(0))
}

/// Blocks of grid points (1024 each, per group) the last grouped display
/// read on this thread had passed to its reducers when its last chunk was
/// read, reset to zero: what the flush freed before the scan ended.
#[doc(hidden)]
pub fn take_fed_before_end() -> usize {
    crate::batch_rate::FED_BEFORE_END.with(|p| p.replace(0))
}
