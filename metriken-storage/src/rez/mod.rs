//! The `.rez` archive format (feature `rez`), read only: the container (v1/v2
//! tar and v3 SQLite), the `.rez`-to-dendro conversion, and opening an
//! archive by content. Moved from rezolus so every consumer reads `.rez`
//! through one crate. Nothing writes a `.rez` any more; recordings are
//! written as dendro archives, and every `.rez` already written stays
//! readable.

pub mod catalog;
#[cfg(test)]
mod dendro_replicate_contract;
pub mod file_id;
pub mod open;
#[allow(clippy::module_inception)]
pub mod rez;
pub mod rez_sqlite;
#[cfg(feature = "write")]
pub mod to_dendro;
pub mod wal;
pub mod wire;
