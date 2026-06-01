//! Test fixture builders for parquet files.
//!
//! Available with the `fixtures` feature flag (or in test builds). Use
//! [`FixtureBuilder`] for synthetic deterministic fixtures, and
//! [`ParquetAugmentor`] (added separately) to scale up real captures.

mod synthetic;
mod augment;

pub use synthetic::{Fixture, FixtureBuilder};
pub use augment::ParquetAugmentor;
