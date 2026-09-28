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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
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
    /// Sort a long segment by `(occupant, timestamp)` at seal.
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
            sort_long: true,
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

/// How a slotted group's members map onto a long table.
struct LongLayout {
    /// The group's member counts (counters, gauges, histograms).
    arity: (usize, usize, usize),
    /// The metric columns: fixed descriptors, no occupant labels.
    schema: GroupSchema,
    schema_hash: (u64, u64),
    /// Per slot, in first-appearance order: its identity labels and, per
    /// member kind, which (member index, column index) pairs it holds.
    slots: Vec<Slot>,
}

struct Slot {
    labels: BTreeMap<String, String>,
    counters: Vec<(usize, usize)>,
    gauges: Vec<(usize, usize)>,
    histograms: Vec<(usize, usize)>,
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

    /// Lay a group out: long if any member carries a slot `id`.
    fn of(schema: Arc<GroupSchema>, long_groups: bool) -> Self {
        let members = || {
            schema
                .counters
                .iter()
                .chain(&schema.gauges)
                .chain(&schema.histograms)
        };
        if !long_groups || !members().any(|d| d.metadata.contains_key("id")) {
            return Layout::Wide(schema);
        }
        // A label is the occupant's when every metric of a slot agrees on
        // it (`comm`, `pid`, `id`, `__uid__`), and the metric column's when
        // it differs between a slot's metrics (`op` on a per-op table).
        // Storage keys are always the column's.
        let mut by_slot: BTreeMap<&str, Vec<&MetricDesc>> = BTreeMap::new();
        for d in members() {
            let slot = d.metadata.get("id").map(String::as_str).unwrap_or("");
            by_slot.entry(slot).or_default().push(d);
        }
        let mut per_metric: BTreeSet<String> = BTreeSet::new();
        for members in by_slot.values() {
            let keys: BTreeSet<&String> = members.iter().flat_map(|d| d.metadata.keys()).collect();
            for k in keys {
                let first = members[0].metadata.get(k);
                if members.iter().any(|d| d.metadata.get(k) != first) {
                    per_metric.insert(k.clone());
                }
            }
        }
        let is_occupant_key = |k: &str| !STORAGE_KEYS.contains(&k) && !per_metric.contains(k);
        let fixed = |d: &MetricDesc| -> BTreeMap<String, String> {
            d.metadata
                .iter()
                .filter(|(k, _)| !is_occupant_key(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        let mut long = GroupSchema::default();
        // A column is one fixed metadata set. It is named for its metric,
        // with a suffix when a group holds that metric more than once
        // (`op=read`, `op=write`); readers go by metadata, not name.
        let mut columns: [HashMap<BTreeMap<String, String>, usize>; 3] = Default::default();
        let mut names: HashSet<String> = HashSet::new();
        let mut slots: Vec<Slot> = Vec::new();
        let mut slot_index: HashMap<String, usize> = HashMap::new();
        let kinds: [(&Vec<MetricDesc>, usize); 3] = [
            (&schema.counters, 0),
            (&schema.gauges, 1),
            (&schema.histograms, 2),
        ];
        for (list, kind) in kinds {
            for (member, d) in list.iter().enumerate() {
                let metadata = fixed(d);
                let target = match kind {
                    0 => &mut long.counters,
                    1 => &mut long.gauges,
                    _ => &mut long.histograms,
                };
                let col = *columns[kind].entry(metadata.clone()).or_insert_with(|| {
                    let base = metadata
                        .get("metric")
                        .cloned()
                        .unwrap_or_else(|| d.name.clone());
                    let mut name = base.clone();
                    let mut n = 1;
                    while !names.insert(name.clone()) {
                        name = format!("{base}#{n}");
                        n += 1;
                    }
                    target.push(MetricDesc { name, metadata });
                    target.len() - 1
                });
                let slot_id = d.metadata.get("id").cloned().unwrap_or_default();
                let si = *slot_index.entry(slot_id).or_insert_with(|| {
                    slots.push(Slot {
                        labels: d
                            .metadata
                            .iter()
                            .filter(|(k, _)| is_occupant_key(k))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
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
        let schema_hash = long.hash();
        Layout::Long(Arc::new(LongLayout {
            arity: (
                schema.counters.len(),
                schema.gauges.len(),
                schema.histograms.len(),
            ),
            schema: long,
            schema_hash,
            slots,
        }))
    }
}

/// A group's schemas seen recently, by hash. Three, as the `.rez` writer
/// keeps: enough for a schema flipping back and forth.
const SCHEMA_RING_LEN: usize = 3;

#[derive(Default)]
struct GroupState {
    /// Dedup: the window end (or tick) of the last row written.
    last_key: Option<u64>,
    layouts: VecDeque<((u64, u64), Arc<Layout>)>,
    /// Whether this segment's WAL already carries the schema of the given
    /// hash (wide: the group's; long: the metric columns').
    anchored: HashSet<(u64, u64)>,
    /// Long only: occupant number by identity (`__uid__`, or the labels).
    occupants: HashMap<String, u64>,
    /// Long only: occupants seen since the last restatement, with labels.
    seen: BTreeMap<u64, BTreeMap<String, String>>,
    last_restated: Option<u64>,
}

impl GroupState {
    fn layout(&mut self, g: &GroupSnapshot, long_groups: bool) -> Option<Arc<Layout>> {
        if let Some(schema) = &g.schema {
            if let Some((_, l)) = self.layouts.iter().find(|(h, _)| *h == g.schema_hash) {
                return Some(Arc::clone(l));
            }
            let seg: GroupSchema = schema.as_ref().into();
            let layout = Arc::new(Layout::of(Arc::new(seg), long_groups));
            if self.layouts.len() == SCHEMA_RING_LEN {
                self.layouts.pop_back();
            }
            self.layouts
                .push_front((g.schema_hash, Arc::clone(&layout)));
            return Some(layout);
        }
        self.layouts
            .iter()
            .find(|(h, _)| *h == g.schema_hash)
            .map(|(_, l)| Arc::clone(l))
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
                for slot in &l.slots {
                    let mut occ = LongOccupant {
                        occupant: 0,
                        counters: vec![None; l.schema.counters.len()],
                        gauges: vec![None; l.schema.gauges.len()],
                        histograms: vec![None; l.schema.histograms.len()],
                    };
                    let mut any = false;
                    for &(m, c) in &slot.counters {
                        occ.counters[c] = g.counters[m];
                        any |= g.counters[m].is_some();
                    }
                    for &(m, c) in &slot.gauges {
                        occ.gauges[c] = g.gauges[m];
                        any |= g.gauges[m].is_some();
                    }
                    for &(m, c) in &slot.histograms {
                        occ.histograms[c] = g.histograms[m].as_ref().map(|h| {
                            (
                                h.config().grouping_power(),
                                h.config().max_value_power(),
                                h.as_slice().to_vec(),
                            )
                        });
                        any |= g.histograms[m].is_some();
                    }
                    if !any {
                        continue;
                    }
                    let identity = match slot.labels.get("__uid__") {
                        Some(uid) => format!("uid:{uid}"),
                        None => format!("labels:{:?}", slot.labels),
                    };
                    let next = state.occupants.len() as u64;
                    let number = *state.occupants.entry(identity).or_insert(next);
                    if number == next {
                        first_seen.push(Occupant {
                            occupant: number,
                            labels: slot.labels.clone(),
                        });
                    }
                    state.seen.insert(number, slot.labels.clone());
                    occ.occupant = number;
                    present.push(occ);
                }
                let anchor = state.anchored.insert(l.schema_hash);
                let row = WalLongRow {
                    schema_hash: l.schema_hash,
                    schema: anchor.then(|| l.schema.clone()),
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
                            .map(|(occupant, labels)| Occupant { occupant, labels }),
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
