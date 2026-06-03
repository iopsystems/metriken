//! Test fixture builders for parquet files.
//!
//! Available with the `fixtures` feature flag (or in test builds). Use
//! [`FixtureBuilder`] for synthetic deterministic fixtures, and
//! [`ParquetAugmentor`] (added separately) to scale up real captures.

mod augment;
mod synthetic;

pub use augment::ParquetAugmentor;
pub use synthetic::{Fixture, FixtureBuilder};
