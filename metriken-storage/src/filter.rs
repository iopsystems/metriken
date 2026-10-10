//! [`KeepMetrics`]: which columns of an archive's segments survive a copy
//! trimmed to some metrics (dendro's `CopySpec::keep_columns`).
//!
//! dendro's `ColumnFilter` sees one field at a time, so the rules for what a
//! segment cannot do without live here, beside the formats they describe:
//! the time columns and window sidecars of every table, a long table's
//! `occupant` column, and the occupant streams, which are copied whole.

use std::collections::BTreeSet;

use crate::occupants;
use crate::table::{WALL_OFFSET_COLUMN, WINDOW_BEGIN_COLUMN, WINDOW_WIDTH_COLUMN};
use arrow::datatypes::Field;
use dendro::rewrite::ColumnFilter;

/// A long table's column naming each row's occupant.
const OCCUPANT_COLUMN: &str = "occupant";

/// Keep the columns of the named metrics, and every column a segment needs
/// to place its rows: its timestamps, its windows, its occupants.
///
/// A value column is kept when its name, the part of its name before `:`
/// (`latency` for `latency:buckets`), or its `metric` metadata is one of the
/// metrics. A per-metric window sidecar (`<m>:window_begin`) goes with its
/// metric. A table left with no value column is dropped by the copy. An
/// occupant stream is copied whole; one whose table was dropped is the
/// caller's to remove.
pub struct KeepMetrics<'a> {
    metrics: &'a BTreeSet<String>,
}

impl<'a> KeepMetrics<'a> {
    pub fn new(metrics: &'a BTreeSet<String>) -> Self {
        Self { metrics }
    }
}

fn is_structural(name: &str) -> bool {
    name == "timestamp"
        || name == WALL_OFFSET_COLUMN
        || name == WINDOW_BEGIN_COLUMN
        || name == WINDOW_WIDTH_COLUMN
        || name == OCCUPANT_COLUMN
}

/// The metric a per-metric window sidecar belongs to; `None` for the
/// table-level pair and for every other column.
fn window_owner(name: &str) -> Option<&str> {
    name.strip_suffix(":window_begin")
        .or_else(|| name.strip_suffix(":window_width"))
        .filter(|base| !base.is_empty())
}

impl ColumnFilter for KeepMetrics<'_> {
    fn keep(&self, field: &Field) -> bool {
        let name = field.name();
        if is_structural(name) {
            return true;
        }
        if let Some(metric) = window_owner(name) {
            return self.metrics.contains(metric);
        }
        self.metrics.contains(name)
            || name
                .split_once(':')
                .is_some_and(|(base, _)| self.metrics.contains(base))
            || field
                .metadata()
                .get("metric")
                .is_some_and(|m| self.metrics.contains(m))
    }

    fn is_data(&self, field: &Field) -> bool {
        let name = field.name();
        !is_structural(name) && window_owner(name).is_none()
    }

    fn projects(&self, stream: &str) -> bool {
        occupants::table_of(stream).is_none()
    }
}
