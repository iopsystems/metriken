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
