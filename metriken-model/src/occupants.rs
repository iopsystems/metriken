//! The occupant stream's rows: which labels each occupant number of a long
//! table stands for, as a writer stages them and a stream carries them. The
//! stream's parquet encoding is storage's (`metriken-storage`'s `occupants`
//! module).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The suffix that makes a stream a table's occupant stream.
pub const SUFFIX: &str = "/occupants";

/// `table`'s occupant stream.
pub fn stream_of(table: &str) -> String {
    format!("{table}{SUFFIX}")
}

/// The table an occupant stream belongs to, if `stream` is one.
pub fn table_of(stream: &str) -> Option<&str> {
    stream.strip_suffix(SUFFIX)
}

/// One occupant and its labels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occupant {
    pub occupant: u64,
    pub labels: BTreeMap<String, String>,
}

/// A WAL row of an occupant stream: the occupants first seen or restated
/// at one tick, msgpack.
pub fn encode_wal_row(occupants: &[Occupant]) -> Vec<u8> {
    rmp_serde::to_vec(occupants).expect("occupant rows serialize")
}

pub fn decode_wal_row(row: &[u8]) -> Result<Vec<Occupant>, String> {
    rmp_serde::from_slice(row).map_err(|e| format!("decoding an occupant WAL row: {e}"))
}
