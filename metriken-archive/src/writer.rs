//! [`ArchiveWriter`]: metriken snapshots recorded into a dendro archive.
//!
//! dendro's `Writer` supplies the container: one writer thread, one
//! transaction per tick across every source, sealing off the tick path
//! through a [`SegmentEncoder`], checkpoints, eviction, finalize. This module
//! supplies what goes in it (see `docs/journal/2026-09-28-archive-writer.md`):
//!
//! - a group whose members carry a slot `id` is written **long**: one row per
//!   tick and occupant, occupant numbers assigned here, and each occupant's
//!   labels in the table's occupant stream (`<table>/occupants`);
//! - any other group is one row per tick, as a `WalGroupRow`;
//! - [`Encoder`] turns each stream's WAL rows into its segment.
//!
//! Only V3 (acquisition-group) snapshots are ingested; V1/V2 per-sampler
//! snapshots are a follow-up.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dendro::archive::{SourceMeta, WalRow as DWalRow};
use dendro::seal::{SealPolicy, SegmentAccount};
use dendro::segment::{EncodeResult, Segment, SegmentEncoder};
use dendro::writer::{SourceWriter, Writer};
use metriken_exposition::{GroupSnapshot, Snapshot};
use metriken_segment::occupants::{self, Occupant};
use metriken_segment::schema::{GroupSchema, MetricDesc};
use metriken_segment::wal::{self, LongOccupant, WalLongRow, WalRowSource};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// The encoding this module writes, recorded as dendro's `ENCODER` so a
/// reader can refuse one it does not know.
pub const ENCODER_VERSION: &str = "metriken-archive/1";

/// How a writer records.
pub struct WriterConfig {
    /// When a stream's open segment is sealed.
    pub seal: SealPolicy,
    /// Row time between restatements of a long table's live occupants.
    pub restate_every_ns: u64,
    /// Sort a long segment by `(occupant, timestamp)` at seal. Off by
    /// default: on replayed recordings, arrival order was 6–20% smaller at
    /// 100 ms and no slower on the tick path (see the writer's journal
    /// entry). Sorting belongs at compaction.
    pub sort_long: bool,
    /// Write groups with slots long. Off writes every group one row per
    /// tick, which is only useful to compare the two.
    pub long_groups: bool,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            seal: SealPolicy::default(),
            restate_every_ns: 300_000_000_000,
            sort_long: false,
            long_groups: true,
        }
    }
}

/// The streams written long, shared between the recorders (which decide)
/// and the encoder (which seals them on dendro's writer thread).
type LongStreams = Arc<Mutex<HashSet<String>>>;

/// The segment encoder: a long table's rows become a long segment, an
/// occupant stream's rows an occupant segment, anything else a wide table.
pub struct Encoder {
    long: LongStreams,
    sort_long: bool,
}

/// A dendro WAL row, as metriken-segment's materialization reads one.
struct Row<'a>(&'a DWalRow);

impl WalRowSource for Row<'_> {
    fn ts(&self) -> u64 {
        self.0.ts.max(0) as u64
    }

    fn wall_offset(&self) -> i64 {
        self.0.wall_offset
    }

    fn row(&self) -> &[u8] {
        &self.0.row
    }
}

fn boxed(e: impl std::fmt::Display) -> Box<dyn std::error::Error + Send + Sync> {
    e.to_string().into()
}

impl SegmentEncoder for Encoder {
    fn encode(&self, stream: &str, rows: &[DWalRow]) -> EncodeResult {
        let Some(last) = rows.last() else {
            return Ok(None);
        };
        let last_ts = last.ts;
        if occupants::table_of(stream).is_some() {
            let mut decoded: Vec<(u64, Occupant)> = Vec::new();
            for r in rows {
                for o in occupants::decode_wal_row(&r.row).map_err(boxed)? {
                    decoded.push((r.ts.max(0) as u64, o));
                }
            }
            if decoded.is_empty() {
                return Ok(None);
            }
            let refs: Vec<(u64, &Occupant)> = decoded.iter().map(|(t, o)| (*t, o)).collect();
            let bytes =
                occupants::encode_segment(&refs, metriken_segment::table::segment_writer_props())
                    .map_err(boxed)?;
            return Ok(Some(Segment {
                bytes,
                rows: rows.len() as u64,
                first_ts: rows[0].ts,
                last_ts,
                index: None,
            }));
        }
        let adapted: Vec<Row<'_>> = rows.iter().map(Row).collect();
        let long = self
            .long
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(stream);
        let tail = if long {
            wal::materialize_long_wal_tail(stream, &adapted, self.sort_long)
        } else {
            wal::materialize_wal_tail(stream, &adapted)
        }
        .map_err(boxed)?;
        // dendro counts the WAL rows a segment consumes; a long segment has
        // one parquet row per occupant, so `t.rows` is not that count.
        Ok(tail.map(|t| Segment {
            bytes: t.bytes,
            rows: rows.len() as u64,
            first_ts: rows[0].ts,
            last_ts,
            index: None,
        }))
    }

    fn version(&self) -> Option<&str> {
        Some(ENCODER_VERSION)
    }
}

/// The archive being written: dendro's writer, and what every source's
/// recorder shares with the encoder.
pub struct ArchiveWriter {
    inner: Writer,
    long: LongStreams,
    config: Arc<WriterConfig>,
}

/// One tick's rows for one source, as [`SourceRecorder::stage`] produced
/// them, for [`ArchiveWriter::commit`].
pub struct Staged {
    source_id: i64,
    rows: Vec<DWalRow>,
}

impl ArchiveWriter {
    /// Create a new archive at `path`.
    pub fn create(path: &Path, config: WriterConfig) -> Result<Self, Error> {
        let long: LongStreams = Arc::default();
        let encoder = Encoder {
            long: Arc::clone(&long),
            sort_long: config.sort_long,
        };
        let inner = Writer::create(path, Box::new(encoder)).map_err(boxed)?;
        Ok(Self {
            inner,
            long,
            config: Arc::new(config),
        })
    }

    /// Start recording a source: its labels, metadata, and the wall-clock
    /// time its timestamps anchor to.
    pub fn add_source(
        &mut self,
        labels: BTreeMap<String, String>,
        metadata: BTreeMap<String, String>,
        clock_anchor_wall_ns: u64,
    ) -> Result<SourceRecorder, Error> {
        let writer = self
            .inner
            .add_source(SourceMeta {
                labels,
                metadata,
                clock_anchor_wall_ns: clock_anchor_wall_ns as i64,
            })
            .map_err(boxed)?;
        Ok(SourceRecorder {
            source_key: writer.stagger_key().to_string(),
            writer,
            long: Arc::clone(&self.long),
            config: Arc::clone(&self.config),
            groups: HashMap::new(),
            accounts: HashMap::new(),
        })
    }

    /// Commit one tick's staged rows, every source's, in one transaction.
    pub fn commit(&mut self, staged: Vec<Staged>) -> Result<(), Error> {
        let ticks = staged
            .into_iter()
            .filter(|s| !s.rows.is_empty())
            .map(|s| (s.source_id, s.rows))
            .collect();
        self.inner.wal_tick(ticks).map_err(boxed)
    }

    /// Wait for everything sent to the writer thread to land.
    pub fn join(&mut self) -> Result<(), Error> {
        self.inner.join().map_err(boxed)
    }
}

/// A group's schema, as this writer lays it out.
enum Layout {
    /// One row per tick: the group's own schema.
    Wide(Arc<GroupSchema>),
    Long(Arc<LongLayout>),
}

/// How one producer schema of a slotted group maps onto the group's long
/// columns ([`LongColumns`]).
struct LongLayout {
    /// The group's member counts (counters, gauges, histograms).
    arity: (usize, usize, usize),
    /// Per slot, in first-appearance order.
    slots: Vec<Slot>,
}

struct Slot {
    /// The occupant's identity: `__uid__` when it has one, else its labels.
    identity: String,
    labels: Arc<BTreeMap<String, String>>,
    /// The occupant's number, once looked up. A layout belongs to one group,
    /// and a slot's labels are fixed within it, so this never changes.
    number: std::sync::OnceLock<u64>,
    /// Per member kind, (member index, column index) pairs.
    counters: Vec<(usize, usize)>,
    gauges: Vec<(usize, usize)>,
    histograms: Vec<(usize, usize)>,
}

/// A long group's metric columns. Append-only for the recording, so a
/// column index in any cached [`LongLayout`] stays valid, and the columns
/// change (and are re-anchored) only when a metric is new, not when
/// membership does.
#[derive(Default)]
struct LongColumns {
    schema: GroupSchema,
    hash: (u64, u64),
    /// Per member kind, column index by the column's fixed metadata.
    index: [HashMap<String, usize>; 3],
    names: HashSet<String>,
}

/// Keys that describe the metric, never an occupant.
const STORAGE_KEYS: &[&str] = &[
    "metric",
    "metric_type",
    "unit",
    "grouping_power",
    "max_value_power",
    "sampler",
    "description",
];

impl Layout {
    /// The member counts a group's values must have.
    fn arity(&self) -> (usize, usize, usize) {
        match self {
            Layout::Wide(s) => (s.counters.len(), s.gauges.len(), s.histograms.len()),
            Layout::Long(l) => l.arity,
        }
    }
}

/// Whether a group with this schema is written long: any member carries a
/// slot `id`.
fn slotted(schema: &metriken_exposition::GroupSchema) -> bool {
    schema
        .counters
        .iter()
        .chain(&schema.gauges)
        .chain(&schema.histograms)
        .any(|d| d.metadata.contains_key("id"))
}

impl LongLayout {
    /// Lay a slotted group's schema out over its long columns, adding any
    /// column it is the first to need. A member without an `id` is the
    /// occupant of slot `""`.
    fn of(schema: &metriken_exposition::GroupSchema, cols: &mut LongColumns) -> Self {
        type Desc = metriken_exposition::MetricDesc;
        let kinds: [&Vec<Desc>; 3] = [&schema.counters, &schema.gauges, &schema.histograms];
        fn slot_of(d: &metriken_exposition::MetricDesc) -> &str {
            d.metadata.get("id").map(String::as_str).unwrap_or("")
        }

        // A label is the occupant's when every metric of a slot agrees on
        // it (`comm`, `pid`, `id`, `__uid__`), and the metric column's when
        // it differs between a slot's metrics (`op` on a per-op table).
        // Storage keys are always the column's.
        let mut first: HashMap<&str, &Desc> = HashMap::new();
        let mut per_metric: HashSet<&str> = HashSet::new();
        for d in kinds.iter().flat_map(|k| k.iter()) {
            let f = *first.entry(slot_of(d)).or_insert(d);
            if std::ptr::eq(f, d) {
                continue;
            }
            for (k, v) in &d.metadata {
                if f.metadata.get(k) != Some(v) {
                    per_metric.insert(k);
                }
            }
            for k in f.metadata.keys() {
                if !d.metadata.contains_key(k) {
                    per_metric.insert(k);
                }
            }
        }
        let is_occupant_key = |k: &str| !STORAGE_KEYS.contains(&k) && !per_metric.contains(k);

        let mut changed = false;
        let mut slots: Vec<Slot> = Vec::new();
        let mut slot_index: HashMap<&str, usize> = HashMap::new();
        let mut key = String::new();
        for (kind, list) in kinds.into_iter().enumerate() {
            for (member, d) in list.iter().enumerate() {
                key.clear();
                for (k, v) in &d.metadata {
                    if !is_occupant_key(k) {
                        key.push_str(k);
                        key.push('\u{1f}');
                        key.push_str(v);
                        key.push('\u{1e}');
                    }
                }
                let col = match cols.index[kind].get(key.as_str()) {
                    Some(&c) => c,
                    None => {
                        let metadata: BTreeMap<String, String> = d
                            .metadata
                            .iter()
                            .filter(|(k, _)| !is_occupant_key(k))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        // Named for its metric, with a suffix when a group
                        // holds that metric more than once (`op=read`,
                        // `op=write`); readers go by metadata, not name.
                        let base = metadata
                            .get("metric")
                            .cloned()
                            .unwrap_or_else(|| d.name.clone());
                        let mut name = base.clone();
                        let mut n = 1;
                        while !cols.names.insert(name.clone()) {
                            name = format!("{base}#{n}");
                            n += 1;
                        }
                        let target = match kind {
                            0 => &mut cols.schema.counters,
                            1 => &mut cols.schema.gauges,
                            _ => &mut cols.schema.histograms,
                        };
                        target.push(MetricDesc { name, metadata });
                        cols.index[kind].insert(key.clone(), target.len() - 1);
                        changed = true;
                        target.len() - 1
                    }
                };
                let si = *slot_index.entry(slot_of(d)).or_insert_with(|| {
                    let labels: BTreeMap<String, String> = d
                        .metadata
                        .iter()
                        .filter(|(k, _)| is_occupant_key(k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    let identity = match labels.get("__uid__") {
                        Some(uid) => format!("uid:{uid}"),
                        None => format!("labels:{labels:?}"),
                    };
                    slots.push(Slot {
                        identity,
                        labels: Arc::new(labels),
                        number: std::sync::OnceLock::new(),
                        counters: Vec::new(),
                        gauges: Vec::new(),
                        histograms: Vec::new(),
                    });
                    slots.len() - 1
                });
                let pairs = match kind {
                    0 => &mut slots[si].counters,
                    1 => &mut slots[si].gauges,
                    _ => &mut slots[si].histograms,
                };
                pairs.push((member, col));
            }
        }
        if changed {
            cols.hash = cols.schema.hash();
        }
        LongLayout {
            arity: (
                schema.counters.len(),
                schema.gauges.len(),
                schema.histograms.len(),
            ),
            slots,
        }
    }
}

/// A group's schemas seen recently, by hash. Three, as the `.rez` writer
/// keeps: enough for a schema flipping back and forth.
const SCHEMA_RING_LEN: usize = 3;

#[derive(Default)]
struct GroupState {
    /// Dedup: the window end (or tick) of the last row written.
    last_key: Option<u64>,
    /// Whether the group is written long. Decided by the first schema with
    /// members and kept for the recording: a stream holds one kind of WAL
    /// row, and a reader tells them apart by the occupant stream.
    long: Option<bool>,
    layouts: VecDeque<((u64, u64), Arc<Layout>)>,
    /// Whether this segment's WAL already carries the schema of the given
    /// hash (wide: the group's; long: the metric columns').
    anchored: HashSet<(u64, u64)>,
    /// Long only: the metric columns.
    columns: LongColumns,
    /// Long only: occupant number by identity (`__uid__`, or the labels).
    occupants: HashMap<String, u64>,
    /// Long only: occupants seen since the last restatement, with labels.
    seen: BTreeMap<u64, Arc<BTreeMap<String, String>>>,
    last_restated: Option<u64>,
}

impl GroupState {
    fn layout(&mut self, g: &GroupSnapshot, long_groups: bool) -> Option<Arc<Layout>> {
        if let Some((_, l)) = self.layouts.iter().find(|(h, _)| *h == g.schema_hash) {
            return Some(Arc::clone(l));
        }
        let schema = g.schema.as_ref()?;
        let long = *self
            .long
            .get_or_insert_with(|| long_groups && slotted(schema));
        let layout = Arc::new(if long {
            Layout::Long(Arc::new(LongLayout::of(schema, &mut self.columns)))
        } else {
            Layout::Wide(Arc::new(schema.as_ref().into()))
        });
        if self.layouts.len() == SCHEMA_RING_LEN {
            self.layouts.pop_back();
        }
        self.layouts
            .push_front((g.schema_hash, Arc::clone(&layout)));
        Some(layout)
    }
}

/// One source being recorded: its dendro writer handle and the state
/// ingest keeps per group.
pub struct SourceRecorder {
    writer: SourceWriter,
    source_key: String,
    long: LongStreams,
    config: Arc<WriterConfig>,
    groups: HashMap<String, GroupState>,
    accounts: HashMap<String, SegmentAccount>,
}

impl SourceRecorder {
    pub fn source_id(&self) -> i64 {
        self.writer.source_id()
    }

    fn account(&mut self, stream: &str) -> &mut SegmentAccount {
        let (key, policy) = (&self.source_key, &self.config.seal);
        self.accounts
            .entry(stream.to_string())
            .or_insert_with(|| SegmentAccount::open_first(stream, key, policy))
    }

    /// One snapshot's rows for this source, to commit with
    /// [`ArchiveWriter::commit`]. `ts` is the tick's timestamp and
    /// `wall_offset` the wall clock's reading minus it, in nanoseconds.
    pub fn stage(
        &mut self,
        snapshot: &Snapshot,
        ts: u64,
        wall_offset: i64,
    ) -> Result<Staged, Error> {
        let mut rows = Vec::new();
        if let Snapshot::V3(v3) = snapshot {
            let mut done: HashSet<&str> = HashSet::new();
            for g in &v3.groups {
                if !done.insert(g.name.as_str()) {
                    continue;
                }
                self.stage_group(g, ts, wall_offset, &mut rows)?;
            }
        }
        Ok(Staged {
            source_id: self.writer.source_id(),
            rows,
        })
    }

    fn stage_group(
        &mut self,
        g: &GroupSnapshot,
        ts: u64,
        wall_offset: i64,
        rows: &mut Vec<DWalRow>,
    ) -> Result<(), Error> {
        if g.validate().is_err() {
            tracing::warn!("group {} failed validation; skipped this tick", g.name);
            return Ok(());
        }
        let long_groups = self.config.long_groups;
        let state = self.groups.entry(g.name.clone()).or_default();
        let key = g.window.map(|w| w.end_ns).unwrap_or(ts);
        if state.last_key == Some(key) {
            return Ok(());
        }
        // A group with no members has no values to record, and its schema
        // must not decide whether the group is long.
        if g.counters.is_empty() && g.gauges.is_empty() && g.histograms.is_empty() {
            state.last_key = Some(key);
            return Ok(());
        }
        let Some(layout) = state.layout(g, long_groups) else {
            tracing::warn!(
                "group {} arrived without a schema it has sent before; skipped",
                g.name
            );
            return Ok(());
        };
        if layout.arity() != (g.counters.len(), g.gauges.len(), g.histograms.len()) {
            tracing::warn!(
                "group {} does not match its cached schema; skipped this tick",
                g.name
            );
            return Ok(());
        }
        state.last_key = Some(key);
        let window = g.window.map(|w| (w.begin_ns, w.end_ns));
        match layout.as_ref() {
            Layout::Wide(schema) => {
                let anchor = state.anchored.insert(g.schema_hash);
                let row =
                    metriken_exposition::wal_group_row(g, anchor.then(|| schema.as_ref().clone()));
                let bytes = metriken_exposition::group_approx_bytes(g);
                rows.push(DWalRow {
                    stream: g.name.clone(),
                    ts: ts as i64,
                    wall_offset,
                    row: wal::encode_wal_group_row(&row)?,
                });
                self.account(&g.name).add_row(bytes, ts as i64);
            }
            Layout::Long(l) => {
                self.long
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(g.name.clone());
                let mut present = Vec::new();
                let mut first_seen = Vec::new();
                let widths = (
                    state.columns.schema.counters.len(),
                    state.columns.schema.gauges.len(),
                    state.columns.schema.histograms.len(),
                );
                for slot in &l.slots {
                    let any = slot.counters.iter().any(|&(m, _)| g.counters[m].is_some())
                        || slot.gauges.iter().any(|&(m, _)| g.gauges[m].is_some())
                        || slot
                            .histograms
                            .iter()
                            .any(|&(m, _)| g.histograms[m].is_some());
                    if !any {
                        continue;
                    }
                    let mut occ = LongOccupant {
                        occupant: 0,
                        counters: vec![None; widths.0],
                        gauges: vec![None; widths.1],
                        histograms: vec![None; widths.2],
                    };
                    for &(m, c) in &slot.counters {
                        occ.counters[c] = g.counters[m];
                    }
                    for &(m, c) in &slot.gauges {
                        occ.gauges[c] = g.gauges[m];
                    }
                    for &(m, c) in &slot.histograms {
                        occ.histograms[c] = g.histograms[m].as_ref().map(|h| {
                            (
                                h.config().grouping_power(),
                                h.config().max_value_power(),
                                h.as_slice().to_vec(),
                            )
                        });
                    }
                    let number = *slot.number.get_or_init(|| {
                        let next = state.occupants.len() as u64;
                        let number = *state.occupants.entry(slot.identity.clone()).or_insert(next);
                        if number == next {
                            first_seen.push(Occupant {
                                occupant: number,
                                labels: slot.labels.as_ref().clone(),
                            });
                        }
                        number
                    });
                    state
                        .seen
                        .entry(number)
                        .or_insert_with(|| Arc::clone(&slot.labels));
                    occ.occupant = number;
                    present.push(occ);
                }
                // A tick with no occupant present has nothing to record, and
                // writing it could put rows in a long stream before its
                // occupant stream exists.
                if present.is_empty() {
                    return Ok(());
                }
                let hash = state.columns.hash;
                let anchor = state.anchored.insert(hash);
                let row = WalLongRow {
                    schema_hash: hash,
                    schema: anchor.then(|| state.columns.schema.clone()),
                    window,
                    occupants: present,
                };
                let bytes = wal::wal_long_row_approx_bytes(&row);
                rows.push(DWalRow {
                    stream: g.name.clone(),
                    ts: ts as i64,
                    wall_offset,
                    row: wal::encode_wal_long_row(&row)?,
                });
                // Restate every occupant seen since the last restatement,
                // so a live occupant's labels stay inside retention.
                let due = match state.last_restated {
                    None => {
                        state.last_restated = Some(ts);
                        false
                    }
                    Some(t) => ts.saturating_sub(t) >= self.config.restate_every_ns,
                };
                let mut labels_rows = first_seen;
                if due {
                    state.last_restated = Some(ts);
                    let fresh: HashSet<u64> = labels_rows.iter().map(|o| o.occupant).collect();
                    labels_rows.extend(
                        std::mem::take(&mut state.seen)
                            .into_iter()
                            .filter(|(n, _)| !fresh.contains(n))
                            .map(|(occupant, labels)| Occupant {
                                occupant,
                                labels: labels.as_ref().clone(),
                            }),
                    );
                }
                let stream = occupants::stream_of(&g.name);
                if !labels_rows.is_empty() {
                    let n = labels_rows.len();
                    rows.push(DWalRow {
                        stream: stream.clone(),
                        ts: ts as i64,
                        wall_offset,
                        row: occupants::encode_wal_row(&labels_rows),
                    });
                    self.account(&stream).add_row(n * 64, ts as i64);
                }
                self.account(&g.name).add_row(bytes, ts as i64);
            }
        }
        Ok(())
    }

    /// Seal every stream whose open segment is due. Call once per tick,
    /// after [`ArchiveWriter::commit`].
    pub fn maybe_seal(&mut self) -> Result<(), Error> {
        let now = Instant::now();
        let policy = &self.config.seal;
        let mut batch = Vec::new();
        for (stream, account) in &mut self.accounts {
            if account.is_due(now) {
                account.rotate(policy, now);
                batch.push(stream.clone());
            }
        }
        for stream in &batch {
            if let Some(state) = self.groups.get_mut(stream) {
                state.anchored.clear();
            }
        }
        if batch.is_empty() {
            return Ok(());
        }
        self.writer.seal(batch).map_err(boxed)
    }

    /// Merge `patch` into this source's metadata while it records: a key in
    /// `patch` replaces that key, and every other key is kept. Ordered with
    /// the ticks around it, so a patch sent after a tick's commit lands after
    /// that tick, and before [`finalize`](Self::finalize) when sent first.
    ///
    /// For facts learned during a recording, such as the events marking
    /// where a wrapped command started and ended. Fire-and-forget: a patch
    /// the writer cannot apply is logged and skipped, not an error here;
    /// [`sync`](Self::sync) and read it back to know it landed.
    pub fn update_metadata(&mut self, patch: BTreeMap<String, String>) -> Result<(), Error> {
        self.writer.update_metadata(patch).map_err(boxed)
    }

    /// Wait until everything this source sent has landed, so a reader sees
    /// its last tick.
    pub fn sync(&mut self) -> Result<(), Error> {
        self.writer.sync().map_err(boxed)
    }

    /// Drop everything older than `cutoff_ns`. Occupant streams keep one
    /// restatement period more, so a row just inside the cutoff can still
    /// find its occupant's labels.
    pub fn evict_before(&mut self, cutoff_ns: u64) -> Result<(), Error> {
        self.writer
            .evict_streams_before(
                cutoff_ns as i64,
                Box::new(|s: &str| occupants::table_of(s).is_none()),
            )
            .map_err(boxed)?;
        let lagged = cutoff_ns.saturating_sub(self.config.restate_every_ns);
        self.writer
            .evict_streams_before(
                lagged as i64,
                Box::new(|s: &str| occupants::table_of(s).is_some()),
            )
            .map_err(boxed)?;
        Ok(())
    }

    /// Seal every stream's tail and mark the source complete. `last` is the
    /// last tick's `(ts, wall_offset)`.
    pub fn finalize(mut self, last: (u64, i64)) -> Result<(), Error> {
        let batch: Vec<String> = self.accounts.keys().cloned().collect();
        if !batch.is_empty() {
            self.writer.seal(batch).map_err(boxed)?;
        }
        self.writer.finalize((last.0 as i64, last.1)).map_err(boxed)
    }
}
