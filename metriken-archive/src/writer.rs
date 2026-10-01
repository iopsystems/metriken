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
//! A pass taken off a replication stream ([`crate::stream`]) is staged with
//! [`SourceRecorder::stage_streamed`], its rows decoded by a
//! [`StreamDecoder`]. A slot group arrives long, keyed by the producer's
//! occupant keys, and is written long with those keys mapped to occupant
//! numbers; no full member list is laid out.
//!
//! A V1/V2 snapshot (from a producer older than acquisition groups) is
//! written one table per `sampler` label, each metric with its own window,
//! as `WalCell` rows.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use dendro::archive::{SourceMeta, WalRow as DWalRow};
use dendro::seal::{SealPolicy, SegmentAccount};
use dendro::writer::{SourceWriter, Writer};
use metriken_exposition::{GroupSnapshot, Snapshot};
use metriken_segment::occupants::{self, Occupant};
use metriken_segment::schema::{GroupSchema, MetricDesc};
use metriken_segment::wal::{self, LongOccupant, WalLongRow};

type Error = Box<dyn std::error::Error + Send + Sync>;

use crate::encoder::boxed;
use crate::encoder::LongStreams;
pub use crate::encoder::{Encoder, ENCODER_VERSION};
use crate::{default_compression, segment_props as sealed_props};
pub use crate::{Compression, ZstdLevel};

/// How a writer records.
pub struct WriterConfig {
    /// When a stream's open segment is sealed.
    pub seal: SealPolicy,
    /// Row time between restatements of a long table's live occupants.
    pub restate_every_ns: u64,
    /// Sort a long segment by `(occupant, timestamp)` at seal. Off by
    /// default, to keep the cost at seal low; sorting belongs at compaction.
    /// On replayed recordings it was 6–32% smaller on real hosts and 6–20%
    /// larger on synthetic churn (see the writer's journal entry).
    pub sort_long: bool,
    /// The codec every sealed segment is written with. zstd level 3 by
    /// default: on replayed recordings it was 55–57% smaller than LZ4 for
    /// about 4% more encode time, with the same tick latency and query time.
    /// Readers decode either.
    pub compression: Compression,
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
            compression: default_compression(),
        }
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

/// One row of a pass taken off a replication stream (see
/// [`crate::stream`]), for [`SourceRecorder::stage_streamed`].
pub enum StreamedGroup {
    /// A group row, as a snapshot's group: staged as
    /// [`SourceRecorder::stage`] stages it.
    Wide(GroupSnapshot),
    /// A long row of the stream named `name`, keyed by the producer's
    /// occupant keys. Its schema is the group's columns, present on the row
    /// that anchors them.
    Long { name: String, row: WalLongRow },
    /// The occupants first described on `<table>/occupants`: each key's
    /// labels, for the long rows of `table` that follow.
    Occupants {
        table: String,
        occupants: Vec<Occupant>,
    },
}

/// Turns the rows of a replication stream's frames back into
/// [`StreamedGroup`]s, for [`SourceRecorder::stage_streamed`]. One per
/// connection: the schemas it holds are the ones this connection was sent.
///
/// A row on `<table>/occupants` is an [`Occupants`](StreamedGroup::Occupants)
/// row, and marks `table` as long: the first row of a long group always
/// describes its occupants. Any other row of a long table is a
/// [`WalLongRow`]. Any other row is a [`WalGroupRow`], whose schema this
/// holds from the row that carried it; a row whose schema this connection
/// was never sent is skipped and counted in [`unresolved`](Self::unresolved).
///
/// [`WalGroupRow`]: metriken_segment::wal::WalGroupRow
#[derive(Default)]
pub struct StreamDecoder {
    schemas: HashMap<String, ((u64, u64), Arc<metriken_exposition::GroupSchema>)>,
    long: HashSet<String>,
    /// Group rows skipped because their schema was never sent.
    pub unresolved: u64,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// One pass's rows, in the order they arrived. A payload that does not
    /// decode is an error naming its stream.
    pub fn decode(
        &mut self,
        rows: impl IntoIterator<Item = DWalRow>,
    ) -> Result<Vec<StreamedGroup>, String> {
        let mut out = Vec::new();
        for row in rows {
            let stream = row.stream;
            if let Some(table) = occupants::table_of(&stream) {
                self.long.insert(table.to_string());
                out.push(StreamedGroup::Occupants {
                    table: table.to_string(),
                    occupants: occupants::decode_wal_row(&row.row)
                        .map_err(|e| format!("stream {stream}: {e}"))?,
                });
                continue;
            }
            if self.long.contains(&stream) {
                let decoded = wal::decode_wal_long_row(&row.row)
                    .map_err(|e| format!("stream {stream}: {e}"))?;
                out.push(StreamedGroup::Long {
                    name: stream,
                    row: decoded,
                });
                continue;
            }
            let decoded =
                wal::decode_wal_group_row(&row.row).map_err(|e| format!("stream {stream}: {e}"))?;
            let arrived = decoded
                .schema
                .as_ref()
                .map(|s| Arc::new(exposition_schema(s)));
            if let Some(schema) = &arrived {
                self.schemas
                    .insert(stream.clone(), (decoded.schema_hash, Arc::clone(schema)));
            }
            match self.schemas.get(&stream) {
                Some((hash, _)) if *hash == decoded.schema_hash => {}
                _ => {
                    self.unresolved += 1;
                    continue;
                }
            }
            let histograms = decoded
                .histograms
                .into_iter()
                .map(|h| {
                    h.map(|(gp, mvp, buckets)| {
                        histogram::Histogram::from_buckets(gp, mvp, buckets)
                            .map_err(|e| format!("stream {stream}: histogram: {e}"))
                    })
                    .transpose()
                })
                .collect::<Result<Vec<_>, String>>()?;
            out.push(StreamedGroup::Wide(GroupSnapshot {
                name: stream,
                schema_hash: decoded.schema_hash,
                schema: arrived,
                window: decoded.window.map(|(b, e)| metriken::Window::new(b, e)),
                counters: decoded.counters,
                gauges: decoded.gauges,
                histograms,
            }));
        }
        Ok(out)
    }
}

/// A segment-format schema as metriken-exposition's, which a
/// [`GroupSnapshot`] carries.
fn exposition_schema(s: &GroupSchema) -> metriken_exposition::GroupSchema {
    let convert = |list: &[MetricDesc]| {
        list.iter()
            .map(|d| metriken_exposition::MetricDesc {
                name: d.name.clone(),
                metadata: d.metadata.clone(),
            })
            .collect()
    };
    metriken_exposition::GroupSchema {
        counters: convert(&s.counters),
        gauges: convert(&s.gauges),
        histograms: convert(&s.histograms),
    }
}

impl ArchiveWriter {
    /// Create a new archive at `path`.
    pub fn create(path: &Path, config: WriterConfig) -> Result<Self, Error> {
        let long: LongStreams = Arc::default();
        let encoder = Encoder::new(
            Arc::clone(&long),
            config.sort_long,
            sealed_props(config.compression),
        );
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
            samplers: HashMap::new(),
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

impl LongColumns {
    /// The column for a streamed long row's column `d` of member `kind`
    /// (0 counters, 1 gauges, 2 histograms), added if new. Every key of a
    /// streamed column describes the metric; the occupant's labels arrive
    /// separately. Returns the index and whether it was added.
    fn wire_column(&mut self, kind: usize, d: &MetricDesc) -> (usize, bool) {
        let mut key = String::new();
        for (k, v) in &d.metadata {
            key.push_str(k);
            key.push('\u{1f}');
            key.push_str(v);
            key.push('\u{1e}');
        }
        if let Some(&c) = self.index[kind].get(&key) {
            return (c, false);
        }
        let base = d
            .metadata
            .get("metric")
            .cloned()
            .unwrap_or_else(|| d.name.clone());
        let mut name = base.clone();
        let mut n = 1;
        while !self.names.insert(name.clone()) {
            name = format!("{base}#{n}");
            n += 1;
        }
        let target = match kind {
            0 => &mut self.schema.counters,
            1 => &mut self.schema.gauges,
            _ => &mut self.schema.histograms,
        };
        target.push(MetricDesc {
            name,
            metadata: d.metadata.clone(),
        });
        self.index[kind].insert(key, target.len() - 1);
        (target.len() - 1, true)
    }
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
    /// Streamed long rows only: the columns the producer last anchored,
    /// by hash, mapped onto `columns`.
    wire: Option<((u64, u64), Arc<WireColumns>)>,
    /// Streamed long rows only: occupant number by the producer's key.
    keys: HashMap<u64, u64>,
    /// Streamed long rows only: labels sent for keys with no number yet.
    pending: HashMap<u64, Arc<BTreeMap<String, String>>>,
    /// Streamed long rows only: whether a row with no anchor, or an
    /// occupant with no labels, has been warned about.
    warned: bool,
}

/// A streamed long row's columns, mapped onto the group's [`LongColumns`]:
/// per member kind, the writer's column for each of the row's.
struct WireColumns {
    counters: Vec<usize>,
    gauges: Vec<usize>,
    histograms: Vec<usize>,
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

/// A V1/V2 sampler table's ingest state.
#[derive(Default)]
struct SamplerState {
    /// Dedup: the newest window end (or tick) of the last row written.
    last_key: Option<u64>,
    /// The metrics whose metadata this segment's WAL already carries.
    described: HashSet<String>,
}

/// One source being recorded: its dendro writer handle and the state
/// ingest keeps per group.
pub struct SourceRecorder {
    writer: SourceWriter,
    /// V1/V2 only: per sampler table, the last row's dedup key and the
    /// metrics whose metadata this segment's WAL already carries.
    samplers: HashMap<String, SamplerState>,
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
        match snapshot {
            Snapshot::V3(v3) => {
                let mut done: HashSet<&str> = HashSet::new();
                for g in &v3.groups {
                    if !done.insert(g.name.as_str()) {
                        continue;
                    }
                    self.stage_group(g, ts, wall_offset, &mut rows)?;
                }
            }
            Snapshot::V1(s) => self.stage_flat(
                &s.counters,
                &s.gauges,
                &s.histograms,
                ts,
                wall_offset,
                &mut rows,
            )?,
            Snapshot::V2(s) => self.stage_flat(
                &s.counters,
                &s.gauges,
                &s.histograms,
                ts,
                wall_offset,
                &mut rows,
            )?,
        }
        Ok(Staged {
            source_id: self.writer.source_id(),
            rows,
        })
    }

    /// One pass off a replication stream, in the order its rows arrived.
    ///
    /// A long row is written long, its occupant keys mapped to this archive's
    /// occupant numbers. A key gets a number the first time it is present in
    /// a long row, with the labels an [`Occupants`](StreamedGroup::Occupants)
    /// row gave it, and keeps that number for the recording, so labels sent
    /// again (on a reconnect) are not a new occupant. An occupant whose labels
    /// were never sent, or a row whose columns were never anchored, is
    /// skipped with one warning per group.
    pub fn stage_streamed(
        &mut self,
        groups: Vec<StreamedGroup>,
        ts: u64,
        wall_offset: i64,
    ) -> Result<Staged, Error> {
        let mut rows = Vec::new();
        let mut done: HashSet<String> = HashSet::new();
        for g in groups {
            match g {
                StreamedGroup::Wide(g) => {
                    if done.insert(g.name.clone()) {
                        self.stage_group(&g, ts, wall_offset, &mut rows)?;
                    }
                }
                StreamedGroup::Long { name, row } => {
                    if done.insert(name.clone()) {
                        self.stage_long_row(&name, row, ts, wall_offset, &mut rows)?;
                    }
                }
                StreamedGroup::Occupants { table, occupants } => {
                    let state = self.groups.entry(table).or_default();
                    for o in occupants {
                        if !state.keys.contains_key(&o.occupant) {
                            state.pending.insert(o.occupant, Arc::new(o.labels));
                        }
                    }
                }
            }
        }
        Ok(Staged {
            source_id: self.writer.source_id(),
            rows,
        })
    }

    fn stage_long_row(
        &mut self,
        name: &str,
        row: WalLongRow,
        ts: u64,
        wall_offset: i64,
        rows: &mut Vec<DWalRow>,
    ) -> Result<(), Error> {
        let state = self.groups.entry(name.to_string()).or_default();
        let key = row.window.map(|(_, end)| end).unwrap_or(ts);
        if state.last_key == Some(key) {
            return Ok(());
        }
        if state.long == Some(false) {
            if !state.warned {
                tracing::warn!("group {name} is recorded wide and arrived long; skipped");
                state.warned = true;
            }
            return Ok(());
        }
        if let Some(schema) = &row.schema {
            if state.wire.as_ref().map(|(h, _)| *h) != Some(row.schema_hash) {
                let mut changed = false;
                let mut map = |kind: usize, list: &[MetricDesc]| -> Vec<usize> {
                    list.iter()
                        .map(|d| {
                            let (c, added) = state.columns.wire_column(kind, d);
                            changed |= added;
                            c
                        })
                        .collect()
                };
                let wire = WireColumns {
                    counters: map(0, &schema.counters),
                    gauges: map(1, &schema.gauges),
                    histograms: map(2, &schema.histograms),
                };
                if changed {
                    state.columns.hash = state.columns.schema.hash();
                }
                state.wire = Some((row.schema_hash, Arc::new(wire)));
            }
        }
        let wire = match &state.wire {
            Some((hash, wire)) if *hash == row.schema_hash => Arc::clone(wire),
            _ => {
                if !state.warned {
                    tracing::warn!(
                        "group {name}: a long row arrived before its columns; skipped (warned once)"
                    );
                    state.warned = true;
                }
                return Ok(());
            }
        };
        state.long = Some(true);
        state.last_key = Some(key);
        let widths = (
            state.columns.schema.counters.len(),
            state.columns.schema.gauges.len(),
            state.columns.schema.histograms.len(),
        );
        let mut present = Vec::with_capacity(row.occupants.len());
        let mut first_seen = Vec::new();
        for o in row.occupants {
            if o.counters.len() != wire.counters.len()
                || o.gauges.len() != wire.gauges.len()
                || o.histograms.len() != wire.histograms.len()
            {
                if !state.warned {
                    tracing::warn!(
                        "group {name}: an occupant does not match its columns; skipped (warned once)"
                    );
                    state.warned = true;
                }
                continue;
            }
            let number = match state.keys.get(&o.occupant) {
                Some(&n) => n,
                None => {
                    let Some(labels) = state.pending.remove(&o.occupant) else {
                        if !state.warned {
                            tracing::warn!(
                                "group {name}: occupant {} arrived without labels; skipped \
                                 (warned once)",
                                o.occupant
                            );
                            state.warned = true;
                        }
                        continue;
                    };
                    let n = (state.occupants.len() + state.keys.len()) as u64;
                    state.keys.insert(o.occupant, n);
                    first_seen.push(Occupant {
                        occupant: n,
                        labels: labels.as_ref().clone(),
                    });
                    state.seen.insert(n, labels);
                    n
                }
            };
            if let Some(labels) = state.pending.remove(&o.occupant) {
                // Sent again for a key already numbered: a reconnect.
                state.seen.entry(number).or_insert(labels);
            }
            let mut occ = LongOccupant {
                occupant: number,
                counters: vec![None; widths.0],
                gauges: vec![None; widths.1],
                histograms: vec![None; widths.2],
            };
            for (i, v) in o.counters.into_iter().enumerate() {
                occ.counters[wire.counters[i]] = v;
            }
            for (i, v) in o.gauges.into_iter().enumerate() {
                occ.gauges[wire.gauges[i]] = v;
            }
            for (i, v) in o.histograms.into_iter().enumerate() {
                occ.histograms[wire.histograms[i]] = v;
            }
            present.push(occ);
        }
        self.write_long(name, row.window, present, first_seen, ts, wall_offset, rows)
    }

    /// A V1/V2 snapshot: one row per `sampler` label (`"unattributed"`
    /// without one), skipped when the sampler's newest window has not
    /// advanced, each metric's metadata carried on its first row in a
    /// segment. The `.rez` v3 writer's rule, so the two tables match.
    fn stage_flat(
        &mut self,
        counters: &[metriken_exposition::Counter],
        gauges: &[metriken_exposition::Gauge],
        histograms: &[metriken_exposition::Histogram],
        ts: u64,
        wall_offset: i64,
        rows: &mut Vec<DWalRow>,
    ) -> Result<(), Error> {
        use metriken_segment::builder::{cells_approx_bytes, Cell, CellValue};
        use metriken_segment::wal::{WalCell, WalValue};

        struct Entry<'a> {
            name: &'a str,
            metadata: &'a HashMap<String, String>,
            window: Option<(u64, u64)>,
            value: CellValue<'a>,
        }
        let sampler_of = |m: &HashMap<String, String>| -> String {
            m.get("sampler")
                .cloned()
                .unwrap_or_else(|| "unattributed".to_string())
        };
        let mut by_sampler: BTreeMap<String, Vec<Entry<'_>>> = BTreeMap::new();
        for c in counters {
            by_sampler
                .entry(sampler_of(&c.metadata))
                .or_default()
                .push(Entry {
                    name: &c.name,
                    metadata: &c.metadata,
                    window: c.window.map(|w| (w.begin_ns, w.end_ns)),
                    value: CellValue::Counter(c.value),
                });
        }
        for g in gauges {
            by_sampler
                .entry(sampler_of(&g.metadata))
                .or_default()
                .push(Entry {
                    name: &g.name,
                    metadata: &g.metadata,
                    window: g.window.map(|w| (w.begin_ns, w.end_ns)),
                    value: CellValue::Gauge(g.value),
                });
        }
        for h in histograms {
            by_sampler
                .entry(sampler_of(&h.metadata))
                .or_default()
                .push(Entry {
                    name: &h.name,
                    metadata: &h.metadata,
                    window: h.window.map(|w| (w.begin_ns, w.end_ns)),
                    value: CellValue::Histogram(&h.value),
                });
        }

        for (sampler, entries) in by_sampler {
            // A `/` would make the table look like a group's to a reader.
            if sampler.contains('/') {
                tracing::warn!("sampler {sampler:?} contains '/'; its metrics are skipped");
                continue;
            }
            let key = entries
                .iter()
                .filter_map(|e| e.window.map(|(_, end)| end))
                .max()
                .unwrap_or(ts);
            let state = self.samplers.entry(sampler.clone()).or_default();
            if state.last_key.is_some_and(|last| key <= last) {
                continue;
            }
            state.last_key = Some(key);
            let cells: Vec<WalCell> = entries
                .iter()
                .map(|e| WalCell {
                    name: e.name.to_string(),
                    metadata: state.described.insert(e.name.to_string()).then(|| {
                        e.metadata
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    }),
                    value: match e.value {
                        CellValue::Counter(v) => WalValue::Counter(v),
                        CellValue::Gauge(v) => WalValue::Gauge(v),
                        CellValue::Histogram(h) => WalValue::Histogram(
                            h.config().grouping_power(),
                            h.config().max_value_power(),
                            h.as_slice().to_vec(),
                        ),
                    },
                    window: e.window,
                })
                .collect();
            let bytes = cells_approx_bytes(
                &entries
                    .iter()
                    .map(|e| Cell {
                        name: e.name,
                        metadata: e.metadata,
                        window: e.window.map(|(begin_ns, end_ns)| {
                            metriken_segment::window::Window { begin_ns, end_ns }
                        }),
                        value: match e.value {
                            CellValue::Counter(v) => CellValue::Counter(v),
                            CellValue::Gauge(v) => CellValue::Gauge(v),
                            CellValue::Histogram(h) => CellValue::Histogram(h),
                        },
                    })
                    .collect::<Vec<_>>(),
            );
            rows.push(DWalRow {
                stream: sampler.clone(),
                ts: ts as i64,
                wall_offset,
                row: wal::encode_wal_row(&cells)?,
            });
            self.account(&sampler).add_row(bytes, ts as i64);
        }
        Ok(())
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
                        let next = (state.occupants.len() + state.keys.len()) as u64;
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
                self.write_long(&g.name, window, present, first_seen, ts, wall_offset, rows)?;
            }
        }
        Ok(())
    }

    /// Write one tick of a long group: `present` already numbered, and
    /// `first_seen` the occupants numbered this tick. Anchors the columns on
    /// the segment's first row, and restates the occupants seen since the
    /// last restatement when one is due.
    #[allow(clippy::too_many_arguments)]
    fn write_long(
        &mut self,
        name: &str,
        window: Option<(u64, u64)>,
        present: Vec<LongOccupant>,
        first_seen: Vec<Occupant>,
        ts: u64,
        wall_offset: i64,
        rows: &mut Vec<DWalRow>,
    ) -> Result<(), Error> {
        self.long
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string());
        let state = self
            .groups
            .get_mut(name)
            .expect("the caller made the group's state");
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
            stream: name.to_string(),
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
        let stream = occupants::stream_of(name);
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
        self.account(name).add_row(bytes, ts as i64);
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
            if let Some(state) = self.samplers.get_mut(stream) {
                state.described.clear();
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
