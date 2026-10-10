//! A group snapshot's cost against a writer's segment byte budget, and the
//! snapshot as a write-ahead-log row. Both are `metriken-model`'s.

pub use metriken_model::convert::wal_group_row;
pub use metriken_model::cost::group_approx_bytes;
