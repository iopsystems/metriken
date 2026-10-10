//! The `.rez` archive format (feature `rez`): the container (v1/v2 tar and
//! v3 SQLite), its writers, the `.rez`-to-dendro conversion, and opening an
//! archive by content. Moved from rezolus so every consumer reads `.rez`
//! through one crate while the format is retired in favour of dendro.

pub mod catalog;
#[cfg(test)]
mod dendro_compat;
#[cfg(test)]
mod dendro_replicate_contract;
pub mod file_id;
pub mod open;
pub mod parquet_ingest;
#[allow(clippy::module_inception)]
pub mod rez;
pub mod rez_sqlite;
/// The tar (v1/v2) `.rez` writer, kept so tests can build v1/v2 fixtures.
/// Nothing ships that writes this container any more.
#[cfg(any(test, feature = "test-support"))]
pub mod rez_stream;
/// v3 rewrite tooling (`combine`/`filter`/`upgrade`) and report assembly.
pub mod rez_v3_rewrite;
/// The v3 streaming writer.
#[cfg(feature = "write")]
pub mod rez_v3_writer;
pub mod seal_policy;
/// A v3 `.rez` rewritten as a dendro archive.
#[cfg(feature = "write")]
pub mod to_dendro;
pub mod wal;
pub mod wire;
