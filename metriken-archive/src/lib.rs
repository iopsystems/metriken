//! The archive reader and writer are `metriken-storage`'s; this crate
//! re-exports that crate for one release. A query over an
//! [`ArchiveReader`] goes through `metriken_query::MetricsSource`, which
//! `metriken-query` implements for it.

pub use metriken_storage::*;
