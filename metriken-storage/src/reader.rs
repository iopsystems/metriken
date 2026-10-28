//! [`ArchiveReader`]: an archive of parquet segments, one recording's tables
//! (or several recordings' flattened), as one [`SegmentedSource`] per table,
//! opened on first use. A table's
//! live WAL tail is materialized as its newest segment
//! (`crate::wal::materialize_wal_tail`). `metriken-query` implements
//! `MetricsSource` for it and routes each query to the tables holding its
//! metrics.
//!
//! **Same-timeline union.** An archive tables its acquisition GROUPS, not its
//! samplers: `cpu_usage/percpu` and `cpu_usage/softirq` are two tables of one
//! sampler, with disjoint metric sets. A query naming metrics from one table is
//! answered by that table's own (lazy, footer-only) reader. A query naming
//! metrics from several tables of the SAME sampler OF THE SAME source is
//! answered by composing their sources into one union (through the checking
//! [`UnionSource::try_new`](crate::union::UnionSource::try_new), so a producer
//! bug that put one metric name in two tables is an error). The union
//! dispatches by metric name, with no timestamp join: each metric keeps its
//! acquisition window from its own table, and the engine aligns independently
//! timestamped series on its evaluation grid as it always does. Routing groups
//! by `(source, sampler)`, so tables of two different sources never union.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::parquet::MultiParquetSource;
use crate::segmented::SegmentedSource;
use crate::{BufferPool, CompositionSource, DataSource, Labels, UnionChild};

use crate::catalog::Catalog;
use crate::{InMemorySource, IndexRelabel, Reopen};

use crate::occupants::Occupant;
use crate::process::{ProcessGroups, ReadGroup, SourceInfo};

/// Each occupant's first row, in the order the rows came.
fn first_per_occupant(rows: impl IntoIterator<Item = Occupant>) -> Vec<Occupant> {
    let mut seen = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|o| seen.insert(o.occupant))
        .collect()
}

/// An occupant stream's sealed segments, decoded, by [`segment_key`].
type OccupantSegments = BTreeMap<u64, Arc<Vec<Occupant>>>;

/// The saved state of one reader's table, for the same table in the reader
/// that replaces it. It holds no catalog connection.
#[derive(Clone, Default)]
struct Carried {
    /// The built table's state; `None` when it saved none.
    handover: Option<crate::Handover>,
    /// The occupant-stream segments it decoded.
    occupant_segments: OccupantSegments,
}

pub enum TableReader {
    /// A table read from its segments on demand — one segment or many; the
    /// segmented reader fetches only what a query touches either way. A
    /// group table whose slots the identity index describes opens as the
    /// same reader with a relabelling (see [`IndexRelabel`]).
    Segmented(Arc<SegmentedSource>),
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

/// One table's footers and names indexes: the metric names they hold
/// between them, the row cadence of the first, and the interval the first
/// declares ([`declared_interval`]), if it does.
fn probe_one(
    PendingProbe {
        sampler,
        known,
        probes,
        span,
    }: &PendingProbe,
    pool: &Arc<BufferPool>,
) -> Result<ProbedTable, String> {
    let mut names = known.names.clone();
    let (mut interval, mut declared) = match known.first {
        Some((interval, declared)) => (Some(interval), declared),
        None => (None, None),
    };
    for bytes in probes {
        let probe = MultiParquetSource::open_bytes_with_pool(bytes.clone(), Arc::clone(pool))
            .map_err(|e| format!("probing table {sampler}: {e}"))?;
        names.counters.extend(probe.counter_names());
        names.gauges.extend(probe.gauge_names());
        names.histograms.extend(probe.histogram_names());
        names.gauge_histograms.extend(probe.gauge_histogram_names());
        if interval.is_none() {
            declared = declared_interval(&probe);
            interval = Some(probe.interval());
        }
    }
    Ok((
        sampler.clone(),
        names,
        interval.unwrap_or(1.0),
        declared,
        *span,
    ))
}

/// The interval, in seconds, a segment's footer declares as
/// `sampling_interval_ms`: what a standalone parquet file carried, and what
/// `to_dendro` writes on the segments it converts, so that the archive reads
/// at the interval the original was read at. A segment the writer seals
/// declares none. A value that is not a positive integer is not a
/// declaration.
fn declared_interval(probe: &MultiParquetSource) -> Option<f64> {
    probe
        .metadata_get("sampling_interval_ms")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(|ms| ms as f64 / 1e3)
}

/// One table awaiting its footer probes: its key, what its names indexes
/// give (see `table_probes`), the segments to parse, and the span the
/// catalog already answered.
struct PendingProbe {
    sampler: String,
    known: KnownNames,
    probes: Vec<Vec<u8>>,
    span: Option<(u64, u64)>,
}

/// What a table's names indexes (`crate::names`) give before any footer is
/// parsed.
#[derive(Default)]
struct KnownNames {
    names: TableNames,
    /// The first sealed segment's row cadence and declared interval, when
    /// its index gives them.
    first: Option<(f64, Option<f64>)>,
}

/// The row cadence and declared interval a probe of the segment `names`
/// indexes would read (`MultiParquetSource::interval`, [`declared_interval`]),
/// or `None` when the probe would fail (a format or layout this reader does
/// not read, an interval that is not a non-negative integer), so the segment
/// is probed and the open fails as it would without the index.
fn indexed_intervals(names: &crate::names::SegmentNames) -> Option<(f64, Option<f64>)> {
    let kv: Vec<parquet::file::metadata::KeyValue> = [
        (crate::format::FORMAT_KEY, &names.format),
        (crate::long::LAYOUT_KEY, &names.layout),
    ]
    .into_iter()
    .filter_map(|(key, value)| {
        let value = value.clone()?;
        Some(parquet::file::metadata::KeyValue::new(
            key.to_string(),
            value,
        ))
    })
    .collect();
    crate::format::check(Some(&kv)).ok()?;
    let ms = match &names.sampling_interval_ms {
        Some(v) => v.parse::<u64>().ok()?,
        None => 1000,
    };
    let declared = names
        .sampling_interval_ms
        .is_some()
        .then_some(ms)
        .filter(|ms| *ms > 0)
        .map(|ms| ms as f64 / 1e3);
    Some((ms as f64 / 1e3, declared))
}

/// What [`table_probes`] gives: the names the indexes give, and the
/// segments to parse.
type TableProbes = (KnownNames, Vec<Vec<u8>>);

/// What a table's catalog says of its metric names, and the segments whose
/// footers name the rest:
///
/// - a sealed segment with a names index gives its names, and the first
///   sealed segment's gives the table's cadence, with no segment fetched;
/// - the first sealed segment without one is fetched, and each later one
///   whose names fingerprint no earlier fetch has. A later segment with
///   neither (a `.rez`, one sealed before fingerprints, or one dendro
///   merged or copied with a projection, which drops the index) is assumed
///   to hold the first segment's names, which is what the reader assumed of
///   every segment before;
/// - the rows of its live tail that name its columns
///   (`crate::wal::schema_rows`), materialized alone.
fn table_probes(
    db: &dyn Catalog,
    recording_id: i64,
    table: &str,
    sealed: &[(u64, crate::catalog::SegmentMeta)],
    long: bool,
) -> Result<TableProbes, Box<dyn std::error::Error>> {
    use crate::names::Stored;

    let mut known = KnownNames::default();
    let mut probes = Vec::new();
    if !sealed.is_empty() {
        let stored: HashMap<u64, Vec<u8>> = db
            .segment_indexes(recording_id, table)?
            .into_iter()
            .filter_map(|(seq, index)| Some((seq, index?)))
            .collect();
        let mut seen: HashSet<u64> = HashSet::new();
        // Names indexes already added: consecutive segments that hold the
        // same names store the same bytes, which are decoded once.
        let mut added: HashSet<&[u8]> = HashSet::new();
        for (i, (seq, _)) in sealed.iter().enumerate() {
            let raw = stored.get(seq).map(Vec::as_slice);
            if raw.is_some_and(|raw| added.contains(raw)) {
                continue;
            }
            let fetch = match crate::names::read(raw) {
                Some(Stored::Names(names)) => match indexed_intervals(&names) {
                    Some(intervals) => {
                        known.names.add(&names);
                        added.extend(raw);
                        if i == 0 {
                            known.first = Some(intervals);
                        }
                        false
                    }
                    None => true,
                },
                Some(Stored::Fingerprint(fp)) => seen.insert(fp) || i == 0,
                None => i == 0,
            };
            if fetch {
                // By number: this runs inside the open's snapshot, where a
                // listed number still names the listed segment.
                probes.extend(db.segment_bytes(recording_id, table, *seq)?);
            }
        }
    }
    let rows = db.live_wal(recording_id, table)?;
    let naming = crate::wal::schema_rows(table, long, &rows)?;
    let props = crate::segment_props(crate::default_compression());
    let tail = if long {
        crate::wal::materialize_long_wal_tail_with(table, &naming, false, props)?
    } else {
        crate::wal::materialize_wal_tail_with(table, &naming, props)?
    };
    probes.extend(tail.map(|t| t.bytes));
    Ok((known, probes))
}

/// What a probe yields: the table's key, its metric names, its row spacing,
/// the interval its first segment declares, and its span.
type ProbedTable = (String, TableNames, f64, Option<f64>, Option<(u64, u64)>);

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

/// Where a table's rows lie in time, from the catalog: each sealed
/// segment's `(seq, entry)`, and the live WAL's span.
struct RowIndex {
    segments: Vec<(u64, crate::catalog::SegmentMeta)>,
    /// Whether the segments' first and last rows both ascend, so a search
    /// can skip to the range.
    ordered: bool,
    wal: crate::catalog::Span,
}

impl RowIndex {
    fn new(segments: Vec<(u64, crate::catalog::SegmentMeta)>, wal: crate::catalog::Span) -> Self {
        let ordered = segments
            .windows(2)
            .all(|w| w[0].1.first_ts <= w[1].1.first_ts && w[0].1.last_ts <= w[1].1.last_ts);
        Self {
            segments,
            ordered,
            wal,
        }
    }
}

/// The row timestamps of one sealed segment, from its `timestamp` column
/// alone.
fn segment_timestamps(bytes: Vec<u8>) -> Result<Vec<u64>, Box<dyn std::error::Error>> {
    use arrow::array::{Array, UInt64Array};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::arrow::ProjectionMask;

    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes))?;
    let schema = builder.parquet_schema();
    let column = schema
        .root_schema()
        .get_fields()
        .iter()
        .position(|f| f.name() == "timestamp")
        .ok_or("the segment has no timestamp column")?;
    let mask = ProjectionMask::roots(schema, [column]);
    let mut out = Vec::new();
    for batch in builder.with_projection(mask).build()? {
        let batch = batch?;
        let ts = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or("the timestamp column is not UInt64")?;
        out.extend(ts.iter().flatten());
    }
    Ok(out)
}

/// One table's segments, fetched from the archive as a query needs them —
/// the `SegmentStore` the segmented reader pulls through.
///
/// Built when the table is first queried: the sealed segments' sequence
/// numbers from the catalog, and the live WAL tail materialized once as the
/// newest segment. The tail is the one thing held in memory — it is at most
/// a segment's worth of rows, and it is the part of the table SQLite cannot
/// hand back as parquet. Sealed segments are read on demand by their listed
/// catalog entry; one that retention removed, or whose number now names
/// another segment, reads as gone and is skipped.
struct DbSegmentStore {
    db: DbHandle,
    recording_id: i64,
    sampler: String,
    seqs: Vec<u64>,
    /// Each sealed segment's catalog entry, in `seqs` order: a fetch returns
    /// a segment's bytes only while its number still names this entry.
    metas: Vec<crate::catalog::SegmentMeta>,
    /// Each sealed segment's [`segment_key`], in `seqs` order.
    keys: Vec<u64>,
    tail: Option<bytes::Bytes>,
}

/// An id for a sealed segment: its sequence number with its row count and
/// time span. Compaction can give a merged segment the sequence number of
/// the first segment it replaced, and numbering restarts for a stream whose
/// segments were all evicted; the count and span tell those apart. dendro
/// changes a sealed segment's bytes only by compaction, which changes its
/// row count and span.
fn segment_key(seq: u64, meta: &crate::catalog::SegmentMeta) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    (seq, meta.rows, meta.first_ts, meta.last_ts).hash(&mut h);
    h.finish()
}

/// A table's unsealed WAL rows as one segment. A table with an occupant
/// stream is long, and its rows are `WalLongRow`s; they are kept in arrival
/// order, which the long reader handles as well as a sorted segment. Encoded
/// with [`default_compression`](crate::default_compression): the segment is
/// held in memory while the reader is open.
fn live_tail(
    table: &str,
    rows: &[crate::catalog::WalRow],
    long: bool,
) -> Result<Option<crate::wal::MaterializedTail>, Box<dyn std::error::Error>> {
    let props = crate::segment_props(crate::default_compression());
    if long {
        crate::wal::materialize_long_wal_tail_with(table, rows, false, props)
    } else {
        crate::wal::materialize_wal_tail_with(table, rows, props)
    }
}

/// `f`, with every read it makes through `db` inside one
/// [`read_snapshot`](Catalog::read_snapshot).
fn in_snapshot<T, E: From<String>>(
    db: &dyn Catalog,
    f: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let mut f = Some(f);
    let mut out = None;
    db.read_snapshot(&mut || {
        if let Some(f) = f.take() {
            out = Some(f());
        }
        Ok(())
    })?;
    out.unwrap_or_else(|| {
        Err(E::from(
            "the catalog's read_snapshot did not run".to_string(),
        ))
    })
}

impl DbSegmentStore {
    fn build(
        db: DbHandle,
        recording_id: i64,
        sampler: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // One snapshot: a seal committing between the segment list and the
        // WAL read would leave its rows in neither. The tail is encoded after
        // the snapshot ends, so a checkpoint of a live archive is held back
        // only while the reads run.
        let (listed, rows, long) = db
            .with(|db| {
                in_snapshot(db, || {
                    let listed = db.segment_meta(recording_id, sampler)?;
                    let long = db
                        .tables(recording_id)?
                        .contains(&crate::occupants::stream_of(sampler));
                    let rows = db.live_wal(recording_id, sampler)?;
                    Ok::<_, String>((listed, rows, long))
                })
            })
            .map_err(|e| e.to_string())?;
        let tail = live_tail(sampler, &rows, long)
            .map_err(|e| e.to_string())?
            .map(|t| bytes::Bytes::from(t.bytes));
        let keys = listed
            .iter()
            .map(|(seq, meta)| segment_key(*seq, meta))
            .collect();
        let (seqs, metas) = listed.into_iter().unzip();
        Ok(Self {
            db,
            recording_id,
            sampler: sampler.to_string(),
            seqs,
            metas,
            keys,
            tail,
        })
    }
}

impl crate::SegmentStore for DbSegmentStore {
    fn len(&self) -> usize {
        self.seqs.len() + usize::from(self.tail.is_some())
    }

    /// A sealed segment's [`segment_key`]; the live tail has none.
    fn key(&self, idx: usize) -> Option<u64> {
        self.keys.get(idx).copied()
    }

    fn bytes(&self, idx: usize) -> crate::SegmentBytes {
        match self.seqs.get(idx) {
            Some(seq) => {
                let (seq, meta) = (*seq, self.metas[idx]);
                // By the listed entry, not the number alone: a number reused
                // since the listing names a segment whose rows may already be
                // in this view's tail. That segment reads as gone.
                let bytes = self.db.with(|db| {
                    db.segment_bytes_matching(self.recording_id, &self.sampler, seq, &meta)
                })?;
                Ok(bytes.map(bytes::Bytes::from))
            }
            None if idx == self.seqs.len() => Ok(self.tail.clone()),
            None => Ok(None),
        }
    }
}

impl SegmentSource {
    /// The catalog id of the recording this table belongs to; `None` for
    /// a tar archive's in-memory table.
    fn source_id(&self) -> Option<i64> {
        match self {
            SegmentSource::Bytes(_) => None,
            SegmentSource::Db { recording_id, .. }
            | SegmentSource::SharedDb { recording_id, .. } => Some(*recording_id),
        }
    }

    /// This table as a store the segmented reader fetches from on demand.
    /// `None` when the table has no segments and no tail any more — evicted
    /// between the probe and the query.
    fn store(&self) -> Result<Option<Arc<dyn crate::SegmentStore>>, Box<dyn std::error::Error>> {
        let store: Arc<dyn crate::SegmentStore> = match self {
            SegmentSource::Bytes(b) => Arc::new(crate::InMemorySegments::new(b.clone())),
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

    /// The rows of an occupant stream: from each sealed segment, its first
    /// row for each occupant; from the live WAL, every row. Sealed segments
    /// come first, in sequence order. These are the labels of every occupant
    /// the table's rows can name.
    /// Empty for a tar archive, which has no long tables.
    ///
    /// `known` holds sealed segments of the stream already decoded, by
    /// [`segment_key`]; they are not read again. Returned with the rows are
    /// the decoded sealed segments the rows came from.
    fn occupants(
        &self,
        stream: &str,
        known: &OccupantSegments,
    ) -> Result<(Vec<Occupant>, OccupantSegments), Box<dyn std::error::Error>> {
        // One snapshot: a seal committing between the segment list and the
        // WAL read would leave the occupants it moved in neither. Occupant
        // segments not already known are decoded inside it.
        let read = |db: &dyn Catalog, recording_id: i64| -> Result<_, Box<dyn std::error::Error>> {
            in_snapshot(db, || {
                let mut out = Vec::new();
                let mut decoded = OccupantSegments::new();
                for (seq, meta) in db.segment_meta(recording_id, stream)? {
                    let key = segment_key(seq, &meta);
                    let rows = match known.get(&key) {
                        Some(rows) => Arc::clone(rows),
                        // Inside a snapshot a listed segment is still there.
                        // Under a catalog with no snapshot, one that retention
                        // took since the listing, or whose number now names
                        // another segment, is gone, and so are the rows that
                        // named its occupants.
                        None => {
                            match db.segment_bytes_matching(recording_id, stream, seq, &meta)? {
                                // `OccupantLabels::new` keeps the first labels it sees
                                // for an occupant, and segments are read in order, so
                                // a later row for the same occupant in this segment
                                // cannot change any series' labels. The relabel's key
                                // set can lose a key that appears only on such a row;
                                // no series carries that key, so a filter on it
                                // matches the same series either way.
                                Some(bytes) => Arc::new(first_per_occupant(
                                    crate::occupants::decode_segment(&bytes)?
                                        .into_iter()
                                        .map(|(_, o)| o),
                                )),
                                None => continue,
                            }
                        }
                    };
                    out.extend(rows.iter().cloned());
                    decoded.insert(key, rows);
                }
                for row in db.live_wal(recording_id, stream)? {
                    out.extend(crate::occupants::decode_wal_row(&row.row)?);
                }
                Ok((out, decoded))
            })
        };
        self.with_catalog(|db, recording_id, _| read(db, recording_id))
            .unwrap_or_else(|| Ok((Vec::new(), OccupantSegments::new())))
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
#[derive(Clone, Default)]
struct TableNames {
    counters: std::collections::HashSet<String>,
    gauges: std::collections::HashSet<String>,
    histograms: std::collections::HashSet<String>,
    gauge_histograms: std::collections::HashSet<String>,
}

impl TableNames {
    /// Add the names a segment's names index gives.
    fn add(&mut self, names: &crate::names::SegmentNames) {
        self.counters.extend(names.counters.iter().cloned());
        self.gauges.extend(names.gauges.iter().cloned());
        self.histograms.extend(names.histograms.iter().cloned());
        self.gauge_histograms
            .extend(names.gauge_histograms.iter().cloned());
    }

    /// Whether this table holds `metric` under any kind — the routing question.
    fn holds(&self, metric: &str) -> bool {
        self.counters.contains(metric)
            || self.gauges.contains(metric)
            || self.histograms.contains(metric)
            || self.gauge_histograms.contains(metric)
    }
}

impl TableReader {
    /// The table's source.
    pub fn source(&self) -> Arc<dyn DataSource> {
        match self {
            TableReader::Segmented(r) => Arc::clone(r) as Arc<dyn DataSource>,
        }
    }

    /// The table's segmented source.
    pub fn segmented(&self) -> &Arc<SegmentedSource> {
        match self {
            TableReader::Segmented(r) => r,
        }
    }

    /// The table as a child of a same-recording union.
    pub fn union_child(&self) -> UnionChild {
        UnionChild::from_source(self.source())
    }

    /// The same borrow as [`union_child`](Self::union_child), for the other
    /// composition: merging this table into a labelled multi-source rather
    /// than into a same-recording union.
    pub fn composition_source(&self) -> CompositionSource {
        CompositionSource::from_source(self.source())
    }
}

/// One table of a recording: its probed name catalog and span, and its
/// [`SegmentedSource`], opened on first use.
///
/// `sampler` is the table's own key — `<sampler>/<group>` for a V3
/// acquisition-group table, just `<sampler>` for a V2 (or V3 windowless)
/// table. `rez::table_sampler` recovers the manifest-level sampler name from
/// it; this field is never split eagerly because most tables (every V2
/// table, and any V3 sampler with only one group) don't need it to be.
///
/// `recording` is the index of the recording this table belongs to, in its
/// catalog (or in `from_in_memory`'s input). It is what `__recording__` carries
/// for twin recordings (equal labels other than internal ones). Which recording
/// a table belongs to, for grouping a query's tables, is decided by
/// `recording_labels` instead: the index repeats across readers joined by
/// [`ArchiveReader::flatten`]. An index rather than the recording's `dir`
/// string: `dir` is a display name derived from labels (`recording_dir_slug`),
/// not a guaranteed-unique identity — two recordings with the same labels are
/// entirely legal and would collide on `dir`.
pub struct SamplerReader {
    recording: usize,
    /// The labels of the recording this table belongs to, one allocation per
    /// recording, so tables of one recording share it: the pointer identifies
    /// the recording, also across readers [`ArchiveReader::flatten`] joined,
    /// whose `recording` numbers can repeat.
    recording_labels: Arc<BTreeMap<String, String>>,
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
    /// number stands for. See `crate::occupants`.
    occupants: Option<String>,
    /// What reads an indexed table's caller rows into a relabelling. The
    /// archive's own long tables need none (their occupant stream is read
    /// here); a container with an older identity index supplies one.
    index: Option<Arc<dyn IndexRelabel>>,
    pool: Arc<BufferPool>,
    /// Built on first access, never at open.
    reader: std::sync::OnceLock<Option<TableReader>>,
    /// The state saved by the same table in a reader this one replaced (see
    /// [`ArchiveReader::reuse_from`]), taken when this table's reader is
    /// built.
    previous: std::sync::Mutex<Option<Carried>>,
    /// Whether this table's reader saves state for a later reader (see
    /// [`ArchiveReader::keep_handover`]).
    keep: std::sync::atomic::AtomicBool,
    /// The occupant stream's sealed segments this table's reader was built
    /// from, decoded, for a reader that replaces this one.
    occupant_segments: std::sync::Mutex<OccupantSegments>,
    /// The table's sealed segments' catalog entries and its live WAL's span,
    /// as the catalog listed them at open, for
    /// [`has_row_in`](SamplerReader::has_row_in). `None` for a table opened
    /// from bytes.
    row_index: Option<RowIndex>,
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
    /// The index of the recording this table belongs to.
    pub fn recording(&self) -> usize {
        self.recording
    }

    /// Whether this table holds metric `name`, of any kind.
    pub fn holds(&self, name: &str) -> bool {
        self.names.holds(name)
    }

    /// Whether this table holds histogram `name`.
    pub fn holds_histogram(&self, name: &str) -> bool {
        self.names.histograms.contains(name)
    }

    /// Whether this table holds gauge histogram `name`.
    pub fn holds_gauge_histogram(&self, name: &str) -> bool {
        self.names.gauge_histograms.contains(name)
    }

    /// The table's row spacing in seconds. Opened from a catalog: the table's
    /// span over its row count. Opened from bytes: the interval its first
    /// segment declares, else the median gap of its first rows.
    pub fn interval(&self) -> f64 {
        self.interval
    }

    /// Whether this table holds counter `name`.
    pub fn holds_counter(&self, name: &str) -> bool {
        self.names.counters.contains(name)
    }

    /// Whether this table holds gauge `name`.
    pub fn holds_gauge(&self, name: &str) -> bool {
        self.names.gauges.contains(name)
    }

    /// The labels of the recording this table belongs to, as the catalog
    /// holds them.
    pub fn recording_labels(&self) -> &BTreeMap<String, String> {
        &self.recording_labels
    }

    /// Whether `other` is a table of the same recording.
    pub fn same_recording(&self, other: &SamplerReader) -> bool {
        Arc::ptr_eq(&self.recording_labels, &other.recording_labels)
    }

    /// The table's key: `<sampler>/<group>` or `<sampler>`.
    pub fn sampler(&self) -> &str {
        &self.sampler
    }

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

    pub fn reader(&self) -> Option<&TableReader> {
        self.reader
            .get_or_init(|| {
                // Taken before anything can fail, so a table whose build
                // fails holds no predecessor.
                let previous = self
                    .previous
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
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
                let relabel = match self.relabel(previous.as_ref()) {
                    Some(Ok(relabel)) => Some(relabel),
                    Some(Err(())) => return None,
                    None => None,
                };
                let opened = match self.keep.load(std::sync::atomic::Ordering::Relaxed) {
                    true => SegmentedSource::open_after(
                        store,
                        pool,
                        relabel,
                        previous.as_ref().and_then(|p| p.handover.as_ref()),
                    ),
                    false => match relabel {
                        Some(relabel) => {
                            SegmentedSource::open_relabeled_with_pool(store, pool, relabel)
                        }
                        None => SegmentedSource::open_with_pool(store, pool),
                    },
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

    /// The relabelling a table opens with: a long table's occupant stream, or
    /// an indexed table's identity index replayed into occupancy spans by the
    /// reader's [`IndexRelabel`]. `None` for a table with neither, and for an
    /// indexed table when the reader has no hook (logged); `Some(Err(()))`
    /// when the stream or index could not be read or
    /// replayed, which is a failure to open the table, reported the same way
    /// a segment that would not parse is. The one fall-through to the plain
    /// path is an indexed table opened without a hook, which is logged and
    /// reads with the labels its segments carry.
    ///
    /// The occupant-stream segments `previous` decoded are not decoded again.
    fn relabel(
        &self,
        previous: Option<&Carried>,
    ) -> Option<Result<Arc<dyn crate::ColumnRelabel>, ()>> {
        if let Some(stream) = &self.occupants {
            let empty = OccupantSegments::new();
            let known = previous.map_or(&empty, |p| &p.occupant_segments);
            return Some(match self.segments.occupants(stream, known) {
                Ok((rows, decoded)) => {
                    if self.keep.load(std::sync::atomic::Ordering::Relaxed) {
                        *self
                            .occupant_segments
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = decoded;
                    }
                    Ok(Arc::new(crate::long::OccupantLabels::new(rows)))
                }
                Err(e) => {
                    tracing::warn!("reading the occupant stream {stream}: {e}");
                    Err(())
                }
            });
        }
        if !self.indexed {
            return None;
        }
        let Some(hook) = self.index.as_ref() else {
            // A table a 5.x `record --stream` `.rez` indexed, opened without
            // a hook: its slot columns keep the labels each segment carries.
            tracing::warn!(
                "table {} has an identity index this reader does not read; its slot columns \
                 carry the labels each segment was built with",
                self.sampler
            );
            return None;
        };
        let first_row_ts = self.span.map(|(b, _)| b).unwrap_or(0);
        let relabel = self.segments.with_catalog(|db, source_id, table| {
            hook.relabel(db, source_id, table, first_row_ts)
                .map_err(|e| e.into())
        })?;
        Some(relabel.map_err(|e| {
            tracing::warn!("reading the caller-row index for {}: {e}", self.sampler);
        }))
    }

    pub fn row_timestamps(&self) -> &[u64] {
        self.row_timestamps.get_or_init(|| {
            self.reader()
                .map(|r| r.source().sample_timestamps())
                .unwrap_or_default()
        })
    }

    /// Whether this table has a row with a timestamp in `[lo, hi]`: `None`
    /// when it cannot tell.
    ///
    /// Answered from the catalog entries listed at open: a segment (or the
    /// live WAL) with its first or last row inside the range has a row in
    /// it. Only a segment that starts before the range and ends after it is
    /// read, and only its timestamp column; the WAL's rows likewise. A read
    /// that fails, a segment gone since open, and a WAL whose rows were
    /// sealed or evicted since open give `None`. A table opened from bytes
    /// reads its row timestamps.
    pub fn has_row_in(&self, lo: u64, hi: u64) -> Option<bool> {
        if lo > hi {
            return Some(false);
        }
        let inside = |t: u64| (lo..=hi).contains(&t);
        let Some(index) = &self.row_index else {
            self.reader()?;
            let ts = self.row_timestamps();
            let at = ts.partition_point(|&t| t < lo);
            return Some(ts.get(at).is_some_and(|&t| t <= hi));
        };
        // Segments are in time order: skip those ending before the range.
        let from = match index.ordered {
            true => index.segments.partition_point(|(_, m)| m.last_ts < lo),
            false => 0,
        };
        let mut straddling = Vec::new();
        for (seq, meta) in &index.segments[from..] {
            if meta.first_ts > hi {
                if index.ordered {
                    break;
                }
                continue;
            }
            if meta.last_ts < lo {
                continue;
            }
            if inside(meta.first_ts) || inside(meta.last_ts) {
                return Some(true);
            }
            straddling.push((*seq, *meta));
        }
        let mut wal_straddles = false;
        if let (Some(first), Some(last)) = (index.wal.first_ts, index.wal.last_ts) {
            if last >= lo && first <= hi {
                if inside(first) || inside(last) {
                    return Some(true);
                }
                wal_straddles = true;
            }
        }
        if straddling.is_empty() && !wal_straddles {
            return Some(false);
        }
        let read = self.segments.with_catalog(|db, source_id, table| {
            for (seq, meta) in &straddling {
                let Some(bytes) = db.segment_bytes_matching(source_id, table, *seq, meta)? else {
                    return Ok(None);
                };
                if segment_timestamps(bytes)?.into_iter().any(inside) {
                    return Ok(Some(true));
                }
            }
            if wal_straddles {
                let rows = db.live_wal(source_id, table)?;
                if rows.iter().any(|r| inside(r.ts)) {
                    return Ok(Some(true));
                }
                // Rows sealed or evicted since open are no longer in the
                // WAL, so their absence says nothing.
                if rows.first().map(|r| r.ts) != index.wal.first_ts {
                    return Ok(None);
                }
            }
            Ok(Some(false))
        });
        match read {
            Some(Ok(found)) => found,
            Some(Err(e)) => {
                tracing::warn!(
                    "reading the row timestamps of {} between {lo} and {hi}: {e}",
                    self.sampler
                );
                None
            }
            None => None,
        }
    }
}

/// An archive's tables: one recording's, or several recordings' flattened
/// into `tables`.
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
    /// Every recording behind this reader, tables or not: its shared label
    /// map (the identity tables carry) and its catalog index. What
    /// [`series_labels`](Self::series_labels) finds twins among, so a
    /// recording's `__recording__` does not come and go as a twin gains or
    /// loses tables.
    members: Vec<(Arc<BTreeMap<String, String>>, usize)>,
    /// What [`process_groups`](Self::process_groups) groups: one entry per
    /// member, in `members` order.
    sources: Vec<SourceInfo>,
    /// [`series_labels`](Self::series_labels), worked out on first use:
    /// `members` is fixed once the reader is built.
    series_labels: std::sync::OnceLock<SeriesLabels>,
    /// [`process_groups`](Self::process_groups), worked out on first use.
    process_groups: std::sync::OnceLock<ProcessGroups>,
    /// [`read_groups`](Self::read_groups), and each member's read group,
    /// worked out on first use.
    read_groups: std::sync::OnceLock<ReadGroupIndex>,
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
    /// identical label sets** — two endpoints given the same labels do
    /// exactly that, and the recorder only warns. A caller that
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
    /// `open_with_pool` produces. Every recording holds
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
    /// Open with `open_recordings` instead: one
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
                let mut catalog = crate::CompositionCatalog::new(t.interval)
                    .counters(t.names.counters.iter().cloned())
                    .gauges(t.names.gauges.iter().cloned())
                    .histograms(t.names.histograms.iter().cloned())
                    .gauge_histograms(t.names.gauge_histograms.iter().cloned())
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
    /// Every metric's column metadata: `unit`, `description`, `metric_type`,
    /// and the metric's label keys, exactly as the recorder wrote them into the
    /// parquet schema.
    ///
    /// `metriken_query::MetricsSource` answers which metrics exist and what labels they
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

    /// Makes each table built after this call save the state a later
    /// reader of the same archive can start from (see
    /// [`reuse_from`](Self::reuse_from)). A table whose reader is already
    /// built saves none. A caller that reopens the archive calls this on
    /// its first reader, before querying it.
    pub fn keep_handover(&self) {
        for table in &self.tables {
            table.keep.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Lets each table of this reader start from the state `previous` saved
    /// for the same table, for a reader opened over the same archive file
    /// after `previous` was: a live archive reopened to see new rows. It
    /// also calls [`keep_handover`](Self::keep_handover) on this reader.
    /// Call it before querying this reader.
    ///
    /// A table's reader is still built on first query. When the matching
    /// table of `previous` saved state (see [`keep_handover`](Self::keep_handover)
    /// and [`SegmentedSource::handover`]), the new one reads only the
    /// segments sealed since, the live tail, and the occupant-stream
    /// segments not yet decoded, and takes over the segments `previous` had
    /// open when both readers use the same pool. A table whose relabel's
    /// identities are not fixed reuses nothing. Once retention evicts a segment
    /// `previous` read, only the decoded occupant-stream segments are
    /// reused. Tables are matched by recording id and name. A table already
    /// built here is left as it is, and so is a table of a tar archive,
    /// which has no keyed segments.
    ///
    /// `previous` must read the same archive file. A segment is matched by
    /// its sequence number, row count and time span, and a filtered copy of
    /// an archive keeps those for every segment, so a reader of such a copy
    /// gives wrong answers without an error.
    ///
    /// Until its reader is built, a table of this reader holds the state
    /// `previous` saved for it, the segments `previous` had open among
    /// those it reuses, and the decoded occupant-stream segments. A table
    /// of `previous` that was never queried passes on the state and the
    /// occupant-stream segments it was given, without the open segments.
    /// None of it holds a connection to the archive.
    pub fn reuse_from(&self, previous: &ArchiveReader) {
        self.keep_handover();
        for table in &self.tables {
            // A built table ignores `previous`, and a tar table has no keyed
            // segments.
            if table.reader.get().is_some() || table.segments.source_id().is_none() {
                continue;
            }
            let Some(old) = previous.tables.iter().find(|t| {
                t.segments.source_id() == table.segments.source_id() && t.sampler == table.sampler
            }) else {
                continue;
            };
            if Arc::ptr_eq(old, table) {
                continue;
            }
            // A built table gives its saved state. A table that was not
            // queried, or whose build failed, gives the state it was given,
            // without the open segments, which nothing will read.
            let carried = match old.reader.get() {
                Some(Some(TableReader::Segmented(r))) => Some(Carried {
                    handover: r.handover(),
                    occupant_segments: old
                        .occupant_segments
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone(),
                }),
                _ => old
                    .previous
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|c| Carried {
                        handover: c.handover.as_ref().map(|h| h.without_segments()),
                        occupant_segments: c.occupant_segments.clone(),
                    }),
            };
            *table.previous.lock().unwrap_or_else(|e| e.into_inner()) = carried;
        }
    }

    /// Several readers' tables as one reader: the single-source view of a
    /// multi-source archive. Its metadata is the first reader's, and it is
    /// complete when every reader is.
    ///
    /// Flattening several recordings gives a table name several owners,
    /// one per recording: a query naming it is answered from every
    /// recording, each recording's series under its labels
    /// ([`series_labels`](Self::series_labels)).
    /// [`composition_sources`](Self::composition_sources) refuses a
    /// flattened reader.
    pub fn flatten(readers: Vec<ArchiveReader>, filename: Option<String>) -> Self {
        let mut tables = Vec::new();
        let mut metadata = BTreeMap::new();
        let mut complete = true;
        let mut recordings = 0usize;
        let mut members = Vec::new();
        let mut sources = Vec::new();
        for (i, reader) in readers.into_iter().enumerate() {
            recordings += reader.recordings;
            members.extend(reader.members);
            sources.extend(reader.sources);
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
            members,
            sources,
            series_labels: std::sync::OnceLock::new(),
            process_groups: std::sync::OnceLock::new(),
            read_groups: std::sync::OnceLock::new(),
        }
    }

    /// Open the dendro archive at `path` as one reader per read group:
    /// [`from_catalog`](Self::from_catalog), then
    /// [`flatten_read_groups`](Self::flatten_read_groups). Tables are read
    /// from `path` again on their first query, and refused once another file
    /// is at `path`.
    ///
    /// Only a dendro archive opens. A standalone parquet file, a SQLite file
    /// without dendro's stamp (a v3 `.rez`) and a tar (a v1 or v2
    /// `.rez`) are refused by their first bytes, with a message saying to
    /// convert them (`to_dendro::convert_path`, feature `write`); they are
    /// never converted here.
    ///
    /// The identity index of a converted 5.x indexed recording (its caller
    /// rows) is not read (`index` is `None`), as in `rez::open`: its slot
    /// columns keep the labels each segment was built with.
    pub fn open_read_groups(
        path: &std::path::Path,
        pool: Arc<BufferPool>,
    ) -> Result<LabeledRecordings, Box<dyn std::error::Error>> {
        refuse_legacy(&file_head(path)?, &path.display().to_string())?;
        // Noted before the catalog opens, so a file swapped in between is
        // refused on a table's first read.
        let reopen: Reopen = {
            let path = path.to_path_buf();
            let opened = crate::file_id::FileId::of(&path);
            Arc::new(move || {
                if opened.is_some() && crate::file_id::FileId::of(&path) != opened {
                    return Err(format!(
                        "{} was replaced after it was opened",
                        path.display()
                    ));
                }
                Ok(Box::new(crate::DendroCatalog::open(&path)?) as Box<dyn Catalog>)
            })
        };
        let catalog = crate::DendroCatalog::open(path)?;
        Ok(Self::flatten_read_groups(Self::from_catalog(
            Box::new(catalog),
            Some(reopen),
            pool,
            None,
        )?))
    }

    /// [`open_read_groups`](Self::open_read_groups) for an archive held as
    /// bytes (a browser upload). Every table reads from one in-memory
    /// catalog.
    pub fn open_read_groups_from_bytes(
        bytes: Vec<u8>,
        pool: Arc<BufferPool>,
    ) -> Result<LabeledRecordings, Box<dyn std::error::Error>> {
        refuse_legacy(&bytes, "the recording")?;
        let catalog = crate::DendroCatalog::open_bytes(bytes)?;
        Ok(Self::flatten_read_groups(Self::from_catalog(
            Box::new(catalog),
            None,
            pool,
            None,
        )?))
    }

    /// `recordings`, one reader per source, joined into one reader per read
    /// group: what a caller lists, selects and edits as one recording.
    ///
    /// The groups are the ones [`read_groups`](Self::read_groups) finds over
    /// the same sources joined by [`flatten`](Self::flatten) (a source and
    /// its copies, and sources of one producer epoch whose spans in it hold
    /// a row of each, are one; see [`crate::process`]), so a group read here
    /// answers as it does inside a reader of every source. Each entry is the
    /// group's labels ([`ReadGroup::labels`]) and a reader of its sources:
    ///
    /// - [`sources`](Self::sources) are the group's sources in archive order,
    ///   each with its `catalog_index` and `uuid`, which is how a writer
    ///   maps an edit of the group back to the sources it is written to.
    /// - [`metadata`](Self::metadata) is the group's, by
    ///   [`crate::process::merge_metadata`] (for a group of one source, that
    ///   source's metadata). The filename is the first source's.
    /// - Its series carry the group's labels, as they do in a reader of every
    ///   source, including a value the group takes from a copy outside it (a
    ///   source lacking a key takes its uuid's value). Its
    ///   [`read_groups`](Self::read_groups) is that one group.
    ///   Joining these readers again with [`flatten`](Self::flatten) groups
    ///   their sources alone: a value taken from a copy outside them is
    ///   lost, and groups can merge.
    ///
    /// Entries are in order of each group's first source, the order a
    /// listing of the archive's recordings shows, so a position in the
    /// returned list is a recording's position in such a listing within one
    /// open. On a live archive a later overlap can join groups, so positions
    /// can change between opens.
    ///
    /// A reader of one source is grouped. A reader of several sources (one
    /// made by [`flatten`](Self::flatten)) or of none is returned as it is,
    /// with its labels, at its own position in `recordings`:
    /// it cannot be split, so it is taken as one recording already.
    pub fn flatten_read_groups(recordings: LabeledRecordings) -> LabeledRecordings {
        // The readers of one source, by position in `recordings`.
        let single: Vec<usize> = recordings
            .iter()
            .enumerate()
            .filter(|(_, (_, r))| r.sources.len() == 1)
            .map(|(i, _)| i)
            .collect();
        let groups = {
            let infos: Vec<SourceInfo> = single
                .iter()
                .map(|&i| recordings[i].1.sources[0].clone())
                .collect();
            // As `read_group_index`: one source has nothing to group, and is
            // not made to read any rows to find that out.
            if infos.len() < 2 {
                crate::process::read_groups(&infos, &ProcessGroups::default())
            } else {
                let tables: Vec<Vec<&SamplerReader>> = single
                    .iter()
                    .map(|&i| recordings[i].1.tables.iter().map(|t| &**t).collect())
                    .collect();
                let formed = crate::process::form(&infos, &mut |source, lo, hi| {
                    any_row_in(&tables[source], lo, hi)
                });
                crate::process::read_groups(&infos, &formed)
            }
        };
        type Entry = (BTreeMap<String, String>, ArchiveReader);
        let mut slots: Vec<Option<Entry>> = recordings.into_iter().map(Some).collect();
        // Each entry with the position in `recordings` it is ordered by.
        let mut out: Vec<(usize, Entry)> = groups
            .into_iter()
            .map(|group| {
                let mut members: Vec<ArchiveReader> = group
                    .sources
                    .iter()
                    .map(|&s| {
                        slots[single[s]]
                            .take()
                            .expect("a source is in one read group")
                            .1
                    })
                    .collect();
                let first = single[group.sources[0]];
                if members.len() == 1 {
                    let mut reader = members.pop().expect("one member");
                    reader.set_read_group(group.labels.clone());
                    return (first, (group.labels, reader));
                }
                let metadata = crate::process::merge_metadata(
                    &members.iter().map(|m| &m.metadata).collect::<Vec<_>>(),
                );
                let filename = members[0].filename();
                let mut reader = ArchiveReader::flatten(members, filename);
                reader.metadata = metadata;
                reader.set_read_group(group.labels.clone());
                (first, (group.labels, reader))
            })
            .collect();
        out.extend(
            slots
                .into_iter()
                .enumerate()
                .filter_map(|(i, slot)| slot.map(|entry| (i, entry))),
        );
        out.sort_by_key(|(i, _)| *i);
        out.into_iter().map(|(_, entry)| entry).collect()
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

        for (recording, (rec, uuid)) in db.sources_with_uuids()?.into_iter().enumerate() {
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
            //
            // Phase one reads the catalog in one snapshot. A first seal that
            // committed between a table's segment list (empty) and its WAL
            // read (emptied by the seal) would leave the table with no probe,
            // and the reader would not list it for as long as it is open.
            let (pending, measured_intervals, indexed, occupant_streams, mut row_indexes) =
                in_snapshot(&**db, || {
                    let mut pending: Vec<PendingProbe> = Vec::new();
                    let mut measured_intervals: BTreeMap<String, Option<f64>> = BTreeMap::new();
                    let mut row_indexes: HashMap<String, RowIndex> = HashMap::new();
                    // The streams the identity index describes. One catalog query per
                    // recording; a table named here is read through the index.
                    let indexed: HashSet<String> =
                        db.caller_row_streams(rec.id)?.into_iter().collect();

                    // A long table's occupant stream is its labels, not a table of
                    // its own: set aside, and named on the table it belongs to.
                    let (occupant_streams, samplers): (Vec<String>, Vec<String>) = db
                        .tables(rec.id)?
                        .into_iter()
                        .partition(|s| crate::occupants::table_of(s).is_some());
                    let occupant_streams: HashSet<String> = occupant_streams.into_iter().collect();
                    for sampler in samplers {
                        let metas = db.segment_meta(rec.id, &sampler)?;

                        // The metric names the table holds: from sealed segments'
                        // names indexes, else their footers by names fingerprint,
                        // and the live tail's rows that carry a schema. A quiet
                        // sampler in a live hindsight buffer has only the latter:
                        // rows in the WAL, no seal yet. Skipping it would make it
                        // invisible to the reader, which the eager path never did
                        // because `table_segments` splices the tail in.
                        let (known, probes) = table_probes(
                            &**db,
                            rec.id,
                            &sampler,
                            &metas,
                            occupant_streams.contains(&crate::occupants::stream_of(&sampler)),
                        )?;
                        // Nothing sealed and nothing live: the table has no rows at
                        // all, so there is nothing to open. Same skip as the eager path.
                        if probes.is_empty() && known.first.is_none() {
                            continue;
                        }

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
                        // The table's cadence, measured: its span over its gaps. It
                        // is used when the first segment's footer declares no
                        // `sampling_interval_ms` (a declared one wins; see the
                        // probe below): the writer's segments and a `.rez`'s
                        // declare none, which had every archive reporting the 1 s
                        // default whatever it held. The catalog knows the span and
                        // the row count already, so this costs no decode. It is the
                        // staleness hint and the step the viewer opens at, not
                        // something the data is rounded to.
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
                        row_indexes.insert(sampler.clone(), RowIndex::new(metas, wal));

                        pending.push(PendingProbe {
                            sampler,
                            known,
                            probes,
                            span,
                        });
                    }
                    Ok::<_, Box<dyn std::error::Error>>((
                        pending,
                        measured_intervals,
                        indexed,
                        occupant_streams,
                        row_indexes,
                    ))
                })?;

            // Phase two: parse the footers. One probe per table, independent.
            let probed: Vec<Result<ProbedTable, String>> = probe_tables(&pending);

            let recording_labels = Arc::new(rec.labels.clone());
            let mut tables = Vec::new();
            for probe in probed {
                let (sampler, names, probed_interval, declared, span) = probe?;
                // A declared interval wins over the measured one, as it
                // does when the same segments are read from bytes
                // (`from_in_memory`): a recording with dropped or jittered
                // rows measures off its nominal interval, and gauge
                // staleness and an instant `irate` read at it.
                let interval = declared.unwrap_or_else(|| {
                    measured_intervals
                        .get(&sampler)
                        .copied()
                        .flatten()
                        .filter(|i| *i > 0.0)
                        .unwrap_or(probed_interval)
                });
                tables.push(Arc::new(SamplerReader {
                    recording,
                    recording_labels: Arc::clone(&recording_labels),
                    sampler: sampler.clone(),
                    names,
                    span,
                    interval,
                    indexed: indexed.contains(&sampler),
                    row_index: row_indexes.remove(&sampler),
                    occupants: occupant_streams
                        .contains(&crate::occupants::stream_of(&sampler))
                        .then(|| crate::occupants::stream_of(&sampler)),
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
                    previous: std::sync::Mutex::new(None),
                    occupant_segments: std::sync::Mutex::new(OccupantSegments::new()),
                    keep: std::sync::atomic::AtomicBool::new(false),
                    row_timestamps: std::sync::OnceLock::new(),
                }));
            }

            let info = SourceInfo::new(
                rec.labels.clone(),
                recording,
                uuid,
                tables_span(&tables),
                &rec.metadata,
            );
            out.push((
                rec.labels.clone(),
                Self {
                    tables,
                    metadata: rec.metadata,
                    filename: Some(crate::source_name(&rec.labels)),
                    complete: rec.complete,
                    recordings: 1,
                    members: vec![(recording_labels, recording)],
                    sources: vec![info],
                    series_labels: std::sync::OnceLock::new(),
                    process_groups: std::sync::OnceLock::new(),
                    read_groups: std::sync::OnceLock::new(),
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
        let mut members = Vec::new();
        let mut complete = true;
        let mut sources = Vec::new();
        for (recording, rec) in recordings.into_iter().enumerate() {
            complete &= rec.complete;
            let recording_labels = Arc::new(rec.labels.clone());
            members.push((Arc::clone(&recording_labels), recording));
            let first_table = tables.len();
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
                // Probe ONE segment for the name catalog. A
                // `MultiParquetSource` opened from bytes is footer-only and
                // skips the four per-segment identity indexes
                // `SegmentedSource` builds, which is the bulk of what open
                // costs.
                let probe = segments
                    .first()
                    .ok_or_else(|| format!("table {sampler} has no segments"))?;
                let probe =
                    MultiParquetSource::open_bytes_with_pool(probe.clone(), Arc::clone(&pool))
                        .map_err(|e| format!("probing table {sampler}: {e}"))?;
                let names = TableNames {
                    counters: probe.counter_names().into_iter().collect(),
                    gauges: probe.gauge_names().into_iter().collect(),
                    histograms: probe.histogram_names().into_iter().collect(),
                    gauge_histograms: probe.gauge_histogram_names().into_iter().collect(),
                };
                let first_span = probe.time_range();
                // The interval the segment declares, or else the median gap
                // of the first segment's rows, rounded to whole milliseconds
                // as the catalog path rounds its measurement. A source
                // reports 1 s for a segment that declares none.
                let interval = match probe.metadata_get("sampling_interval_ms") {
                    Some(_) => probe.interval(),
                    None => {
                        // From the first segments holding two rows between
                        // them: a short first segment (a seal right after
                        // a start) has no gap of its own.
                        let mut timestamps = probe.sample_timestamps();
                        for more in segments.iter().skip(1) {
                            if timestamps.len() >= 2 {
                                break;
                            }
                            if let Ok(next) = MultiParquetSource::open_bytes_with_pool(
                                more.clone(),
                                Arc::clone(&pool),
                            ) {
                                timestamps.extend(next.sample_timestamps());
                            }
                        }
                        let mut gaps: Vec<u64> = timestamps
                            .windows(2)
                            .map(|w| w[1].saturating_sub(w[0]))
                            .collect();
                        gaps.sort_unstable();
                        match gaps.get(gaps.len() / 2) {
                            Some(&gap) if gap > 0 => (gap as f64 / 1e6).round().max(1.0) / 1e3,
                            _ => probe.interval(),
                        }
                    }
                };
                drop(probe);

                // Segments are in segment order, so the last one carries the
                // table's end. Skipped when there is only one — it is the probe.
                let last_span = match segments.len() {
                    0 | 1 => first_span,
                    _ => {
                        let last = MultiParquetSource::open_bytes_with_pool(
                            segments[segments.len() - 1].clone(),
                            Arc::clone(&pool),
                        )
                        .map_err(|e| format!("probing table {sampler} tail: {e}"))?;
                        let s = last.time_range();
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
                    recording_labels: Arc::clone(&recording_labels),
                    sampler,
                    names,
                    span,
                    interval,
                    indexed: false,
                    row_index: None,
                    occupants: None,
                    index: None,
                    segments: SegmentSource::Bytes(segments),
                    pool: Arc::clone(&pool),
                    reader: std::sync::OnceLock::new(),
                    previous: std::sync::Mutex::new(None),
                    occupant_segments: std::sync::Mutex::new(OccupantSegments::new()),
                    keep: std::sync::atomic::AtomicBool::new(false),
                    row_timestamps: std::sync::OnceLock::new(),
                }));
            }
            sources.push(SourceInfo::new(
                rec.labels.clone(),
                recording,
                None,
                tables_span(&tables[first_table..]),
                &rec.metadata,
            ));
        }
        Ok(Self {
            tables,
            metadata,
            filename,
            complete,
            recordings: recording_count,
            members,
            sources,
            series_labels: std::sync::OnceLock::new(),
            process_groups: std::sync::OnceLock::new(),
            read_groups: std::sync::OnceLock::new(),
        })
    }
}

impl ArchiveReader {
    /// The tables holding any of `metrics`, under any kind. Label matchers
    /// are not consulted: a table holding the metric with no matching
    /// series answers with an empty result.
    pub fn owners_of(&self, metrics: &[String]) -> Vec<&SamplerReader> {
        self.tables
            .iter()
            .map(|t| &**t)
            .filter(|t| metrics.iter().any(|m| t.names.holds(m)))
            .collect()
    }

    /// Counter names across every table, sorted and deduplicated.
    pub fn counter_names(&self) -> Vec<String> {
        union_sorted(
            self.tables
                .iter()
                .map(|t| t.names.counters.iter().cloned().collect()),
        )
    }

    /// Gauge names across every table, sorted and deduplicated.
    pub fn gauge_names(&self) -> Vec<String> {
        union_sorted(
            self.tables
                .iter()
                .map(|t| t.names.gauges.iter().cloned().collect()),
        )
    }

    /// Histogram names across every table, sorted and deduplicated.
    pub fn histogram_names(&self) -> Vec<String> {
        union_sorted(
            self.tables
                .iter()
                .map(|t| t.names.histograms.iter().cloned().collect()),
        )
    }

    /// Gauge histogram names across every table, sorted and deduplicated.
    pub fn gauge_histogram_names(&self) -> Vec<String> {
        union_sorted(
            self.tables
                .iter()
                .map(|t| t.names.gauge_histograms.iter().cloned().collect()),
        )
    }

    /// The label sets of counter `name`, from every table holding it, each
    /// with its recording's labels ([`series_labels`](Self::series_labels)).
    pub fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.labeled_listing(
            |t| t.names.counters.contains(name),
            |r| r.source().counter_labels(name),
        )
    }

    /// The label sets of gauge `name`, as [`counter_labels`](Self::counter_labels).
    pub fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.labeled_listing(
            |t| t.names.gauges.contains(name),
            |r| r.source().gauge_labels(name),
        )
    }

    /// The label sets of histogram `name`, as [`counter_labels`](Self::counter_labels).
    pub fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.labeled_listing(
            |t| t.names.histograms.contains(name),
            |r| r.source().histogram_labels(name),
        )
    }

    /// The label sets of gauge histogram `name`, as
    /// [`counter_labels`](Self::counter_labels).
    pub fn gauge_histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.labeled_listing(
            |t| t.names.gauge_histograms.contains(name),
            |r| r.source().gauge_histogram_labels(name),
        )
    }

    fn labeled_listing(
        &self,
        holds: impl Fn(&SamplerReader) -> bool,
        list: impl Fn(&TableReader) -> Vec<BTreeMap<String, String>>,
    ) -> Vec<BTreeMap<String, String>> {
        let extras = self.series_labels();
        let index = self.read_group_index();
        // A label set one table of a read group lists, by group and table
        // key: the sources of one process group list each series once.
        let mut seen: HashSet<(usize, &str, BTreeMap<String, String>)> = HashSet::new();
        let mut out = Vec::new();
        for t in self.tables.iter().filter(|t| holds(t)) {
            let Some(r) = t.reader() else {
                continue;
            };
            let extra = extras.of(t);
            let group = self
                .source_position(t)
                .filter(|&p| index.groups[index.of[p]].sources.len() > 1)
                .map(|p| index.of[p]);
            for mut set in list(r) {
                crate::labeled::add_labels_map(&mut set, extra);
                if let Some(g) = group {
                    if !seen.insert((g, t.sampler.as_str(), set.clone())) {
                        continue;
                    }
                }
                out.push(set);
            }
        }
        out
    }

    /// How many recordings this reader holds, tables or not.
    pub fn recording_count(&self) -> usize {
        self.members.len()
    }

    /// Every source behind this reader, tables or not: its labels, catalog
    /// index, uuid, span and producer epochs. In catalog order, and for
    /// readers joined by [`flatten`](Self::flatten) in the order they were
    /// joined. [`process_groups`](Self::process_groups) names sources by
    /// their position here.
    pub fn sources(&self) -> &[SourceInfo] {
        &self.sources
    }

    /// The sources grouped by producer process: copies (one uuid) taken as
    /// one, bridges set aside, and the rest joined where they share a
    /// producer epoch and overlap in it by a row of each (see
    /// [`crate::process`]). Worked out on first use and kept. Which rows a
    /// source holds is read from the catalog entries listed at open, and a
    /// segment's timestamp column when that segment straddles a whole
    /// intersection ([`SamplerReader::has_row_in`]); a source holds a row
    /// when any of its tables does, and cannot tell when none does and one
    /// cannot tell.
    ///
    /// Queries read each process group as one recording, through
    /// [`read_groups`](Self::read_groups).
    pub fn process_groups(&self) -> &ProcessGroups {
        self.process_groups.get_or_init(|| {
            // Each source's tables, by the address of its shared label map.
            let mut tables: Vec<Vec<&SamplerReader>> = vec![Vec::new(); self.members.len()];
            let position: HashMap<usize, usize> = self
                .members
                .iter()
                .enumerate()
                .map(|(i, (labels, _))| (Arc::as_ptr(labels) as usize, i))
                .collect();
            for t in &self.tables {
                if let Some(&i) = position.get(&(Arc::as_ptr(&t.recording_labels) as usize)) {
                    tables[i].push(t);
                }
            }
            crate::process::form(&self.sources, &mut |source, lo, hi| {
                any_row_in(&tables[source], lo, hi)
            })
        })
    }

    /// What a query reads as one recording: each process group, or each part
    /// of one whose sources carry different values of a label, with the
    /// labels its series carry (see [`crate::process::read_groups`]). A
    /// source in no multi-source process group is a read group of one,
    /// labelled with its own labels. Worked out on first use and kept.
    pub fn read_groups(&self) -> &[ReadGroup] {
        &self.read_group_index().groups
    }

    /// The read group of `table`'s source, as an index into
    /// [`read_groups`](Self::read_groups); `None` for a table of another
    /// reader.
    pub fn read_group_of(&self, table: &SamplerReader) -> Option<usize> {
        let position = self.source_position(table)?;
        Some(self.read_group_index().of[position])
    }

    /// The position of `table`'s source in [`sources`](Self::sources);
    /// `None` for a table of another reader.
    pub fn source_position(&self, table: &SamplerReader) -> Option<usize> {
        self.read_group_index()
            .positions
            .get(&(Arc::as_ptr(&table.recording_labels) as usize))
            .copied()
    }

    fn read_group_index(&self) -> &ReadGroupIndex {
        self.read_groups.get_or_init(|| {
            // A reader of one source has nothing to group, and is not made
            // to read any rows to find that out.
            let groups = if self.members.len() < 2 {
                crate::process::read_groups(&self.sources, &ProcessGroups::default())
            } else {
                crate::process::read_groups(&self.sources, self.process_groups())
            };
            ReadGroupIndex::new(groups, &self.members)
        })
    }

    /// Make this reader one read group labelled `labels`, the labels its
    /// group has among the archive's sources. Those can hold a value its own
    /// sources do not carry (a key filled from a copy outside the group, by
    /// [`crate::process::read_groups`]), which a grouping of its own sources
    /// alone would not find.
    fn set_read_group(&mut self, labels: BTreeMap<String, String>) {
        let groups = vec![ReadGroup {
            sources: (0..self.members.len()).collect(),
            labels,
        }];
        self.read_groups = std::sync::OnceLock::from(ReadGroupIndex::new(groups, &self.members));
        self.series_labels = std::sync::OnceLock::new();
    }

    /// The labels each recording's series carry: its read group's labels
    /// ([`read_groups`](Self::read_groups)), so the sources of one process
    /// group carry one label set and their series union; and for read groups
    /// whose labels other than internal ones are equal
    /// ([`is_matched_label`](crate::labels::is_matched_label): two processes
    /// recorded with equal labels, or an endpoint's recordings before and
    /// after a producer change, which differ in `__producer__`),
    /// [`RECORDING_LABEL`](crate::labeled::RECORDING_LABEL) with the
    /// catalog index of the group's first source, so their series stay
    /// apart. Twins are found among every read group, tables or not, and the
    /// index does not change when a live archive is reopened. Recordings of
    /// readers joined by [`flatten`](Self::flatten) can share an index; a set
    /// of twins in which two do is numbered by the position of each group's
    /// first source instead.
    pub fn series_labels(&self) -> &SeriesLabels {
        self.series_labels.get_or_init(|| {
            let ReadGroupIndex { groups, of, .. } = self.read_group_index();
            // Read groups by the labels a query matches on (internal labels
            // other than `__recording__` left out: `__producer__` differs
            // between an endpoint's recordings before and after a producer
            // change, and `without` and binary matching ignore it), so twins
            // are found in one pass.
            let matched: Vec<BTreeMap<String, String>> = groups
                .iter()
                .map(|group| {
                    group
                        .labels
                        .iter()
                        .filter(|(k, _)| crate::labels::is_matched_label(k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                })
                .collect();
            let mut twins: HashMap<&BTreeMap<String, String>, Vec<usize>> = HashMap::new();
            for (g, key) in matched.iter().enumerate() {
                twins.entry(key).or_default().push(g);
            }
            let first = |g: usize| groups[g].sources[0];
            let labelled: Vec<Labels> = groups
                .iter()
                .enumerate()
                .map(|(g, group)| {
                    let mut extra = Labels {
                        inner: group.labels.clone(),
                    };
                    let set = &twins[&matched[g]];
                    if set.len() > 1 {
                        // Catalog indices, unless two twins share one (readers
                        // of different archives joined by `flatten`): then the
                        // whole set is numbered by position, so no two collide.
                        let mut indices: Vec<usize> =
                            set.iter().map(|t| self.members[first(*t)].1).collect();
                        indices.sort_unstable();
                        indices.dedup();
                        let value = if indices.len() == set.len() {
                            self.members[first(g)].1
                        } else {
                            first(g)
                        };
                        extra.inner.insert(
                            crate::labeled::RECORDING_LABEL.to_string(),
                            value.to_string(),
                        );
                    }
                    extra
                })
                .collect();
            let by_recording = self
                .members
                .iter()
                .enumerate()
                .map(|(position, (labels, _))| {
                    (Arc::as_ptr(labels) as usize, labelled[of[position]].clone())
                })
                .collect();
            SeriesLabels { by_recording }
        })
    }

    /// The union of the tables' probed spans, in nanoseconds. From the
    /// probe, not the readers: this is asked before any query runs, and
    /// opening every table to answer it would undo the lazy build.
    pub fn time_range_ns(&self) -> Option<(u64, u64)> {
        tables_span(&self.tables)
    }

    /// The finest probed table interval in seconds, or 1.0 when no table
    /// has one.
    pub fn interval(&self) -> f64 {
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

    /// The (first) recording's file-level metadata; for a reader of a read
    /// group from [`flatten_read_groups`](Self::flatten_read_groups), the
    /// group's ([`crate::process::merge_metadata`]).
    pub fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    /// The archive's display name, when it has one.
    pub fn filename(&self) -> Option<String> {
        self.filename.clone()
    }
}

/// The first bytes of the file at `path`, enough to tell the containers
/// [`refuse_legacy`] refuses: up to the end of a tar header.
fn file_head(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut head = Vec::with_capacity(TAR_HEADER);
    std::fs::File::open(path)
        .and_then(|f| f.take(TAR_HEADER as u64).read_to_end(&mut head))
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    Ok(head)
}

/// A tar header's length, and where its `ustar` magic sits.
const TAR_HEADER: usize = 512;
const TAR_MAGIC_AT: usize = 257;

/// Refuse a recording whose first bytes are one of the legacy containers,
/// which only a conversion reads: a standalone parquet file (`PAR1`), a
/// SQLite file without dendro's stamp (a v3 `.rez`), or a tar (`ustar` at
/// 257, a v1 or v2 `.rez`). Anything else is left to the dendro open.
fn refuse_legacy(head: &[u8], name: &str) -> Result<(), String> {
    if crate::DendroCatalog::is_archive_bytes(head) {
        return Ok(());
    }
    let kind = if head.starts_with(b"PAR1") {
        "a standalone parquet file"
    } else if head.starts_with(b"SQLite format 3\0") {
        "a SQLite file without dendro's stamp (a v3 .rez)"
    } else if head.get(TAR_MAGIC_AT..TAR_MAGIC_AT + 5) == Some(b"ustar".as_slice()) {
        "a tar archive (a v1 or v2 .rez)"
    } else {
        return Ok(());
    };
    Err(format!(
        "{name} is {kind}, not a dendro archive; convert it to a .dendro with `rezolus \
         recording upgrade` or metriken_storage::to_dendro::convert_path, and open that"
    ))
}

/// Whether a source whose tables are `tables` holds a row in `[lo, hi]`, as
/// [`crate::process::form`] asks: when any of them does, `Some(false)` when
/// every one can tell it does not, `None` otherwise.
fn any_row_in(tables: &[&SamplerReader], lo: u64, hi: u64) -> Option<bool> {
    let mut unknown = false;
    for t in tables {
        match t.has_row_in(lo, hi) {
            Some(true) => return Some(true),
            Some(false) => {}
            None => unknown = true,
        }
    }
    (!unknown).then_some(false)
}

/// The union of `tables`' probed spans.
fn tables_span(tables: &[Arc<SamplerReader>]) -> Option<(u64, u64)> {
    tables
        .iter()
        .filter_map(|t| t.span)
        .reduce(|(a0, a1), (b0, b1)| (a0.min(b0), a1.max(b1)))
}

fn union_sorted(iters: impl Iterator<Item = Vec<String>>) -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for v in iters {
        set.extend(v);
    }
    set.into_iter().collect()
}

/// [`ArchiveReader::read_groups`], each member's read group, and each
/// member's position by the address of its shared label map.
struct ReadGroupIndex {
    groups: Vec<ReadGroup>,
    of: Vec<usize>,
    positions: HashMap<usize, usize>,
}

impl ReadGroupIndex {
    /// The index of `groups` over a reader's `members`.
    fn new(groups: Vec<ReadGroup>, members: &[(Arc<BTreeMap<String, String>>, usize)]) -> Self {
        let mut of = vec![0; members.len()];
        for (g, group) in groups.iter().enumerate() {
            for &s in &group.sources {
                of[s] = g;
            }
        }
        let positions = members
            .iter()
            .enumerate()
            .map(|(i, (labels, _))| (Arc::as_ptr(labels) as usize, i))
            .collect();
        Self {
            groups,
            of,
            positions,
        }
    }
}

/// The labels each recording's series carry, from
/// [`ArchiveReader::series_labels`].
pub struct SeriesLabels {
    /// Keyed by the address of the recording's shared label map, which
    /// identifies the recording (see `SamplerReader::recording_labels`).
    by_recording: HashMap<usize, Labels>,
}

impl SeriesLabels {
    /// The labels the series of `table`'s recording carry.
    pub fn of(&self, table: &SamplerReader) -> &Labels {
        static EMPTY: std::sync::OnceLock<Labels> = std::sync::OnceLock::new();
        let ptr = Arc::as_ptr(&table.recording_labels) as usize;
        self.by_recording
            .get(&ptr)
            .unwrap_or_else(|| EMPTY.get_or_init(Labels::default))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A names index gives the interval a probe of its footer would read,
    /// and nothing when the probe would fail, so the segment is probed.
    #[test]
    fn an_indexed_interval_is_what_the_footer_probe_reads() {
        let names = |interval: Option<&str>, format: Option<&str>| crate::names::SegmentNames {
            sampling_interval_ms: interval.map(str::to_string),
            format: format.map(str::to_string),
            ..Default::default()
        };
        assert_eq!(indexed_intervals(&names(None, None)), Some((1.0, None)));
        assert_eq!(
            indexed_intervals(&names(Some("100"), Some("1"))),
            Some((0.1, Some(0.1)))
        );
        assert_eq!(
            indexed_intervals(&names(Some("0"), None)),
            Some((0.0, None))
        );
        assert_eq!(indexed_intervals(&names(Some("x"), None)), None);
        let newer = (crate::format::FORMAT_VERSION + 1).to_string();
        assert_eq!(indexed_intervals(&names(None, Some(&newer))), None);
        let other_layout = crate::names::SegmentNames {
            layout: Some("columnar".into()),
            ..Default::default()
        };
        assert_eq!(indexed_intervals(&other_layout), None);
        let long = crate::names::SegmentNames {
            layout: Some(crate::long::LAYOUT_LONG.into()),
            ..Default::default()
        };
        assert_eq!(indexed_intervals(&long), Some((1.0, None)));
    }

    /// A later row for an occupant, even with other labels, is dropped; the
    /// first rows keep their order.
    #[test]
    fn first_per_occupant_keeps_each_first_row_in_order() {
        let occ = |n: u64, comm: &str| Occupant {
            occupant: n,
            labels: [("comm".to_string(), comm.to_string())].into(),
        };
        let kept = first_per_occupant([occ(5, "a"), occ(3, "b"), occ(5, "c"), occ(3, "b")]);
        let got: Vec<(u64, &str)> = kept
            .iter()
            .map(|o| (o.occupant, o.labels["comm"].as_str()))
            .collect();
        assert_eq!(got, vec![(5, "a"), (3, "b")]);
    }
}
