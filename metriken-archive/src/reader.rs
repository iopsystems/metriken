//! [`ArchiveReader`]: an archive of parquet segments read as one
//! `metriken_query::MetricsSource`, composed of one sub-source per table: a
//! `ParquetReader` for a single-segment table, a `SegmentedParquetReader`
//! for one sealed more than once. A table's live WAL tail is materialized
//! as its newest segment (`metriken_segment::wal::materialize_wal_tail`).
//!
//! Moved from rezolus's `crates/rez` (`RezReader`), phase 3 of
//! `docs/journal/2026-09-28-high-cardinality-stack.md`.
//!
//! **Same-timeline union.** An archive tables its acquisition GROUPS, not its
//! samplers: `cpu_usage/percpu` and `cpu_usage/softirq` are two tables of
//! one sampler, with disjoint metric sets. A query naming metrics from one
//! table is answered by that table's own (lazy, footer-only) reader. A query
//! naming metrics from several tables of the SAME sampler OF THE SAME source
//! is answered by composing their readers into one
//! [`metriken_query::UnionMetricsSource`] (through its checking `try_new`,
//! so a producer bug that put one metric name in two tables is a loud
//! error, not a silent first-wins). The union dispatches by metric name, with
//! no timestamp join: each metric keeps its acquisition window from its own
//! table, and the engine aligns independently timestamped series on its
//! evaluation grid as it always does. Routing groups by `(source, sampler)`,
//! so tables of two different sources never union.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use metriken_query::{
    BufferPool, CompositionSource, MetricsSource, ParquetReader, QueryError, QueryOptions,
    QueryResult, RateMode, SegmentedParquetReader, UnionChild, UnionError, UnionMetricsSource,
};

use crate::catalog::Catalog;
use crate::{InMemorySource, IndexRelabel, Reopen};

enum TableReader {
    /// A table read from its segments on demand — one segment or many; the
    /// segmented reader fetches only what a query touches either way. A
    /// group table whose slots the identity index describes opens as the
    /// same reader with a relabelling (see [`IndexRelabel`]).
    Segmented(SegmentedParquetReader),
}

/// Parse one probe segment's footer per table.
///
/// **Threaded natively, sequential on wasm32.** The parallel version is a
/// measured win — footer parsing is ~0.45 ms per table and linear in TABLE
/// count, so it is what grows as samplers and acquisition groups multiply —
/// but `std::thread::spawn` on `wasm32-unknown-unknown` panics rather than
/// failing to compile, so a shared implementation would build cleanly and then
/// abort in the browser on the first archive opened.
#[cfg(not(target_arch = "wasm32"))]
fn probe_tables(pending: &[PendingProbe]) -> Vec<Result<ProbedTable, String>> {
    // Chunked, and each worker gets its OWN small pool.
    //
    // One thread per table spawned 50 threads that then serialized on the
    // shared pool's mutex — 0.45 ms/table became 0.25 ms, where the core count
    // says it should have collapsed. A probe reader is thrown away the moment
    // its names are read, so it has nothing to gain from the shared pool and
    // everything to lose by contending for it. Chunking amortizes spawn cost
    // over the same threads.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(pending.len().max(1));
    let chunk = pending.len().div_ceil(workers.max(1));
    std::thread::scope(|scope| {
        let handles: Vec<_> = pending
            .chunks(chunk.max(1))
            .map(|batch| {
                scope.spawn(move || {
                    // 8 MiB is ample for footer-only reads and is never
                    // shared, so it cannot be contended.
                    let pool = BufferPool::new(8 * 1024 * 1024);
                    batch
                        .iter()
                        .map(|p| probe_one(p, &pool))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| {
                h.join()
                    .unwrap_or_else(|_| vec![Err("probe panicked".to_string())])
            })
            .collect()
    })
}

#[cfg(target_arch = "wasm32")]
fn probe_tables(pending: &[PendingProbe]) -> Vec<Result<ProbedTable, String>> {
    let pool = BufferPool::new(8 * 1024 * 1024);
    pending.iter().map(|p| probe_one(p, &pool)).collect()
}

/// One table's footer: the metric names it holds and its row cadence.
fn probe_one(
    (sampler, bytes, span): &PendingProbe,
    pool: &Arc<BufferPool>,
) -> Result<ProbedTable, String> {
    let probe = ParquetReader::open_bytes_with_pool(bytes.clone(), Arc::clone(pool))
        .map_err(|e| format!("probing table {sampler}: {e}"))?;
    let names = TableNames {
        counters: probe.counter_names().into_iter().collect(),
        gauges: probe.gauge_names().into_iter().collect(),
        histograms: probe.histogram_names().into_iter().collect(),
    };
    Ok((sampler.clone(), names, probe.interval(), *span))
}

/// One table awaiting its footer probe: its key, the segment bytes to parse,
/// and the span the catalog already answered.
type PendingProbe = (String, Vec<u8>, Option<(u64, u64)>);

/// What a probe yields: the table's key, its metric names, its row spacing and
/// its span.
type ProbedTable = (String, TableNames, f64, Option<(u64, u64)>);

/// Where a table's segment payloads come from.
///
/// v2 (tar) has no index — the whole archive is already in memory by the time
/// the reader sees it, so its bytes are handed over directly. v3 (SQLite) is
/// indexed by `(recording_id, sampler, seq)`, so a table's payload can be
/// fetched when it is first queried and never before. That is the difference
/// worth having: at open the reader needs one segment per table for its name
/// catalog, and the catalog answers everything else.
enum SegmentSource {
    Bytes(Vec<Vec<u8>>),
    Db {
        /// Opens the archive again, for a table read after the reader's own
        /// catalog handle is gone.
        reopen: Reopen,
        recording_id: i64,
        sampler: String,
    },
    /// A catalog that exists only in memory, shared by every table of the
    /// archive it came from.
    ///
    /// The `Db` arm above reopens the file per lazy read, which a byte-backed
    /// archive cannot do — there is no path, and re-deserializing the image
    /// per table would copy the whole archive once per table. So this arm
    /// shares one connection instead. The `Mutex` is what makes that legal:
    /// `rusqlite::Connection` is `Send` but not `Sync`, and a `SamplerReader`
    /// is read from several threads on the native probe path.
    SharedDb {
        db: Arc<std::sync::Mutex<Box<dyn Catalog>>>,
        recording_id: i64,
        sampler: String,
    },
}

/// A connection to the archive a store reads through: the file, opened once
/// on first use and kept, or the catalog every table of a byte-backed
/// archive shares.
enum DbHandle {
    Reopen {
        reopen: Reopen,
        conn: std::sync::Mutex<Option<Box<dyn Catalog>>>,
    },
    Shared(Arc<std::sync::Mutex<Box<dyn Catalog>>>),
}

impl DbHandle {
    fn with<T>(
        &self,
        f: impl FnOnce(&dyn Catalog) -> Result<T, String>,
    ) -> Result<T, Box<dyn std::error::Error + Send + Sync>> {
        match self {
            DbHandle::Reopen { reopen, conn } => {
                let mut conn = conn.lock().unwrap_or_else(|e| e.into_inner());
                if conn.is_none() {
                    *conn = Some(reopen()?);
                }
                Ok(f(conn.as_deref().expect("opened above"))?)
            }
            DbHandle::Shared(db) => {
                // A poisoned lock means another thread panicked mid-read. The
                // catalog is read-only here, so nothing is half-written and
                // the data is still good.
                let db = db.lock().unwrap_or_else(|e| e.into_inner());
                Ok(f(db.as_ref())?)
            }
        }
    }
}

/// One table's segments, fetched from the archive as a query needs them —
/// the `SegmentStore` the segmented reader pulls through.
///
/// Built when the table is first queried: the sealed segments' sequence
/// numbers from the catalog, and the live WAL tail materialized once as the
/// newest segment. The tail is the one thing held in memory — it is at most
/// a segment's worth of rows, and it is the part of the table SQLite cannot
/// hand back as parquet. Sealed segments are read by sequence number on
/// demand; one that retention has removed since reads as gone and the
/// reader skips it, which is exactly what a live hindsight buffer wants.
struct DbSegmentStore {
    db: DbHandle,
    recording_id: i64,
    sampler: String,
    seqs: Vec<u64>,
    tail: Option<bytes::Bytes>,
}

/// A table's unsealed WAL rows as one segment. A table with an occupant
/// stream is long, and its rows are `WalLongRow`s; they are kept in arrival
/// order, which the long reader handles as well as a sorted segment. Encoded
/// with [`default_compression`](crate::default_compression): the segment is
/// held in memory while the reader is open.
fn live_tail(
    db: &dyn Catalog,
    recording_id: i64,
    table: &str,
    long: bool,
) -> Result<Option<metriken_segment::wal::MaterializedTail>, Box<dyn std::error::Error>> {
    let rows = db.live_wal(recording_id, table)?;
    let props = crate::segment_props(crate::default_compression());
    if long {
        metriken_segment::wal::materialize_long_wal_tail_with(table, &rows, false, props)
    } else {
        metriken_segment::wal::materialize_wal_tail_with(table, &rows, props)
    }
}

impl DbSegmentStore {
    fn build(
        db: DbHandle,
        recording_id: i64,
        sampler: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (seqs, tail) = db
            .with(|db| {
                let seqs: Vec<u64> = db
                    .segment_meta(recording_id, sampler)?
                    .into_iter()
                    .map(|(seq, _)| seq)
                    .collect();
                let long = db
                    .tables(recording_id)?
                    .contains(&metriken_segment::occupants::stream_of(sampler));
                let tail = live_tail(db, recording_id, sampler, long)
                    .map_err(|e| e.to_string())?
                    .map(|t| bytes::Bytes::from(t.bytes));
                Ok((seqs, tail))
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            db,
            recording_id,
            sampler: sampler.to_string(),
            seqs,
            tail,
        })
    }
}

impl metriken_query::SegmentStore for DbSegmentStore {
    fn len(&self) -> usize {
        self.seqs.len() + usize::from(self.tail.is_some())
    }

    fn bytes(&self, idx: usize) -> metriken_query::SegmentBytes {
        match self.seqs.get(idx) {
            Some(seq) => {
                let seq = *seq;
                let bytes = self
                    .db
                    .with(|db| db.segment_bytes(self.recording_id, &self.sampler, seq))?;
                Ok(bytes.map(bytes::Bytes::from))
            }
            None if idx == self.seqs.len() => Ok(self.tail.clone()),
            None => Ok(None),
        }
    }
}

impl SegmentSource {
    /// This table as a store the segmented reader fetches from on demand.
    /// `None` when the table has no segments and no tail any more — evicted
    /// between the probe and the query.
    fn store(
        &self,
    ) -> Result<Option<Arc<dyn metriken_query::SegmentStore>>, Box<dyn std::error::Error>> {
        let store: Arc<dyn metriken_query::SegmentStore> = match self {
            SegmentSource::Bytes(b) => Arc::new(metriken_query::InMemorySegments::new(b.clone())),
            SegmentSource::Db {
                reopen,
                recording_id,
                sampler,
            } => Arc::new(DbSegmentStore::build(
                DbHandle::Reopen {
                    reopen: Arc::clone(reopen),
                    conn: std::sync::Mutex::new(None),
                },
                *recording_id,
                sampler,
            )?),
            SegmentSource::SharedDb {
                db,
                recording_id,
                sampler,
            } => Arc::new(DbSegmentStore::build(
                DbHandle::Shared(Arc::clone(db)),
                *recording_id,
                sampler,
            )?),
        };
        Ok((!store.is_empty()).then_some(store))
    }

    /// Every row of an occupant stream, sealed segments first and then the
    /// live WAL: the labels of every occupant the table's rows can name.
    /// Empty for a tar archive, which has no long tables.
    fn occupants(
        &self,
        stream: &str,
    ) -> Result<Vec<metriken_segment::occupants::Occupant>, Box<dyn std::error::Error>> {
        fn read(
            db: &dyn Catalog,
            recording_id: i64,
            stream: &str,
        ) -> Result<Vec<metriken_segment::occupants::Occupant>, Box<dyn std::error::Error>>
        {
            let mut out = Vec::new();
            for (seq, _) in db.segment_meta(recording_id, stream)? {
                // A segment retention took since the catalog read is gone,
                // and so are the rows that named its occupants.
                if let Some(bytes) = db.segment_bytes(recording_id, stream, seq)? {
                    out.extend(
                        metriken_segment::occupants::decode_segment(&bytes)?
                            .into_iter()
                            .map(|(_, o)| o),
                    );
                }
            }
            for row in db.live_wal(recording_id, stream)? {
                out.extend(metriken_segment::occupants::decode_wal_row(&row.row)?);
            }
            Ok(out)
        }
        self.with_catalog(|db, recording_id, _| read(db, recording_id, stream))
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    /// Run `f` against this table's catalog, with its source id and table
    /// name. `None` for an in-memory table, which has no catalog.
    fn with_catalog<T>(
        &self,
        f: impl FnOnce(&dyn Catalog, i64, &str) -> Result<T, Box<dyn std::error::Error>>,
    ) -> Option<Result<T, Box<dyn std::error::Error>>> {
        match self {
            SegmentSource::Bytes(_) => None,
            SegmentSource::Db {
                reopen,
                recording_id,
                sampler,
            } => Some(match reopen() {
                Ok(db) => f(db.as_ref(), *recording_id, sampler),
                Err(e) => Err(e.into()),
            }),
            SegmentSource::SharedDb {
                db,
                recording_id,
                sampler,
            } => {
                let db = db.lock().unwrap_or_else(|e| e.into_inner());
                Some(f(db.as_ref(), *recording_id, sampler))
            }
        }
    }
}

/// A table's metric names by kind, probed from one segment's footer.
///
/// Kept split rather than merged because `counter_names`/`gauge_names`/
/// `histogram_names` are distinct questions on `MetricsSource` — a merged set
/// would answer all three with the same list, which is wrong and would not
/// have failed loudly.
#[derive(Default)]
struct TableNames {
    counters: std::collections::HashSet<String>,
    gauges: std::collections::HashSet<String>,
    histograms: std::collections::HashSet<String>,
}

impl TableNames {
    /// Whether this table holds `metric` under any kind — the routing question.
    fn holds(&self, metric: &str) -> bool {
        self.counters.contains(metric)
            || self.gauges.contains(metric)
            || self.histograms.contains(metric)
    }
}

impl TableReader {
    fn as_dyn(&self) -> &dyn MetricsSource {
        match self {
            TableReader::Segmented(r) => r,
        }
    }

    fn union_child(&self) -> UnionChild {
        match self {
            TableReader::Segmented(r) => UnionChild::from(r),
        }
    }

    /// The same borrow as [`union_child`](Self::union_child), for the other
    /// composition: merging this table into a labelled multi-source rather
    /// than into a same-recording union.
    fn composition_source(&self) -> CompositionSource {
        match self {
            TableReader::Segmented(r) => CompositionSource::from(r),
        }
    }
}

/// One opened per-table reader. A table is one or more parquet segments, so
/// the backing source is either a plain `ParquetReader` (single segment) or a
/// `SegmentedParquetReader` (many) — both are `MetricsSource`, and everything
/// below this point treats them identically.
///
/// `sampler` is the table's own key — `<sampler>/<group>` for a V3
/// acquisition-group table, just `<sampler>` for a V2 (or V3 windowless)
/// table. `rez::table_sampler` recovers the manifest-level sampler name from
/// it; this field is never split eagerly because most tables (every V2
/// table, and any V3 sampler with only one group) don't need it to be.
///
/// `recording` is the index (within `from_recordings`'s input) of the
/// recording this table belongs to — carried so `route()` can tell "two
/// group tables of one sampler in one recording" (union) apart from "the
/// same sampler's table in two DIFFERENT recordings" (still a refusal; see
/// the module docs). An index rather than the recording's `dir` string:
/// `dir` is a display name derived from labels (`recording_dir_slug`), not a
/// guaranteed-unique identity — two recordings with the same labels are
/// entirely legal and would collide on `dir`.
struct SamplerReader {
    recording: usize,
    sampler: String,
    /// Every metric name this table holds, probed from ONE segment's footer at
    /// open. A table's segments share a schema — `schema_hash` is what asserts
    /// it — so one probe answers routing for all of them.
    ///
    /// This exists so `owners` can decide whether a query could touch this
    /// table WITHOUT building its reader. Routing used to ask each table's open
    /// reader for its columns, which meant every table in the archive was
    /// opened before any query was parsed: measured at ~1.37 ms per segment
    /// over 418 segments, of which a typical query needs 11%.
    names: TableNames,
    /// The table's row-time span, probed from its FIRST and LAST segment's
    /// footers at open.
    ///
    /// `time_range` is asked on paths that never query — `mcp query` calls it
    /// before evaluating anything — and answering it from the full readers
    /// forced every table open, which defeated the whole point of the lazy
    /// build. Two footers per table answers it instead of all of them.
    span: Option<(u64, u64)>,
    /// The table's row spacing, probed from the same first segment.
    ///
    /// A table is one sampler's cadence, so any of its segments answers for all
    /// of them. Like `span`, this is asked on paths that never query
    /// (`describe-metrics`, `analyze-correlation`) and would otherwise open
    /// every table to find out.
    interval: f64,
    /// Where the full segment set comes from, resolved on first use.
    segments: SegmentSource,
    /// Whether the identity index describes this table's slots — the archive
    /// holds `caller_rows` for its stream — in which case the table is read
    /// through the reader's [`IndexRelabel`] rather than straight off its
    /// parquet.
    indexed: bool,
    /// The table's occupant stream, when it is a long table: its series
    /// carry only an occupant number, and this stream says which labels each
    /// number stands for. See `metriken_segment::occupants`.
    occupants: Option<String>,
    /// What reads an indexed table's caller rows into a relabelling. The
    /// archive's own long tables need none (their occupant stream is read
    /// here); a container with an older identity index supplies one.
    index: Option<Arc<dyn IndexRelabel>>,
    pool: Arc<BufferPool>,
    /// Built on first access, never at open.
    reader: std::sync::OnceLock<Option<TableReader>>,
    /// Row timestamps, read once.
    ///
    /// Reading them means decoding a whole column, and the cross-cadence
    /// policy asks for them on every query that spans samplers. A reader's
    /// view is fixed once it is open — a live archive is re-opened, and the
    /// WAL tail is materialized at open — so one read is enough, and the
    /// viewer holds its readers for the life of the process.
    row_timestamps: std::sync::OnceLock<Vec<u64>>,
}

impl SamplerReader {
    /// The table's reader, built on first use — `None` if its segments have
    /// gone since the probe.
    ///
    /// Every table opens as the segmented reader over a store that fetches
    /// segments from the archive as queries touch them: nothing is read here
    /// beyond the catalog and the live WAL tail, and a query pays for the
    /// segments in its range rather than for the table. The splice below
    /// PromQL evaluation is what makes a `rate()` window straddling a seal
    /// boundary compute on complete data; a one-segment table has nothing to
    /// splice and costs nothing extra for going through it.
    ///
    /// **Fallible because a `.rez` is readable while it is written.** This used
    /// to `.expect("segments opened at probe time cannot fail to reopen")`,
    /// which holds for a finished archive and not for the live one the format
    /// advertises: `table_segments` returns the sealed segments plus the
    /// materialized WAL tail, and hindsight's retention deletes as it goes, so
    /// a quiet sampler's only rows can be evicted between the probe that named
    /// this table and the query that opens it. That left the viewer and MCP
    /// panicking on a rolling buffer — the one thing the buffer exists to be
    /// read as.
    ///
    /// A vanished table is reported as absent, which is what a table with no
    /// rows already is: `from_v3` skips it at open, and a query naming only
    /// metrics it held gets the ordinary "references no metric present in this
    /// .rez" error rather than a crash. `OnceLock<Option<_>>` so a table that
    /// has gone is not re-fetched on every subsequent query.
    /// Per-metric column metadata, read from this table's FIRST segment.
    ///
    /// A table's segments share a schema (`schema_hash` is what asserts it), so
    /// one footer answers for all of them. Read on demand rather than at open:
    /// nothing on the query path wants this, and probing it for every table of
    /// a large archive would pay a cost the routing catalog deliberately avoids.
    ///
    /// A metric spans one column per label set; the first column carrying a
    /// given `metric` key wins, since they agree on unit and description and
    /// differ only in their labels.
    fn metric_metadata(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

        let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();

        let first = match self.segments.store() {
            Ok(Some(store)) => store.bytes(0),
            Ok(None) => return out,
            Err(e) => {
                tracing::warn!("fetching segments for {}: {e}", self.sampler);
                return out;
            }
        };
        let first = match first {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return out,
            Err(e) => {
                tracing::warn!("fetching the first segment of {}: {e}", self.sampler);
                return out;
            }
        };

        let builder = match ParquetRecordBatchReaderBuilder::try_new(first) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("reading schema for {}: {e}", self.sampler);
                return out;
            }
        };

        for field in builder.schema().fields() {
            let meta = field.metadata();
            let Some(name) = meta.get("metric") else {
                continue;
            };
            out.entry(name.clone())
                .or_insert_with(|| meta.clone().into_iter().collect());
        }

        out
    }

    fn reader(&self) -> Option<&TableReader> {
        self.reader
            .get_or_init(|| {
                let pool = Arc::clone(&self.pool);
                // The store fetches segments as queries touch them; nothing
                // is read here beyond the catalog and the live WAL tail.
                let store = match self.segments.store() {
                    Ok(Some(store)) => store,
                    // Empty is the eviction case; an error is a genuine read
                    // failure. Both mean this table cannot answer, and neither
                    // is worth taking the process down for.
                    Ok(None) => {
                        self.warn_evicted();
                        return None;
                    }
                    Err(e) => {
                        tracing::warn!("fetching segments for {}: {e}", self.sampler);
                        return None;
                    }
                };
                let opened = match self.relabel() {
                    Some(Ok(relabel)) => {
                        SegmentedParquetReader::open_relabeled_with_pool(store, pool, relabel)
                    }
                    Some(Err(())) => return None,
                    None => SegmentedParquetReader::open_with_pool(store, pool),
                };
                opened
                    .map(TableReader::Segmented)
                    .map_err(|e| {
                        tracing::warn!("reopening table {}: {e}", self.sampler);
                    })
                    .ok()
            })
            .as_ref()
    }

    fn warn_evicted(&self) {
        tracing::warn!(
            "table {} had rows at open and none now; it was evicted or \
             rotated while being read, and is reported as absent",
            self.sampler
        );
    }

    /// The relabelling an indexed table opens with: its identity index
    /// replayed into occupancy spans. `None` for a table the index does not
    /// describe; `Some(Err(()))` when the index could not be read or
    /// replayed, which is a failure to open the table, reported the same way
    /// a segment that would not parse is — never a silent fall-through to
    /// the plain path, which would file every reused slot's rows under its
    /// first occupant.
    fn relabel(&self) -> Option<Result<Arc<dyn metriken_query::ColumnRelabel>, ()>> {
        if let Some(stream) = &self.occupants {
            return Some(match self.segments.occupants(stream) {
                Ok(rows) => Ok(Arc::new(metriken_query::long::OccupantLabels::new(rows))),
                Err(e) => {
                    tracing::warn!("reading the occupant stream {stream}: {e}");
                    Err(())
                }
            });
        }
        if !self.indexed {
            return None;
        }
        let hook = self.index.as_ref()?;
        let first_row_ts = self.span.map(|(b, _)| b).unwrap_or(0);
        let relabel = self.segments.with_catalog(|db, source_id, table| {
            hook.relabel(db, source_id, table, first_row_ts)
                .map_err(|e| e.into())
        })?;
        Some(relabel.map_err(|e| {
            tracing::warn!("reading the caller-row index for {}: {e}", self.sampler);
        }))
    }

    fn row_timestamps(&self) -> &[u64] {
        self.row_timestamps.get_or_init(|| {
            self.reader()
                .map(|r| r.as_dyn().sample_timestamps())
                .unwrap_or_default()
        })
    }
}

/// A `.rez` archive presented as one `MetricsSource`. Phase B: a single
/// recording; every recording's tables are flattened into `tables`
/// (multi-recording faceting is Phase C).
pub struct ArchiveReader {
    /// Shared so a lazy composition child can hold a table and open it on
    /// first use after this reader has handed it out.
    tables: Vec<Arc<SamplerReader>>,
    /// The (first) recording's file-level metadata, for `source`/`version`/etc.
    metadata: BTreeMap<String, String>,
    filename: Option<String>,
    /// Whether every recording behind this reader was cleanly finalized.
    ///
    /// Carried rather than only logged because the log has no reader in a
    /// browser: `tracing::warn!` in wasm goes nowhere, and "this recording
    /// stops earlier than the run did" is exactly the kind of thing a
    /// consumer must be able to say out loud. See [`Self::complete`].
    complete: bool,
    /// How many recordings this reader's tables came from.
    ///
    /// Set once at construction rather than derived from
    /// `SamplerReader::recording`, because that field's meaning is NOT uniform:
    /// `from_v3_db` numbers recordings archive-globally while `from_recordings`
    /// numbers them per call. Counting distinct values across tables therefore
    /// happens to be right today only because `open_with_pool`'s tar branch
    /// passes every recording in one call. Rebuilding that branch on top of
    /// `open_recordings` -- the obvious cleanup -- would give every table
    /// `recording == 0` and silently defeat the check in
    /// [`composition_sources`](Self::composition_sources) with its test still
    /// green. Carrying the count here makes the invariant explicit and
    /// refactor-proof.
    recordings: usize,
}

/// One `ArchiveReader` per recording, paired with that recording's label set.
pub type LabeledRecordings = Vec<(BTreeMap<String, String>, ArchiveReader)>;

impl ArchiveReader {
    /// This recording's tables as composition sources, for merging the whole
    /// recording into a labelled multi-source next to other artifacts — see
    /// `metriken_query::ParquetBuilder::source_labeled`.
    ///
    /// This is the cross-*artifact* composition, distinct from the
    /// same-recording union `route` builds: there, several tables of one
    /// sampler answer one query under their own identities; here, the whole
    /// recording becomes one labelled participant among several recordings,
    /// each contributing the SAME metric names under a different injected
    /// label. That is why these are `CompositionSource` and not `UnionChild`
    /// — the two composers have opposite disjointness rules.
    ///
    /// **This opens every table.** Routing deliberately builds only the tables
    /// a query names (see `SamplerReader::reader`), because a typical query
    /// touches a small fraction of a large archive. A composed source cannot
    /// work that way: the builder dispatches across its children by metric
    /// name and needs them all up front. Callers pay the archive's full open
    /// cost, so compose once and reuse the result rather than per query.
    ///
    /// A table whose segments cannot be parsed is skipped rather than
    /// poisoning the composition, matching `SamplerReader::reader` — unless
    /// EVERY table is skipped, which is an error rather than an empty vec (see
    /// below).
    ///
    /// # The caller owns label uniqueness
    ///
    /// The injected label is what keeps two recordings' identically-named
    /// series apart, and this method cannot see it. `open_recordings` does not
    /// solve that on the caller's behalf: **two recordings may legally carry
    /// identical label sets** — two endpoints that infer the same `source` and
    /// `host` do exactly that, and the recorder only warns. A caller that
    /// derives its injected label from a recording's own labels can therefore
    /// give both arms the same one and reproduce the merge this method refuses,
    /// one level up. Derive the label from something the caller knows is
    /// unique per participant instead.
    ///
    /// # Composition tolerates name-sharing that `route` refuses
    ///
    /// `route` answers a query naming metrics from several tables of one
    /// sampler through `UnionMetricsSource::try_new`, which REFUSES when those
    /// tables share a metric name. This composition has no equivalent check,
    /// because it is eager and whole-archive: refusing here would reject an
    /// entire archive over a name the caller may never query. Shared names are
    /// real and shipped — `gpu_nvidia` and `gpu_amd_smi` both publish
    /// `gpu_utilization` — so on a host running both, a composed query that
    /// aggregates the name away (`sum by (id) (gpu_utilization)`) sums across
    /// vendors where `route` would have refused. The series remain
    /// distinguishable by their `sampler` label; a caller that must not
    /// conflate them should group by it.
    ///
    /// # Errors
    ///
    /// Refuses a reader that **flattens several recordings**, which is what
    /// [`open_with_pool`](Self::open_with_pool) produces. Every recording holds
    /// the same sampler names, so composing a flattened reader hands the
    /// builder several children carrying the same metric names under one
    /// label. `ParquetBuilder` does not dedup or dispatch — it CONCATENATES
    /// (`MultiParquetSource`: "same (metric, label) pairs in multiple files
    /// produce duplicate series") — so the result is duplicate,
    /// indistinguishable series and a silently doubled aggregate, with
    /// histograms interleaving into one series index. Not, as an earlier
    /// version of this comment claimed, one recording replacing another: that
    /// is `UnionSource`'s first-wins, a different composer.
    ///
    /// Open with [`open_recordings`](Self::open_recordings) instead: one
    /// reader per recording, each composable under its own label.
    ///
    /// Also refuses when every table was skipped but the recording has tables,
    /// since an empty composition is indistinguishable from an arm that was
    /// simply flat. A recording with no tables at all returns `Ok(vec![])` —
    /// check [`is_empty`](Self::is_empty) first if that matters.
    pub fn composition_sources(
        &self,
    ) -> Result<Vec<CompositionSource>, Box<dyn std::error::Error>> {
        if self.recordings > 1 {
            return Err(format!(
                "cannot compose a reader flattening {} recordings: they publish the same \
                 metric names, so composing them emits duplicate, indistinguishable series \
                 and double-counts every aggregation over them. Open the archive with \
                 `open_recordings` and compose each recording under its own label.",
                self.recordings
            )
            .into());
        }

        // Each table is a lazy child: its names, span and interval come from
        // the catalog this reader probed at open, and the table itself opens
        // the first time a composed query names one of its metrics. Composing
        // an archive used to open every table here — every segment footer of
        // every table, before any query — which on a 1.3 GB archive was
        // seconds per artifact and, before the segmented reader stopped
        // holding bytes, 4 GB.
        //
        // A table whose segments cannot be opened when it is finally asked
        // answers empty, logged by the child; there is no longer an up-front
        // moment at which "none of them could be opened" is known.
        let sources: Vec<CompositionSource> = self
            .tables
            .iter()
            .map(|t| {
                let mut catalog = metriken_query::CompositionCatalog::new(t.interval)
                    .counters(t.names.counters.iter().cloned())
                    .gauges(t.names.gauges.iter().cloned())
                    .histograms(t.names.histograms.iter().cloned())
                    .metadata(
                        self.metadata
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    );
                if let Some((start, end)) = t.span {
                    catalog = catalog.time_range_ns(start, end);
                }
                let table = Arc::clone(t);
                CompositionSource::lazy(catalog, move || {
                    Ok(table.reader().map(TableReader::composition_source))
                })
            })
            .collect();

        Ok(sources)
    }
    /// The evaluation timestamps a composed query needs to stay faithful to
    /// this recording's cadences, or `None` when it does not need any.
    ///
    /// Querying this reader directly applies the cross-cadence policy itself
    /// (see `query_range_opts`): when a query spans samplers recording at
    /// different rates, the points are moved onto the SLOW table's own rows, so
    /// every point lands where both operands genuinely have data. A caller that
    /// instead composes [`composition_sources`](Self::composition_sources) into
    /// a labelled multi-source queries that composed reader, never this one, and
    /// so silently loses the policy — the composed reader falls back to the
    /// uniform grid and holds the slow operand's value forward between its real
    /// readings.
    ///
    /// Pass the result to `QueryOptions::with_eval_timestamps` on the composed
    /// query to restore it:
    ///
    /// ```rust,ignore
    /// let mut opts = QueryOptions::default();
    /// if let Some(points) = reader.eval_timestamps_for(query, step_s, opts.rate_mode) {
    ///     opts = opts.with_eval_timestamps(Some(points));
    /// }
    /// composed.query_range_opts(query, start_s, end_s, step_s, &opts)
    /// ```
    ///
    /// `None` means the composed query needs no adjustment: the query touches
    /// one cadence (the overwhelming majority), or `rate_mode` is
    /// [`RateMode::Raw`], which places points at real un-snapped sample times
    /// by contract and must not have them relocated.
    ///
    /// # This answers for ONE recording
    ///
    /// The timestamps are absolute, so they describe this recording's timeline
    /// and no other. A caller composing SEVERAL recordings cannot simply pick
    /// one: two jobs that ran at different wall-clock times share no instants,
    /// and imposing one's rows on the other puts every point where the other
    /// has no data — the very fault this exists to avoid. Apply this when
    /// exactly one recording is in the composition, or when every recording
    /// answers with the same timestamps; otherwise there is no well-defined
    /// alignment and the uniform grid is the honest fallback.
    pub fn eval_timestamps_for(
        &self,
        query: &str,
        step_s: f64,
        rate_mode: RateMode,
    ) -> Option<Arc<[u64]>> {
        self.cross_cadence_eval_timestamps(query, step_s, rate_mode)
    }

    /// Every metric's column metadata: `unit`, `description`, `metric_type`,
    /// and the metric's label keys, exactly as the recorder wrote them into the
    /// parquet schema.
    ///
    /// [`MetricsSource`] answers which metrics exist and what labels they
    /// carry, but not what they MEAN — a consumer building a metric catalog
    /// (systemslab populates one at import) needs the unit and description too,
    /// and those live only in the columns' arrow metadata.
    ///
    /// Reads one segment footer per table, on demand. Nothing on the query path
    /// calls this, so the cost is paid only by a consumer that wants the
    /// catalog, and never at open.
    ///
    /// Tables hold disjoint metric names, so merging is unambiguous in the
    /// normal case. Where two samplers deliberately share a name — `gpu_nvidia`
    /// and `gpu_amd_smi` both publish `gpu_utilization`, since only one
    /// populates on a given host — the first table wins. They describe the same
    /// vendor-neutral quantity, so unit and description agree.
    pub fn metric_metadata(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for table in &self.tables {
            for (metric, meta) in table.metric_metadata() {
                out.entry(metric).or_insert(meta);
            }
        }
        out
    }

    /// The tables whose reader has been built so far. Tables open lazily,
    /// on the first query or composed read that names one of their metrics;
    /// this is how a caller (or a test) sees which have.
    pub fn opened_tables(&self) -> Vec<String> {
        self.tables
            .iter()
            .filter(|t| t.reader.get().is_some())
            .map(|t| t.sampler.clone())
            .collect()
    }

    /// Whether this recording holds no tables at all.
    ///
    /// An arm that produced no rows — an endpoint that was reachable but never
    /// scraped successfully — still gets a manifest row, so "how many
    /// recordings" and "how many recordings carry data" are different
    /// questions. Consumers that can only read one recording care about the
    /// second one: an empty arm collides with nothing.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Whether every recording behind this reader was cleanly finalized.
    ///
    /// False means the archive was opened while it was being written, or was
    /// copied or killed mid-run: it is readable up to its last checkpoint, and
    /// rows after that are simply not in it. That is worth SAYING rather than
    /// rendering as a recording that ended early — most sharply for a `.rez`
    /// handed over as bytes, where SQLite's own `-wal` sidecar (a separate
    /// file, holding commits not yet checkpointed into this one) did not come
    /// along. Reading the same archive by path, next to its sidecar, sees
    /// further.
    pub fn complete(&self) -> bool {
        self.complete
    }

    /// Several readers' tables as one reader: the single-source view of a
    /// multi-source archive. Its metadata is the first reader's, and it is
    /// complete when every reader is.
    ///
    /// Flattening several recordings gives every table name several
    /// owners, so routing refuses a query that names one, and
    /// [`composition_sources`](Self::composition_sources) refuses outright.
    /// Prefer one reader per source.
    pub fn flatten(readers: Vec<ArchiveReader>, filename: Option<String>) -> Self {
        let mut tables = Vec::new();
        let mut metadata = BTreeMap::new();
        let mut complete = true;
        let mut recordings = 0usize;
        for (i, reader) in readers.into_iter().enumerate() {
            recordings += reader.recordings;
            if i == 0 {
                metadata = reader.metadata;
            }
            complete &= reader.complete;
            tables.extend(reader.tables);
        }
        Self {
            tables,
            metadata,
            filename,
            complete,
            recordings,
        }
    }

    /// One reader per source of an archive's catalog, without materializing
    /// it: the catalog answers spans with no segment read, and one segment
    /// per table answers the name catalog. Everything else waits until a
    /// query asks for that table.
    ///
    /// `reopen` opens the archive again for a table first read after this
    /// call returns (a file); `None` makes every table share `db` behind
    /// a lock (bytes, which cannot be reopened). `index` reads a table that
    /// has caller rows under its name into a relabelling.
    ///
    /// `path` is `Some` only when the archive is a file. It decides how a
    /// table fetches its segments later: from the file, reopened per read, or
    /// from this one shared in-memory catalog. Everything else — the probe,
    /// the spans, the WAL tail — is identical, which is the point.
    pub fn from_catalog(
        db: Box<dyn Catalog>,
        reopen: Option<Reopen>,
        pool: Arc<BufferPool>,
        index: Option<Arc<dyn IndexRelabel>>,
    ) -> Result<LabeledRecordings, Box<dyn std::error::Error>> {
        let shared = Arc::new(std::sync::Mutex::new(db));
        let db = shared.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();

        for (recording, rec) in db.sources()?.into_iter().enumerate() {
            if let Some(wrote) = rec.metadata.get(dendro::keys::ENCODER) {
                if !crate::encoder::READABLE_ENCODERS.contains(&wrote.as_str()) {
                    return Err(format!(
                        "recording {} was written by encoder {wrote:?}, which this reader \
                         does not decode (it reads {}); read it with a newer release",
                        crate::source_name(&rec.labels),
                        crate::encoder::READABLE_ENCODERS.join(", ")
                    )
                    .into());
                }
            }
            if !rec.complete {
                tracing::warn!(
                    "recording {} was not cleanly finalized; it was recovered up to its \
                     last checkpoint and data after that may be missing",
                    crate::source_name(&rec.labels)
                );
            }
            // Two phases on purpose. SQLite is serial (one connection, and
            // `rusqlite::Connection` is not `Sync`) but cheap; parsing a
            // segment footer is the expensive half and is independent per
            // table. Measured at ~0.45 ms per table before splitting them —
            // linear in TABLE COUNT, not archive bytes, so it is the cost that
            // grows as samplers and acquisition groups multiply.
            let mut pending: Vec<PendingProbe> = Vec::new();
            let mut measured_intervals: BTreeMap<String, Option<f64>> = BTreeMap::new();
            // The streams the identity index describes. One catalog query per
            // recording; a table named here is read through the index.
            let indexed: HashSet<String> = db.caller_row_streams(rec.id)?.into_iter().collect();

            // A long table's occupant stream is its labels, not a table of
            // its own: set aside, and named on the table it belongs to.
            let (occupant_streams, samplers): (Vec<String>, Vec<String>) = db
                .tables(rec.id)?
                .into_iter()
                .partition(|s| metriken_segment::occupants::table_of(s).is_some());
            let occupant_streams: HashSet<String> = occupant_streams.into_iter().collect();
            for sampler in samplers {
                let metas = db.segment_meta(rec.id, &sampler)?;

                // The probe segment is the first SEALED one — or, when a table
                // has none, its materialized WAL tail. A quiet sampler in a
                // live hindsight buffer is exactly that: rows in the WAL, no
                // seal yet. Skipping it here would make it invisible to the
                // reader, which the eager path never did because
                // `table_segments` splices the tail in.
                let probe_bytes = match metas.first() {
                    Some((seq, _)) => db.segment_bytes(rec.id, &sampler, *seq)?,
                    None => live_tail(
                        &**db,
                        rec.id,
                        &sampler,
                        occupant_streams
                            .contains(&metriken_segment::occupants::stream_of(&sampler)),
                    )?
                    .map(|t| t.bytes),
                };
                // Nothing sealed and nothing live: the table has no rows at
                // all, so there is nothing to open. Same skip as the eager path.
                let Some(bytes) = probe_bytes else {
                    continue;
                };

                // Span from the catalog, widened by the live WAL: a hindsight
                // buffer's newest rows are unsealed, and a span that stopped at
                // the last seal would report the archive as ending before its
                // most recent data. No BLOB is read for either.
                let (_, sealed) = db.segment_span(rec.id, &sampler)?;
                let wal = db.live_wal_span(rec.id, &sampler)?;
                let span = match (
                    sealed.first_ts.into_iter().chain(wal.first_ts).min(),
                    sealed.last_ts.into_iter().chain(wal.last_ts).max(),
                ) {
                    (Some(b), Some(e)) => Some((b, e)),
                    _ => None,
                };
                // The table's cadence, measured: its span over its gaps. A
                // segment footer states one too, but only because a producer
                // wrote it there, and a `.rez` segment does not — which had
                // every archive reporting the 1 s default whatever it held.
                // The catalog knows the span and the row count already, so
                // this costs no decode. Nothing needs it to be exact any
                // more: it is the staleness hint and the step the viewer
                // opens at, not something the data is rounded to.
                let rows = sealed.rows + wal.rows;
                let measured = span.filter(|_| rows >= 2).map(|(b, e)| {
                    let gap_ns = e.saturating_sub(b) as f64 / (rows - 1) as f64;
                    // Rounded to whole milliseconds, which is what a real
                    // interval is. Purely so the grid the viewer opens at is a
                    // round number and two recordings of one nominal cadence
                    // agree on it — 99.996714ms and 100.004ms would otherwise
                    // be different grids in an A/B. Nothing is rounded TO this.
                    (gap_ns / 1e6).round().max(1.0) / 1e3
                });
                measured_intervals.insert(sampler.clone(), measured);

                pending.push((sampler, bytes, span));
            }

            // Phase two: parse the footers. One probe per table, independent.
            let probed: Vec<Result<ProbedTable, String>> = probe_tables(&pending);

            let mut tables = Vec::new();
            for probe in probed {
                let (sampler, names, probed_interval, span) = probe?;
                let interval = measured_intervals
                    .get(&sampler)
                    .copied()
                    .flatten()
                    .filter(|i| *i > 0.0)
                    .unwrap_or(probed_interval);
                tables.push(Arc::new(SamplerReader {
                    recording,
                    sampler: sampler.clone(),
                    names,
                    span,
                    interval,
                    indexed: indexed.contains(&sampler),
                    occupants: occupant_streams
                        .contains(&metriken_segment::occupants::stream_of(&sampler))
                        .then(|| metriken_segment::occupants::stream_of(&sampler)),
                    index: index.clone(),
                    segments: match &reopen {
                        Some(reopen) => SegmentSource::Db {
                            reopen: Arc::clone(reopen),
                            recording_id: rec.id,
                            sampler,
                        },
                        None => SegmentSource::SharedDb {
                            db: Arc::clone(&shared),
                            recording_id: rec.id,
                            sampler,
                        },
                    },
                    pool: Arc::clone(&pool),
                    reader: std::sync::OnceLock::new(),
                    row_timestamps: std::sync::OnceLock::new(),
                }));
            }

            out.push((
                rec.labels.clone(),
                Self {
                    tables,
                    metadata: rec.metadata,
                    filename: Some(crate::source_name(&rec.labels)),
                    complete: rec.complete,
                    recordings: 1,
                },
            ));
        }
        Ok(out)
    }

    /// A reader over sources already in memory: each table's segments as
    /// parquet bytes, oldest first. No catalog, no WAL, no caller rows.
    pub fn from_in_memory(
        recordings: Vec<InMemorySource>,
        filename: Option<String>,
        pool: Arc<BufferPool>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let metadata = recordings
            .first()
            .map(|r| r.metadata.clone())
            .unwrap_or_default();
        // Captured before the Vec is consumed: this is the reader's recording
        // count, which `composition_sources` refuses on.
        let recording_count = recordings.len();
        let mut tables = Vec::new();
        let mut complete = true;
        for (recording, rec) in recordings.into_iter().enumerate() {
            complete &= rec.complete;
            if !rec.complete {
                tracing::warn!(
                    "recording {} was not cleanly finalized; it was recovered up to its \
                     last checkpoint and data after that may be missing",
                    rec.name
                );
            }
            for (sampler, segments) in rec.tables {
                // A single-segment table keeps the plain reader: the streaming
                // writer's slow samplers and every atomically written archive
                // land here, and there is nothing for the splice to do.
                // Multi-segment tables go to the segment-aware source, which
                // splices raw samples below PromQL evaluation so a `rate()`
                // window straddling a seal boundary still computes on complete
                // data. Both open footer-only against the shared pool.
                // Probe ONE segment for the name catalog. A plain
                // `ParquetReader` is footer-only and skips the four per-segment
                // identity indexes `SegmentedParquetReader` builds, which is
                // the bulk of what open used to cost.
                let probe = segments
                    .first()
                    .ok_or_else(|| format!("table {sampler} has no segments"))?;
                let probe = ParquetReader::open_bytes_with_pool(probe.clone(), Arc::clone(&pool))
                    .map_err(|e| format!("probing table {sampler}: {e}"))?;
                let names = TableNames {
                    counters: probe.counter_names().into_iter().collect(),
                    gauges: probe.gauge_names().into_iter().collect(),
                    histograms: probe.histogram_names().into_iter().collect(),
                };
                let first_span = probe.time_range_ns();
                let interval = probe.interval();
                drop(probe);

                // Segments are in segment order, so the last one carries the
                // table's end. Skipped when there is only one — it is the probe.
                let last_span = match segments.len() {
                    0 | 1 => first_span,
                    _ => {
                        let last = ParquetReader::open_bytes_with_pool(
                            segments[segments.len() - 1].clone(),
                            Arc::clone(&pool),
                        )
                        .map_err(|e| format!("probing table {sampler} tail: {e}"))?;
                        let s = last.time_range_ns();
                        drop(last);
                        s
                    }
                };
                let span = match (first_span, last_span) {
                    (Some((b, _)), Some((_, e))) => Some((b, e)),
                    (only, None) | (None, only) => only,
                };

                tables.push(Arc::new(SamplerReader {
                    recording,
                    sampler,
                    names,
                    span,
                    interval,
                    indexed: false,
                    occupants: None,
                    index: None,
                    segments: SegmentSource::Bytes(segments),
                    pool: Arc::clone(&pool),
                    reader: std::sync::OnceLock::new(),
                    row_timestamps: std::sync::OnceLock::new(),
                }));
            }
        }
        Ok(Self {
            tables,
            metadata,
            filename,
            complete,
            recordings: recording_count,
        })
    }

    /// Sub-readers that hold at least one metric the query references.
    ///
    /// Answered from each table's name catalog, so a table the query cannot
    /// touch is never opened. `referenced_metrics` is parse-only — it does not
    /// need a source, which is exactly why routing can use it and `columns`
    /// (which expands selectors through the source's column map) cannot.
    ///
    /// Label matchers are not consulted: a table holding the metric with no
    /// matching series answers with an empty result, which is correct, and
    /// skipping it here would route on data the catalog does not carry.
    fn owners(&self, query: &str) -> Result<Vec<&SamplerReader>, QueryError> {
        let referenced = metriken_query::referenced_metrics(query)?;
        Ok(self
            .tables
            .iter()
            .map(|t| &**t)
            .filter(|t| referenced.iter().any(|m| t.names.holds(m)))
            .collect())
    }

    /// Resolve the reader that answers every metric a query references: the
    /// single owning table directly, or — when every owner lives in ONE
    /// RECORDING — a fresh [`UnionMetricsSource`] over exactly those tables.
    ///
    /// Tables of different samplers union freely within a recording. They did
    /// not always: a query spanning two samplers used to be refused as
    /// "cross-timeline", because there was no way to say what treating two
    /// separately-read values as simultaneous costs. The query engine now
    /// prices that itself — operands whose acquisition edges differ have their
    /// bands widened to the union of both spans — so the refusal has nothing
    /// left to protect. Measured, the join costs 1.5–3.0 ms on a 32-core host,
    /// under 1% of a 200 ms interval.
    ///
    /// Still refused: the SAME sampler across two DIFFERENT recordings of a
    /// multi-recording (A/B) archive. Those are genuinely different timelines
    /// — different agents, hosts or arms — and unioning them would let
    /// first-wins silently answer from one recording. Errors too when the
    /// query references no known metric.
    fn route(&self, query: &str) -> Result<Routed<'_>, QueryError> {
        let owners = self.owners(query)?;
        match owners.as_slice() {
            [] => Err(QueryError::ParseError(format!(
                "query references no metric present in this .rez: {query}"
            ))),
            // A table whose segments have gone since the probe is absent, so
            // its metrics are too: the same error a query naming a metric this
            // archive never held gets.
            [one] => one.reader().map(Routed::Direct).ok_or_else(|| {
                QueryError::ParseError(format!(
                    "query references {query}, whose table ({}) has been evicted since \
                     this archive was opened",
                    one.sampler
                ))
            }),
            many => {
                // Group owners by (RECORDING, SAMPLER), not by sampler alone:
                // two group tables of one sampler are a same-timeline union
                // ONLY within one recording. `from_recordings` flattens every
                // recording's tables into one `tables` vec, so a metric
                // present in every recording of a multi-recording (A/B)
                // archive — e.g. `cpu_cycles` in each side's `cpu_usage`
                // table — would otherwise look exactly like two group tables
                // of one sampler, and unioning them would let
                // `UnionSource`'s first-wins silently answer from ONE
                // recording instead of refusing (see the module docs).
                // `rez::table_sampler` is the identity function for every V2
                // (or unsplit V3) table, so within one recording this
                // reduces to today's behavior whenever nothing actually
                // split.
                let mut groups: Vec<usize> = many.iter().map(|t| t.recording).collect();
                groups.sort();
                groups.dedup();
                match groups.as_slice() {
                    [_] => {
                        // Building the union only touches each table's
                        // already-open, footer-level name catalog (no
                        // row-group decode), so a fresh one per query is
                        // cheap enough not to need caching.
                        //
                        // `try_new`, not `new`: this composition set is
                        // derived from archive bytes (table schemas plus
                        // parsed table keys), not hand-picked by trusted
                        // code, so a producer/archive bug that put the same
                        // metric name in two "disjoint" group tables of this
                        // sampler must be a loud error, not `UnionSource`'s
                        // silent first-wins.
                        // `filter_map`, not `map`: on a live archive one of
                        // several owners can have been evicted between the
                        // probe and here, and the rest still answer.
                        let children: Vec<UnionChild> = many
                            .iter()
                            .filter_map(|t| t.reader().map(TableReader::union_child))
                            .collect();
                        if children.is_empty() {
                            return Err(QueryError::ParseError(format!(
                                "query references {query}, whose tables have all been \
                                 evicted since this archive was opened"
                            )));
                        }
                        UnionMetricsSource::try_new(children)
                            .map(Routed::Union)
                            .map_err(|e| match e {
                                UnionError::NonDisjoint { duplicates } => {
                                    // Two very different situations produce
                                    // this, and conflating them tells the
                                    // operator their archive is corrupt when
                                    // it is not. Distinct SAMPLERS sharing a
                                    // metric name is legitimate and shipped:
                                    // `gpu_amd_smi` and `gpu_nvidia` both
                                    // publish the vendor-neutral
                                    // `gpu_utilization`, `gpu_temperature` and
                                    // six more, because only one of them ever
                                    // populates on a given host. Two group
                                    // tables of ONE sampler sharing a name is
                                    // a real archive defect.
                                    let mut samplers: Vec<&str> = many
                                        .iter()
                                        .map(|t| crate::table_sampler(&t.sampler))
                                        .collect();
                                    samplers.sort();
                                    samplers.dedup();
                                    if samplers.len() > 1 {
                                        QueryError::ParseError(format!(
                                            "query {query} references metric name(s) published \
                                             by more than one sampler ({}), so it is ambiguous \
                                             which is meant: {}. This is not an archive fault — \
                                             those samplers deliberately share vendor-neutral \
                                             names. Query one of them at a time.",
                                            samplers.join(", "),
                                            duplicates.join(", ")
                                        ))
                                    } else {
                                        QueryError::ParseError(format!(
                                            "query {query} references metric name(s) present in \
                                             more than one acquisition-group table of the same \
                                             sampler — the archive's own tables are not \
                                             disjoint, which should never happen: {}",
                                            duplicates.join(", ")
                                        ))
                                    }
                                }
                                UnionError::Empty => {
                                    unreachable!("the `many` arm always has at least 2 owners")
                                }
                            })
                    }
                    _ => {
                        // Only one case reaches here now: metrics drawn from
                        // more than one RECORDING of a multi-recording
                        // archive. Those are different agents, hosts or arms
                        // on genuinely different timelines, and the widened
                        // band does not make them comparable — unioning them
                        // would let first-wins silently answer from one side.
                        let mut samplers: Vec<&str> = many
                            .iter()
                            .map(|t| crate::table_sampler(&t.sampler))
                            .collect();
                        samplers.sort();
                        samplers.dedup();
                        Err(QueryError::ParseError(format!(
                            "query {query} references metrics ({}) from {} different \
                             recordings of this multi-recording .rez — cross-recording \
                             queries are not supported; query one recording at a time \
                             (see `ArchiveReader::open_recordings`)",
                            samplers.join(", "),
                            groups.len()
                        )))
                    }
                }
            }
        }
    }
}

/// What `route()` resolves a query to: either a borrowed reference straight
/// into one of `ArchiveReader`'s own tables (the common, zero-allocation case),
/// or an owned same-timeline union built fresh for this one query.
enum Routed<'a> {
    Direct(&'a TableReader),
    Union(UnionMetricsSource),
}

impl Routed<'_> {
    fn as_dyn(&self) -> &dyn MetricsSource {
        match self {
            Routed::Direct(r) => r.as_dyn(),
            Routed::Union(u) => u,
        }
    }
}

/// The typical spacing between consecutive rows, or `None` for fewer than two
/// rows (no gap to measure).
///
/// The median, not the mean: a sampler's rows are irregular — 30 s then 60 s
/// apart on a real recording — and a mean is dragged around by the long gaps
/// and by any restart-sized hole in the middle of a recording. The median
/// answers "how often does this table usually produce a row", which is the
/// question being asked.
fn typical_gap_ns(timestamps: &[u64]) -> Option<u64> {
    if timestamps.len() < 2 {
        return None;
    }
    let mut gaps: Vec<u64> = timestamps
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .collect();
    gaps.sort_unstable();
    Some(gaps[gaps.len() / 2]).filter(|g| *g > 0)
}

impl ArchiveReader {
    /// The timestamps a query should be evaluated at, when it spans samplers of
    /// different cadence — `None` when it does not and the uniform grid is
    /// right.
    ///
    /// The grid walks `start + k·step`. A query combining a fast sampler with a
    /// slow one therefore produces most of its points where the slow sampler
    /// has no reading at all: that value is held forward and combined with the fast
    /// operand as if the two were simultaneous.
    ///
    /// The grid cannot be tuned out of this. A slow sampler's rows are not
    /// evenly spaced — measured on a real recording, one sampler's readings
    /// fell 30 s apart and then 60 s apart — so no step and no phase puts a
    /// uniform grid on them. Two earlier attempts are worth recording:
    /// coarsening the STEP relocated the grid and made the combined band
    /// explode (0.85% wide before, 6.7x after), and widening only the averaging
    /// SPAN left the points on the grid, still between the slow sampler's real
    /// readings.
    ///
    /// So hand the engine the slow sampler's own row timestamps. Every point then
    /// lands where both operands genuinely have data, and each rate averages
    /// over the gap it actually spans.
    ///
    /// Returns `None` unless the query really touches more than one cadence, so
    /// single-sampler queries — the overwhelming majority — are untouched.
    ///
    /// Also `None` under [`RateMode::Raw`], which already answers this question
    /// its own way: Raw places points at the real, un-snapped sample
    /// timestamps. Relocating them would contradict that contract, and would
    /// break the query outright — Raw's counter producer reads sample pairs
    /// and ignores supplied points, while the gauge producers honour them, so
    /// a counter-and-gauge expression would have its two sides land on
    /// different instants and intersect nowhere.
    fn cross_cadence_eval_timestamps(
        &self,
        query: &str,
        step_s: f64,
        rate_mode: RateMode,
    ) -> Option<Arc<[u64]>> {
        use crate::table_sampler;

        if matches!(rate_mode, RateMode::Raw) {
            return None;
        }

        let owners = self.owners(query).ok()?;
        if owners.len() < 2 {
            return None;
        }

        // Cadence comes from the ROWS, not from `interval()`: that reports the
        // recording's nominal interval, which every table in an archive shares
        // — on a real recording a 1 s sampler and a 30 s one both answered 1.0,
        // so asking it can never detect a cadence difference. These are the
        // instants the query path reads, nothing having rounded them.
        //
        // Cadence is a property of the SAMPLER, not of a table. Two group
        // tables of one sampler are read together on one schedule; a group that
        // dedups or skips ticks is sparse WITHIN that cadence, not a second
        // cadence, and relocating a query onto its rows would silently change
        // the answer for a query that merely named a sibling group's metric.
        //
        // So a sampler's cadence is the spacing of its DENSEST participating
        // table — the one that shows the underlying read schedule.
        let mut by_sampler: BTreeMap<&str, (u64, &[u64])> = BTreeMap::new();
        for t in &owners {
            let ts = t.row_timestamps();
            let Some(gap) = typical_gap_ns(ts) else {
                continue;
            };
            by_sampler
                .entry(table_sampler(&t.sampler))
                .and_modify(|slot| {
                    if gap < slot.0 {
                        *slot = (gap, ts);
                    }
                })
                .or_insert((gap, ts));
        }
        if by_sampler.len() < 2 {
            return None;
        }

        let fastest = by_sampler.values().map(|(gap, _)| *gap).min()?;
        let (slowest, timestamps) = by_sampler.into_values().max_by_key(|(gap, _)| *gap)?;
        // Deliberately a ratio, not equality: gaps measured from real rows are
        // never exactly equal, so "different cadence" has to mean *materially*
        // different. A sampler read at least twice as far apart as another is a
        // different cadence in any sense that matters here.
        if slowest < fastest.saturating_mul(2) {
            return None;
        }
        // A slow sampler finer than the step is already oversampled by the
        // grid; moving off it would only lose points.
        if (slowest as f64) <= step_s * 1e9 {
            return None;
        }
        Some(timestamps.into())
    }
}

impl MetricsSource for ArchiveReader {
    // ── Query methods: route to the sub-reader owning the referenced metrics. ──
    fn query_range_opts(
        &self,
        expr: &str,
        start_s: f64,
        end_s: f64,
        step_s: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        let aligned;
        let opts = match self.cross_cadence_eval_timestamps(expr, step_s, opts.rate_mode) {
            Some(points) => {
                // Clone and set the one field: `QueryOptions` is
                // `#[non_exhaustive]`, so it cannot be built by literal from
                // here — and cloning preserves whatever else the caller set.
                aligned = opts.clone().with_eval_timestamps(Some(points));
                &aligned
            }
            None => opts,
        };
        self.route(expr)?
            .as_dyn()
            .query_range_opts(expr, start_s, end_s, step_s, opts)
    }
    fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
        self.route(expr)?.as_dyn().query(expr, time)
    }
    fn columns(&self, query: &str) -> Result<HashSet<String>, QueryError> {
        // columns() is answerable as the union — it never crosses timelines.
        let mut out = HashSet::new();
        for t in self.owners(query)? {
            let Some(r) = t.reader() else {
                continue;
            };
            out.extend(r.as_dyn().columns(query)?);
        }
        Ok(out)
    }

    // ── Union metadata / naming / labels ──
    fn counter_names(&self) -> Vec<String> {
        union_sorted(
            self.tables
                .iter()
                .map(|t| t.names.counters.iter().cloned().collect()),
        )
    }
    fn gauge_names(&self) -> Vec<String> {
        union_sorted(
            self.tables
                .iter()
                .map(|t| t.names.gauges.iter().cloned().collect()),
        )
    }
    fn histogram_names(&self) -> Vec<String> {
        union_sorted(
            self.tables
                .iter()
                .map(|t| t.names.histograms.iter().cloned().collect()),
        )
    }
    fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.tables
            .iter()
            .filter(|t| t.names.counters.contains(name))
            .filter_map(|t| t.reader())
            .flat_map(|r| r.as_dyn().counter_labels(name))
            .collect()
    }
    fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.tables
            .iter()
            .filter(|t| t.names.gauges.contains(name))
            .filter_map(|t| t.reader())
            .flat_map(|r| r.as_dyn().gauge_labels(name))
            .collect()
    }
    fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.tables
            .iter()
            .filter(|t| t.names.histograms.contains(name))
            .filter_map(|t| t.reader())
            .flat_map(|r| r.as_dyn().histogram_labels(name))
            .collect()
    }

    // ── Time / interval: union extent, finest interval ──
    fn time_range(&self) -> Option<(f64, f64)> {
        // Seconds view of the same probed spans — see `time_range_ns`.
        self.time_range_ns()
            .map(|(b, e)| (b as f64 / 1e9, e as f64 / 1e9))
    }
    fn time_range_ns(&self) -> Option<(u64, u64)> {
        // From the probed spans, not the readers: this is asked before any
        // query runs, and answering it through `reader()` would open every
        // table and undo the lazy build.
        self.tables
            .iter()
            .filter_map(|t| t.span)
            .reduce(|(a0, a1), (b0, b1)| (a0.min(b0), a1.max(b1)))
    }
    fn interval(&self) -> f64 {
        // Probed per table, not read through `reader()` — see `span`. The
        // finest cadence still wins; only where the number comes from changed.
        let finest = self
            .tables
            .iter()
            .map(|t| t.interval)
            .filter(|i| *i > 0.0)
            .fold(f64::INFINITY, f64::min);
        if finest.is_finite() {
            finest
        } else {
            1.0
        }
    }

    // ── File-level metadata from the recording manifest ──
    fn source(&self) -> String {
        self.metadata.get("source").cloned().unwrap_or_default()
    }
    fn version(&self) -> String {
        self.metadata.get("version").cloned().unwrap_or_default()
    }
    fn filename(&self) -> Option<String> {
        self.filename.clone()
    }
    fn metadata_get(&self, key: &str) -> Option<String> {
        self.metadata.get(key).cloned()
    }
    fn file_metadata(&self) -> HashMap<String, String> {
        self.metadata
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

fn union_sorted(iters: impl Iterator<Item = Vec<String>>) -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for v in iters {
        set.extend(v);
    }
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The typical gap is the MEDIAN, so one long hole (a restart, a missed
    /// poll) does not masquerade as the table's cadence. Moved from rezolus.
    #[test]
    fn typical_gap_is_robust_to_a_single_long_hole() {
        const S: u64 = 1_000_000_000;
        let steady: Vec<u64> = (0..10).map(|i| i * S).collect();
        assert_eq!(typical_gap_ns(&steady), Some(S));

        let mut holed = steady.clone();
        holed.extend((0..10).map(|i| 110 * S + i * S));
        assert_eq!(
            typical_gap_ns(&holed),
            Some(S),
            "a mean would be dragged upward by the hole; the median must not be"
        );

        assert_eq!(typical_gap_ns(&[]), None);
        assert_eq!(typical_gap_ns(&[42]), None);
    }
}
