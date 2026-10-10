//! Growing a wide [`Table`](crate::table::Table) row by row: `TableBuilder`
//! for a table of individually windowed metrics, `GroupTableBuilder` for an
//! acquisition group's table with one window per row.

use std::collections::{HashMap, HashSet};

use tracing::warn;

use crate::table::{Column, Table, Values};
use crate::window::Window;

/// One reading's value, in the three shapes a `.rez` column can hold.
///
/// The histogram variant borrows an H2 histogram rather than bare buckets: a
/// column stores the whole thing, and the `histogram` crate is pure Rust, so
/// carrying it costs the read path nothing.
#[derive(Debug)]
pub enum CellValue<'a> {
    Counter(u64),
    Gauge(i64),
    Histogram(&'a histogram::Histogram),
}

/// One reading in one row, as the archive stores it: a name, the producer's
/// metadata, an optional acquisition window, and a value.
///
/// **The builder's input type, deliberately separate from [`Entry`].** Rows
/// reach a table from two directions — an agent snapshot at ingest, and this
/// archive's own WAL when a live tail is materialized into a segment — and
/// only the first has `metriken-exposition` values behind it. Making the
/// builder speak `Entry` forced the second to rebuild `Counter`/`Gauge`/
/// `Histogram` values it had just decoded, purely to have something to borrow,
/// which put a metrics-registry dependency (and with it `linkme`, which has no
/// wasm32 implementation) on the READ path of the archive format.
pub struct Cell<'a> {
    pub name: &'a str,
    pub metadata: &'a HashMap<String, String>,
    pub window: Option<Window>,
    pub value: CellValue<'a>,
}

impl Cell<'_> {
    /// The `metric_type` string the parquet reader keys on to reconstruct the
    /// column's value shape (counter vs gauge; histograms carry a `:buckets`
    /// suffix, so their `metric_type` is informational).
    fn metric_type(&self) -> &'static str {
        match self.value {
            CellValue::Counter(_) => "counter",
            CellValue::Gauge(_) => "gauge",
            CellValue::Histogram(_) => "histogram",
        }
    }
}

pub use metriken_model::cost::{HISTOGRAM_BUCKET_BYTES, VALUE_SLOT_BYTES, WINDOW_SLOT_BYTES};

/// Per-cell overhead: value slot + window slot.
///
/// **Both slots, and that is the point.** This is a bound on resident memory,
/// so it has to charge what a cell actually costs — a window is pushed for
/// every counted cell, so counting only the value slot understates a scalar
/// cell by more than half and lets a scalar-heavy table run well past the byte
/// cap that is supposed to bound it. `push_row_accumulates_approx_bytes` and
/// `approx_bytes_counts_the_window_slot` pin both halves against the layout.
pub const CELL_OVERHEAD_BYTES: usize = VALUE_SLOT_BYTES + WINDOW_SLOT_BYTES;

/// What one row of `entries` adds to a table's `approx_bytes`.
///
/// **Split out so the byte cap binds identically whether or not a container
/// keeps the rows.** The v2 writer buffers into a `TableBuilder` and gets this
/// figure as a side effect of `push_row`; the v3 writer keeps only the WAL and
/// rebuilds the table at seal time, so it has no builder to ask and must arrive
/// at the same number to seal at the same row. Two spellings of "what a row
/// costs" would drift, and the symptom — two containers producing differently
/// sized segments from identical input — is invisible until someone compares
/// them.
///
/// A cell whose shape does not match its column's established type is skipped
/// by `push_row` and charged nothing here, for the same reason: it is not
/// stored.
pub fn cells_approx_bytes(cells: &[Cell<'_>]) -> usize {
    cells
        .iter()
        .map(|c| match c.value {
            CellValue::Counter(_) | CellValue::Gauge(_) => CELL_OVERHEAD_BYTES,
            CellValue::Histogram(h) => {
                CELL_OVERHEAD_BYTES + h.as_slice().len() * HISTOGRAM_BUCKET_BYTES
            }
        })
        .sum()
}

/// A growing per-sampler table. Columns are sparse: shorter than the row count
/// until padded (a metric absent in some rows gets `None` there).
pub struct TableBuilder {
    sampler: String,
    timestamps: Vec<u64>,
    wall_offsets: Vec<i64>,
    order: Vec<String>,
    columns: HashMap<String, Column>,
    /// A writer's dedup state (see [`last_key`](Self::last_key)).
    last_key: Option<u64>,
    approx_bytes: usize,
}

impl TableBuilder {
    pub fn new(sampler: String) -> Self {
        Self {
            sampler,
            timestamps: Vec::new(),
            wall_offsets: Vec::new(),
            order: Vec::new(),
            columns: HashMap::new(),
            last_key: None,
            approx_bytes: 0,
        }
    }

    /// Approximate in-memory bytes accumulated by the cells pushed so far.
    ///
    /// This is what the streaming writer's seal threshold is measured in.
    /// Serialized parquet size cannot be estimated cheaply — a dry-run encode
    /// is exactly the cost that gets moved off the scrape thread, and static
    /// per-row guesses are off by orders of magnitude for histogram tables — so
    /// the cap bounds the two things it can measure exactly and in O(1) per
    /// entry: the builder's memory footprint and the encoder's input size.
    /// Counts only pushed cells — null back-padding of a late-appearing column
    /// is not accounted, so the number slightly under-reports a sparse table.
    /// Resets with the builder: a fresh builder (a post-rotation segment)
    /// starts at zero.
    /// A writer's seal decision may keep its own count instead (rezolus's
    /// `SegmentAccount` does); this is what such a count is pinned against.
    pub fn approx_bytes(&self) -> usize {
        self.approx_bytes
    }

    /// Rows appended so far (the row-count seal threshold, and the
    /// "never seal an empty builder" test).
    /// Rows appended so far.
    ///
    pub fn rows(&self) -> usize {
        self.timestamps.len()
    }

    /// The dedup key of the last row pushed, for a writer that skips a row
    /// identical to the previous one. The builder only stores it.
    pub fn last_key(&self) -> Option<u64> {
        self.last_key
    }

    pub fn set_last_key(&mut self, key: u64) {
        self.last_key = Some(key);
    }

    /// The columns pushed so far, in no particular order.
    pub fn columns(&self) -> impl Iterator<Item = &Column> {
        self.columns.values()
    }

    /// How many rows `col` holds so far (a sparse column is shorter than the
    /// table until padded).
    pub fn col_len(col: &Column) -> usize {
        match &col.values {
            Values::Counter(v) => v.len(),
            Values::Gauge(v) => v.len(),
            Values::Histogram(v) => v.len(),
        }
    }

    fn pad(col: &mut Column, to: usize) {
        while Self::col_len(col) < to {
            match &mut col.values {
                Values::Counter(v) => v.push(None),
                Values::Gauge(v) => v.push(None),
                Values::Histogram(v) => v.push(None),
            }
            col.windows.push(None);
        }
    }

    /// Append one row: `snapshot_ts` is the row's timestamp and
    /// `wall_offset_ns` the wall-clock observation for that tick (raw
    /// `SystemTime` reading minus `snapshot_ts`), stored once per row in the
    /// table-level `:wall_offset` sidecar.
    pub fn push_row(&mut self, snapshot_ts: u64, wall_offset_ns: i64, cells: &[Cell<'_>]) {
        let row = self.timestamps.len();
        self.timestamps.push(snapshot_ts);
        self.wall_offsets.push(wall_offset_ns);
        let mut added_bytes = 0usize;
        for e in cells {
            let name = e.name.to_string();
            let order = &mut self.order;
            let col = self.columns.entry(name.clone()).or_insert_with(|| {
                order.push(name.clone());
                let values = match e.value {
                    CellValue::Counter(_) => Values::Counter(Vec::new()),
                    CellValue::Gauge(_) => Values::Gauge(Vec::new()),
                    CellValue::Histogram(_) => Values::Histogram(Vec::new()),
                };
                let mut metadata = e.metadata.clone();
                metadata
                    .entry("metric_type".to_string())
                    .or_insert_with(|| e.metric_type().to_string());
                Column {
                    name,
                    metadata,
                    values,
                    windows: Vec::new(),
                }
            });
            Self::pad(col, row);
            // The window is pushed only where the value was: an entry whose
            // shape does not match the column's established type is skipped
            // entirely (an agent restart can remap a numeric id and flip a
            // column from counter to gauge mid-recording). Pushing the window
            // regardless would leave `windows` one longer than `values` and
            // shift every later row's window onto the wrong value.
            let cell_bytes = match (&e.value, &mut col.values) {
                (CellValue::Counter(c), Values::Counter(v)) => {
                    v.push(Some(*c));
                    Some(CELL_OVERHEAD_BYTES)
                }
                (CellValue::Gauge(g), Values::Gauge(v)) => {
                    v.push(Some(*g));
                    Some(CELL_OVERHEAD_BYTES)
                }
                (CellValue::Histogram(h), Values::Histogram(v)) => {
                    // The clone below copies the whole bucket `Box<[u64]>` —
                    // 496 buckets ≈ 4 KB at the `HISTOGRAM_GROUPING_POWER` = 3
                    // that `docs/principles.md` standardizes on. One histogram
                    // cell is ~100x a scalar one, which is why the seal cap
                    // counts bytes rather than rows.
                    let buckets = h.as_slice().len();
                    v.push(Some((*h).clone()));
                    Some(CELL_OVERHEAD_BYTES + buckets * HISTOGRAM_BUCKET_BYTES)
                }
                _ => None,
            };
            if let Some(bytes) = cell_bytes {
                col.windows.push(e.window);
                added_bytes += bytes;
            }
        }
        self.approx_bytes += added_bytes;
    }

    /// Return a finished column's growth slack to the allocator.
    ///
    /// Every column here was built by repeated `push`, so its capacity is a
    /// power-of-two ceiling over its length. `approx_bytes` counts *pushed
    /// cells*, so the seal cap is honest about the data and silent about the
    /// slack, and a table can hand the writer up to twice what the cap allowed.
    /// Padding is already done by the time this runs, so the length is final
    /// and the shrink is exact.
    ///
    /// Pre-sizing the columns instead is worse: `Vec::with_capacity(max_rows)`
    /// over-allocates badly for exactly the wide tables that seal on the byte
    /// cap long before they reach the row cap.
    fn shrink(col: &mut Column) {
        match &mut col.values {
            Values::Counter(v) => v.shrink_to_fit(),
            Values::Gauge(v) => v.shrink_to_fit(),
            Values::Histogram(v) => v.shrink_to_fit(),
        }
        col.windows.shrink_to_fit();
    }

    /// Consume the builder into the table the writer will encode.
    ///
    /// **Shrinking here targets the seal-time peak, not the accumulation
    /// footprint.** An open builder keeps its slack for as long as it is open —
    /// that is what makes the pushes amortized-O(1) — and what this reclaims is
    /// the slack on a table about to sit in the writer's channel and then be
    /// encoded, the moment when the batch, the arrow copy and the parquet
    /// output buffer are all resident together.
    ///
    /// This runs on the scrape thread, so its cost is bounded deliberately: one
    /// realloc-and-copy per column, and the transient double-allocation a
    /// shrink needs is one column's worth, never the whole table's.
    pub fn finish(mut self) -> Table {
        let rows = self.timestamps.len();
        let columns = self
            .order
            .iter()
            .map(|name| {
                let mut col = self.columns.remove(name).unwrap();
                Self::pad(&mut col, rows);
                Self::shrink(&mut col);
                col
            })
            .collect();
        self.timestamps.shrink_to_fit();
        self.wall_offsets.shrink_to_fit();
        Table {
            sampler: self.sampler,
            timestamps: self.timestamps,
            wall_offsets: self.wall_offsets,
            columns,
            table_window: None,
        }
    }
}

/// A growing V3 acquisition-group table: like [`TableBuilder`], but rows
/// carry ONE table-level acquisition window (`Table::table_window`)
/// instead of a window per metric, and membership per row comes from a
/// [`GroupSchema`] rather than from an `Entry` list. Columns are still
/// sparse and keyed by name — a schema-hash change mid-table (a cgroup
/// added/removed) is handled the same lazy-padding way `TableBuilder`
/// handles a metric appearing or vanishing.
pub struct GroupTableBuilder {
    name: String,
    timestamps: Vec<u64>,
    wall_offsets: Vec<i64>,
    windows: Vec<Option<Window>>,
    order: Vec<String>,
    columns: HashMap<String, Column>,
    /// Descriptor name -> the key in `columns` its CURRENT generation writes
    /// to. A relabeled slot keeps its descriptor name and gets a new key here,
    /// so later rows for that slot find the new column rather than the old
    /// one. See `get_or_create`.
    live: HashMap<String, String>,
    /// Members whose histogram failed to rebuild and have already been
    /// reported. A malformed cell usually repeats every tick, so without this
    /// one bad member would log per row for the life of the segment.
    reported_bad_histograms: HashSet<String>,
}

impl GroupTableBuilder {
    pub fn new(name: String) -> Self {
        Self {
            name,
            live: HashMap::new(),
            reported_bad_histograms: HashSet::new(),
            timestamps: Vec::new(),
            wall_offsets: Vec::new(),
            windows: Vec::new(),
            order: Vec::new(),
            columns: HashMap::new(),
        }
    }

    /// Rows pushed so far — lets a caller that skipped some input rows (an
    /// un-anchored WAL tail) tell "nothing pushed at all" from "pushed a
    /// partial table" without inspecting the finished `Table`.
    pub fn rows(&self) -> usize {
        self.timestamps.len()
    }

    fn col_len(col: &Column) -> usize {
        match &col.values {
            Values::Counter(v) => v.len(),
            Values::Gauge(v) => v.len(),
            Values::Histogram(v) => v.len(),
        }
    }

    /// Pad a column up to `to` rows. Unlike `TableBuilder::pad`, this never
    /// touches `col.windows` — a group table's per-column `windows` stays
    /// empty for its whole life; the window lives on the table, not the
    /// column (see `Table::table_window`).
    fn pad(col: &mut Column, to: usize) {
        while Self::col_len(col) < to {
            match &mut col.values {
                Values::Counter(v) => v.push(None),
                Values::Gauge(v) => v.push(None),
                Values::Histogram(v) => v.push(None),
            }
        }
    }

    /// The column a descriptor writes into, creating it on first sight — and
    /// creating a NEW one when the descriptor's labels have changed.
    ///
    /// A group slot's descriptor name is `{metric_id}x{slot}` whatever its
    /// labels are, so a slot that is relabeled or reused keeps the same name.
    /// Keying on the name alone meant the second identity wrote into the first
    /// one's column, and the column kept the first one's metadata: a
    /// filesystem remounted elsewhere, or a reused PID, had its values filed
    /// under the previous occupant's labels, with nothing at the seam and no
    /// way for any later read to recover the split (issue #1205).
    ///
    /// So identity here is `(name, labels)`, not `name`. A changed label set
    /// opens a fresh column whose physical name carries a generation suffix,
    /// because parquet field names must be unique within a schema.
    ///
    /// **The suffix is invisible to queries.** `metriken_query`'s
    /// `parse_schema` takes a column's series name from its `metric` metadata
    /// and only falls back to the field name when that is absent, and
    /// `SeriesIdentity` keys on `(name, labels)` — so the two generations
    /// present as two series distinguished by their labels, which is what they
    /// are. Nothing downstream needs to learn about generations.
    ///
    /// `#` as the separator, deliberately: every sidecar suffix this format
    /// uses is `:`-prefixed (`:window_begin`, `:window_width`, `:buckets`), and
    /// `rez::read_table_parquet` strips those by suffix match. A `:`-prefixed
    /// generation marker would be mistaken for a sidecar.
    fn get_or_create(
        &mut self,
        desc: &crate::schema::MetricDesc,
        metric_type: &'static str,
        empty: Values,
    ) -> &mut Column {
        let mut metadata: HashMap<String, String> = desc
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        metadata
            .entry("metric_type".to_string())
            .or_insert_with(|| metric_type.to_string());

        // The live column for this descriptor name, if its labels still match.
        // Compared on the FULL metadata map, `metric_type` included: the map is
        // what becomes the column's parquet field metadata, so anything that
        // differs in it is a different column.
        let key = match self.live.get(desc.name.as_str()) {
            Some(key)
                if self
                    .columns
                    .get(key)
                    .is_some_and(|c| c.metadata == metadata) =>
            {
                key.clone()
            }
            _ => {
                // First sight, or a relabel. Either way this descriptor needs a
                // column of its own; find a field name nothing else has taken.
                let mut key = desc.name.clone();
                let mut generation = 1usize;
                while self.columns.contains_key(&key) {
                    generation += 1;
                    key = format!("{}#{generation}", desc.name);
                }
                self.live.insert(desc.name.clone(), key.clone());
                self.order.push(key.clone());
                self.columns.insert(
                    key.clone(),
                    Column {
                        name: key.clone(),
                        metadata,
                        values: empty,
                        windows: Vec::new(),
                    },
                );
                key
            }
        };

        self.columns
            .get_mut(&key)
            .expect("the key was just resolved against `columns`")
    }

    /// Append one row: `ts`/`wall_offset_ns` as in `TableBuilder::push_row`,
    /// `window` the group's single shared acquisition window for this tick,
    /// and `schema` the [`GroupSchema`] the three value slices align with
    /// (counters, then gauges, then histograms, matching `GroupSnapshot`'s
    /// own field order). A member's `None` slot ("registered, no reading
    /// this tick") is pushed as `None` in its column — it stays a member,
    /// it just has no value this row, per V3's membership-from-registration
    /// design.
    ///
    /// Fallible for the same reason `materialize_wal_tail` is: a histogram
    /// cell's `(grouping_power, max_value_power, buckets)` came off the WAL
    /// (msgpack, not re-validated by `GroupSnapshot::validate` — that only
    /// checks arity/hash), so rebuilding it through
    /// `histogram::Histogram::from_buckets` can fail on a malformed payload.
    /// Errors rather than panics, matching the v2 writer's WAL-recovery path.
    #[allow(clippy::too_many_arguments)]
    pub fn push_row(
        &mut self,
        ts: u64,
        wall_offset_ns: i64,
        window: Option<Window>,
        schema: &crate::schema::GroupSchema,
        counters: &[Option<u64>],
        gauges: &[Option<i64>],
        // `(grouping_power, max_value_power, buckets)` per histogram slot —
        // the same shape `WalValue::Histogram` carries, kept as a bare tuple
        // here (rather than importing `rez_v3_writer`'s WAL-row type) so this
        // lower-level format module does not depend on the writer module.
        histograms: &[Option<(u8, u8, Vec<u64>)>],
    ) {
        let row = self.timestamps.len();
        self.timestamps.push(ts);
        self.wall_offsets.push(wall_offset_ns);
        self.windows.push(window);

        for (desc, v) in schema.counters.iter().zip(counters) {
            let col = self.get_or_create(desc, "counter", Values::Counter(Vec::new()));
            Self::pad(col, row);
            if let Values::Counter(vs) = &mut col.values {
                vs.push(*v);
            }
        }
        for (desc, v) in schema.gauges.iter().zip(gauges) {
            let col = self.get_or_create(desc, "gauge", Values::Gauge(Vec::new()));
            Self::pad(col, row);
            if let Values::Gauge(vs) = &mut col.values {
                vs.push(*v);
            }
        }
        let table_name = self.name.clone();
        for (desc, v) in schema.histograms.iter().zip(histograms) {
            let member_name = desc.name.clone();

            // Decoded BEFORE `get_or_create` borrows the column, because
            // reporting a bad cell needs `&mut self` too.
            let decoded = match v {
                Some((gp, mvp, buckets)) => {
                    let rebuilt =
                        match histogram::Histogram::from_buckets(*gp, *mvp, buckets.clone()) {
                            Ok(h) => Some(h),
                            Err(e) => {
                                // Missing beats wrong, and beats stuck: the
                                // CELL is dropped, not the row and not the
                                // table. Reported once per member per segment
                                // — a malformed cell usually repeats every
                                // tick, and logging per row would bury it.
                                if self.reported_bad_histograms.insert(member_name.clone()) {
                                    warn!(
                                        "dropping malformed histogram cells for {member_name} \
                                         in {table_name}: {e}. That metric reads as absent for \
                                         affected rows; every other metric in the group is \
                                         unaffected."
                                    );
                                }
                                None
                            }
                        };
                    // Stamped even when the rebuild failed, and that is
                    // deliberate. Both `read_table_parquet` and
                    // `metriken_query::ParquetReader` need
                    // `grouping_power`/`max_value_power` in the parquet FIELD
                    // metadata to reconstruct a histogram column at all —
                    // without them the column silently drops out of
                    // `histogram_names()` once resealed and reopened. If the
                    // first row a column ever sees is malformed, dropping the
                    // config with it would lose the whole column's identity,
                    // not just one cell.
                    Some((rebuilt, *gp, *mvp))
                }
                None => None,
            };

            let col = self.get_or_create(desc, "histogram", Values::Histogram(Vec::new()));
            Self::pad(col, row);
            if let Some((_, gp, mvp)) = &decoded {
                col.metadata
                    .entry("grouping_power".to_string())
                    .or_insert_with(|| gp.to_string());
                col.metadata
                    .entry("max_value_power".to_string())
                    .or_insert_with(|| mvp.to_string());
            }
            if let Values::Histogram(vs) = &mut col.values {
                vs.push(decoded.and_then(|(h, _, _)| h));
            }
        }
    }

    /// Consume the builder into the table the writer will encode. Mirrors
    /// `TableBuilder::finish`, but stamps `table_window` (`Some`) instead of
    /// leaving each column's `windows` populated.
    pub fn finish(mut self) -> Table {
        let rows = self.timestamps.len();
        let columns = self
            .order
            .iter()
            .map(|name| {
                let mut col = self.columns.remove(name).unwrap();
                Self::pad(&mut col, rows);
                match &mut col.values {
                    Values::Counter(v) => v.shrink_to_fit(),
                    Values::Gauge(v) => v.shrink_to_fit(),
                    Values::Histogram(v) => v.shrink_to_fit(),
                }
                col
            })
            .collect();
        self.timestamps.shrink_to_fit();
        self.wall_offsets.shrink_to_fit();
        self.windows.shrink_to_fit();
        Table {
            sampler: self.name,
            timestamps: self.timestamps,
            wall_offsets: self.wall_offsets,
            columns,
            table_window: Some(self.windows),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `approx_bytes` bounds the DATA, and a `Vec` grown by push carries up to
    // 2x that in capacity, which a writer would then hold across its channel
    // and the encode. `finish` returns the slack. 100 rows is chosen to land
    // between powers of two, so a build that stops shrinking fails here
    // instead of coincidentally passing. Moved from rezolus with the builder.
    #[test]
    fn finish_returns_the_growth_slack() {
        let meta: HashMap<String, String> = [("sampler".to_string(), "s".to_string())]
            .into_iter()
            .collect();
        let mut b = TableBuilder::new("s".to_string());
        for row in 0..100u64 {
            b.push_row(
                row * 1_000,
                0,
                &[
                    Cell {
                        name: "0",
                        metadata: &meta,
                        window: None,
                        value: CellValue::Counter(1),
                    },
                    Cell {
                        name: "1",
                        metadata: &meta,
                        window: None,
                        value: CellValue::Gauge(1),
                    },
                ],
            );
        }
        // Pre-condition: without it a shrink that does nothing still passes.
        assert!(
            b.timestamps.capacity() > b.timestamps.len(),
            "the fixture must actually have slack to return"
        );

        let t = b.finish();
        assert_eq!(t.timestamps.capacity(), t.timestamps.len(), "timestamps");
        assert_eq!(
            t.wall_offsets.capacity(),
            t.wall_offsets.len(),
            "wall_offsets"
        );
        assert_eq!(
            t.columns.len(),
            2,
            "one counter column and one gauge column"
        );
        for col in &t.columns {
            let (len, cap) = match &col.values {
                Values::Counter(v) => (v.len(), v.capacity()),
                Values::Gauge(v) => (v.len(), v.capacity()),
                Values::Histogram(v) => (v.len(), v.capacity()),
            };
            assert_eq!(cap, len, "{} values", col.name);
            assert_eq!(
                col.windows.capacity(),
                col.windows.len(),
                "{} windows",
                col.name
            );
        }
    }
}
